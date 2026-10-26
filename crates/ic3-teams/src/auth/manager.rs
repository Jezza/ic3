//! The token manager: one background task that owns the access token and the
//! Skype token, keeps both fresh (spec §4.3) and publishes every change
//! through a `watch` channel.
//!
//! Refreshes are single-flight by construction (spec §4.4.2): every request
//! goes through the task's queue, so at most one refresh is ever in progress,
//! and requests that arrive while one runs are answered with its result.
//!
//! The task is spawned lazily by `Tokens` (decision #20): building a client
//! needs neither a runtime nor the network.
//!
//! Tokens for other resources (`Scoped`: the CSA token of spec A.2, the
//! Microsoft Graph token for files) are separate, lazy leases beside them,
//! granted from the same refresh token: each on its first request
//! ([`TokenHandle::csa_token`], [`TokenHandle::graph_token`]) and renewed on
//! demand in its last fifth, never on a timer. Each has its own
//! single-flight group and never changes the main state, the main tokens or
//! their retry timer; only an `invalid_grant` reaches the main path, as one
//! access-token refresh (decision #49).

use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex, OnceLock, PoisonError};
use std::time::Duration;

use tokio::sync::{mpsc, oneshot, watch};
use tokio::task::JoinHandle;
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;

use super::oauth::{self, GrantOutcome, TokenResponse};
use super::{CSA_SCOPE, CsaAudience, Endpoints, GRAPH_SCOPE, StoredSession, TokenStore};
use crate::{Error, Result, Secret, Timestamp, now_ms};

/// The wall clock (ms since the epoch); injectable so tests can jump it.
pub(crate) type WallClock = Arc<dyn Fn() -> Timestamp + Send + Sync>;

/// How long to wait before retrying a refresh that failed for a reason other
/// than a dead session.
const RETRY_AFTER: Duration = Duration::from_secs(30);
/// Lifetimes beyond this are clamped so instant arithmetic cannot overflow.
const MAX_LIFETIME: Duration = Duration::from_secs(365 * 24 * 60 * 60);
/// Shorter lifetimes are treated as this long, so a server reporting a tiny
/// (or zero) `expires_in` cannot drive a refresh hot loop.
const MIN_LIFETIME: Duration = Duration::from_secs(60);
/// Requests waiting for the manager; senders wait when it is full.
const REQUEST_QUEUE: usize = 32;

/// Which token a caller wants renewed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RefreshKind {
    /// The Azure AD access token (and, if the Skype token is missing, that
    /// too, since `Ready` means both are present).
    AccessToken,
    /// The Skype token via `authz`; the access token is renewed first when it
    /// is missing or in the last fifth of its lifetime.
    SkypeToken,
    /// The CSA token, a separate grant (spec A.2); never changes the main
    /// state. Under [`CsaAudience::Spaces`] it means `AccessToken`.
    CsaToken,
    /// The Microsoft Graph token, a separate grant like the CSA token;
    /// never changes the main state.
    GraphToken,
}

impl RefreshKind {
    /// The scoped token this kind renews; `None` for the main tokens.
    fn scoped(self) -> Option<Scoped> {
        match self {
            RefreshKind::AccessToken | RefreshKind::SkypeToken => None,
            RefreshKind::CsaToken => Some(Scoped::Csa),
            RefreshKind::GraphToken => Some(Scoped::Graph),
        }
    }
}

/// A token for a resource other than the main one, granted on demand from
/// the session's refresh token (a lazy lease of its own; see the module
/// docs).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) enum Scoped {
    /// CSA (`chatsvcagg`, spec A.2), under [`CsaAudience::Chatsvcagg`].
    Csa,
    /// Microsoft Graph (shared files).
    Graph,
}

impl Scoped {
    const ALL: [Scoped; 2] = [Scoped::Csa, Scoped::Graph];

    /// The v2 `scope` the grant asks for.
    fn scope(self) -> &'static str {
        match self {
            Scoped::Csa => CSA_SCOPE,
            Scoped::Graph => GRAPH_SCOPE,
        }
    }

    /// For log lines.
    fn label(self) -> &'static str {
        match self {
            Scoped::Csa => "CSA",
            Scoped::Graph => "Graph",
        }
    }

    /// What a 401 with this token renews.
    pub(crate) fn refresh_kind(self) -> RefreshKind {
        match self {
            Scoped::Csa => RefreshKind::CsaToken,
            Scoped::Graph => RefreshKind::GraphToken,
        }
    }
}

/// Where the manager is in its life.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TokenState {
    /// A refresh is in progress and there is no complete set of tokens yet.
    Starting,
    /// Both tokens are present and being kept fresh.
    Ready,
    /// The session is gone; the user must sign in again.
    ReauthRequired,
    /// The last refresh failed for a reason other than a dead session; the
    /// message is the error. A retry is due in 30 s. Earlier tokens stay in
    /// the snapshot until they actually expire, then they are dropped.
    Failed(String),
}

/// What the manager publishes: the current tokens and where chat lives.
#[derive(Clone, Debug)]
pub struct TokenSnapshot {
    /// Increases with every published change.
    pub version: u64,
    /// See [`TokenState`].
    pub state: TokenState,
    /// Azure AD access token (`Authorization: Bearer`).
    pub access_token: Option<Secret<String>>,
    /// Wall clock, milliseconds; 0 without a token.
    pub access_expires_at: Timestamp,
    /// Skype token (`X-Skypetoken`).
    pub skype_token: Option<Secret<String>>,
    /// Wall clock, milliseconds; 0 without a token.
    pub skype_expires_at: Timestamp,
    /// Chat-service base URL from `authz`, without a trailing slash.
    pub chat_service: Option<String>,
    /// Region reported by `authz`.
    pub region: Option<String>,
    /// AMS base URL from `authz` (`regionGtms.ams`), when given (§9.27).
    pub ams_service: Option<String>,
    /// The session's endpoint id (trouter's `epid`).
    pub endpoint_id: Option<String>,
    /// The CSA token (`Authorization: Bearer` on `/api/csa/…`), granted on
    /// first use by [`TokenHandle::csa_token`]; `None` until then.
    pub csa_token: Option<Secret<String>>,
    /// Wall clock, milliseconds; 0 without a CSA token.
    pub csa_expires_at: Timestamp,
    /// Bumped whenever the CSA token changes (granted or cleared).
    pub csa_version: u64,
}

impl TokenSnapshot {
    /// No tokens, nothing known.
    fn empty(version: u64, state: TokenState) -> TokenSnapshot {
        TokenSnapshot {
            version,
            state,
            access_token: None,
            access_expires_at: 0,
            skype_token: None,
            skype_expires_at: 0,
            chat_service: None,
            region: None,
            ams_service: None,
            endpoint_id: None,
            csa_token: None,
            csa_expires_at: 0,
            csa_version: 0,
        }
    }
}

/// A caller's side of the manager: read the current tokens, wait for them,
/// or ask for a refresh.
#[derive(Clone, Debug)]
pub struct TokenHandle {
    rx: watch::Receiver<TokenSnapshot>,
    tx: mpsc::Sender<Request>,
    /// Which token CSA requests carry; fixed when the manager starts.
    csa_audience: CsaAudience,
}

impl TokenHandle {
    /// The latest published snapshot.
    pub fn current(&self) -> TokenSnapshot {
        self.rx.borrow().clone()
    }

    /// A receiver that sees every published snapshot.
    pub fn watch(&self) -> watch::Receiver<TokenSnapshot> {
        self.rx.clone()
    }

    /// Waits until both tokens are present. While refreshes are failing
    /// (`Failed`) the kept tokens are still returned as long as neither has
    /// expired; once one has, `Err(Protocol)` with the failure (the manager
    /// keeps retrying). `Err(ReauthRequired)` when the user must sign in;
    /// `Err(Protocol)` too when the manager has stopped.
    pub async fn ready(&self) -> Result<TokenSnapshot> {
        let mut rx = self.rx.clone();
        loop {
            if rx.has_changed().is_err() {
                return Err(stopped());
            }
            let snap = rx.borrow_and_update().clone();
            let complete = snap.access_token.is_some() && snap.skype_token.is_some();
            match &snap.state {
                TokenState::Ready | TokenState::Failed(_) if complete => return Ok(snap),
                TokenState::ReauthRequired => return Err(Error::ReauthRequired),
                TokenState::Failed(m) => {
                    return Err(Error::Protocol(format!("token refresh failed: {m}")));
                }
                _ => rx.changed().await.map_err(|_| stopped())?,
            }
        }
    }

    /// Renews `kind` unless that token has already been renewed in a
    /// snapshot newer than `observed_version` (then the current snapshot is
    /// returned without any network). Concurrent callers share one refresh.
    pub async fn force_refresh(
        &self,
        kind: RefreshKind,
        observed_version: u64,
    ) -> Result<TokenSnapshot> {
        self.request(|reply| Request::Refresh {
            kind,
            observed: observed_version,
            reply,
        })
        .await
    }

    /// A bearer token for CSA (`/api/csa/…`, spec A.2). Under
    /// [`CsaAudience::Chatsvcagg`] the manager answers from the CSA lease
    /// while it is outside its last fifth and grants a new one otherwise;
    /// under [`CsaAudience::Spaces`] it is the main access token. A CSA
    /// failure never changes the main session (decision #49);
    /// `Err(ReauthRequired)` when there is no session.
    pub async fn csa_token(&self) -> Result<Secret<String>> {
        Ok(self.csa_token_versioned().await?.0)
    }

    /// [`csa_token`](Self::csa_token) plus the version to hand
    /// [`force_refresh`](Self::force_refresh) (with
    /// [`csa_refresh_kind`](Self::csa_refresh_kind)) after a 401.
    pub(crate) async fn csa_token_versioned(&self) -> Result<(Secret<String>, u64)> {
        match self.csa_audience {
            CsaAudience::Spaces => {
                let snap = self.ready().await?;
                let token = snap
                    .access_token
                    .clone()
                    .ok_or_else(|| Error::Protocol("no access token".into()))?;
                Ok((token, snap.version))
            }
            CsaAudience::Chatsvcagg => self.scoped_token_versioned(Scoped::Csa).await,
        }
    }

    /// A bearer token for Microsoft Graph (shared files), granted on first
    /// use and renewed in its last fifth, like the CSA token: a failure
    /// never changes the main session; `Err(ReauthRequired)` when there is
    /// no session.
    pub async fn graph_token(&self) -> Result<Secret<String>> {
        Ok(self.scoped_token_versioned(Scoped::Graph).await?.0)
    }

    /// The `scope` token from its lease, or a new grant, plus the version
    /// to hand [`force_refresh`](Self::force_refresh) (with
    /// [`Scoped::refresh_kind`]) after a 401.
    pub(crate) async fn scoped_token_versioned(
        &self,
        scope: Scoped,
    ) -> Result<(Secret<String>, u64)> {
        self.request(|reply| Request::Scoped { scope, reply }).await
    }

