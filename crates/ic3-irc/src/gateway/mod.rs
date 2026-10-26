//! The gateway: one async state loop that owns the Teams side (store, event
//! stream, network operations) and every attached IRC client.
//!
//! Connections ([`crate::conn`]) do registration and capability negotiation
//! themselves and then talk to the gateway only through [`Input`]s; the
//! gateway answers by queueing lines on each client's channel. Network calls
//! run as spawned tasks whose results come back into the loop as closures (`OpFn`), so
//! the state is only ever touched from the loop.

mod commands;
mod deliver;
mod history;
mod presence;
mod session;
#[cfg(test)]
mod tests;

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::future::Future;
use std::sync::Arc;
use std::time::Duration;

use futures::StreamExt;
use ic3_teams::{ConversationId, Event, Mri, Store, Timestamp};
use tokio::sync::mpsc;
use tokio::task::{AbortHandle, JoinSet};
use tokio::time::{Instant, MissedTickBehavior};
use tokio_util::sync::CancellationToken;

use crate::backend::{Backend, EventStreamBox};
use crate::caps::Caps;
use crate::irc::{Message, format_time};
use crate::media::MediaTable;
use crate::names::{Directory, SERVICE_NICK, USER_HOST};

pub use session::Phase;

/// A connection's id.
pub type ClientId = u64;

/// Lines queued per client before it counts as too slow and is dropped.
/// Generous: a client's first attach (every channel, then history for each)
/// queues tens of thousands of lines in a few seconds.
pub const CLIENT_QUEUE: usize = 65_536;

/// How often the loop wakes for time-based work.
const TICK: Duration = Duration::from_secs(2);

/// What a connection tells the gateway.
#[derive(Debug)]
pub enum Input {
    /// Registration finished.
    Attach {
        id: ClientId,
        nick: String,
        caps: Caps,
        out: mpsc::Sender<String>,
        /// Cancelled by the gateway to close the connection.
        close: CancellationToken,
    },
    /// The client's capabilities changed after registration.
    Caps { id: ClientId, caps: Caps },
    /// A command from a registered client.
    Line { id: ClientId, msg: Message },
    /// The connection closed.
    Detach { id: ClientId },
}

/// Settings the gateway needs.
#[derive(Clone, Debug)]
pub struct GatewayConfig {
    /// The server name in prefixes and numerics.
    pub server_name: String,
    /// The nick used before the Teams name is known.
    pub default_nick: String,
    /// Group chats with no activity for this long are not joined (they are
    /// joined when a message arrives); `None` joins all. The starting
    /// value: `Server`'s `cutoff` changes it and keeps it in `settings_path`.
    pub max_chat_age: Option<Duration>,
    /// Where run-time settings are kept; `None` keeps them in memory only.
    pub settings_path: Option<std::path::PathBuf>,
    /// The `soju.im/FILEHOST` upload URL, when uploads are served.
    pub filehost: Option<String>,
    /// Messages per page fetched for history.
    pub page_size: usize,
}

impl Default for GatewayConfig {
    fn default() -> Self {
        GatewayConfig {
            server_name: "ic3-irc".into(),
            default_nick: "me".into(),
            max_chat_age: Some(Duration::from_secs(60 * 24 * 3600)),
            settings_path: None,
            filehost: None,
            page_size: 50,
        }
    }
}

/// One attached client.
#[derive(Debug)]
struct ClientState {
    out: mpsc::Sender<String>,
    close: CancellationToken,
    caps: Caps,
    /// Batch references handed out so far.
    batch_seq: u64,
    /// A multiline batch being received: reference → (target, tags, lines).
    inbound_batch: Option<InboundBatch>,
    /// Metadata keys subscribed to.
    metadata_subs: BTreeSet<String>,
}

/// A `draft/multiline` batch a client is sending.
#[derive(Debug)]
struct InboundBatch {
    reference: String,
    target: String,
    /// The opening `BATCH` line's tags (label, `+reply`, …).
    tags: Vec<(String, Option<String>)>,
    text: String,
    lines: usize,
}

