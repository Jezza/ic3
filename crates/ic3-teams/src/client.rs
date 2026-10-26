//! [`Client`]: the public entry point that ties auth, http, api and trouter
//! together (spec §4.10).
//!
//! - [`Client::builder`] → [`ClientBuilder::build`] does no network and needs
//!   no runtime; the token manager starts on the first call that needs a
//!   token (spec §4.3), so REST works without [`Client::connect`].
//! - [`Client::connect`] starts trouter only and returns an [`EventStream`];
//!   dropping the stream stops that trouter and nothing else.
//! - `Client` is a cheap `Clone` (`Arc` inside). Background tasks never hold
//!   the client: they hold only the pieces they need plus a child of its
//!   cancellation token, so dropping the last handle cancels them all, and
//!   [`Client::shutdown`] cancels and awaits them.
//! - Whatever yields [`Error::ReauthRequired`] — a REST call, the token
//!   manager, trouter — the stored session is cleared by then (spec §7):
//!   the token manager clears it on a rejected refresh token, and the client
//!   hooks the http layer to clear it when a request is rejected even after
//!   a refresh.

use std::path::PathBuf;
use std::pin::Pin;
use std::sync::{Arc, Mutex, PoisonError};
use std::task::{Context, Poll};
use std::time::Duration;

use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tokio::time::Instant;
use tokio_util::sync::{CancellationToken, DropGuard};

use crate::api::{Api, ApiOptions, PagingMode};
use crate::auth::manager::{ManagerDeps, Tokens};
use crate::auth::oauth::{self, PollOutcome};
use crate::auth::{
    CsaAudience, DEFAULT_TENANT, DeviceCodePrompt, Endpoints, StoredSession, TokenHandle,
    TokenStore,
};
use crate::events::{Event, RawEventHook};
use crate::http::Http;
use crate::media::MediaOptions;
use crate::trouter::{self, TransportFactory, TrouterConfig, TrouterDeps, WsTransportFactory};
use crate::types::{Me, now_ms};
use crate::{Capture, Error, Result, Secret};

/// Capacity of the channel behind [`EventStream`] (spec §4.7). trouter never
/// waits on it: on overflow it drops events and later emits
/// `NeedsCatchUp { reason: Overflow }`.
pub const EVENT_CHANNEL_CAPACITY: usize = 1024;

/// What `slow_down` adds to the device-code polling interval (RFC 8628 §3.5).
const SLOW_DOWN_STEP: Duration = Duration::from_secs(5);

/// Run-time switches for behaviour whose protocol shape is unverified
/// (spec §9 "runtime switches").
#[derive(Clone, Debug, PartialEq)]
pub struct ClientOptions {
    /// Also register the two calling apps purple-teams registers (§9.3);
    /// default `false`.
    pub register_all_apps: bool,
    /// Reconnect trouter after every Skype-token renewal (§9.5); default
    /// `true`.
    pub reconnect_on_token_renewal: bool,
    /// The `/api/mt/{segment}` tried first for display names (§9.9);
    /// default `apac`.
    pub profile_segment: String,
    /// How further pages are requested (§9.1, §9.2); default `auto`.
    pub paging: PagingMode,
    /// Which token CSA requests carry (spec A.2, §9.12); default
    /// `chatsvcagg`. Builder and smoke only
    /// (decision #50).
    pub csa_audience: CsaAudience,
    /// The CSA `{geo}` tried first (§9.13); `None` derives it from the authz
    /// region. Builder and smoke only (decision #50).
    pub csa_geo: Option<String>,
    /// Send `properties.qtdMsgs` with replies (§9.15); default `true`.
    /// Builder and smoke only (decision #50).
    pub send_quoted_ids: bool,
    /// `mark_read` also clears the mark-as-unread bookmark
    /// (`consumptionHorizonBookmark`, §9.7b); default `true`. Builder and
    /// smoke only (decisions #50, #57).
    pub clear_unread_bookmark: bool,
    /// Image and file transfers: size cap, AMS and Graph hosts, how files
    /// are shared (§9.27, §9.31).
    pub media: MediaOptions,
}

impl Default for ClientOptions {
    fn default() -> Self {
        ClientOptions {
            register_all_apps: false,
            // UNVERIFIED(§9.5): reconnecting after each renewal is the only behaviour known to keep delivery going.
            reconnect_on_token_renewal: true,
            profile_segment: ApiOptions::default().profile_segment,
            paging: PagingMode::Auto,
            csa_audience: CsaAudience::default(),
            csa_geo: None,
            send_quoted_ids: true,
            clear_unread_bookmark: true,
            media: MediaOptions::default(),
        }
    }
}

/// Configures and builds a [`Client`]. Only the token store is required.
#[derive(Default)]
pub struct ClientBuilder {
    tenant: Option<String>,
    store: Option<Arc<dyn TokenStore>>,
    capture_dir: Option<PathBuf>,
    options: ClientOptions,
    raw_hook: Option<RawEventHook>,
    endpoints: Option<Endpoints>,
    factory: Option<Arc<dyn TransportFactory>>,
    #[cfg(test)]
    http_client: Option<reqwest::Client>,
}

impl std::fmt::Debug for ClientBuilder {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ClientBuilder")
            .field("tenant", &self.tenant)
            .field("has_token_store", &self.store.is_some())
            .field("capture_dir", &self.capture_dir)
            .field("options", &self.options)
            .field("endpoints", &self.endpoints)
            .finish_non_exhaustive()
    }
}

impl ClientBuilder {
    /// The Azure AD tenant: a domain or GUID. Default `organizations`.
    pub fn tenant(mut self, t: &str) -> Self {
        self.tenant = Some(t.to_string());
        self
    }

    /// Where the session lives between runs (required).
    pub fn token_store(self, s: impl TokenStore) -> Self {
        self.token_store_arc(Arc::new(s))
    }

    /// [`token_store`](Self::token_store) for a store the caller keeps a
    /// handle to.
    #[doc(hidden)]
    pub fn token_store_arc(mut self, s: Arc<dyn TokenStore>) -> Self {
        self.store = Some(s);
        self
    }

