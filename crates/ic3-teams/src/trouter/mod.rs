//! The trouter push channel (spec §4.6): socket.io framing, body decoding,
//! session negotiation, registrar registration, and the connection state
//! machine that turns pushed `/messaging` requests into [`Event`]s.
//!
//! `run` owns one task: it connects, authenticates, registers, pings and
//! reconnects. It never awaits the consumer (§4.7): events go out with
//! `try_send`; on overflow the dropped events are counted and
//! `NeedsCatchUp { Overflow }` follows once the consumer drains. Pings run
//! in the same frame loop (decision #18), which only ever waits on the
//! socket, timers and the spawned registration, so a slow consumer cannot
//! stop them.

pub(crate) mod connect;
pub(crate) mod frames;
pub(crate) mod transport;

#[cfg(test)]
mod tests;

use std::convert::Infallible;
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use serde_json::Value;
use tokio::sync::mpsc::{self, error::TrySendError};
use tokio::task::JoinHandle;
use tokio::time::{Instant, MissedTickBehavior, interval_at};
use tokio_util::sync::CancellationToken;
use tracing::{debug, warn};

use crate::auth::{TokenHandle, TokenSnapshot};
use crate::capture::{Capture, Direction};
use crate::events::{
    CatchUpReason, DedupRing, DisconnectReason, Event, EventMessage, RawEvent, RawEventHook,
    map_event_message,
};
use crate::http::Http;
use crate::redact::{redact_json, redact_text, redact_url};
use crate::types::now_ms;
use crate::{Error, Result};
use connect::{TrouterSession, con_num, negotiate, register, websocket_url};
use frames::{
    Frame, correlation_vector, decode_request_body, encode_ack, encode_ephemeral, encode_sequenced,
    parse_frame, ping, user_activity, user_authenticate,
};
pub use transport::{Transport, TransportFactory, WsTransportFactory};

/// How many EventMessage `(id, time)` pairs the duplicate ring keeps (§4.7).
const DEDUP_CAPACITY: usize = 64;
/// Registrations are renewed this long before their TTL runs out (§4.6).
const REREGISTER_MARGIN: Duration = Duration::from_secs(10);
/// Upper bound of the random jitter added to each backoff step.
const MAX_JITTER_MS: u64 = 250;
/// A registered session that lasted this long resets the reconnect backoff
/// (one dropped right after registering does not: no reconnect storm).
const STABLE_SESSION: Duration = Duration::from_secs(30);
/// `trouter.message_loss` re-registers only when no registration is in
/// flight and none succeeded within this long (live 2026-10-14: five losses
/// in ~30 s right after connecting).
const MESSAGE_LOSS_REREGISTER_GAP: Duration = Duration::from_secs(30);

/// trouter connection settings.
#[derive(Clone, Debug)]
pub(crate) struct TrouterConfig {
    /// This client's endpoint id (`epid`, registrar `registrationId`).
    pub endpoint_id: String,
    /// Also register purple-teams' two calling apps (§4.6 step 4).
    pub register_all_apps: bool,
    /// Reconnect after every Skype-token renewal (§4.6 "Token renewal");
    /// `false` keeps the socket open across renewals.
    // UNVERIFIED(§9.5): whether delivery continues on a socket opened with a since-renewed Skype token.
    pub reconnect_on_token_renewal: bool,
    /// Sequenced `ping` cadence.
    pub ping_interval: Duration,
    /// Reconnect when nothing arrives for this long (checked at each ping).
    pub silence_timeout: Duration,
    /// First reconnect delay; also the delay after a registered session.
    pub backoff_min: Duration,
    /// Reconnect delay cap.
    pub backoff_max: Duration,
    /// Registration lifetime; re-registered at TTL − 10 s.
    pub register_ttl: Duration,
    /// First half of `con_num` (`{process_start_ms}_{counter}`).
    pub process_start_ms: i64,
}