/// The result of a spawned operation, handled in the loop.
pub(crate) type OpFn = Box<dyn FnOnce(&mut Gateway) + Send>;

/// The gateway's state.
pub struct Gateway {
    backend: Arc<dyn Backend>,
    cfg: GatewayConfig,
    clients: BTreeMap<ClientId, ClientState>,
    /// Our own nick, shared by every client.
    nick: String,
    dir: Directory,
    store: Store,
    media: Arc<MediaTable>,
    session: session::Session,
    events: Option<EventStreamBox>,
    ops: JoinSet<OpFn>,
    /// Abort handles of tracked tasks by name (sign-in, connect, …).
    tasks: HashMap<&'static str, AbortHandle>,
    /// Channels the clients are in, by conversation.
    joined: BTreeSet<ConversationId>,
    /// Sends waiting for their echo, by client message id.
    pending_sends: HashMap<String, deliver::PendingSend>,
    /// Typing shown to clients: (conversation, sender) → last announced.
    typing_shown: HashMap<(ConversationId, Mri), Instant>,
    /// When we last told Teams we are typing, per conversation.
    typing_sent: HashMap<ConversationId, Instant>,
    /// The last text announced in an `EDIT`, per message.
    edits_sent: HashMap<ic3_teams::MessageId, String>,
    /// Chats idle longer than this are not joined (`None`: no limit).
    pub(crate) cutoff: Option<Duration>,
    pub(crate) settings: crate::settings::Settings,
    /// `CHATHISTORY` requests waiting for a fetch.
    history_queue: std::collections::VecDeque<history::HistoryJob>,
    /// How many history fetches may run at once.
    limiter: history::Limiter,
    /// Per conversation, how far back the store is known to be complete.
    history_windows: HashMap<ConversationId, Timestamp>,
    presence: presence::PresenceState,
}

impl std::fmt::Debug for Gateway {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Gateway")
            .field("nick", &self.nick)
            .field("clients", &self.clients.len())
            .field("joined", &self.joined.len())
            .field("phase", &self.session.phase)
            .finish_non_exhaustive()
    }
}

/// Runs the gateway until `inputs` closes.
pub async fn run(
    backend: Arc<dyn Backend>,
    cfg: GatewayConfig,
    media: Arc<MediaTable>,
    mut inputs: mpsc::Receiver<Input>,
) {
    let mut gw = Gateway::new(backend, cfg, media);
    gw.start();
    let mut tick = tokio::time::interval(TICK);
    tick.set_missed_tick_behavior(MissedTickBehavior::Delay);
    loop {
        tokio::select! {
            input = inputs.recv() => match input {
                Some(input) => gw.handle_input(input),
                None => break,
            },
            Some(done) = gw.ops.join_next() => match done {
                Ok(f) => f(&mut gw),
                Err(e) if e.is_cancelled() => {}
                Err(e) => tracing::error!(error = %e, "gateway task panicked"),
            },
            item = next_event(&mut gw.events) => gw.handle_stream_item(item),
            _ = tick.tick() => gw.on_tick(),
        }
        gw.reap_dead_clients();
    }
    gw.backend.shutdown().await;
}

async fn next_event(events: &mut Option<EventStreamBox>) -> Option<Event> {
    match events {
        Some(s) => s.next().await,
        None => std::future::pending().await,
    }
}

impl Gateway {
    pub(crate) fn new(
        backend: Arc<dyn Backend>,
        cfg: GatewayConfig,
        media: Arc<MediaTable>,
    ) -> Gateway {
        let nick = cfg.default_nick.clone();
        let settings = cfg
            .settings_path
            .as_deref()
            .map(crate::settings::Settings::load)
            .unwrap_or_default();
        let cutoff = settings.cutoff().unwrap_or(cfg.max_chat_age);
        Gateway {
            backend,
            cfg,
            clients: BTreeMap::new(),
            nick,
            dir: Directory::new(),
            store: Store::new(),
            media,
            session: session::Session::default(),
            events: None,
            ops: JoinSet::new(),
            tasks: HashMap::new(),
            joined: BTreeSet::new(),
            pending_sends: HashMap::new(),
            typing_shown: HashMap::new(),
            typing_sent: HashMap::new(),
            edits_sent: HashMap::new(),
            cutoff,
            settings,
            history_queue: std::collections::VecDeque::new(),
            limiter: history::Limiter::default(),
            history_windows: HashMap::new(),
            presence: presence::PresenceState::default(),
        }
    }