    /// Record every HTTP exchange and trouter frame, redacted, under `p`
    /// (spec §4.11). The directory is created by [`build`](Self::build).
    pub fn capture_dir(self, p: impl Into<PathBuf>) -> Self {
        self.capture_dir_opt(Some(p.into()))
    }

    /// [`capture_dir`](Self::capture_dir), or none.
    #[doc(hidden)]
    pub fn capture_dir_opt(mut self, p: Option<PathBuf>) -> Self {
        self.capture_dir = p;
        self
    }

    /// Replaces every run-time switch at once.
    pub fn options(mut self, o: ClientOptions) -> Self {
        self.options = o;
        self
    }

    /// See [`ClientOptions::register_all_apps`].
    pub fn register_all_apps(mut self, b: bool) -> Self {
        self.options.register_all_apps = b;
        self
    }

    /// See [`ClientOptions::reconnect_on_token_renewal`].
    pub fn reconnect_on_token_renewal(mut self, b: bool) -> Self {
        self.options.reconnect_on_token_renewal = b;
        self
    }

    /// See [`ClientOptions::profile_segment`].
    pub fn profile_segment(mut self, s: &str) -> Self {
        self.options.profile_segment = s.to_string();
        self
    }

    /// See [`ClientOptions::paging`].
    pub fn paging(mut self, p: PagingMode) -> Self {
        self.options.paging = p;
        self
    }

    /// See [`ClientOptions::csa_audience`].
    pub fn csa_audience(mut self, a: CsaAudience) -> Self {
        self.options.csa_audience = a;
        self
    }

    /// See [`ClientOptions::csa_geo`].
    pub fn csa_geo(mut self, geo: &str) -> Self {
        self.options.csa_geo = Some(geo.to_string());
        self
    }

    /// See [`ClientOptions::send_quoted_ids`].
    pub fn send_quoted_ids(mut self, b: bool) -> Self {
        self.options.send_quoted_ids = b;
        self
    }

    /// See [`ClientOptions::clear_unread_bookmark`].
    pub fn clear_unread_bookmark(mut self, b: bool) -> Self {
        self.options.clear_unread_bookmark = b;
        self
    }

    /// See [`ClientOptions::media`].
    pub fn media(mut self, m: MediaOptions) -> Self {
        self.options.media = m;
        self
    }

    /// Called for every pushed EventMessage trouter accepts, with a redacted
    /// copy of its body (for diagnostics).
    pub fn raw_event_hook(mut self, h: RawEventHook) -> Self {
        self.raw_hook = Some(h);
        self
    }

    /// Service base URLs (tests point them at a mock server).
    #[doc(hidden)]
    pub fn endpoints(mut self, e: Endpoints) -> Self {
        self.endpoints = Some(e);
        self
    }

    /// How trouter opens its websocket (tests inject a fake).
    #[doc(hidden)]
    pub fn transport_factory(mut self, f: Arc<dyn TransportFactory>) -> Self {
        self.factory = Some(f);
        self
    }

    /// The `reqwest::Client` to use instead of the production one (tests
    /// under paused time need one without timers).
    #[cfg(test)]
    pub(crate) fn http_client(mut self, c: reqwest::Client) -> Self {
        self.http_client = Some(c);
        self
    }

    /// Builds the client. No network, no runtime needed. Fails without a
    /// token store, or when the capture directory cannot be created.
    pub fn build(self) -> Result<Client> {
        let store = self
            .store
            .ok_or_else(|| Error::Protocol("token_store is required".into()))?;
        let endpoints = self.endpoints.unwrap_or_default();
        #[cfg(test)]
        let client = match self.http_client {
            Some(c) => c,
            None => Http::build_client()?,
        };
        #[cfg(not(test))]
        let client = Http::build_client()?;
        let capture = self
            .capture_dir
            .map(Capture::new)
            .transpose()?
            .map(Arc::new);
        let cancel = CancellationToken::new();
        let tokens = Arc::new(Tokens::new(ManagerDeps {
            http: client.clone(),
            endpoints: endpoints.clone(),
            store: store.clone(),
            cancel: cancel.child_token(),
            csa_audience: self.options.csa_audience,
        }));
        let http = Arc::new(Http::new(client, tokens.clone(), endpoints, capture));
        let api = Api::new(
            http.clone(),
            ApiOptions {
                paging: self.options.paging,
                profile_segment: self.options.profile_segment.clone(),
                csa_geo: self.options.csa_geo.clone(),
                send_quoted_ids: self.options.send_quoted_ids,
                clear_unread_bookmark: self.options.clear_unread_bookmark,
                media: self.options.media.clone(),
                ..ApiOptions::default()
            },
        );
        Ok(Client {
            inner: Arc::new(Inner {
                tenant: self.tenant.unwrap_or_else(|| DEFAULT_TENANT.to_string()),
                store,
                options: self.options,
                raw_hook: self.raw_hook,
                http,
                api,
                tokens,
                factory: self.factory.unwrap_or_else(|| Arc::new(WsTransportFactory)),
                cancel,
                trouter: Mutex::new(None),
                push_address: Arc::new(Mutex::new(None)),
            }),
        })
    }
}

/// The entry point: REST through [`api`](Self::api), push events through
/// [`connect`](Self::connect). Cheap to clone; every clone is the same
/// client.
#[derive(Clone)]
pub struct Client {
    inner: Arc<Inner>,
}

impl std::fmt::Debug for Client {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Client")
            .field("tenant", &self.inner.tenant)
            .field("options", &self.inner.options)
            .finish_non_exhaustive()
    }
}

struct Inner {
    tenant: String,
    store: Arc<dyn TokenStore>,
    options: ClientOptions,
    raw_hook: Option<RawEventHook>,
    http: Arc<Http>,
    api: Api,
    tokens: Arc<Tokens>,
    factory: Arc<dyn TransportFactory>,
    /// Parent of every background task's token.
    cancel: CancellationToken,
    /// The running trouter, if [`Client::connect`] was called.
    trouter: Mutex<Option<Background>>,
    /// The registered trouter connection's address, for presence.
    push_address: Arc<Mutex<Option<String>>>,
}