impl TrouterConfig {
    /// The §4.6 defaults: ping every 30 s, 90 s silence timeout, 1 s → 60 s
    /// backoff, 86 400 s registrations; reconnect on token renewal.
    pub fn new(endpoint_id: String) -> Self {
        TrouterConfig {
            endpoint_id,
            register_all_apps: false,
            reconnect_on_token_renewal: true,
            ping_interval: Duration::from_secs(30),
            silence_timeout: Duration::from_secs(90),
            backoff_min: Duration::from_secs(1),
            backoff_max: Duration::from_secs(60),
            register_ttl: Duration::from_secs(86_400),
            process_start_ms: process_start_ms(),
        }
    }
}

/// When this process first asked: the stable half of `con_num`.
fn process_start_ms() -> i64 {
    static START: OnceLock<i64> = OnceLock::new();
    *START.get_or_init(now_ms)
}

/// What [`run`] works with.
pub(crate) struct TrouterDeps {
    pub http: Arc<Http>,
    pub factory: Arc<dyn TransportFactory>,
    /// The event stream's sender; dropping the receiver stops `run`.
    pub tx: mpsc::Sender<Event>,
    pub cancel: CancellationToken,
    pub capture: Option<Arc<Capture>>,
    pub raw_hook: Option<RawEventHook>,
    /// The current connection's address (`surl`), once registered: where
    /// other services (presence) push to.
    pub push_address: Arc<std::sync::Mutex<Option<String>>>,
}

/// Keeps a trouter connection up until `deps.cancel` fires, the event
/// receiver is dropped, or the session needs a new sign-in (then
/// [`Event::ReauthRequired`] is the last event).
///
/// Each attempt negotiates, connects and registers; [`Event::Connected`]
/// follows the first successful registration of each connection. Every
/// disconnect emits [`Event::Disconnected`] with the delay before the next
/// attempt: 1 s doubling (plus jitter) to 60 s, reset by a registered
/// session that lasted [`STABLE_SESSION`]; zero after a Skype-token renewal.
pub(crate) async fn run(cfg: TrouterConfig, deps: TrouterDeps) {
    let mut t = Trouter {
        backoff: cfg.backoff_min,
        cfg,
        deps,
        dedup: DedupRing::new(DEDUP_CAPACITY),
        overflow_pending: false,
        dropped: 0,
        typing_dropped: 0,
        counter: 0,
        registered_at: None,
        last_registered: None,
    };
    loop {
        t.counter += 1;
        let reason = match t.session().await {
            End::Reconnect(reason) => reason,
            End::Reauth => {
                warn!("trouter: sign-in required; stopping");
                t.send_last(Event::ReauthRequired).await;
                return;
            }
            End::Cancelled | End::ConsumerGone => {
                debug!("trouter stopped");
                return;
            }
        };
        let retry_in = if reason == DisconnectReason::TokenRenewal {
            Duration::ZERO
        } else {
            t.backoff
        };
        debug!(?reason, ?retry_in, "trouter disconnected");
        t.emit(Event::Disconnected { reason, retry_in });
        if !retry_in.is_zero() {
            tokio::select! {
                () = tokio::time::sleep(retry_in) => {}
                () = t.deps.cancel.cancelled() => return,
                () = t.deps.tx.closed() => return,
            }
            t.backoff = (t.backoff * 2 + jitter()).min(t.cfg.backoff_max);
        }
    }
}

/// 0–250 ms.
fn jitter() -> Duration {
    Duration::from_millis(rand::random_range(0..=MAX_JITTER_MS))
}

/// Why a session ended.
#[derive(Debug)]
enum End {
    /// Reconnect after the backoff.
    Reconnect(DisconnectReason),
    Cancelled,
    ConsumerGone,
    Reauth,
}

impl From<DisconnectReason> for End {
    fn from(r: DisconnectReason) -> End {
        End::Reconnect(r)
    }
}

/// A negotiated, connected socket, before `1::`.
struct Connection {
    tokens: TokenHandle,
    snap: TokenSnapshot,
    session: TrouterSession,
    ws: Box<dyn Transport>,
}