    /// What a CSA 401 renews: the CSA token, or the access token under
    /// [`CsaAudience::Spaces`].
    pub(crate) fn csa_refresh_kind(&self) -> RefreshKind {
        match self.csa_audience {
            CsaAudience::Chatsvcagg => RefreshKind::CsaToken,
            CsaAudience::Spaces => RefreshKind::AccessToken,
        }
    }

    /// Adopts a freshly granted session (after device-code login): persists
    /// it, uses `access` as-is and fetches the Skype token.
    pub(crate) async fn install(
        &self,
        session: StoredSession,
        access: TokenResponse,
    ) -> Result<TokenSnapshot> {
        self.request(|reply| Request::Install {
            session,
            access,
            reply,
        })
        .await
    }

    /// Forgets the session (logout): clears the store and the tokens.
    pub(crate) async fn clear(&self) -> Result<()> {
        self.request(|reply| Request::Clear { reply }).await
    }

    async fn request<T>(
        &self,
        make: impl FnOnce(oneshot::Sender<Result<T>>) -> Request,
    ) -> Result<T> {
        let (reply, answer) = oneshot::channel();
        self.tx.send(make(reply)).await.map_err(|_| stopped())?;
        answer.await.map_err(|_| stopped())?
    }
}

fn stopped() -> Error {
    Error::Protocol("token manager stopped".into())
}

/// What the manager needs; cloned into the task.
#[derive(Clone)]
pub(crate) struct ManagerDeps {
    pub http: reqwest::Client,
    pub endpoints: Endpoints,
    pub store: Arc<dyn TokenStore>,
    pub cancel: CancellationToken,
    /// Which token CSA requests carry (spec A.2).
    pub csa_audience: CsaAudience,
}

/// Starts the manager task. It loads the stored session and refreshes at
/// once; with no session it reports `ReauthRequired` and waits for
/// `install`.
pub(crate) fn spawn_token_manager(deps: ManagerDeps) -> (TokenHandle, JoinHandle<()>) {
    spawn_token_manager_with_clock(deps, Arc::new(now_ms))
}

/// [`spawn_token_manager`] with the wall clock leases are also checked
/// against.
pub(crate) fn spawn_token_manager_with_clock(
    deps: ManagerDeps,
    clock: WallClock,
) -> (TokenHandle, JoinHandle<()>) {
    let snap = TokenSnapshot::empty(0, TokenState::Starting);
    let (watch_tx, watch_rx) = watch::channel(snap.clone());
    let (req_tx, req_rx) = mpsc::channel(REQUEST_QUEUE);
    let csa_audience = deps.csa_audience;
    let manager = Manager {
        deps,
        clock,
        tx: watch_tx,
        session: None,
        snap,
        access: None,
        skype: None,
        scoped: HashMap::new(),
        retry: None,
        access_version: 0,
        skype_version: 0,
    };
    let join = tokio::spawn(manager.run(req_rx));
    (
        TokenHandle {
            rx: watch_rx,
            tx: req_tx,
            csa_audience,
        },
        join,
    )
}

enum Request {
    Refresh {
        kind: RefreshKind,
        observed: u64,
        reply: oneshot::Sender<Result<TokenSnapshot>>,
    },
    Install {
        session: StoredSession,
        access: TokenResponse,
        reply: oneshot::Sender<Result<TokenSnapshot>>,
    },
    Clear {
        reply: oneshot::Sender<Result<()>>,
    },
    /// A scoped token (CSA only under `CsaAudience::Chatsvcagg`).
    Scoped {
        scope: Scoped,
        reply: oneshot::Sender<Result<(Secret<String>, u64)>>,
    },
}

impl Request {
    /// Answers with a copy of `e` (a shared failure).
    fn fail(self, e: &Error) {
        match self {
            Request::Refresh { reply, .. } | Request::Install { reply, .. } => {
                let _ = reply.send(Err(clone_err(e)));
            }
            Request::Scoped { reply, .. } => {
                let _ = reply.send(Err(clone_err(e)));
            }
            Request::Clear { reply } => {
                let _ = reply.send(Err(clone_err(e)));
            }
        }
    }
}

/// A single-flight group (spec A.2): an attempt's failure is shared only
/// with the waiters of its own group.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Flight {
    /// Access and Skype token refreshes.
    Main,
    /// One scoped token's grants.
    Scoped(Scoped),
}

/// When a token was obtained and how long it lives, on the monotonic clock
/// (so it follows `tokio::time::pause` in tests) and on the wall clock: the
/// monotonic clock stops while the machine is suspended, the token's real
/// expiry does not.
#[derive(Clone, Copy, Debug)]
struct Lease {
    issued: Instant,
    lifetime: Duration,
    /// Wall-clock issue time (ms).
    wall_issued: Timestamp,
}

impl Lease {
    fn new(expires_in_secs: u64, wall_now: Timestamp) -> Lease {
        Lease {
            issued: Instant::now(),
            lifetime: Duration::from_secs(expires_in_secs).clamp(MIN_LIFETIME, MAX_LIFETIME),
            wall_issued: wall_now,
        }
    }

    /// Refresh at 80 % of the lifetime (spec §4.3).
    fn due_at(&self) -> Instant {
        self.issued + self.due_after()
    }

    fn due_after(&self) -> Duration {
        self.lifetime.saturating_mul(4) / 5
    }

    /// In the last fifth of its lifetime (or beyond), by either clock.
    fn is_due(&self, wall_now: Timestamp) -> bool {
        Instant::now() >= self.due_at() || wall_now >= self.wall_after(self.due_after())
    }

    fn expires_at(&self) -> Instant {
        self.issued + self.lifetime
    }

    /// Past its lifetime, by either clock.
    fn is_expired(&self, wall_now: Timestamp) -> bool {
        Instant::now() >= self.expires_at() || wall_now >= self.wall_after(self.lifetime)
    }

    fn wall_after(&self, d: Duration) -> Timestamp {
        let ms = i64::try_from(d.as_millis()).unwrap_or(i64::MAX);
        self.wall_issued.saturating_add(ms)
    }
}

/// One scoped token: absent until granted, its lease, and a version bumped
/// whenever it changes (granted or cleared).
#[derive(Default)]
struct ScopedLease {
    token: Option<Secret<String>>,
    lease: Option<Lease>,
    /// Wall clock, milliseconds; 0 without a token.
    expires_at: Timestamp,
    version: u64,
}

struct Manager {
    deps: ManagerDeps,
    clock: WallClock,
    tx: watch::Sender<TokenSnapshot>,
    session: Option<StoredSession>,
    snap: TokenSnapshot,
    access: Option<Lease>,
    skype: Option<Lease>,
    /// The scoped tokens (absent until first granted or cleared); not part
    /// of `next_due` (no timer).
    scoped: HashMap<Scoped, ScopedLease>,
    /// After a failure: when to try again, and what. `Some` exactly while
    /// the state is `Failed`.
    retry: Option<(Instant, RefreshKind)>,
    /// The snapshot version that first carried the current access token.
    access_version: u64,
    /// The snapshot version that first carried the current Skype token.
    skype_version: u64,
}

impl Manager {
    async fn run(mut self, mut rx: mpsc::Receiver<Request>) {
        let cancel = self.deps.cancel.clone();
        self.load_session().await;
        loop {
            let due = self.next_due();
            tokio::select! {
                biased;
                () = cancel.cancelled() => break,
                req = rx.recv() => match req {
                    None => break,
                    Some(req) => self.handle_all(req, &mut rx).await,
                },
                () = sleep_until_opt(due) => self.scheduled().await,
            }
        }
        tracing::debug!("token manager stopped");
    }

    async fn load_session(&mut self) {
        match self.deps.store.load().await {
            Ok(Some(session)) => {
                self.snap.endpoint_id = Some(session.endpoint_id.clone());
                self.session = Some(session);
                // The outcome is in the published snapshot.
                let _ = self.refresh(RefreshKind::SkypeToken).await;
            }
            Ok(None) => self.set_state(TokenState::ReauthRequired),
            Err(e) => {
                tracing::warn!(error = %e, "cannot read the stored session; sign-in required");
                self.set_state(TokenState::ReauthRequired);
            }
        }
    }

    /// The next scheduled wake-up: the earlier token's 80 % mark while
    /// `Ready`; the retry or the earlier kept token's expiry while `Failed`;
    /// nothing otherwise.
    fn next_due(&self) -> Option<Instant> {
        let leases = self.access.iter().chain(&self.skype);
        match self.snap.state {
            TokenState::Ready => leases.map(Lease::due_at).min(),
            TokenState::Failed(_) => {
                let retry = self.retry.map(|(at, _)| at);
                retry.into_iter().chain(leases.map(Lease::expires_at)).min()
            }
            TokenState::Starting | TokenState::ReauthRequired => None,
        }
    }

    async fn scheduled(&mut self) {
        if matches!(self.snap.state, TokenState::Failed(_)) {
            self.drop_expired();
            if let Some((at, kind)) = self.retry
                && Instant::now() >= at
            {
                let _ = self.refresh(kind).await;
            }
            return;
        }
        let now = (self.clock)();
        let kind = if self.skype.is_none_or(|l| l.is_due(now)) {
            RefreshKind::SkypeToken
        } else {
            RefreshKind::AccessToken
        };
        let _ = self.refresh(kind).await;
    }

    /// While `Failed`, a kept token stays usable until its lifetime is up;
    /// then it leaves the snapshot so `ready()` stops handing it out.
    fn drop_expired(&mut self) {
        let mut dropped = false;
        let now = (self.clock)();
        if self.access.is_some_and(|l| l.is_expired(now)) {
            self.access = None;
            self.snap.access_token = None;
            self.snap.access_expires_at = 0;
            dropped = true;
        }
        if self.skype.is_some_and(|l| l.is_expired(now)) {
            self.skype = None;
            self.snap.skype_token = None;
            self.snap.skype_expires_at = 0;
            dropped = true;
        }
        if dropped {
            tracing::warn!("a token expired while refreshes keep failing");
            self.publish();
        }
    }

    async fn handle_all(&mut self, first: Request, rx: &mut mpsc::Receiver<Request>) {
        let mut queue = VecDeque::from([first]);
        while let Some(req) = queue.pop_front() {
            queue.extend(self.handle(req, rx).await);
        }
    }