impl Drop for Inner {
    fn drop(&mut self) {
        self.cancel.cancel();
    }
}

/// A spawned background task and the token that stops it.
struct Background {
    cancel: CancellationToken,
    join: JoinHandle<()>,
}

impl Background {
    /// Cancels the task and waits for it to finish (trouter closes its
    /// socket on the way out).
    async fn stop(self) {
        self.cancel.cancel();
        if let Err(e) = self.join.await {
            tracing::warn!(error = %e, "trouter task failed");
        }
    }
}

impl Client {
    /// Starts configuring a client.
    pub fn builder() -> ClientBuilder {
        ClientBuilder::default()
    }

    /// `true` when there is no stored session: run the device-code login.
    pub async fn needs_login(&self) -> Result<bool> {
        Ok(self.inner.store.load().await?.is_none())
    }

    /// Starts a device-code sign-in (spec §4.3): show
    /// [`DeviceCodeLogin::prompt`] to the user, then await
    /// [`DeviceCodeLogin::wait`].
    pub async fn start_device_login(&self) -> Result<DeviceCodeLogin> {
        let inner = &self.inner;
        let start =
            oauth::start_device_code(inner.http.raw(), inner.http.endpoints(), &inner.tenant)
                .await?;
        // The prompt's expiry is wall-clock; polling runs on the monotonic
        // clock.
        let remaining =
            u64::try_from(start.prompt.expires_at.saturating_sub(now_ms())).unwrap_or(0);
        Ok(DeviceCodeLogin {
            prompt: start.prompt,
            device_code: start.device_code,
            interval: start.interval,
            deadline: Instant::now() + Duration::from_millis(remaining),
            client: self.clone(),
        })
    }

    /// Signs out: forgets the tokens and clears the stored session.
    pub async fn logout(&self) -> Result<()> {
        self.inner.api.forget_me().await;
        if let Some(handle) = self.inner.tokens.try_handle()
            && handle.clear().await.is_ok()
        {
            return Ok(());
        }
        // Never started, or already stopped: the store is all there is.
        self.inner.store.clear().await
    }

    /// The chatsvc REST client. Works without [`connect`](Self::connect).
    pub fn api(&self) -> &Api {
        &self.inner.api
    }

    /// The signed-in user, fetched on first use and cached.
    pub async fn me(&self) -> Result<Me> {
        self.inner.api.me().await
    }

    /// See [`Api::user_state`].
    pub async fn user_state(&self) -> Result<(crate::csa::UserState, crate::csa::ParseReport)> {
        self.inner.api.user_state().await
    }

    /// Starts trouter and returns its events. Dropping the stream stops
    /// trouter (REST keeps working). There is one trouter per client: a
    /// second call stops the first, whose stream then ends.
    pub async fn connect(&self) -> Result<EventStream> {
        let inner = &self.inner;
        let mut cfg = TrouterConfig::new(self.endpoint_id().await?);
        cfg.register_all_apps = inner.options.register_all_apps;
        cfg.reconnect_on_token_renewal = inner.options.reconnect_on_token_renewal;
        let (tx, rx) = mpsc::channel(EVENT_CHANNEL_CAPACITY);
        let cancel = inner.cancel.child_token();
        let deps = TrouterDeps {
            http: inner.http.clone(),
            factory: inner.factory.clone(),
            tx,
            cancel: cancel.clone(),
            capture: inner.http.capture().cloned(),
            raw_hook: inner.raw_hook.clone(),
            push_address: inner.push_address.clone(),
        };
        let join = tokio::spawn(trouter::run(cfg, deps));
        let previous = inner
            .trouter
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .replace(Background {
                cancel: cancel.clone(),
                join,
            });
        if let Some(previous) = previous {
            previous.stop().await;
        }
        Ok(EventStream {
            rx,
            _guard: cancel.drop_guard(),
        })
    }

    /// Stops every background task (trouter, token manager) and waits for
    /// them. The client is unusable afterwards.
    pub async fn shutdown(&self) {
        let inner = &self.inner;
        inner.cancel.cancel();
        let trouter = inner
            .trouter
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .take();
        if let Some(trouter) = trouter {
            trouter.stop().await;
        }
        inner.tokens.shutdown().await;
    }

    /// The run-time switches this client was built with.
    pub fn options(&self) -> &ClientOptions {
        &self.inner.options
    }

    /// The push connection's address (its `surl`) once it has registered
    /// (after [`Event::Connected`]), for
    /// [`Api::subscribe_presence`](crate::Api::subscribe_presence); `None`
    /// before.
    pub fn push_address(&self) -> Option<String> {
        self.inner
            .push_address
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    /// This installation's endpoint id (trouter `epid`), from the stored
    /// session; `ReauthRequired` without one.
    pub async fn endpoint_id(&self) -> Result<String> {
        match self.inner.store.load().await? {
            Some(session) => Ok(session.endpoint_id),
            None => Err(Error::ReauthRequired),
        }
    }

    /// The token manager's handle (starting it if needed).
    pub async fn tokens(&self) -> &TokenHandle {
        self.inner.tokens.handle().await
    }
}

/// A device-code sign-in in progress. Show [`prompt`](Self::prompt), then
/// await [`wait`](Self::wait); dropping the `wait` future cancels it.
#[derive(Debug)]
pub struct DeviceCodeLogin {
    prompt: DeviceCodePrompt,
    device_code: Secret<String>,
    interval: Duration,
    deadline: Instant,
    client: Client,
}

impl DeviceCodeLogin {
    /// What to show the user: the code and where to enter it.
    pub fn prompt(&self) -> &DeviceCodePrompt {
        &self.prompt
    }