struct Trouter {
    cfg: TrouterConfig,
    deps: TrouterDeps,
    dedup: DedupRing,
    /// Events were dropped and `NeedsCatchUp { Overflow }` is still owed.
    overflow_pending: bool,
    /// Events dropped since the last `NeedsCatchUp { Overflow }`.
    dropped: u64,
    /// Typing events dropped on a full channel (never caught up).
    typing_dropped: u64,
    /// Connection attempts so far (`con_num`'s second half).
    counter: u32,
    /// The delay before the next reconnect.
    backoff: Duration,
    /// When the current connection first registered.
    registered_at: Option<Instant>,
    /// When the current connection last registered successfully.
    last_registered: Option<Instant>,
}

impl Trouter {
    /// One connection, from negotiation to its end.
    async fn session(&mut self) -> End {
        let setup = tokio::select! {
            r = self.setup() => r,
            () = self.deps.cancel.cancelled() => return End::Cancelled,
            () = self.deps.tx.closed() => return End::ConsumerGone,
        };
        let mut conn = match setup {
            Ok(conn) => conn,
            Err(end) => return end,
        };
        let mut registration = None;
        self.registered_at = None;
        self.last_registered = None;
        let Err(end) = self.frame_loop(&mut conn, &mut registration).await;
        if let Some(task) = registration {
            task.abort();
        }
        if self
            .registered_at
            .take()
            .is_some_and(|t| t.elapsed() >= STABLE_SESSION)
        {
            self.backoff = self.cfg.backoff_min;
        }
        conn.ws.close().await;
        end
    }

    /// §4.6 steps 1–3: tokens, `/v4/a`, the session GET, the websocket.
    async fn setup(&self) -> std::result::Result<Connection, End> {
        let tokens = self.deps.http.tokens().handle().await.clone();
        let snap = match tokens.ready().await {
            Ok(snap) => snap,
            Err(Error::ReauthRequired) => return Err(End::Reauth),
            Err(e) => {
                warn!(error = %redact_text(&e.to_string()), "trouter: tokens not ready");
                return Err(DisconnectReason::SocketError.into());
            }
        };
        let con = con_num(self.cfg.process_start_ms, self.counter);
        let session = match negotiate(&self.deps.http, &self.cfg.endpoint_id, &con).await {
            Ok(s) => s,
            Err(Error::ReauthRequired) => return Err(End::Reauth),
            Err(e) => {
                warn!(error = %redact_text(&e.to_string()), "trouter negotiation failed");
                return Err(DisconnectReason::SocketError.into());
            }
        };
        let Some(skype_token) = snap.skype_token.as_ref() else {
            return Err(DisconnectReason::SocketError.into());
        };
        let url = websocket_url(&session, &self.cfg.endpoint_id, &con);
        debug!(url = %redact_url(&url), "trouter connecting");
        match self.deps.factory.connect(&url, skype_token).await {
            Ok(ws) => Ok(Connection {
                tokens,
                snap,
                session,
                ws,
            }),
            Err(e) => {
                warn!(error = %redact_text(&e.to_string()), "trouter websocket connect failed");
                Err(DisconnectReason::SocketError.into())
            }
        }
    }