    /// Serves one request. Returns requests taken off the queue while
    /// serving it that still need an answer.
    async fn handle(&mut self, req: Request, rx: &mut mpsc::Receiver<Request>) -> Vec<Request> {
        match req {
            Request::Refresh {
                kind,
                observed,
                reply,
            } => match self.effective(kind) {
                // A scoped lease: its own version, never the main state or
                // the 30 s spacing.
                kind if let Some(scope) = kind.scoped() => {
                    let lease = self.scoped.get(&scope);
                    if lease.is_some_and(|l| l.token.is_some() && l.version > observed) {
                        let _ = reply.send(Ok(self.snap.clone()));
                        return Vec::new();
                    }
                    let result = self.refresh_scoped(scope).await.map(|_| self.snap.clone());
                    let _ = reply.send(clone_result(&result));
                    self.drain(rx, Flight::Scoped(scope), result.as_ref().err())
                }
                kind => {
                    // Renewed since the caller looked (by a schedule, or by
                    // the refresh it queued behind): answer without network.
                    // Only a renewal of the token the caller asked for counts.
                    if self.snap.state == TokenState::Ready && self.renewed_version(kind) > observed
                    {
                        let _ = reply.send(Ok(self.snap.clone()));
                        return Vec::new();
                    }
                    // While failing, refreshes keep the 30 s spacing: a
                    // request before the retry is due hears the last failure,
                    // so failing requests cannot each start a doomed refresh.
                    if let (TokenState::Failed(m), Some((at, _))) = (&self.snap.state, self.retry)
                        && Instant::now() < at
                    {
                        let _ =
                            reply.send(Err(Error::Protocol(format!("token refresh failed: {m}"))));
                        return Vec::new();
                    }
                    let result = self.refresh(kind).await;
                    let _ = reply.send(clone_result(&result));
                    // Single-flight: whoever queued up meanwhile is served
                    // next. After a success the check above answers each of
                    // them from this refresh without network, unless it
                    // renewed the other token only (then that caller gets one
                    // refresh of its own). A failure is shared with the main
                    // waiters only; scoped waiters are served as usual.
                    self.drain(rx, Flight::Main, result.as_ref().err())
                }
            },
            Request::Scoped { scope, reply } => {
                if let Some(lease) = self.scoped_from_lease(scope) {
                    let _ = reply.send(Ok(lease));
                    return Vec::new();
                }
                let result = self.refresh_scoped(scope).await;
                let err = result.as_ref().err().map(clone_err);
                let _ = reply.send(result);
                self.drain(rx, Flight::Scoped(scope), err.as_ref())
            }
            Request::Install {
                session,
                access,
                reply,
            } => {
                let _ = reply.send(self.install(session, access).await);
                Vec::new()
            }
            Request::Clear { reply } => {
                let _ = reply.send(self.forget_session().await);
                Vec::new()
            }
        }
    }

    /// `CsaToken` means `AccessToken` when CSA uses the main token.
    fn effective(&self, kind: RefreshKind) -> RefreshKind {
        if kind == RefreshKind::CsaToken && self.deps.csa_audience == CsaAudience::Spaces {
            RefreshKind::AccessToken
        } else {
            kind
        }
    }

    /// The single-flight group of `req`; `None` for install / clear.
    fn flight(&self, req: &Request) -> Option<Flight> {
        match req {
            Request::Refresh { kind, .. } => Some(
                self.effective(*kind)
                    .scoped()
                    .map_or(Flight::Main, Flight::Scoped),
            ),
            Request::Scoped { scope, .. } => Some(Flight::Scoped(*scope)),
            Request::Install { .. } | Request::Clear { .. } => None,
        }
    }

    /// Takes every queued request: after a failed attempt (`err`) those of
    /// `flight` share it; the rest — and all of them after a success, so the
    /// version and lease checks answer them without network — are returned
    /// to be served next.
    fn drain(
        &self,
        rx: &mut mpsc::Receiver<Request>,
        flight: Flight,
        err: Option<&Error>,
    ) -> Vec<Request> {
        let mut deferred = Vec::new();
        while let Ok(req) = rx.try_recv() {
            match err {
                Some(e) if self.flight(&req) == Some(flight) => req.fail(e),
                _ => deferred.push(req),
            }
        }
        deferred
    }

    /// The `scope` token and its version, from the lease while it is
    /// outside its last fifth.
    fn scoped_from_lease(&self, scope: Scoped) -> Option<(Secret<String>, u64)> {
        let now = (self.clock)();
        let l = self.scoped.get(&scope)?;
        let token = l.token.clone()?;
        l.lease
            .filter(|lease| !lease.is_due(now))
            .map(|_| (token, l.version))
    }

    /// One grant of a scoped token (spec A.2 for CSA): on success stores
    /// the token and its lease, bumps its version, publishes and persists a
    /// rotated refresh token. Never changes the main state, tokens or retry
    /// timer — except that an `invalid_grant` runs one main `AccessToken`
    /// refresh, whose own rules decide whether the session ends (decision
    /// #49). Transport failures are `Network`, an unparseable answer
    /// `Protocol`, OAuth errors `Http` with the raw body.
    async fn refresh_scoped(&mut self, scope: Scoped) -> Result<(Secret<String>, u64)> {
        let Some(session) = &self.session else {
            return Err(Error::ReauthRequired);
        };
        let outcome = oauth::refresh_grant(
            &self.deps.http,
            &self.deps.endpoints,
            &session.tenant,
            &session.refresh_token,
            scope.scope(),
        )
        .await?;
        match outcome {
            GrantOutcome::Granted(token) => {
                tracing::info!(
                    expires_in = token.expires_in,
                    "{} token granted",
                    scope.label()
                );
                let lease = Lease::new(token.expires_in, (self.clock)());
                let l = self.scoped.entry(scope).or_default();
                l.token = Some(token.access_token.clone());
                l.lease = Some(lease);
                l.expires_at = oauth::expiry_from_now(token.expires_in);
                l.version += 1;
                let version = l.version;
                self.mirror_scoped();
                self.publish();
                self.persist_rotated(token.refresh_token).await;
                Ok((token.access_token, version))
            }
            GrantOutcome::Refused {
                status,
                error,
                description,
                body,
            } => {
                tracing::warn!(status, error = ?error, description = ?description, "{} token grant refused", scope.label());
                // Not proof of a dead session (consent, pre-authorisation and
                // scope problems read the same): one main refresh decides —
                // unless the main path is already failing and its 30 s
                // retry is still pending (keep its spacing).
                let main_retry_pending = self.retry.is_some_and(|(at, _)| Instant::now() < at);
                if error.as_deref() == Some("invalid_grant")
                    && !main_retry_pending
                    && matches!(
                        self.refresh(RefreshKind::AccessToken).await,
                        Err(Error::ReauthRequired)
                    )
                {
                    return Err(Error::ReauthRequired);
                }
                Err(Error::Http { status, body })
            }
        }
    }

    /// Stores a rotated refresh token in the session and persists it: the
    /// one code path for every rotation (main and CSA grants).
    async fn persist_rotated(&mut self, rotated: Option<Secret<String>>) {
        if let (Some(rotated), Some(session)) = (rotated, &mut self.session) {
            session.refresh_token = rotated;
            if let Err(e) = self.deps.store.save(session).await {
                tracing::warn!(error = %e, "cannot persist the rotated refresh token; the next run will need a sign-in");
            }
        }
    }

    /// Drops every scoped token and its lease (a new session), keeping
    /// each version monotonic; does not publish.
    fn clear_scoped(&mut self) {
        for scope in Scoped::ALL {
            let l = self.scoped.entry(scope).or_default();
            l.token = None;
            l.lease = None;
            l.expires_at = 0;
            l.version += 1;
        }
        self.mirror_scoped();
    }

    /// Copies the CSA lease into the snapshot's `csa_*` fields (the Graph
    /// token is not published); does not publish.
    fn mirror_scoped(&mut self) {
        let csa = self.scoped.get(&Scoped::Csa);
        self.snap.csa_token = csa.and_then(|l| l.token.clone());
        self.snap.csa_expires_at = csa.map_or(0, |l| l.expires_at);
        self.snap.csa_version = csa.map_or(0, |l| l.version);
    }

    async fn install(
        &mut self,
        session: StoredSession,
        access: TokenResponse,
    ) -> Result<TokenSnapshot> {
        // Persist first: if that fails nothing changes and the caller hears.
        self.deps.store.save(&session).await?;
        self.snap.endpoint_id = Some(session.endpoint_id.clone());
        self.session = Some(session);
        self.skype = None;
        self.snap.skype_token = None;
        self.snap.skype_expires_at = 0;
        self.snap.chat_service = None;
        self.snap.region = None;
        self.snap.ams_service = None;
        self.clear_scoped();
        self.set_access(access.access_token, access.expires_in);
        self.retry = None;
        self.set_state(TokenState::Starting);
        self.refresh(RefreshKind::SkypeToken).await
    }

    /// Runs one refresh and records its outcome in the snapshot: `Ready` on
    /// success, `ReauthRequired` (store cleared) on a rejected refresh token,
    /// `Failed` with a retry in 30 s on anything else.
    async fn refresh(&mut self, kind: RefreshKind) -> Result<TokenSnapshot> {
        if self.session.is_none() {
            return Err(Error::ReauthRequired);
        }
        // A retry with no complete set of tokens is `Starting`; with kept
        // tokens it stays `Failed`, so they remain served meanwhile.
        if matches!(self.snap.state, TokenState::Failed(_))
            && (self.access.is_none() || self.skype.is_none())
        {
            self.set_state(TokenState::Starting);
        }
        match self.refresh_steps(kind).await {
            Ok(()) => {
                self.retry = None;
                self.snap.state = TokenState::Ready;
                self.publish();
                Ok(self.snap.clone())
            }
            Err(Error::ReauthRequired) => {
                tracing::warn!("the refresh token was rejected; sign-in required");
                if let Err(e) = self.forget_session().await {
                    tracing::warn!(error = %e, "cannot clear the stored session");
                }
                Err(Error::ReauthRequired)
            }
            Err(e) => {
                tracing::warn!(error = %e, ?kind, "token refresh failed; retrying in {} s", RETRY_AFTER.as_secs());
                self.retry = Some((Instant::now() + RETRY_AFTER, kind));
                self.snap.state = TokenState::Failed(e.to_string());
                self.publish();
                Err(e)
            }
        }
    }

    /// Access token first, then `authz` — never the other way round. An
    /// `authz` 401 with an access token not renewed in this refresh renews
    /// it once and runs `authz` again (the token may be dead before its
    /// time, or expired while the machine slept).
    async fn refresh_steps(&mut self, kind: RefreshKind) -> Result<()> {
        let now = (self.clock)();
        // Scoped kinds never reach the main path (`handle` routes them to
        // `refresh_scoped`); they would read as `AccessToken` here.
        let need_access = match kind {
            RefreshKind::AccessToken | RefreshKind::CsaToken | RefreshKind::GraphToken => true,
            RefreshKind::SkypeToken => self.access.is_none_or(|l| l.is_due(now)),
        };
        let need_skype = match kind {
            RefreshKind::SkypeToken => true,
            RefreshKind::AccessToken | RefreshKind::CsaToken | RefreshKind::GraphToken => {
                self.skype.is_none()
            }
        };
        if need_access {
            self.refresh_access().await?;
            if need_skype {
                self.publish();
            }
        }
        if need_skype {
            match self.refresh_skype().await {
                Err(Error::Http { status: 401, .. }) if !need_access => {
                    tracing::info!("authz rejected the access token; renewing it once");
                    self.refresh_access().await?;
                    self.publish();
                    self.refresh_skype().await?;
                }
                r => r?,
            }
        }
        Ok(())
    }