    /// Polls until the user has signed in, honouring the service's interval
    /// (plus 5 s per `slow_down`), then stores the session and hands the
    /// tokens to the token manager. Fails when the code expires or the
    /// service rejects the flow.
    pub async fn wait(mut self) -> Result<()> {
        let inner = &self.client.inner;
        loop {
            tokio::time::sleep(self.interval).await;
            if Instant::now() >= self.deadline {
                return Err(Error::Protocol("device code expired".into()));
            }
            let poll = oauth::poll_device_code(
                inner.http.raw(),
                inner.http.endpoints(),
                &inner.tenant,
                &self.device_code,
            )
            .await?;
            match poll {
                PollOutcome::Pending => {}
                PollOutcome::SlowDown => self.interval += SLOW_DOWN_STEP,
                PollOutcome::Granted(token) => {
                    let refresh_token = token.refresh_token.clone().ok_or_else(|| {
                        Error::Protocol("device code grant without a refresh_token".into())
                    })?;
                    // One endpoint id per installation: keep the stored one.
                    let endpoint_id = match inner.store.load().await? {
                        Some(old) => old.endpoint_id,
                        None => uuid::Uuid::new_v4().to_string(),
                    };
                    let session = StoredSession {
                        refresh_token,
                        tenant: inner.tenant.clone(),
                        endpoint_id,
                    };
                    let installed = inner.tokens.handle().await.install(session, token).await;
                    // After the install (even a failed one: the session is
                    // already stored), so a `me()` that raced it with the old
                    // tokens cannot leave the previous user cached.
                    inner.api.forget_me().await;
                    installed?;
                    return Ok(());
                }
            }
        }
    }
}

/// trouter's events (spec §4.7), from [`Client::connect`]. Dropping it stops
/// trouter.
#[derive(Debug)]
pub struct EventStream {
    rx: mpsc::Receiver<Event>,
    _guard: DropGuard,
}

impl EventStream {
    /// The next event; `None` once trouter has stopped.
    pub async fn recv(&mut self) -> Option<Event> {
        self.rx.recv().await
    }
}

impl futures::Stream for EventStream {
    type Item = Event;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Event>> {
        self.rx.poll_recv(cx)
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::time::Duration;

    use futures::StreamExt;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    use super::*;
    use crate::auth::{CsaAudience, RefreshKind};
    use crate::auth::{Endpoints, MemoryTokenStore, StoredSession, TokenStore};
    use crate::test_support::{hold_clock, mount_auth, mount_me, mount_trouter_http, test_client};
    use crate::trouter::transport::FakeFactory;
    use crate::{Error, Event, PagingMode, Secret};

    const CONVERSATIONS: &str = "/v1/users/ME/conversations";
    const DEVICE_CODE: &str = "/organizations/oauth2/devicecode";
    const DEVICE_TOKEN: &str = "/organizations/oauth2/token";
    const REGISTRAR: &str = "/registrar/prod/V2/registrations";

    fn stored(rt: &str) -> StoredSession {
        StoredSession {
            refresh_token: Secret::new(rt.into()),
            tenant: "organizations".into(),
            endpoint_id: "ep".into(),
        }
    }

    async fn seeded_store() -> Arc<MemoryTokenStore> {
        let store = Arc::new(MemoryTokenStore::default());
        store.save(&stored("rt")).await.unwrap();
        store
    }

    /// A client over `store` with every service at `s` and a paused-time
    /// safe HTTP client.
    fn client_on(s: &MockServer, store: Arc<MemoryTokenStore>) -> Client {
        Client::builder()
            .token_store_arc(store)
            .endpoints(Endpoints::all(&s.uri()))
            .http_client(test_client())
            .build()
            .unwrap()
    }

    async fn mount_conversations(s: &MockServer) {
        Mock::given(method("GET"))
            .and(path(CONVERSATIONS))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(serde_json::json!({"conversations": []})),
            )
            .mount(s)
            .await;
    }