    /// §4.6 steps 4–6 and the frame table, until the connection ends.
    /// `registration` is the in-flight registrar call, if any; the caller
    /// aborts it.
    async fn frame_loop(
        &mut self,
        conn: &mut Connection,
        registration: &mut Option<JoinHandle<Result<()>>>,
    ) -> std::result::Result<Infallible, End> {
        let mut seq: u64 = 1;
        let mut last_rx = Instant::now();
        let mut registered = false;
        let mut pings = interval_at(
            Instant::now() + self.cfg.ping_interval,
            self.cfg.ping_interval,
        );
        pings.set_missed_tick_behavior(MissedTickBehavior::Delay);
        let rereg_period = self
            .cfg
            .register_ttl
            .saturating_sub(REREGISTER_MARGIN)
            .max(Duration::from_secs(1));
        let mut rereg = interval_at(Instant::now() + rereg_period, rereg_period);
        rereg.set_missed_tick_behavior(MissedTickBehavior::Delay);
        let mut tokens_rx = conn.tokens.watch();
        // Compare once at the start too: a renewal may have landed during setup.
        tokens_rx.mark_changed();
        let skype_at_connect = conn.snap.skype_token.clone();
        loop {
            tokio::select! {
                () = self.deps.cancel.cancelled() => return Err(End::Cancelled),
                () = self.deps.tx.closed() => return Err(End::ConsumerGone),
                frame = conn.ws.recv() => {
                    let text = match frame {
                        Ok(Some(text)) => text,
                        Ok(None) => {
                            debug!("trouter socket closed");
                            return Err(DisconnectReason::SocketError.into());
                        }
                        Err(e) => {
                            debug!(error = %redact_text(&e.to_string()), "trouter socket error");
                            return Err(DisconnectReason::SocketError.into());
                        }
                    };
                    last_rx = Instant::now();
                    match parse_frame(&text) {
                        Frame::Connected => {
                            self.capture(Direction::In, "trouter-connected", &text, None);
                            let access = conn.snap.access_token.as_ref().map_or("", |t| t.expose());
                            let params = Value::Object(conn.session.connectparams.clone());
                            self.send(&mut *conn.ws, "trouter-authenticate", encode_ephemeral(&user_authenticate(access, &params))).await?;
                            self.send(&mut *conn.ws, "trouter-activity", encode_sequenced(seq, &user_activity(&correlation_vector()))).await?;
                            seq += 1;
                            self.start_registration(registration, &conn.session);
                        }
                        Frame::Request { id, method, url, headers, body } => {
                            self.send(&mut *conn.ws, "trouter-ack", encode_ack(id)).await?;
                            let messaging = url.ends_with("/messaging");
                            let presence = url.ends_with("/unifiedPresenceService");
                            let decoded = (messaging || presence || self.deps.capture.is_some())
                                .then(|| decode_request_body(&headers, &body));
                            let ok = decoded.as_ref().and_then(|d| d.as_ref().ok());
                            self.capture(Direction::In, "trouter-request", &text, ok);
                            match decoded {
                                Some(decoded) if messaging => self.handle_messaging(&url, decoded),
                                Some(Ok(body)) if presence => {
                                    let list = crate::types::parse_presence(&body);
                                    if !list.is_empty() {
                                        self.emit(Event::Presence(list));
                                    }
                                }
                                _ => debug!(%method, url = %redact_url(&url), "non-messaging trouter request ignored"),
                            }
                        }
                        Frame::Event { name, .. } if name == "trouter.message_loss" => {
                            self.capture(Direction::In, "trouter-event", &text, None);
                            let recent = self
                                .last_registered
                                .is_some_and(|t| t.elapsed() < MESSAGE_LOSS_REREGISTER_GAP);
                            if registration.is_none() && !recent {
                                warn!("trouter reported message loss; re-registering");
                                self.start_registration(registration, &conn.session);
                            } else {
                                debug!("trouter reported message loss; registration recent or in flight");
                            }
                            self.emit(Event::NeedsCatchUp { reason: CatchUpReason::MessageLoss });
                        }
                        Frame::Event { name, .. } => {
                            self.capture(Direction::In, "trouter-event", &text, None);
                            debug!(name = %redact_text(&name), "trouter event ignored");
                        }
                        Frame::Disconnect => {
                            self.capture(Direction::In, "trouter-disconnect", &text, None);
                            debug!("trouter server disconnect");
                            return Err(DisconnectReason::ServerDisconnect.into());
                        }
                        Frame::Error(e) => {
                            self.capture(Direction::In, "trouter-error", &text, None);
                            warn!(error = %redact_text(&e), "trouter error frame");
                            return Err(DisconnectReason::SocketError.into());
                        }
                        Frame::Heartbeat | Frame::Ack(_) => {
                            self.capture(Direction::In, "trouter-frame", &text, None);
                        }
                        Frame::Unknown(s) => {
                            self.capture(Direction::In, "trouter-unknown", &text, None);
                            debug!(frame = %redact_text(&s), "unknown trouter frame ignored");
                        }
                    }
                }
                done = async { registration.as_mut().expect("guarded by is_some").await }, if registration.is_some() => {
                    *registration = None;
                    if matches!(done, Ok(Ok(()))) {
                        self.last_registered = Some(Instant::now());
                    }
                    match done {
                        Ok(Ok(())) if registered => debug!("trouter re-registered"),
                        Ok(Ok(())) => {
                            registered = true;
                            self.registered_at = Some(Instant::now());
                            debug!("trouter registered");
                            *self
                                .deps
                                .push_address
                                .lock()
                                .unwrap_or_else(std::sync::PoisonError::into_inner) =
                                Some(conn.session.surl.clone());
                            self.emit(Event::Connected);
                        }
                        Ok(Err(Error::ReauthRequired)) => return Err(End::Reauth),
                        Ok(Err(e)) => {
                            warn!(error = %redact_text(&e.to_string()), "trouter registration failed");
                            return Err(DisconnectReason::RegistrationFailed.into());
                        }
                        Err(e) => {
                            warn!(error = %e, "trouter registration task failed");
                            return Err(DisconnectReason::RegistrationFailed.into());
                        }
                    }
                }
                _ = pings.tick() => {
                    if last_rx.elapsed() >= self.cfg.silence_timeout {
                        warn!(silent_for = ?last_rx.elapsed(), "trouter silent; reconnecting");
                        return Err(DisconnectReason::Silence.into());
                    }
                    self.send(&mut *conn.ws, "trouter-ping", encode_sequenced(seq, &ping())).await?;
                    seq += 1;
                    self.flush_overflow();
                }
                _ = rereg.tick(), if registered => {
                    self.start_registration(registration, &conn.session);
                }
                // Room again (the permit is released at once): the owed
                // NeedsCatchUp goes out as soon as the consumer drains.
                true = async { self.deps.tx.reserve().await.is_ok() }, if self.overflow_pending => {
                    self.flush_overflow();
                }
                Ok(()) = tokens_rx.changed() => {
                    let renewed = {
                        let cur = tokens_rx.borrow();
                        cur.skype_token.is_some() && cur.skype_token != skype_at_connect
                    };
                    if renewed && self.cfg.reconnect_on_token_renewal {
                        debug!("skype token renewed; reconnecting trouter");
                        return Err(DisconnectReason::TokenRenewal.into());
                    }
                }
            }
        }
    }