    async fn refresh_access(&mut self) -> Result<()> {
        let Some(session) = &self.session else {
            return Err(Error::ReauthRequired);
        };
        let token = oauth::refresh_access_token(
            &self.deps.http,
            &self.deps.endpoints,
            &session.tenant,
            &session.refresh_token,
        )
        .await?;
        tracing::info!(expires_in = token.expires_in, "access token refreshed");
        self.set_access(token.access_token, token.expires_in);
        self.persist_rotated(token.refresh_token).await;
        Ok(())
    }

    async fn refresh_skype(&mut self) -> Result<()> {
        let Some(access) = &self.snap.access_token else {
            return Err(Error::Protocol("authz without an access token".into()));
        };
        let authz = oauth::authz(&self.deps.http, &self.deps.endpoints, access).await?;
        tracing::info!(expires_in = authz.expires_in, region = ?authz.region, "skype token refreshed");
        self.skype = Some(Lease::new(authz.expires_in, (self.clock)()));
        self.snap.skype_token = Some(authz.skype_token);
        self.snap.skype_expires_at = oauth::expiry_from_now(authz.expires_in);
        self.snap.chat_service = Some(authz.chat_service);
        self.snap.region = authz.region;
        self.snap.ams_service = authz.ams_service;
        Ok(())
    }

    fn set_access(&mut self, token: Secret<String>, expires_in: u64) {
        self.access = Some(Lease::new(expires_in, (self.clock)()));
        self.snap.access_token = Some(token);
        self.snap.access_expires_at = oauth::expiry_from_now(expires_in);
    }

    /// Drops the session and every token; the store is cleared first so a
    /// watcher reacting to `ReauthRequired` cannot race the old file.
    async fn forget_session(&mut self) -> Result<()> {
        let cleared = self.deps.store.clear().await;
        self.session = None;
        self.access = None;
        self.skype = None;
        self.retry = None;
        self.snap = TokenSnapshot::empty(self.snap.version, TokenState::ReauthRequired);
        // Scoped versions stay monotonic across a sign-out.
        self.clear_scoped();
        self.publish();
        cleared
    }

    fn set_state(&mut self, state: TokenState) {
        if self.snap.state != state {
            self.snap.state = state;
            self.publish();
        }
    }

    fn publish(&mut self) {
        self.snap.version += 1;
        let prev = self.tx.send_replace(self.snap.clone());
        if prev.access_token != self.snap.access_token {
            self.access_version = self.snap.version;
        }
        if prev.skype_token != self.snap.skype_token {
            self.skype_version = self.snap.version;
        }
    }

    /// The snapshot version that first carried the current token of `kind`.
    fn renewed_version(&self, kind: RefreshKind) -> u64 {
        match kind {
            RefreshKind::AccessToken => self.access_version,
            RefreshKind::SkypeToken => self.skype_version,
            RefreshKind::CsaToken | RefreshKind::GraphToken => kind
                .scoped()
                .and_then(|s| self.scoped.get(&s))
                .map_or(0, |l| l.version),
        }
    }
}

async fn sleep_until_opt(due: Option<Instant>) {
    match due {
        Some(at) => tokio::time::sleep_until(at).await,
        None => std::future::pending().await,
    }
}

fn clone_result(r: &Result<TokenSnapshot>) -> Result<TokenSnapshot> {
    match r {
        Ok(snap) => Ok(snap.clone()),
        Err(e) => Err(clone_err(e)),
    }
}

/// `Error` is not `Clone` (`Io` carries an `io::Error`); this is close enough
/// for the waiters that share one refresh's outcome.
fn clone_err(e: &Error) -> Error {
    match e {
        Error::ReauthRequired => Error::ReauthRequired,
        Error::Http { status, body } => Error::Http {
            status: *status,
            body: body.clone(),
        },
        Error::Protocol(m) => Error::Protocol(m.clone()),
        Error::Invalid(m) => Error::Invalid(m),
        Error::Network(m) => Error::Network(m.clone()),
        Error::Io(e) => Error::Io(std::io::Error::new(e.kind(), e.to_string())),
    }
}

/// The lazily started manager: `handle()` spawns it on first use, so
/// building a client needs no runtime and touches no network.
pub(crate) struct Tokens {
    deps: ManagerDeps,
    started: OnceLock<Started>,
}

struct Started {
    handle: TokenHandle,
    join: Mutex<Option<JoinHandle<()>>>,
}

impl Tokens {
    pub(crate) fn new(deps: ManagerDeps) -> Tokens {
        Tokens {
            deps,
            started: OnceLock::new(),
        }
    }

    /// The handle, starting the manager if this is the first call.
    pub(crate) async fn handle(&self) -> &TokenHandle {
        &self
            .started
            .get_or_init(|| {
                let (handle, join) = spawn_token_manager(self.deps.clone());
                Started {
                    handle,
                    join: Mutex::new(Some(join)),
                }
            })
            .handle
    }

    /// The handle if the manager has been started.
    pub(crate) fn try_handle(&self) -> Option<&TokenHandle> {
        self.started.get().map(|s| &s.handle)
    }