    async fn mount_device_code(s: &MockServer, interval: u64, expires_in: u64) {
        Mock::given(method("POST"))
            .and(path(DEVICE_CODE))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "user_code": "CODE",
                "device_code": "dc",
                "verification_url": "https://microsoft.com/devicelogin",
                "interval": interval,
                "expires_in": expires_in,
            })))
            .mount(s)
            .await;
    }

    async fn mount_poll(s: &MockServer, error: &str, times: Option<u64>) {
        let mock = Mock::given(method("POST"))
            .and(path(DEVICE_TOKEN))
            .respond_with(
                ResponseTemplate::new(400).set_body_json(serde_json::json!({"error": error})),
            );
        match times {
            Some(n) => mock.up_to_n_times(n).mount(s).await,
            None => mock.mount(s).await,
        }
    }

    async fn mount_granted(s: &MockServer) {
        Mock::given(method("POST"))
            .and(path(DEVICE_TOKEN))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "access_token": "at-dc",
                "refresh_token": "rt-dc",
                "expires_in": 3600,
            })))
            .mount(s)
            .await;
    }

    #[tokio::test]
    async fn build_does_no_network_and_rest_works_without_connect() {
        let s = MockServer::start().await;
        mount_auth(&s).await;
        Mock::given(method("GET"))
            .and(path(CONVERSATIONS))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(serde_json::json!({"conversations": []})),
            )
            .expect(1)
            .mount(&s)
            .await;
        let store = MemoryTokenStore::default();
        store.save(&stored("rt")).await.unwrap();
        let client = Client::builder()
            .tenant("organizations")
            .token_store(store)
            .endpoints(Endpoints::all(&s.uri()))
            .build()
            .unwrap();
        assert!(
            s.received_requests().await.unwrap().is_empty(),
            "no network at build"
        );
        assert!(!client.needs_login().await.unwrap());
        assert!(
            client
                .api()
                .conversations(None, None)
                .await
                .unwrap()
                .items
                .is_empty()
        );
        assert_eq!(client.endpoint_id().await.unwrap(), "ep");
        client.shutdown().await;
    }

    #[test]
    fn build_needs_no_runtime() {
        let client = Client::builder()
            .token_store(MemoryTokenStore::default())
            .build();
        assert!(client.is_ok());
    }

    #[test]
    fn client_and_friends_are_send_and_sync() {
        fn assert_send_sync<T: Send + Sync + 'static>() {}
        fn assert_clone<T: Clone>() {}
        assert_send_sync::<Client>();
        assert_clone::<Client>();
        assert_send_sync::<DeviceCodeLogin>();
        assert_send_sync::<EventStream>();
        assert_send_sync::<ClientBuilder>();
    }

    #[tokio::test(start_paused = true)]
    async fn device_login_flow_persists_session_and_installs_tokens() {
        let s = MockServer::start().await;
        mount_auth(&s).await;
        mount_device_code(&s, 5, 900).await;
        mount_poll(&s, "authorization_pending", Some(2)).await;
        mount_granted(&s).await;
        mount_me(&s).await;
        let store = Arc::new(MemoryTokenStore::default());
        let client = client_on(&s, store.clone());
        assert!(client.needs_login().await.unwrap());
        let login = client.start_device_login().await.unwrap();
        assert_eq!(login.prompt().user_code, "CODE");
        assert_eq!(
            login.prompt().verification_url,
            "https://microsoft.com/devicelogin"
        );
        let t0 = tokio::time::Instant::now();
        login.wait().await.unwrap();
        assert!(
            t0.elapsed() >= Duration::from_secs(15),
            "three polls, 5 s apart"
        );
        let saved = store.load().await.unwrap().unwrap();
        assert_eq!(saved.refresh_token.expose(), "rt-dc");
        assert_eq!(saved.tenant, "organizations");
        assert!(uuid::Uuid::parse_str(&saved.endpoint_id).is_ok());
        assert_eq!(client.endpoint_id().await.unwrap(), saved.endpoint_id);
        let snap = client.tokens().await.current();
        assert_eq!(snap.access_token.unwrap().expose(), "at-dc");
        assert!(snap.skype_token.is_some(), "authz ran after install");
        assert_eq!(client.me().await.unwrap().display_name, "Ada Example");
        assert!(!client.needs_login().await.unwrap());
        client.logout().await.unwrap();
        assert!(client.needs_login().await.unwrap());
        assert!(store.load().await.unwrap().is_none());
        client.shutdown().await;
    }

    #[tokio::test(start_paused = true)]
    async fn device_login_keeps_an_existing_endpoint_id() {
        let s = MockServer::start().await;
        mount_auth(&s).await;
        mount_device_code(&s, 5, 900).await;
        mount_granted(&s).await;
        let store = seeded_store().await;
        let client = client_on(&s, store.clone());
        client
            .start_device_login()
            .await
            .unwrap()
            .wait()
            .await
            .unwrap();
        let saved = store.load().await.unwrap().unwrap();
        assert_eq!(saved.refresh_token.expose(), "rt-dc");
        assert_eq!(saved.endpoint_id, "ep");
        client.shutdown().await;
    }

    #[tokio::test(start_paused = true)]
    async fn slow_down_adds_five_seconds_to_the_interval() {
        let s = MockServer::start().await;
        mount_auth(&s).await;
        mount_device_code(&s, 5, 900).await;
        mount_poll(&s, "slow_down", Some(1)).await;
        mount_granted(&s).await;
        let client = client_on(&s, Arc::new(MemoryTokenStore::default()));
        let login = client.start_device_login().await.unwrap();
        let t0 = tokio::time::Instant::now();
        login.wait().await.unwrap();
        let elapsed = t0.elapsed();
        assert!(
            elapsed >= Duration::from_secs(15) && elapsed < Duration::from_secs(20),
            "5 s, then 10 s after slow_down: {elapsed:?}"
        );
        client.shutdown().await;
    }

    #[tokio::test(start_paused = true)]
    async fn an_expired_device_code_stops_waiting() {
        let s = MockServer::start().await;
        mount_device_code(&s, 5, 12).await;
        mount_poll(&s, "authorization_pending", None).await;
        let client = client_on(&s, Arc::new(MemoryTokenStore::default()));
        let login = client.start_device_login().await.unwrap();
        let err = login.wait().await.unwrap_err();
        assert!(
            matches!(&err, Error::Protocol(m) if m.contains("expired")),
            "{err:?}"
        );
        let polls = s
            .received_requests()
            .await
            .unwrap()
            .iter()
            .filter(|r| r.url.path() == DEVICE_TOKEN)
            .count();
        assert_eq!(polls, 2, "polls at 5 s and 10 s; 15 s is past the expiry");
        assert!(client.needs_login().await.unwrap());
    }

    #[tokio::test]
    async fn login_wait_can_be_cancelled_by_dropping_the_future() {
        let s = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path(DEVICE_CODE))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "user_code": "CODE", "device_code": "dc", "verification_url": "u",
                "interval": 5, "expires_in": 900,
            })))
            .mount(&s)
            .await;
        mount_poll(&s, "authorization_pending", None).await;
        let client = Client::builder()
            .token_store(MemoryTokenStore::default())
            .endpoints(Endpoints::all(&s.uri()))
            .build()
            .unwrap();
        let login = client.start_device_login().await.unwrap();
        let h = tokio::spawn(login.wait());
        tokio::time::sleep(Duration::from_millis(1)).await;
        h.abort();
        assert!(h.await.unwrap_err().is_cancelled());
        assert!(client.needs_login().await.unwrap());
    }

    #[tokio::test(start_paused = true)]
    async fn connect_streams_events_and_drop_stops_trouter() {
        let s = MockServer::start().await;
        mount_auth(&s).await;
        mount_trouter_http(&s).await;
        let (factory, mut conns) = FakeFactory::new();
        let store = MemoryTokenStore::default();
        store.save(&stored("rt")).await.unwrap();
        let client = Client::builder()
            .token_store(store)
            .endpoints(Endpoints::all(&s.uri()))
            .transport_factory(factory)
            .http_client(test_client())
            .build()
            .unwrap();
        let mut events = client.connect().await.unwrap();
        let mut conn = hold_clock(conns.recv()).await.unwrap();
        assert!(conn.url.contains("epid=ep"), "endpoint id from the session");
        conn.to_client.send(Ok(Some("1::".into()))).await.unwrap();
        assert!(matches!(
            hold_clock(events.next()).await,
            Some(Event::Connected)
        ));
        drop(events);
        // The fake transport (and its sender) is dropped when the trouter
        // task ends, so the receiver drains to None.
        hold_clock(async { while conn.from_client.recv().await.is_some() {} }).await;
        mount_conversations(&s).await;
        assert!(
            hold_clock(client.api().conversations(None, None))
                .await
                .is_ok(),
            "REST still usable after dropping the stream"
        );
        hold_clock(client.shutdown()).await;
    }

    #[tokio::test(start_paused = true)]
    async fn shutdown_stops_trouter_and_the_token_manager() {
        let s = MockServer::start().await;
        mount_auth(&s).await;
        mount_trouter_http(&s).await;
        let (factory, mut conns) = FakeFactory::new();
        let client = Client::builder()
            .token_store_arc(seeded_store().await)
            .endpoints(Endpoints::all(&s.uri()))
            .transport_factory(factory)
            .http_client(test_client())
            .build()
            .unwrap();
        let mut events = client.connect().await.unwrap();
        let mut conn = hold_clock(conns.recv()).await.unwrap();
        conn.to_client.send(Ok(Some("1::".into()))).await.unwrap();
        assert!(matches!(
            hold_clock(events.next()).await,
            Some(Event::Connected)
        ));
        hold_clock(client.shutdown()).await;
        // Awaited, not just cancelled: the transport is already gone (only
        // frames it sent before stopping are left).
        loop {
            match conn.from_client.try_recv() {
                Ok(_) => {}
                Err(e) => {
                    assert_eq!(e, mpsc::error::TryRecvError::Disconnected);
                    break;
                }
            }
        }
        assert!(conn.to_client.is_closed(), "trouter closed its socket");
        assert!(hold_clock(events.next()).await.is_none());
        let tokens = client.tokens().await;
        assert!(matches!(
            tokens.ready().await,
            Err(Error::Protocol(m)) if m.contains("stopped")
        ));
    }

    #[tokio::test(start_paused = true)]
    async fn stopping_a_background_task_waits_for_it_to_finish() {
        use std::sync::atomic::{AtomicBool, Ordering};
        let cancel = CancellationToken::new();
        let done = Arc::new(AtomicBool::new(false));
        let (token, flag) = (cancel.clone(), done.clone());
        let join = tokio::spawn(async move {
            token.cancelled().await;
            // Like trouter closing its socket on the way out.
            tokio::time::sleep(Duration::from_millis(20)).await;
            flag.store(true, Ordering::SeqCst);
        });
        Background { cancel, join }.stop().await;
        assert!(done.load(Ordering::SeqCst));
    }

    #[tokio::test(start_paused = true)]
    async fn dropping_the_last_client_stops_trouter() {
        let s = MockServer::start().await;
        mount_auth(&s).await;
        mount_trouter_http(&s).await;
        let (factory, mut conns) = FakeFactory::new();
        let client = Client::builder()
            .token_store_arc(seeded_store().await)
            .endpoints(Endpoints::all(&s.uri()))
            .transport_factory(factory)
            .http_client(test_client())
            .build()
            .unwrap();
        let mut events = client.connect().await.unwrap();
        let conn = hold_clock(conns.recv()).await.unwrap();
        conn.to_client.send(Ok(Some("1::".into()))).await.unwrap();
        assert!(matches!(
            hold_clock(events.next()).await,
            Some(Event::Connected)
        ));
        drop(client);
        // Inner::drop cancelled the trouter's parent token: the stream ends.
        assert!(hold_clock(events.next()).await.is_none());
    }

    #[tokio::test(start_paused = true)]
    async fn a_second_connect_replaces_the_first_trouter() {
        let s = MockServer::start().await;
        mount_auth(&s).await;
        mount_trouter_http(&s).await;
        let (factory, mut conns) = FakeFactory::new();
        let client = Client::builder()
            .token_store_arc(seeded_store().await)
            .endpoints(Endpoints::all(&s.uri()))
            .transport_factory(factory)
            .http_client(test_client())
            .build()
            .unwrap();
        let mut first = client.connect().await.unwrap();
        let _conn = hold_clock(conns.recv()).await.unwrap();
        let mut second = hold_clock(client.connect()).await.unwrap();
        assert!(
            hold_clock(first.recv()).await.is_none(),
            "one trouter per endpoint id: the first stream ends"
        );
        let conn = hold_clock(conns.recv()).await.unwrap();
        conn.to_client.send(Ok(Some("1::".into()))).await.unwrap();
        assert!(matches!(
            hold_clock(second.recv()).await,
            Some(Event::Connected)
        ));
        hold_clock(client.shutdown()).await;
    }

    #[tokio::test]
    async fn connect_without_a_session_is_reauth_required() {
        let client = Client::builder()
            .token_store(MemoryTokenStore::default())
            .endpoints(Endpoints::all("http://127.0.0.1:1"))
            .build()
            .unwrap();
        assert!(matches!(
            client.endpoint_id().await,
            Err(Error::ReauthRequired)
        ));
        assert!(matches!(client.connect().await, Err(Error::ReauthRequired)));
    }

    #[tokio::test]
    async fn me_is_fetched_once_and_cached() {
        let s = MockServer::start().await;
        mount_auth(&s).await;
        Mock::given(method("GET"))
            .and(path("/v1/users/ME/properties"))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(
                    serde_json::from_str::<serde_json::Value>(fixture!(
                        "9.09-names/me-properties.json"
                    ))
                    .unwrap(),
                ),
            )
            .expect(1)
            .mount(&s)
            .await;
        let client = client_on(&s, seeded_store().await);
        assert_eq!(client.me().await.unwrap().display_name, "Ada Example");
        assert_eq!(client.me().await.unwrap().display_name, "Ada Example");
        client.shutdown().await;
    }

    /// `/v1/users/ME/properties` for `name`, answered once.
    async fn mount_me_once(s: &MockServer, name: &str) {
        let user_details = serde_json::json!({ "name": name }).to_string();
        Mock::given(method("GET"))
            .and(path("/v1/users/ME/properties"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "skypeName": format!("orgid:{name}"),
                "userDetails": user_details,
            })))
            .up_to_n_times(1)
            .mount(s)
            .await;
    }

    #[tokio::test(start_paused = true)]
    async fn a_new_session_refetches_me() {
        let s = MockServer::start().await;
        mount_auth(&s).await;
        mount_device_code(&s, 5, 900).await;
        mount_granted(&s).await;
        mount_me_once(&s, "Alice").await;
        mount_me_once(&s, "Bob").await;
        mount_me_once(&s, "Carol").await;
        let client = client_on(&s, Arc::new(MemoryTokenStore::default()));
        client
            .start_device_login()
            .await
            .unwrap()
            .wait()
            .await
            .unwrap();
        assert_eq!(client.me().await.unwrap().display_name, "Alice");
        assert_eq!(client.me().await.unwrap().display_name, "Alice", "cached");
        client.logout().await.unwrap();
        client
            .start_device_login()
            .await
            .unwrap()
            .wait()
            .await
            .unwrap();
        assert_eq!(client.me().await.unwrap().display_name, "Bob");
        // A login over a live session (no logout first) also starts afresh.
        client
            .start_device_login()
            .await
            .unwrap()
            .wait()
            .await
            .unwrap();
        assert_eq!(client.me().await.unwrap().display_name, "Carol");
        client.shutdown().await;
    }

    #[tokio::test]
    async fn logout_before_any_token_use_clears_the_store_without_network() {
        let s = MockServer::start().await;
        let store = seeded_store().await;
        let client = client_on(&s, store.clone());
        client.logout().await.unwrap();
        assert!(client.needs_login().await.unwrap());
        assert!(s.received_requests().await.unwrap().is_empty());
    }

    // Spec §7 / Task 10 ruling: whichever path yields `ReauthRequired`, the
    // stored session is gone afterwards.

    #[tokio::test]
    async fn a_second_401_after_a_good_refresh_keeps_the_session() {
        let s = MockServer::start().await;
        mount_auth(&s).await;
        Mock::given(method("GET"))
            .and(path(CONVERSATIONS))
            .respond_with(ResponseTemplate::new(401))
            .mount(&s)
            .await;
        let store = seeded_store().await;
        let client = client_on(&s, store.clone());
        assert!(matches!(
            client.api().conversations(None, None).await,
            Err(Error::Http { status: 401, .. })
        ));
        assert!(store.load().await.unwrap().is_some(), "session kept");
        assert!(!client.needs_login().await.unwrap());
        assert!(client.tokens().await.ready().await.is_ok());
        client.shutdown().await;
    }

    #[tokio::test]
    async fn a_rejected_refresh_token_clears_the_session() {
        let s = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/organizations/oauth2/v2.0/token"))
            .respond_with(
                ResponseTemplate::new(400)
                    .set_body_json(serde_json::json!({"error": "invalid_grant"})),
            )
            .mount(&s)
            .await;
        let store = seeded_store().await;
        let client = client_on(&s, store.clone());
        assert!(matches!(client.me().await, Err(Error::ReauthRequired)));
        assert!(store.load().await.unwrap().is_none(), "session cleared");
        assert!(client.needs_login().await.unwrap());
        client.shutdown().await;
    }

    #[tokio::test(start_paused = true)]
    async fn reauth_in_the_trouter_clears_the_session_and_ends_the_stream() {
        let s = MockServer::start().await;
        mount_auth(&s).await;
        mount_trouter_http(&s).await;
        let (factory, mut conns) = FakeFactory::new();
        let store = seeded_store().await;
        let client = Client::builder()
            .token_store_arc(store.clone())
            .endpoints(Endpoints::all(&s.uri()))
            .transport_factory(factory)
            .http_client(test_client())
            .build()
            .unwrap();
        let mut events = client.connect().await.unwrap();
        let conn = hold_clock(conns.recv()).await.unwrap();
        // Registration meets a 401 and the refresh that follows finds the
        // refresh token revoked.
        s.reset().await;
        Mock::given(method("POST"))
            .and(path(REGISTRAR))
            .respond_with(ResponseTemplate::new(401))
            .mount(&s)
            .await;
        Mock::given(method("POST"))
            .and(path("/organizations/oauth2/v2.0/token"))
            .respond_with(
                ResponseTemplate::new(400)
                    .set_body_json(serde_json::json!({"error": "invalid_grant"})),
            )
            .mount(&s)
            .await;
        conn.to_client.send(Ok(Some("1::".into()))).await.unwrap();
        assert!(matches!(
            hold_clock(events.next()).await,
            Some(Event::ReauthRequired)
        ));
        assert!(hold_clock(events.next()).await.is_none(), "trouter stopped");
        assert!(store.load().await.unwrap().is_none(), "session cleared");
        assert!(client.needs_login().await.unwrap());
        hold_clock(client.shutdown()).await;
    }

    #[test]
    fn option_defaults_follow_the_spec() {
        let o = ClientOptions::default();
        assert!(!o.register_all_apps);
        assert!(o.reconnect_on_token_renewal);
        assert_eq!(o.profile_segment, "apac");
        assert_eq!(o.paging, PagingMode::Auto);
        let c = Client::builder()
            .token_store(MemoryTokenStore::default())
            .build()
            .unwrap();
        assert_eq!(c.options(), &ClientOptions::default());
        assert_eq!(c.api().options().conversations_page_size, 100);
    }

    #[tokio::test]
    async fn csa_audience_reaches_the_token_manager() {
        let s = MockServer::start().await;
        mount_auth(&s).await;
        let default = client_on(&s, seeded_store().await);
        assert_eq!(default.options().csa_audience, CsaAudience::Chatsvcagg);
        assert_eq!(
            default.tokens().await.csa_refresh_kind(),
            RefreshKind::CsaToken
        );
        // A second server: the token mocks count per server.
        let s2 = MockServer::start().await;
        mount_auth(&s2).await;
        let spaces = Client::builder()
            .token_store_arc(seeded_store().await)
            .endpoints(Endpoints::all(&s2.uri()))
            .http_client(test_client())
            .csa_audience(CsaAudience::Spaces)
            .build()
            .unwrap();
        assert_eq!(spaces.options().csa_audience, CsaAudience::Spaces);
        assert_eq!(
            spaces.tokens().await.csa_token().await.unwrap().expose(),
            "at-1",
            "spaces: the main access token, no extra grant"
        );
        default.shutdown().await;
        spaces.shutdown().await;
    }

    #[test]
    fn csa_geo_reaches_the_api_options() {
        let c = Client::builder()
            .token_store(MemoryTokenStore::default())
            .csa_geo("apac")
            .build()
            .unwrap();
        assert_eq!(c.options().csa_geo.as_deref(), Some("apac"));
        assert_eq!(c.api().options().csa_geo.as_deref(), Some("apac"));
        let d = Client::builder()
            .token_store(MemoryTokenStore::default())
            .build()
            .unwrap();
        assert_eq!(d.api().options().csa_geo, None);
    }

    #[test]
    fn send_quoted_ids_reaches_the_api_options() {
        let on = Client::builder()
            .token_store(MemoryTokenStore::default())
            .build()
            .unwrap();
        assert!(on.options().send_quoted_ids && on.api().options().send_quoted_ids);
        let off = Client::builder()
            .token_store(MemoryTokenStore::default())
            .send_quoted_ids(false)
            .build()
            .unwrap();
        assert!(!off.options().send_quoted_ids && !off.api().options().send_quoted_ids);
    }

    #[test]
    fn clear_unread_bookmark_reaches_the_api_options() {
        let on = Client::builder()
            .token_store(MemoryTokenStore::default())
            .build()
            .unwrap();
        assert!(on.options().clear_unread_bookmark && on.api().options().clear_unread_bookmark);
        let off = Client::builder()
            .token_store(MemoryTokenStore::default())
            .clear_unread_bookmark(false)
            .build()
            .unwrap();
        assert!(!off.options().clear_unread_bookmark && !off.api().options().clear_unread_bookmark);
    }

    #[tokio::test]
    async fn options_are_plumbed() {
        let c = Client::builder()
            .register_all_apps(true)
            .reconnect_on_token_renewal(false)
            .profile_segment("emea")
            .paging(PagingMode::Window)
            .token_store(MemoryTokenStore::default())
            .build()
            .unwrap();
        assert_eq!(
            c.options(),
            &ClientOptions {
                register_all_apps: true,
                reconnect_on_token_renewal: false,
                profile_segment: "emea".into(),
                paging: PagingMode::Window,
                ..ClientOptions::default()
            }
        );
        assert_eq!(c.api().options().paging, PagingMode::Window);
        assert_eq!(c.api().options().profile_segment, "emea");
        let whole = ClientOptions {
            register_all_apps: true,
            ..ClientOptions::default()
        };
        let c = Client::builder()
            .options(whole.clone())
            .token_store(MemoryTokenStore::default())
            .build()
            .unwrap();
        assert_eq!(c.options(), &whole);
        assert!(
            Client::builder().build().is_err(),
            "a token store is required"
        );
    }

    /// Spec §4.11 end to end: with `capture_dir`, REST exchanges (tokens,
    /// authz, conversations, trouter's v4a/session/registrar) and trouter
    /// frames land on disk, redacted.
    #[tokio::test(start_paused = true)]
    async fn capture_dir_records_http_and_frames() {
        let s = MockServer::start().await;
        mount_auth(&s).await;
        mount_trouter_http(&s).await;
        mount_conversations(&s).await;
        let dir = tempfile::tempdir().unwrap();
        let (factory, mut conns) = FakeFactory::new();
        let store = MemoryTokenStore::default();
        store.save(&stored("rt-stored")).await.unwrap();
        let client = Client::builder()
            .token_store(store)
            .endpoints(Endpoints::all(&s.uri()))
            .transport_factory(factory)
            .http_client(test_client())
            .capture_dir(dir.path())
            .build()
            .unwrap();
        hold_clock(client.api().conversations(None, None))
            .await
            .unwrap();
        let mut events = client.connect().await.unwrap();
        let conn = hold_clock(conns.recv()).await.unwrap();
        conn.to_client.send(Ok(Some("1::".into()))).await.unwrap();
        assert!(matches!(
            hold_clock(events.next()).await,
            Some(Event::Connected)
        ));
        hold_clock(client.shutdown()).await;

        let files: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .map(|e| e.unwrap().path())
            .collect();
        let names: Vec<String> = files
            .iter()
            .map(|f| f.file_name().unwrap().to_string_lossy().into_owned())
            .collect();
        assert!(
            files.len() >= 4,
            "conversations, v4a, session, registrar, ws frames: {names:?}"
        );
        assert!(names.iter().any(|n| n.contains("-http-")), "{names:?}");
        assert!(names.iter().any(|n| n.contains("-ws-in-")), "{names:?}");
        assert!(names.iter().any(|n| n.contains("-ws-out-")), "{names:?}");
        for f in &files {
            let t = std::fs::read_to_string(f).unwrap();
            for secret in [
                "sk-1",
                "at-1",
                "rt-1",
                "rt-stored",
                "sr-value",
                "sig-value",
                "ccid-xyz",
                "AbCdEf123456",
            ] {
                assert!(!t.contains(secret), "{f:?} leaked {secret}:\n{t}");
            }
        }
    }

    #[test]
    fn a_capture_dir_is_created_at_build() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("captures");
        let c = Client::builder()
            .token_store(MemoryTokenStore::default())
            .capture_dir(&target)
            .build()
            .unwrap();
        assert!(c.api().capture().is_some());
        assert!(target.is_dir());
        let c = Client::builder()
            .token_store(MemoryTokenStore::default())
            .capture_dir_opt(None)
            .build()
            .unwrap();
        assert!(c.api().capture().is_none());
    }
}