    /// Spawns a registration (so neither pings nor frames wait on HTTP),
    /// replacing any still in flight.
    fn start_registration(
        &self,
        slot: &mut Option<JoinHandle<Result<()>>>,
        session: &TrouterSession,
    ) {
        if let Some(old) = slot.take() {
            old.abort();
        }
        let http = self.deps.http.clone();
        let endpoint_id = self.cfg.endpoint_id.clone();
        let surl = session.surl.clone();
        let all = self.cfg.register_all_apps;
        *slot = Some(tokio::spawn(async move {
            register(&http, &endpoint_id, &surl, all).await
        }));
    }

    /// A `/messaging` request body: EventMessage → dedup → map → raw hook →
    /// events. Anything unusable is logged at debug and dropped.
    fn handle_messaging(&mut self, url: &str, decoded: Result<Value>) {
        let body = match decoded {
            Ok(body) => body,
            Err(e) => {
                debug!(error = %e, "undecodable trouter request body dropped");
                return;
            }
        };
        let Some(em) = EventMessage::parse(&body) else {
            debug!("non-EventMessage /messaging body dropped");
            return;
        };
        if self.dedup.seen_or_insert(em.id, em.time.as_deref()) {
            debug!(id = ?em.id, "duplicate EventMessage dropped");
            return;
        }
        let events = map_event_message(&em);
        if let Some(hook) = &self.deps.raw_hook {
            let mut body = em.raw.clone();
            redact_json(&mut body);
            hook(&RawEvent {
                url: redact_url(url),
                resource_type: em.resource_type.clone(),
                time: em.time.clone(),
                id: em.id,
                body,
                mapped: events.iter().map(|e| event_name(e).to_string()).collect(),
            });
        }
        for event in events {
            self.emit(event);
        }
    }