    /// Stops the manager and waits for it; nothing to do if it never started.
    pub(crate) async fn shutdown(&self) {
        let Some(started) = self.started.get() else {
            return;
        };
        self.deps.cancel.cancel();
        let join = started
            .join
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .take();
        if let Some(join) = join
            && let Err(e) = join.await
        {
            tracing::warn!(error = %e, "token manager task failed");
        }
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::Duration;

    use tokio::task::JoinHandle;
    use tokio_util::sync::CancellationToken;
    use wiremock::matchers::{body_string_contains, method, path};
    use wiremock::{Mock, MockServer, Respond, ResponseTemplate};

    use super::*;
    use crate::auth::oauth::TokenResponse;
    use crate::auth::{CsaAudience, Endpoints, MemoryTokenStore, StoredSession, TokenStore};
    use crate::{Error, Secret};

    const TOKEN: &str = "/organizations/oauth2/v2.0/token";
    const AUTHZ: &str = "/api/authsvc/v1.0/authz";

    /// Answers the n-th call (1-based) with `make(n)`, so every refresh hands
    /// out a new token.
    struct Counted<F>(AtomicU64, F);

    impl<F: Fn(u64) -> serde_json::Value + Send + Sync> Respond for Counted<F> {
        fn respond(&self, _: &wiremock::Request) -> ResponseTemplate {
            let n = self.0.fetch_add(1, Ordering::SeqCst) + 1;
            ResponseTemplate::new(200).set_body_json((self.1)(n))
        }
    }

    /// Refresh → `at-{n}` / `rt-{n}`; authz → `sk-{n}`, region `de`, chat
    /// service = the mock server.
    pub(crate) async fn mount_success_mocks(
        s: &MockServer,
        skype_expires: u64,
        access_expires: u64,
    ) {
        Mock::given(method("POST"))
            .and(path(TOKEN))
            .respond_with(Counted(AtomicU64::new(0), move |n| {
                serde_json::json!({
                    "access_token": format!("at-{n}"),
                    "refresh_token": format!("rt-{n}"),
                    "expires_in": access_expires,
                })
            }))
            .mount(s)
            .await;
        let chat = s.uri();
        Mock::given(method("POST"))
            .and(path(AUTHZ))
            .respond_with(Counted(AtomicU64::new(0), move |n| {
                serde_json::json!({
                    "tokens": {"skypeToken": format!("sk-{n}"), "expiresIn": skype_expires},
                    "region": "de",
                    "regionGtms": {"chatService": chat},
                })
            }))
            .mount(s)
            .await;
    }

    pub(crate) fn session(rt: &str) -> StoredSession {
        StoredSession {
            refresh_token: Secret::new(rt.into()),
            tenant: "organizations".into(),
            endpoint_id: "ep".into(),
        }
    }

    /// A client without the connection pool's idle timer. Under paused time
    /// the runtime auto-advances the clock to the next pending timer whenever
    /// it parks on real I/O (every wiremock round-trip), so reqwest's default
    /// 90 s pool timer would jump the clock mid-request and with it the
    /// refresh schedule.
    pub(crate) fn quiet_client() -> reqwest::Client {
        reqwest::Client::builder()
            .pool_idle_timeout(None)
            .build()
            .unwrap()
    }

    fn deps(s: &MockServer, store: Arc<dyn TokenStore>) -> ManagerDeps {
        deps_for(s, store, CsaAudience::Chatsvcagg)
    }

    fn deps_for(
        s: &MockServer,
        store: Arc<dyn TokenStore>,
        csa_audience: CsaAudience,
    ) -> ManagerDeps {
        ManagerDeps {
            http: quiet_client(),
            endpoints: Endpoints::all(&s.uri()),
            store,
            cancel: CancellationToken::new(),
            csa_audience,
        }
    }

    /// Success mocks, a store seeded with `rt-0`, and a running manager.
    async fn rig(
        skype_expires: u64,
        access_expires: u64,
    ) -> (
        MockServer,
        TokenHandle,
        Arc<MemoryTokenStore>,
        JoinHandle<()>,
    ) {
        let s = MockServer::start().await;
        mount_success_mocks(&s, skype_expires, access_expires).await;
        let store = Arc::new(MemoryTokenStore::default());
        store.save(&session("rt-0")).await.unwrap();
        let (h, j) = spawn_token_manager(deps(&s, store.clone()));
        (s, h, store, j)
    }

    /// The first published snapshot with `version >= v`.
    async fn version_at_least(h: &TokenHandle, v: u64) -> TokenSnapshot {
        let mut rx = h.watch();
        rx.wait_for(|s| s.version >= v).await.unwrap().clone()
    }

    async fn ready_snapshot(h: &TokenHandle) -> TokenSnapshot {
        let mut rx = h.watch();
        rx.wait_for(|s| s.state == TokenState::Ready)
            .await
            .unwrap()
            .clone()
    }

    async fn paths(s: &MockServer) -> Vec<String> {
        s.received_requests()
            .await
            .unwrap()
            .iter()
            .map(|r| r.url.path().to_string())
            .collect()
    }

    #[tokio::test(start_paused = true)]
    async fn lease_lifetimes_are_floored_at_60s() {
        // A tiny (or zero) expires_in would otherwise schedule a refresh
        // every few seconds, or in a hot loop.
        for secs in [0, 1, 5, 59] {
            assert_eq!(
                Lease::new(secs, 0).lifetime,
                Duration::from_secs(60),
                "{secs}"
            );
        }
        assert_eq!(Lease::new(60, 0).lifetime, Duration::from_secs(60));
        assert_eq!(Lease::new(3600, 0).lifetime, Duration::from_secs(3600));
        let l = Lease::new(0, 0);
        assert_eq!(l.due_at() - l.issued, Duration::from_secs(48));
        assert!(!l.is_due(0));
        assert!(!l.is_due(47_999));
        assert!(l.is_due(48_000), "due by the wall clock");
        assert!(!l.is_expired(59_999));
        assert!(l.is_expired(60_000), "expired by the wall clock");
    }

    #[test]
    fn handle_and_tokens_are_shareable_across_tasks() {
        fn check<T: Send + Sync + 'static>() {}
        check::<TokenHandle>();
        check::<Tokens>();
    }

    #[tokio::test(start_paused = true)]
    async fn ready_after_initial_refresh_and_authz() {
        let (s, h, store, _j) = rig(3800, 3600).await;
        let snap = h.ready().await.unwrap();
        assert_eq!(snap.state, TokenState::Ready);
        assert_eq!(snap.skype_token.clone().unwrap().expose(), "sk-1");
        assert_eq!(snap.access_token.clone().unwrap().expose(), "at-1");
        assert_eq!(snap.chat_service.as_deref(), Some(s.uri().as_str()));
        assert_eq!(snap.region.as_deref(), Some("de"));
        assert_eq!(snap.endpoint_id.as_deref(), Some("ep"));
        assert!(snap.access_expires_at > crate::now_ms() + 3_500_000);
        assert!(snap.skype_expires_at > crate::now_ms() + 3_700_000);
        assert_eq!(
            store.load().await.unwrap().unwrap().refresh_token.expose(),
            "rt-1",
            "rotated refresh token persisted immediately"
        );
        let debug = format!("{snap:?}");
        assert!(
            !debug.contains("sk-1") && !debug.contains("at-1"),
            "{debug}"
        );
        assert_eq!(paths(&s).await, [TOKEN, AUTHZ]);
    }

    #[tokio::test(start_paused = true)]
    async fn scheduled_refreshes_keep_access_before_authz() {
        // skype 100 s (due at 80 s); access 90 s (due at 72 s) → by 81 s both
        // refreshed, access token always before authz.
        let (s, h, _store, _j) = rig(100, 90).await;
        let v = h.ready().await.unwrap().version;
        tokio::time::advance(Duration::from_secs(81)).await;
        let snap = version_at_least(&h, v + 2).await;
        assert_eq!(snap.skype_token.unwrap().expose(), "sk-2");
        assert_eq!(snap.access_token.unwrap().expose(), "at-2");
        assert_eq!(paths(&s).await, [TOKEN, AUTHZ, TOKEN, AUTHZ]);
    }

    #[tokio::test(start_paused = true)]
    async fn each_token_is_refreshed_on_its_own_schedule() {
        // Same lifetimes, but visit the two due times separately: at 73 s only
        // the access token is due; at 81 s only the Skype token is, and the
        // access token (8 s old) is not in its last fifth, so authz alone.
        let (s, h, _store, _j) = rig(100, 90).await;
        let v = h.ready().await.unwrap().version;
        tokio::time::advance(Duration::from_secs(73)).await;
        let snap = version_at_least(&h, v + 1).await;
        assert_eq!(snap.access_token.unwrap().expose(), "at-2");
        assert_eq!(snap.skype_token.unwrap().expose(), "sk-1");
        assert_eq!(snap.state, TokenState::Ready);
        assert_eq!(paths(&s).await, [TOKEN, AUTHZ, TOKEN]);
        tokio::time::advance(Duration::from_secs(8)).await;
        let snap = version_at_least(&h, v + 2).await;
        assert_eq!(snap.access_token.unwrap().expose(), "at-2");
        assert_eq!(snap.skype_token.unwrap().expose(), "sk-2");
        assert_eq!(paths(&s).await, [TOKEN, AUTHZ, TOKEN, AUTHZ]);
    }

    #[tokio::test(start_paused = true)]
    async fn invalid_grant_clears_store_and_reports_reauth() {
        let s = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path(TOKEN))
            .respond_with(
                ResponseTemplate::new(400)
                    .set_body_json(serde_json::json!({"error":"invalid_grant"})),
            )
            .mount(&s)
            .await;
        let store = Arc::new(MemoryTokenStore::default());
        store
            .save(&StoredSession {
                refresh_token: Secret::new("rt".into()),
                tenant: "organizations".into(),
                endpoint_id: "ep".into(),
            })
            .await
            .unwrap();
        let (h, _j) = spawn_token_manager(ManagerDeps {
            http: reqwest::Client::new(),
            endpoints: Endpoints::all(&s.uri()),
            store: store.clone(),
            cancel: CancellationToken::new(),
            csa_audience: CsaAudience::Chatsvcagg,
        });
        assert!(matches!(h.ready().await, Err(Error::ReauthRequired)));
        assert_eq!(store.load().await.unwrap(), None);
        assert_eq!(h.current().state, TokenState::ReauthRequired);
        assert!(h.current().endpoint_id.is_none());
        // Nothing to refresh with: no network, same answer.
        assert!(matches!(
            h.force_refresh(RefreshKind::AccessToken, 0).await,
            Err(Error::ReauthRequired)
        ));
        assert_eq!(paths(&s).await, [TOKEN]);
    }

    #[tokio::test(start_paused = true)]
    async fn force_refresh_is_single_flight() {
        let (s, h, _store, _j) = rig(3800, 3600).await;
        let v = h.ready().await.unwrap().version;
        let (a, b) = tokio::join!(
            h.force_refresh(RefreshKind::SkypeToken, v),
            h.force_refresh(RefreshKind::SkypeToken, v)
        );
        assert_eq!(a.unwrap().skype_token.unwrap().expose(), "sk-2");
        assert_eq!(b.unwrap().skype_token.unwrap().expose(), "sk-2");
        let authz_calls = paths(&s).await.iter().filter(|p| *p == AUTHZ).count();
        assert_eq!(authz_calls, 2, "initial + exactly one forced");
        // stale observed version → no network
        let v2 = h.current().version;
        h.force_refresh(RefreshKind::SkypeToken, v).await.unwrap();
        assert_eq!(h.current().version, v2);
        assert_eq!(paths(&s).await.len(), 3);
    }

    #[tokio::test(start_paused = true)]
    async fn force_refresh_after_an_access_only_refresh_still_runs_authz() {
        // access due at 80 s, skype at 3040 s: at 81 s only the access token
        // is renewed. A caller that saw sk-1 rejected must still get a new
        // Skype token even though a newer snapshot exists.
        let (s, h, _store, _j) = rig(3800, 100).await;
        let v = h.ready().await.unwrap().version;
        tokio::time::advance(Duration::from_secs(81)).await;
        let snap = version_at_least(&h, v + 1).await;
        let v1 = snap.version;
        assert_eq!(snap.access_token.unwrap().expose(), "at-2");
        assert_eq!(snap.skype_token.unwrap().expose(), "sk-1");
        let snap = h.force_refresh(RefreshKind::SkypeToken, v).await.unwrap();
        assert_eq!(snap.skype_token.unwrap().expose(), "sk-2");
        assert_eq!(paths(&s).await, [TOKEN, AUTHZ, TOKEN, AUTHZ]);
        // at-2 is newer than v: a caller that saw at-1 is answered by it.
        let snap = h.force_refresh(RefreshKind::AccessToken, v).await.unwrap();
        assert_eq!(snap.access_token.unwrap().expose(), "at-2");
        assert_eq!(paths(&s).await.len(), 4, "no network");
        // The reverse: a caller that saw at-2 (at v1) is not answered by the
        // Skype-only renewal since.
        let snap = h.force_refresh(RefreshKind::AccessToken, v1).await.unwrap();
        assert_eq!(snap.access_token.unwrap().expose(), "at-3");
        assert_eq!(paths(&s).await, [TOKEN, AUTHZ, TOKEN, AUTHZ, TOKEN]);
    }

    #[tokio::test(start_paused = true)]
    async fn queued_refreshes_of_another_kind_are_not_answered_by_this_one() {
        let (s, h, _store, _j) = rig(3800, 3600).await;
        let v = h.ready().await.unwrap().version;
        // Both are queued before the manager runs; the Skype request must not
        // be answered by the access-only refresh it waited behind.
        let (a, b) = tokio::join!(
            h.force_refresh(RefreshKind::AccessToken, v),
            h.force_refresh(RefreshKind::SkypeToken, v)
        );
        assert_eq!(a.unwrap().access_token.unwrap().expose(), "at-2");
        assert_eq!(b.unwrap().skype_token.unwrap().expose(), "sk-2");
        assert_eq!(paths(&s).await, [TOKEN, AUTHZ, TOKEN, AUTHZ]);
    }

    #[tokio::test(start_paused = true)]
    async fn no_session_means_reauth_until_install() {
        let s = MockServer::start().await;
        mount_success_mocks(&s, 3800, 3600).await;
        let store = Arc::new(MemoryTokenStore::default());
        let (h, _j) = spawn_token_manager(ManagerDeps {
            http: reqwest::Client::new(),
            endpoints: Endpoints::all(&s.uri()),
            store: store.clone(),
            cancel: CancellationToken::new(),
            csa_audience: CsaAudience::Chatsvcagg,
        });
        assert!(matches!(h.ready().await, Err(Error::ReauthRequired)));
        h.install(
            StoredSession {
                refresh_token: Secret::new("rt-0".into()),
                tenant: "organizations".into(),
                endpoint_id: "ep".into(),
            },
            TokenResponse {
                access_token: Secret::new("at-0".into()),
                refresh_token: None,
                expires_in: 3600,
            },
        )
        .await
        .unwrap();
        let snap = h.ready().await.unwrap();
        assert_eq!(
            snap.access_token.unwrap().expose(),
            "at-0",
            "install uses the granted access token without refreshing"
        );
        assert_eq!(snap.skype_token.unwrap().expose(), "sk-1");
        assert_eq!(snap.endpoint_id.as_deref(), Some("ep"));
        assert!(store.load().await.unwrap().is_some());
        assert_eq!(paths(&s).await, [AUTHZ]);
    }