    /// Spawns `fut`; its output, a closure, runs on the gateway when done.
    pub(crate) fn spawn<F, Fut>(&mut self, f: F) -> AbortHandle
    where
        F: FnOnce(Arc<dyn Backend>) -> Fut,
        Fut: Future<Output = OpFn> + Send + 'static,
    {
        let fut = f(self.backend.clone());
        self.ops.spawn(fut)
    }

    /// [`spawn`](Self::spawn), remembered under `name` so it can be aborted
    /// (a second task under the same name aborts the first).
    pub(crate) fn spawn_named<F, Fut>(&mut self, name: &'static str, f: F)
    where
        F: FnOnce(Arc<dyn Backend>) -> Fut,
        Fut: Future<Output = OpFn> + Send + 'static,
    {
        let h = self.spawn(f);
        if let Some(old) = self.tasks.insert(name, h) {
            old.abort();
        }
    }

    /// Runs `f` on the gateway after `delay`.
    pub(crate) fn after(&mut self, delay: Duration, f: impl FnOnce(&mut Gateway) + Send + 'static) {
        self.ops.spawn(async move {
            tokio::time::sleep(delay).await;
            Box::new(f) as OpFn
        });
    }

    fn handle_input(&mut self, input: Input) {
        match input {
            Input::Attach {
                id,
                nick,
                caps,
                out,
                close,
            } => self.attach(id, nick, caps, out, close),
            Input::Caps { id, caps } => {
                if let Some(c) = self.clients.get_mut(&id) {
                    c.caps = caps;
                }
            }
            Input::Line { id, msg } => {
                if self.clients.contains_key(&id) {
                    self.handle_command(id, msg);
                }
            }
            Input::Detach { id } => {
                self.clients.remove(&id);
                self.presence.monitors.remove(&id);
            }
        }
    }

    fn on_tick(&mut self) {
        self.typing_tick();
        self.session_tick();
        // A fetch held back while another stalled may go now.
        self.pump_history();
        self.pump_presence();
    }

    fn reap_dead_clients(&mut self) {
        self.clients.retain(|id, c| {
            let alive = !c.out.is_closed();
            if !alive {
                tracing::info!(client = id, "client dropped");
            }
            alive
        });
    }

    // ----- Identity -----

    /// Our `nick!user@host`.
    fn self_mask(&self) -> String {
        format!("{}!{}@{USER_HOST}", self.nick, self.nick)
    }

    /// A Teams user's `nick!user@host`.
    fn user_mask(nick: &str) -> String {
        format!("{nick}!{nick}@{USER_HOST}")
    }

    fn service_mask(&self) -> String {
        format!("{SERVICE_NICK}!{SERVICE_NICK}@{}", self.cfg.server_name)
    }

    /// The nick for `mri` (ours for the signed-in user).
    fn nick_of(&mut self, mri: &Mri, name: Option<&str>) -> String {
        if self.store.me() == Some(mri) {
            return self.nick.clone();
        }
        let known = self.store.display_name(mri).map(str::to_owned);
        self.dir.nick(mri, known.as_deref().or(name))
    }

    // ----- Output -----

    /// Queues `msg` for client `id`, adapted to its capabilities. A client
    /// whose queue is full is dropped.
    fn send_to(&mut self, id: ClientId, msg: Message) {
        let Some(c) = self.clients.get_mut(&id) else {
            return;
        };
        let Some(msg) = adapt(msg, &c.caps) else {
            return;
        };
        if let Err(e) = c.out.try_send(msg.to_string()) {
            match e {
                mpsc::error::TrySendError::Full(_) => tracing::warn!(
                    client = id,
                    queued = CLIENT_QUEUE,
                    "client is not reading fast enough; dropping it"
                ),
                mpsc::error::TrySendError::Closed(_) => {
                    tracing::info!(client = id, "client connection already closed")
                }
            }
            if let Some(c) = self.clients.remove(&id) {
                c.close.cancel();
            }
        }
    }

    /// Queues `msgs` for client `id` as the reply to a command carrying
    /// `label`: one line gets the label, several are wrapped in a
    /// `labeled-response` batch, none is an `ACK`.
    fn reply(&mut self, id: ClientId, label: Option<&str>, msgs: Vec<Message>) {
        let labelled = label.filter(|_| self.caps(id).is_some_and(Caps::labels));
        let Some(label) = labelled else {
            for m in msgs {
                self.send_to(id, m);
            }
            return;
        };
        match msgs.len() {
            0 => {
                let ack = Message::new("ACK", Vec::<String>::new())
                    .with_source(self.cfg.server_name.clone())
                    .with_tag("label", label);
                self.send_to(id, ack);
            }
            1 => {
                let m = msgs
                    .into_iter()
                    .next()
                    .unwrap_or_default()
                    .with_tag("label", label);
                self.send_to(id, m);
            }
            _ => {
                let r = self.batch_ref(id);
                let start = Message::new("BATCH", [format!("+{r}"), "labeled-response".into()])
                    .with_source(self.cfg.server_name.clone())
                    .with_tag("label", label);
                self.send_to(id, start);
                for mut m in msgs {
                    if m.tag("batch").is_none() {
                        m.set_tag("batch", Some(r.clone()));
                    }
                    self.send_to(id, m);
                }
                let end = Message::new("BATCH", [format!("-{r}")])
                    .with_source(self.cfg.server_name.clone());
                self.send_to(id, end);
            }
        }
    }

    /// Queues `msg` for every client (each adapted to its capabilities).
    fn broadcast(&mut self, msg: &Message) {
        let ids: Vec<ClientId> = self.clients.keys().copied().collect();
        for id in ids {
            self.send_to(id, msg.clone());
        }
    }

    /// A fresh batch reference for client `id`.
    fn batch_ref(&mut self, id: ClientId) -> String {
        match self.clients.get_mut(&id) {
            Some(c) => {
                c.batch_seq += 1;
                format!("b{}", c.batch_seq)
            }
            None => "b0".into(),
        }
    }

    fn caps(&self, id: ClientId) -> Option<&Caps> {
        self.clients.get(&id).map(|c| &c.caps)
    }

    /// A numeric from the server to us.
    fn numeric(&self, code: &str, params: impl IntoIterator<Item = String>) -> Message {
        let mut p = vec![self.nick.clone()];
        p.extend(params);
        Message::new(code, p).with_source(self.cfg.server_name.clone())
    }

    /// A standard-replies `FAIL`.
    fn fail(&self, command: &str, code: &str, context: &[&str], text: &str) -> Message {
        let mut p = vec![command.to_string(), code.to_string()];
        p.extend(context.iter().map(|s| s.to_string()));
        p.push(text.to_string());
        Message::new("FAIL", p).with_source(self.cfg.server_name.clone())
    }

    /// A private message from the service user to us.
    fn service_msg(&self, text: &str) -> Message {
        Message::new("PRIVMSG", [self.nick.clone(), text.to_string()])
            .with_source(self.service_mask())
            .with_tag("time", format_time(ic3_teams::now_ms()))
    }

    /// Tells every client something through the service user.
    fn announce(&mut self, text: &str) {
        tracing::info!(message = %text, "announce");
        let m = self.service_msg(text);
        self.broadcast(&m);
    }

    // ----- Registration -----

    fn attach(
        &mut self,
        id: ClientId,
        nick: String,
        caps: Caps,
        out: mpsc::Sender<String>,
        close: CancellationToken,
    ) {
        tracing::info!(client = id, %nick, "client attached");
        self.clients.insert(
            id,
            ClientState {
                out,
                close,
                caps,
                batch_seq: 0,
                inbound_batch: None,
                metadata_subs: BTreeSet::new(),
            },
        );
        // Before sign-in the client's nick is ours; afterwards the Teams name is.
        if self.store.me().is_none() && self.clients.len() == 1 {
            self.nick = nick;
        }
        self.welcome(id);
        self.burst(id);
    }

    fn welcome(&mut self, id: ClientId) {
        let srv = self.cfg.server_name.clone();
        let nick = self.nick.clone();
        let version = concat!("ic3-irc-", env!("CARGO_PKG_VERSION"));
        let mut isupport = vec![
            "CASEMAPPING=ascii".to_string(),
            "CHANTYPES=#".into(),
            "CHANMODES=,,,nt".into(),
            "PREFIX=(ov)@+".into(),
            "NETWORK=Teams".into(),
            "NICKLEN=30".into(),
            "CHANNELLEN=64".into(),
            "TOPICLEN=1024".into(),
            "UTF8ONLY".into(),
            format!("CHATHISTORY={}", crate::caps::CHATHISTORY_MAX),
            "MSGREFTYPES=timestamp,msgid".into(),
            format!("MONITOR={}", presence::MONITOR_MAX),
        ];
        if let Some(f) = &self.cfg.filehost {
            isupport.push(format!("SOJU.IM/FILEHOST={f}"));
        }
        let mut lines = vec![
            self.numeric("001", [format!("Welcome to Teams, {nick}")]),
            self.numeric("002", [format!("Your host is {srv}, running {version}")]),
            self.numeric(
                "003",
                ["This server bridges Microsoft Teams chat".to_string()],
            ),
            self.numeric(
                "004",
                [srv.clone(), version.into(), "i".into(), "nt".into()],
            ),
        ];
        let mut params = isupport;
        params.push("are supported by this server".into());
        lines.push(self.numeric("005", params));
        lines.push(self.numeric("375", [format!("- {srv} Message of the day -")]));
        lines.push(self.numeric("372", [format!("- {}", self.session.status_text())]));
        lines.push(self.numeric("376", ["End of /MOTD command.".to_string()]));
        for l in lines {
            self.send_to(id, l);
        }
    }

    /// Joins client `id` to every shown channel, or tells it about sign-in.
    fn burst(&mut self, id: ClientId) {
        if let Some(text) = self.session.sign_in_text() {
            let m = self.service_msg(&text);
            self.send_to(id, m);
        }
        let joined: Vec<ConversationId> = self.joined.iter().cloned().collect();
        for conv in joined {
            self.join_burst(id, &conv);
        }
    }
}

/// Adapts a message to a client's capabilities: drops tags it did not ask
/// for, and whole messages it cannot understand. `None` means "do not send".
fn adapt(mut msg: Message, caps: &Caps) -> Option<Message> {
    let needs = match msg.command.as_str() {
        "TAGMSG" => caps.tags(),
        "EDIT" => caps.edit() && caps.tags(),
        "REDACT" => caps.redaction(),
        "MARKREAD" => caps.read_marker(),
        "BATCH" => caps.batch(),
        "METADATA" => caps.metadata(),
        "AWAY" => caps.has("away-notify"),
        "FAIL" | "WARN" | "NOTE" => true,
        _ => true,
    };
    if !needs {
        return None;
    }
    if msg.tags.is_empty() {
        return Some(msg);
    }
    msg.tags.retain(|(k, _)| match k.as_str() {
        "time" => caps.time(),
        "batch" => caps.batch(),
        "label" => caps.labels(),
        _ => caps.tags(),
    });
    Some(msg)
}

/// Milliseconds → `timestamp=…` reference.
fn timestamp_ref(ms: Timestamp) -> String {
    format!("timestamp={}", format_time(ms))
}