    /// Hands `event` to the consumer without waiting; counts it as dropped
    /// when the channel is full.
    fn emit(&mut self, event: Event) {
        self.flush_overflow();
        match self.deps.tx.try_send(event) {
            Ok(()) => {}
            Err(TrySendError::Full(Event::Typing { .. })) => {
                // Momentary: dropping it loses nothing a catch-up could
                // recover (decision #55).
                self.typing_dropped += 1;
                debug!(
                    dropped = self.typing_dropped,
                    "event channel full; typing event dropped"
                );
            }
            Err(TrySendError::Full(event)) => {
                self.dropped += 1;
                self.overflow_pending = true;
                debug!(
                    event = event_name(&event),
                    dropped = self.dropped,
                    "event channel full; event dropped"
                );
            }
            // The frame loop notices through `tx.closed()`.
            Err(TrySendError::Closed(_)) => {}
        }
    }

    /// Sends the owed `NeedsCatchUp { Overflow }` if there is room now.
    fn flush_overflow(&mut self) {
        if self.overflow_pending
            && self.deps.tx.capacity() > 0
            && self
                .deps
                .tx
                .try_send(Event::NeedsCatchUp {
                    reason: CatchUpReason::Overflow,
                })
                .is_ok()
        {
            self.overflow_reported();
        }
    }

    fn overflow_reported(&mut self) {
        warn!(
            dropped = self.dropped,
            "event channel overflowed; consumer asked to catch up"
        );
        self.overflow_pending = false;
        self.dropped = 0;
    }

    /// The final event before `run` returns: waits for room (nothing else is
    /// left to keep alive), unless cancelled or the consumer is gone.
    async fn send_last(&self, event: Event) {
        tokio::select! {
            _ = self.deps.tx.send(event) => {}
            () = self.deps.cancel.cancelled() => {}
        }
    }

    /// Captures and sends one frame; a failed send ends the connection.
    async fn send(
        &self,
        ws: &mut dyn Transport,
        label: &str,
        text: String,
    ) -> std::result::Result<(), End> {
        self.capture(Direction::Out, label, &text, None);
        ws.send(text).await.map_err(|e| {
            debug!(error = %redact_text(&e.to_string()), "trouter send failed");
            End::Reconnect(DisconnectReason::SocketError)
        })
    }

    fn capture(&self, dir: Direction, label: &str, raw: &str, decoded: Option<&Value>) {
        if let Some(capture) = &self.deps.capture {
            capture.record_frame(label, dir, raw, decoded);
        }
    }
}

/// The variant name of `e`, as [`RawEvent::mapped`] lists it.
fn event_name(e: &Event) -> &'static str {
    match e {
        Event::NewMessage(_) => "NewMessage",
        Event::MessageEdited(_) => "MessageEdited",
        Event::MessageDeleted { .. } => "MessageDeleted",
        Event::ReactionsChanged { .. } => "ReactionsChanged",
        Event::ReadStateChanged { .. } => "ReadStateChanged",
        Event::ConversationUpdated(_) => "ConversationUpdated",
        Event::Typing { .. } => "Typing",
        Event::Presence(_) => "Presence",
        Event::Connected => "Connected",
        Event::NeedsCatchUp { .. } => "NeedsCatchUp",
        Event::Disconnected { .. } => "Disconnected",
        Event::ReauthRequired => "ReauthRequired",
    }
}