    #[tokio::test(start_paused = true)]
    async fn transient_failure_is_reported_and_retried_after_30s() {
        let s = MockServer::start().await;
        // The first refresh meets a 503; everything after that succeeds.
        Mock::given(method("POST"))
            .and(path(TOKEN))
            .respond_with(ResponseTemplate::new(503).set_body_string("gateway"))
            .up_to_n_times(1)
            .mount(&s)
            .await;
        mount_success_mocks(&s, 3800, 3600).await;
        let store = Arc::new(MemoryTokenStore::default());
        store.save(&session("rt-0")).await.unwrap();
        let (h, _j) = spawn_token_manager(deps(&s, store.clone()));
        let err = h.ready().await.unwrap_err();
        assert!(
            matches!(&err, Error::Protocol(m) if m == "token refresh failed: http 503: gateway"),
            "{err:?}"
        );
        assert_eq!(
            h.current().state,
            TokenState::Failed("http 503: gateway".into())
        );
        assert_eq!(
            store.load().await.unwrap(),
            Some(session("rt-0")),
            "a transient failure keeps the session"
        );
        tokio::time::advance(Duration::from_secs(29)).await;
        assert_eq!(paths(&s).await, [TOKEN], "no retry before 30 s");
        tokio::time::advance(Duration::from_secs(1)).await;
        let snap = ready_snapshot(&h).await;
        assert_eq!(snap.access_token.unwrap().expose(), "at-1");
        assert_eq!(snap.skype_token.unwrap().expose(), "sk-1");
        assert_eq!(paths(&s).await, [TOKEN, TOKEN, AUTHZ]);
        assert!(h.ready().await.is_ok());
    }

    #[tokio::test(start_paused = true)]
    async fn failed_refresh_serves_kept_tokens_until_they_expire() {
        let (s, h, _store, _j) = rig(3800, 3600).await;
        let v = h.ready().await.unwrap().version;
        // From now on the refresh endpoint is down.
        s.reset().await;
        Mock::given(method("POST"))
            .and(path(TOKEN))
            .respond_with(ResponseTemplate::new(500).set_body_string("down"))
            .mount(&s)
            .await;
        let failed = TokenState::Failed("http 500: down".into());
        tokio::time::advance(Duration::from_secs(2880)).await; // 80 % of 3600
        let snap = version_at_least(&h, v + 1).await;
        assert_eq!(snap.state, failed);
        // Both kept tokens are unexpired: still served.
        let snap = h.ready().await.unwrap();
        assert_eq!(snap.state, failed);
        assert_eq!(snap.access_token.unwrap().expose(), "at-1");
        assert_eq!(snap.skype_token.unwrap().expose(), "sk-1");
        // The 30 s retry schedule carries on meanwhile, without interrupting
        // service.
        let before = paths(&s).await.len();
        tokio::time::advance(Duration::from_secs(30)).await;
        let retried = version_at_least(&h, snap.version + 1).await;
        assert_eq!(retried.state, failed);
        assert_eq!(paths(&s).await.len(), before + 1, "one retry at +30 s");
        assert_eq!(
            h.ready().await.unwrap().access_token.unwrap().expose(),
            "at-1"
        );
        // The access token expires at 3600 s: dropped, and ready() errors.
        // (On the paused clock the overdue retry fires at the same instant;
        // with an incomplete token set it passes through `Starting`, so wait
        // for the `Failed` that follows it.)
        tokio::time::advance(Duration::from_secs(690)).await;
        let mut rx = h.watch();
        let is_failed = |s: &TokenSnapshot| matches!(s.state, TokenState::Failed(_));
        let snap = rx
            .wait_for(|s| s.access_token.is_none() && is_failed(s))
            .await
            .unwrap()
            .clone();
        assert_eq!(snap.state, failed);
        assert!(
            snap.skype_token.is_some(),
            "skype token (3800 s) not yet expired"
        );
        assert!(matches!(
            h.ready().await,
            Err(Error::Protocol(m)) if m == "token refresh failed: http 500: down"
        ));
        // The Skype token follows at 3800 s.
        tokio::time::advance(Duration::from_secs(200)).await;
        let snap = rx
            .wait_for(|s| s.skype_token.is_none() && is_failed(s))
            .await
            .unwrap()
            .clone();
        assert_eq!(snap.state, failed);
        assert!(snap.access_token.is_none());
        assert!(matches!(h.ready().await, Err(Error::Protocol(_))));
    }

    /// A wall clock a test can move (ms), as after a suspend: the monotonic
    /// clock stands still while it jumps.
    fn movable_clock(start: Timestamp) -> (Arc<std::sync::atomic::AtomicI64>, WallClock) {
        let now = Arc::new(std::sync::atomic::AtomicI64::new(start));
        let read = now.clone();
        (now, Arc::new(move || read.load(Ordering::SeqCst)))
    }

    #[tokio::test(start_paused = true)]
    async fn tokens_past_their_wall_clock_expiry_count_as_due() {
        // Suspend/resume: the monotonic clock did not move, the wall clock
        // moved two hours. A chatsvc 401 asks for a new Skype token; the
        // access token is expired by the wall clock, so it is renewed first
        // (authz with the dead one would only fail).
        let s = MockServer::start().await;
        mount_success_mocks(&s, 3800, 3600).await;
        let store = Arc::new(MemoryTokenStore::default());
        store.save(&session("rt-0")).await.unwrap();
        let (wall, clock) = movable_clock(1_760_000_000_000);
        let (h, _j) = spawn_token_manager_with_clock(deps(&s, store), clock);
        let v = h.ready().await.unwrap().version;
        wall.fetch_add(2 * 60 * 60 * 1000, Ordering::SeqCst);
        let snap = h.force_refresh(RefreshKind::SkypeToken, v).await.unwrap();
        assert_eq!(snap.access_token.unwrap().expose(), "at-2");
        assert_eq!(snap.skype_token.unwrap().expose(), "sk-2");
        assert_eq!(paths(&s).await, [TOKEN, AUTHZ, TOKEN, AUTHZ]);
    }

    /// authz: rejects the access token `reject` with 401 once it has been
    /// used before; otherwise hands out `sk-{n}`.
    struct AuthzRejectingReuse {
        calls: AtomicU64,
        reject: &'static str,
        chat: String,
    }

    impl Respond for AuthzRejectingReuse {
        fn respond(&self, r: &wiremock::Request) -> ResponseTemplate {
            let n = self.calls.fetch_add(1, Ordering::SeqCst) + 1;
            let bearer = r
                .headers
                .get("authorization")
                .and_then(|v| v.to_str().ok())
                .unwrap_or_default();
            if n > 1 && bearer == format!("Bearer {}", self.reject) {
                return ResponseTemplate::new(401);
            }
            ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "tokens": {"skypeToken": format!("sk-{n}"), "expiresIn": 3800},
                "region": "de",
                "regionGtms": {"chatService": self.chat},
            }))
        }
    }

    #[tokio::test(start_paused = true)]
    async fn an_authz_401_renews_the_access_token_once_and_retries() {
        let s = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path(AUTHZ))
            .respond_with(AuthzRejectingReuse {
                calls: AtomicU64::new(0),
                reject: "at-1",
                chat: s.uri(),
            })
            .mount(&s)
            .await;
        mount_success_mocks(&s, 3800, 3600).await;
        let store = Arc::new(MemoryTokenStore::default());
        store.save(&session("rt-0")).await.unwrap();
        let (h, _j) = spawn_token_manager(deps(&s, store));
        let v = h.ready().await.unwrap().version;
        let snap = h.force_refresh(RefreshKind::SkypeToken, v).await.unwrap();
        assert_eq!(snap.state, TokenState::Ready);
        assert_eq!(snap.access_token.unwrap().expose(), "at-2");
        assert_eq!(snap.skype_token.unwrap().expose(), "sk-3");
        assert_eq!(paths(&s).await, [TOKEN, AUTHZ, AUTHZ, TOKEN, AUTHZ]);
    }

    #[tokio::test(start_paused = true)]
    async fn refresh_requests_while_failed_wait_for_the_30s_retry() {
        let (s, h, _store, _j) = rig(3800, 3600).await;
        let v = h.ready().await.unwrap().version;
        s.reset().await;
        Mock::given(method("POST"))
            .and(path(AUTHZ))
            .respond_with(ResponseTemplate::new(500).set_body_string("down"))
            .mount(&s)
            .await;
        assert!(h.force_refresh(RefreshKind::SkypeToken, v).await.is_err());
        assert_eq!(paths(&s).await.len(), 1);
        // Every failing request asks again; none of them reaches authz
        // before the retry is due.
        for _ in 0..3 {
            let err = h
                .force_refresh(RefreshKind::SkypeToken, v)
                .await
                .unwrap_err();
            assert!(
                matches!(&err, Error::Protocol(m) if m == "token refresh failed: http 500: down"),
                "{err:?}"
            );
        }
        assert_eq!(paths(&s).await.len(), 1, "no doomed authz per request");
        tokio::time::advance(Duration::from_secs(30)).await;
        let mut rx = h.watch();
        let seen = rx.borrow_and_update().version;
        rx.wait_for(|snap| snap.version > seen).await.unwrap();
        assert_eq!(paths(&s).await.len(), 2, "the scheduled retry");
    }

    #[tokio::test(start_paused = true)]
    async fn clear_forgets_the_session() {
        let (s, h, store, _j) = rig(3800, 3600).await;
        h.ready().await.unwrap();
        h.clear().await.unwrap();
        let snap = h.current();
        assert_eq!(snap.state, TokenState::ReauthRequired);
        assert!(snap.access_token.is_none() && snap.skype_token.is_none());
        assert!(snap.chat_service.is_none() && snap.endpoint_id.is_none());
        assert_eq!(store.load().await.unwrap(), None);
        assert!(matches!(h.ready().await, Err(Error::ReauthRequired)));
        assert!(matches!(
            h.force_refresh(RefreshKind::SkypeToken, 0).await,
            Err(Error::ReauthRequired)
        ));
        assert_eq!(paths(&s).await.len(), 2, "no network after clear");
    }

    #[tokio::test(start_paused = true)]
    async fn lazy_tokens_spawns_once() {
        let (s, _h, store, _j) = rig(3800, 3600).await;
        drop(_h); // only need the server + store
        let t = Tokens::new(ManagerDeps {
            http: reqwest::Client::new(),
            endpoints: Endpoints::all(&s.uri()),
            store,
            cancel: CancellationToken::new(),
            csa_audience: CsaAudience::Chatsvcagg,
        });
        assert!(t.try_handle().is_none());
        let (a, b) = tokio::join!(t.handle(), t.handle());
        assert!(std::ptr::eq(a, b));
        a.ready().await.unwrap();
        assert!(t.try_handle().is_some());
    }

    #[tokio::test(start_paused = true)]
    async fn shutdown_stops_the_manager() {
        let s = MockServer::start().await;
        mount_success_mocks(&s, 3800, 3600).await;
        let store = Arc::new(MemoryTokenStore::default());
        store.save(&session("rt-0")).await.unwrap();
        let t = Tokens::new(deps(&s, store));
        t.shutdown().await; // never started: nothing to stop
        assert!(t.try_handle().is_none());
        let h = t.handle().await.clone();
        h.ready().await.unwrap();
        t.shutdown().await;
        assert!(matches!(
            h.force_refresh(RefreshKind::SkypeToken, 0).await,
            Err(Error::Protocol(m)) if m == "token manager stopped"
        ));
        assert!(matches!(
            h.ready().await,
            Err(Error::Protocol(m)) if m == "token manager stopped"
        ));
        assert!(h.watch().has_changed().is_err(), "watch is closed");
    }

    /// The CSA scope as it appears in the form body.
    const CSA_FORM: &str = "scope=https%3A%2F%2Fchatsvcagg.teams.microsoft.com%2F.default";

    /// CSA grants → `csa-{n}` / `rt-csa-{n}`, `expires_in` seconds. Mount
    /// it before [`mount_success_mocks`]: both answer the token endpoint,
    /// and the first mounted match wins.
    pub(crate) async fn mount_csa_grant(s: &MockServer, expires_in: u64) {
        Mock::given(method("POST"))
            .and(path(TOKEN))
            .and(body_string_contains(CSA_FORM))
            .respond_with(Counted(AtomicU64::new(0), move |n| {
                serde_json::json!({
                    "access_token": format!("csa-{n}"),
                    "refresh_token": format!("rt-csa-{n}"),
                    "expires_in": expires_in,
                })
            }))
            .mount(s)
            .await;
    }

    /// CSA grants answered with `status` and `body`.
    async fn mount_csa_refusal(s: &MockServer, status: u16, body: serde_json::Value) {
        Mock::given(method("POST"))
            .and(path(TOKEN))
            .and(body_string_contains(CSA_FORM))
            .respond_with(ResponseTemplate::new(status).set_body_json(body))
            .mount(s)
            .await;
    }

    /// Token-endpoint calls so far: (CSA grants, main refreshes).
    pub(crate) async fn grants(s: &MockServer) -> (usize, usize) {
        let reqs = s.received_requests().await.unwrap();
        let token: Vec<_> = reqs
            .iter()
            .filter(|r| r.url.path().ends_with("/oauth2/v2.0/token"))
            .collect();
        let csa = token
            .iter()
            .filter(|r| String::from_utf8_lossy(&r.body).contains("chatsvcagg"))
            .count();
        (csa, token.len() - csa)
    }

    /// A store seeded with `rt-0`, CSA grants lasting `csa_expires` s, the
    /// success mocks, and a running manager for `audience`.
    async fn csa_rig(
        csa_expires: u64,
        audience: CsaAudience,
    ) -> (
        MockServer,
        TokenHandle,
        Arc<MemoryTokenStore>,
        JoinHandle<()>,
    ) {
        let s = MockServer::start().await;
        mount_csa_grant(&s, csa_expires).await;
        mount_success_mocks(&s, 3800, 3600).await;
        let store = Arc::new(MemoryTokenStore::default());
        store.save(&session("rt-0")).await.unwrap();
        let (h, j) = spawn_token_manager(deps_for(&s, store.clone(), audience));
        (s, h, store, j)
    }

    /// A [`MemoryTokenStore`] that counts saves.
    #[derive(Default)]
    struct CountingStore {
        inner: MemoryTokenStore,
        saves: AtomicU64,
    }

    #[async_trait::async_trait]
    impl TokenStore for CountingStore {
        async fn load(&self) -> crate::Result<Option<StoredSession>> {
            self.inner.load().await
        }
        async fn save(&self, s: &StoredSession) -> crate::Result<()> {
            self.saves.fetch_add(1, Ordering::SeqCst);
            self.inner.save(s).await
        }
        async fn clear(&self) -> crate::Result<()> {
            self.inner.clear().await
        }
    }

    #[tokio::test(start_paused = true)]
    async fn csa_token_is_granted_lazily_and_kept_apart_from_the_main_tokens() {
        let (s, h, store, _j) = csa_rig(3600, CsaAudience::Chatsvcagg).await;
        let main = h.ready().await.unwrap();
        assert!(main.csa_token.is_none() && main.csa_expires_at == 0 && main.csa_version == 0);
        assert_eq!(grants(&s).await, (0, 1), "nothing granted until asked");
        assert_eq!(h.csa_refresh_kind(), RefreshKind::CsaToken);
        assert_eq!(h.csa_token().await.unwrap().expose(), "csa-1");
        let snap = h.current();
        assert_eq!(snap.csa_token.clone().unwrap().expose(), "csa-1");
        assert_eq!(snap.csa_version, 1);
        assert!(snap.csa_expires_at > crate::now_ms() + 3_500_000);
        assert_eq!(snap.state, TokenState::Ready);
        assert_eq!(snap.access_token.clone().unwrap().expose(), "at-1");
        assert_eq!(snap.skype_token.clone().unwrap().expose(), "sk-1");
        assert!(!format!("{snap:?}").contains("csa-1"), "redacted");
        assert_eq!(
            store.load().await.unwrap().unwrap().refresh_token.expose(),
            "rt-csa-1",
            "the CSA grant's rotation is persisted"
        );
        // From the lease: no network.
        assert_eq!(h.csa_token().await.unwrap().expose(), "csa-1");
        assert_eq!(grants(&s).await, (1, 1));
        // A main waiter that saw `main` is not answered by the CSA publish.
        let snap = h
            .force_refresh(RefreshKind::SkypeToken, main.version)
            .await
            .unwrap();
        assert_eq!(snap.skype_token.unwrap().expose(), "sk-2");
    }

    #[tokio::test(start_paused = true)]
    async fn a_csa_rotation_is_persisted_once() {
        let s = MockServer::start().await;
        mount_csa_grant(&s, 3600).await;
        mount_success_mocks(&s, 3800, 3600).await;
        let store = Arc::new(CountingStore::default());
        store.inner.save(&session("rt-0")).await.unwrap();
        let (h, _j) = spawn_token_manager(deps(&s, store.clone()));
        h.ready().await.unwrap();
        assert_eq!(store.saves.load(Ordering::SeqCst), 1, "the main rotation");
        h.csa_token().await.unwrap();
        h.csa_token().await.unwrap();
        assert_eq!(
            store.saves.load(Ordering::SeqCst),
            2,
            "one save for the CSA rotation, none for the lease hit"
        );
        assert_eq!(
            store.load().await.unwrap().unwrap().refresh_token.expose(),
            "rt-csa-1"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn the_csa_lease_is_renewed_in_its_last_fifth_on_demand() {
        let (s, h, _store, _j) = csa_rig(100, CsaAudience::Chatsvcagg).await;
        h.ready().await.unwrap();
        assert_eq!(h.csa_token().await.unwrap().expose(), "csa-1");
        tokio::time::advance(Duration::from_secs(79)).await;
        assert_eq!(
            h.csa_token().await.unwrap().expose(),
            "csa-1",
            "not due before 80 s"
        );
        tokio::time::advance(Duration::from_secs(2)).await;
        assert_eq!(
            grants(&s).await,
            (1, 1),
            "no timer: nothing happens until asked"
        );
        assert_eq!(h.csa_token().await.unwrap().expose(), "csa-2");
        assert_eq!(h.current().csa_version, 2);
    }

    #[tokio::test(start_paused = true)]
    async fn concurrent_csa_requests_share_one_grant() {
        let (s, h, _store, _j) = csa_rig(3600, CsaAudience::Chatsvcagg).await;
        h.ready().await.unwrap();
        let (a, b, c) = tokio::join!(
            h.csa_token(),
            h.csa_token(),
            h.force_refresh(RefreshKind::CsaToken, 0)
        );
        assert_eq!(a.unwrap().expose(), "csa-1");
        assert_eq!(b.unwrap().expose(), "csa-1");
        assert_eq!(c.unwrap().csa_token.unwrap().expose(), "csa-1");
        assert_eq!(grants(&s).await, (1, 1));
    }

    #[tokio::test(start_paused = true)]
    async fn a_forced_csa_refresh_is_answered_by_a_newer_grant() {
        let (s, h, _store, _j) = csa_rig(3600, CsaAudience::Chatsvcagg).await;
        let main = h.ready().await.unwrap();
        let (token, v) = h.csa_token_versioned().await.unwrap();
        assert_eq!(token.expose(), "csa-1");
        assert_eq!(v, 1);
        let snap = h.force_refresh(RefreshKind::CsaToken, v).await.unwrap();
        assert_eq!(snap.csa_token.unwrap().expose(), "csa-2");
        assert_eq!(snap.csa_version, 2);
        // Someone else saw csa-1 too: answered by csa-2, without network.
        let snap = h.force_refresh(RefreshKind::CsaToken, v).await.unwrap();
        assert_eq!(snap.csa_token.clone().unwrap().expose(), "csa-2");
        assert_eq!(grants(&s).await, (2, 1));
        assert_eq!(snap.state, TokenState::Ready);
        assert_eq!(
            snap.access_token, main.access_token,
            "main tokens untouched"
        );
        assert_eq!(snap.skype_token, main.skype_token);
    }

    #[tokio::test(start_paused = true)]
    async fn a_csa_failure_reaches_only_csa_waiters() {
        let s = MockServer::start().await;
        mount_csa_refusal(&s, 500, serde_json::json!({"error": "server_error"})).await;
        mount_success_mocks(&s, 3800, 3600).await;
        let store = Arc::new(MemoryTokenStore::default());
        store.save(&session("rt-0")).await.unwrap();
        let (h, _j) = spawn_token_manager(deps(&s, store.clone()));
        let main = h.ready().await.unwrap();
        let (csa, skype) = tokio::join!(
            h.csa_token(),
            h.force_refresh(RefreshKind::SkypeToken, main.version)
        );
        assert!(
            matches!(&csa, Err(Error::Http { status: 500, body }) if body.contains("server_error")),
            "{csa:?}"
        );
        assert_eq!(
            skype.unwrap().skype_token.unwrap().expose(),
            "sk-2",
            "the main waiter got its own refresh, not the CSA failure"
        );
        let snap = h.current();
        assert_eq!(snap.state, TokenState::Ready);
        assert!(snap.csa_token.is_none());
        assert_eq!(snap.access_token.unwrap().expose(), "at-1");
        assert_eq!(
            store.load().await.unwrap(),
            Some(session("rt-1")),
            "session kept"
        );
    }

    /// Review Focus #1: a proxy page or login redirect instead of a token.
    #[tokio::test(start_paused = true)]
    async fn an_unparseable_csa_grant_is_a_protocol_error_for_the_csa_caller_only() {
        let s = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path(TOKEN))
            .and(body_string_contains(CSA_FORM))
            .respond_with(ResponseTemplate::new(200).set_body_string("<html>login</html>"))
            .mount(&s)
            .await;
        mount_success_mocks(&s, 3800, 3600).await;
        let store = Arc::new(MemoryTokenStore::default());
        store.save(&session("rt-0")).await.unwrap();
        let (h, _j) = spawn_token_manager(deps(&s, store.clone()));
        let main = h.ready().await.unwrap();
        let err = h.csa_token().await.unwrap_err();
        assert!(matches!(err, Error::Protocol(_)), "{err:?}");
        let snap = h.current();
        assert_eq!(snap.state, TokenState::Ready);
        assert_eq!(snap.access_token, main.access_token);
        assert_eq!(snap.skype_token, main.skype_token);
        assert_eq!(grants(&s).await, (1, 1), "no main refresh");
        assert_eq!(store.load().await.unwrap(), Some(session("rt-1")));
    }

    #[tokio::test(start_paused = true)]
    async fn a_main_failure_reaches_only_main_waiters_and_csa_still_works_while_failed() {
        let (s, h, _store, _j) = csa_rig(3600, CsaAudience::Chatsvcagg).await;
        let main = h.ready().await.unwrap();
        // From now on authz is down; CSA grants still work.
        s.reset().await;
        mount_csa_grant(&s, 3600).await;
        Mock::given(method("POST"))
            .and(path(AUTHZ))
            .respond_with(ResponseTemplate::new(500).set_body_string("down"))
            .mount(&s)
            .await;
        let (skype, csa) = tokio::join!(
            h.force_refresh(RefreshKind::SkypeToken, main.version),
            h.csa_token()
        );
        assert!(
            matches!(skype, Err(Error::Http { status: 500, .. })),
            "{skype:?}"
        );
        assert_eq!(
            csa.unwrap().expose(),
            "csa-1",
            "granted although the main state is Failed"
        );
        assert_eq!(
            h.current().state,
            TokenState::Failed("http 500: down".into())
        );
    }

    #[tokio::test(start_paused = true)]
    async fn a_csa_invalid_grant_runs_one_main_refresh_and_keeps_a_live_session() {
        let s = MockServer::start().await;
        mount_csa_refusal(
            &s,
            400,
            serde_json::json!({"error": "invalid_grant", "error_description": "AADSTS65001: consent"}),
        )
        .await;
        mount_success_mocks(&s, 3800, 3600).await;
        let store = Arc::new(MemoryTokenStore::default());
        store.save(&session("rt-0")).await.unwrap();
        let (h, _j) = spawn_token_manager(deps(&s, store.clone()));
        h.ready().await.unwrap();
        let err = h.csa_token().await.unwrap_err();
        assert!(
            matches!(&err, Error::Http { status: 400, body } if body.contains("invalid_grant")),
            "{err:?}"
        );
        assert_eq!(grants(&s).await, (1, 2), "one main refresh decided it");
        let snap = h.current();
        assert_eq!(snap.state, TokenState::Ready);
        assert_eq!(snap.access_token.unwrap().expose(), "at-2");
        assert_eq!(store.load().await.unwrap(), Some(session("rt-2")));
    }

    #[tokio::test(start_paused = true)]
    async fn a_csa_invalid_grant_keeps_the_main_retry_spacing() {
        let s = MockServer::start().await;
        mount_csa_refusal(&s, 400, serde_json::json!({"error": "invalid_grant"})).await;
        mount_success_mocks(&s, 3800, 3600).await;
        let store = Arc::new(MemoryTokenStore::default());
        store.save(&session("rt-0")).await.unwrap();
        let (h, _j) = spawn_token_manager(deps(&s, store.clone()));
        let main = h.ready().await.unwrap();
        // authz goes down: the main path fails and schedules its 30 s retry.
        s.reset().await;
        mount_csa_refusal(&s, 400, serde_json::json!({"error": "invalid_grant"})).await;
        Mock::given(method("POST"))
            .and(path(AUTHZ))
            .respond_with(ResponseTemplate::new(500).set_body_string("down"))
            .mount(&s)
            .await;
        assert!(
            h.force_refresh(RefreshKind::SkypeToken, main.version)
                .await
                .is_err()
        );
        let err = h.csa_token().await.unwrap_err();
        assert!(matches!(err, Error::Http { status: 400, .. }), "{err:?}");
        assert_eq!(
            grants(&s).await,
            (1, 0),
            "no main refresh before the retry is due"
        );
        assert_eq!(
            h.current().state,
            TokenState::Failed("http 500: down".into())
        );
        assert_eq!(
            store.load().await.unwrap(),
            Some(session("rt-1")),
            "session kept"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn a_csa_invalid_grant_ends_the_session_only_when_the_main_refresh_says_so() {
        let (s, h, store, _j) = csa_rig(3600, CsaAudience::Chatsvcagg).await;
        h.ready().await.unwrap();
        s.reset().await;
        Mock::given(method("POST"))
            .and(path(TOKEN))
            .respond_with(
                ResponseTemplate::new(400)
                    .set_body_json(serde_json::json!({"error": "invalid_grant"})),
            )
            .mount(&s)
            .await;
        assert!(matches!(h.csa_token().await, Err(Error::ReauthRequired)));
        assert_eq!(grants(&s).await, (1, 1));
        assert_eq!(h.current().state, TokenState::ReauthRequired);
        assert_eq!(store.load().await.unwrap(), None);
        // Signed out: no more grants.
        assert!(matches!(h.csa_token().await, Err(Error::ReauthRequired)));
        assert_eq!(grants(&s).await, (1, 1));
    }

    #[tokio::test(start_paused = true)]
    async fn csa_without_a_session_is_reauth_without_network() {
        let s = MockServer::start().await;
        let (h, _j) = spawn_token_manager(deps(&s, Arc::new(MemoryTokenStore::default())));
        assert!(matches!(h.csa_token().await, Err(Error::ReauthRequired)));
        assert!(matches!(
            h.force_refresh(RefreshKind::CsaToken, 0).await,
            Err(Error::ReauthRequired)
        ));
        assert!(s.received_requests().await.unwrap().is_empty());
    }

    #[tokio::test(start_paused = true)]
    async fn install_and_clear_drop_the_csa_token() {
        let (_s, h, _store, _j) = csa_rig(3600, CsaAudience::Chatsvcagg).await;
        h.ready().await.unwrap();
        h.csa_token().await.unwrap();
        let v1 = h.current().csa_version;
        let snap = h
            .install(
                session("rt-9"),
                TokenResponse {
                    access_token: Secret::new("at-9".into()),
                    refresh_token: None,
                    expires_in: 3600,
                },
            )
            .await
            .unwrap();
        assert!(snap.csa_token.is_none() && snap.csa_expires_at == 0);
        assert!(snap.csa_version > v1);
        assert_eq!(
            h.csa_token().await.unwrap().expose(),
            "csa-2",
            "a new session gets a new grant"
        );
        let v2 = h.current().csa_version;
        h.clear().await.unwrap();
        let snap = h.current();
        assert!(snap.csa_token.is_none());
        assert!(snap.csa_version > v2, "monotonic across a sign-out");
    }

    #[tokio::test(start_paused = true)]
    async fn under_spaces_csa_uses_the_main_access_token() {
        let (s, h, _store, _j) = csa_rig(3600, CsaAudience::Spaces).await;
        let main = h.ready().await.unwrap();
        assert_eq!(h.csa_refresh_kind(), RefreshKind::AccessToken);
        let (token, v) = h.csa_token_versioned().await.unwrap();
        assert_eq!(token.expose(), "at-1");
        assert_eq!(v, main.version);
        // A CsaToken refresh means the access token here.
        let snap = h.force_refresh(RefreshKind::CsaToken, v).await.unwrap();
        assert_eq!(snap.access_token.unwrap().expose(), "at-2");
        assert!(snap.csa_token.is_none());
        assert_eq!(grants(&s).await, (0, 2));
    }

    /// The Graph token is a lease of its own beside the CSA one: lazy,
    /// its own version and single-flight group, invisible in the snapshot's
    /// `csa_*` fields, and dropped with the session.
    #[tokio::test(start_paused = true)]
    async fn the_graph_token_is_a_separate_lazy_lease() {
        let s = MockServer::start().await;
        // Before the main mocks: the first mounted match wins.
        Mock::given(method("POST"))
            .and(path(TOKEN))
            .and(body_string_contains(
                "scope=https%3A%2F%2Fgraph.microsoft.com%2F.default",
            ))
            .respond_with(Counted(AtomicU64::new(0), |n| {
                serde_json::json!({
                    "access_token": format!("graph-{n}"),
                    "refresh_token": format!("rt-graph-{n}"),
                    "expires_in": 3600,
                })
            }))
            .mount(&s)
            .await;
        mount_csa_grant(&s, 3600).await;
        mount_success_mocks(&s, 3800, 3600).await;
        let store = Arc::new(MemoryTokenStore::default());
        store.save(&session("rt-0")).await.unwrap();
        let (h, _j) = spawn_token_manager(deps(&s, store.clone()));
        let main = h.ready().await.unwrap();
        let (a, b) = tokio::join!(h.graph_token(), h.graph_token());
        assert_eq!(a.unwrap().expose(), "graph-1");
        assert_eq!(b.unwrap().expose(), "graph-1", "one shared grant");
        assert_eq!(
            store.load().await.unwrap().unwrap().refresh_token.expose(),
            "rt-graph-1"
        );
        let snap = h.current();
        assert!(snap.csa_token.is_none() && snap.csa_version == 0);
        assert_eq!(snap.access_token, main.access_token);
        assert!(!format!("{snap:?}").contains("graph-1"));
        // The CSA lease is untouched by it, and the other way round.
        assert_eq!(h.csa_token().await.unwrap().expose(), "csa-1");
        let (token, v) = h.scoped_token_versioned(Scoped::Graph).await.unwrap();
        assert_eq!((token.expose().as_str(), v), ("graph-1", 1));
        // A 401 renews it once; a second caller that saw graph-1 is answered
        // by graph-2 without network.
        h.force_refresh(RefreshKind::GraphToken, v).await.unwrap();
        h.force_refresh(RefreshKind::GraphToken, v).await.unwrap();
        assert_eq!(h.graph_token().await.unwrap().expose(), "graph-2");
        assert_eq!(h.csa_token().await.unwrap().expose(), "csa-1");
        let reqs = s.received_requests().await.unwrap();
        let graph_grants = reqs
            .iter()
            .filter(|r| String::from_utf8_lossy(&r.body).contains("graph.microsoft.com"))
            .count();
        assert_eq!(graph_grants, 2);
        h.clear().await.unwrap();
        assert!(matches!(h.graph_token().await, Err(Error::ReauthRequired)));
    }

    #[test]
    fn clone_err_keeps_invalid() {
        assert!(matches!(
            clone_err(&Error::Invalid("x")),
            Error::Invalid("x")
        ));
    }
}
