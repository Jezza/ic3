//! The Teams side of the gateway: sign-in, the initial sync, the push
//! connection, catch-up, and which conversations are shown as channels.

use std::collections::{BTreeSet, HashSet, VecDeque};
use std::time::Duration;

use ic3_teams::{
    CatchUpResult, Conversation, ConversationId, ConversationKind, DeviceCodePrompt, Event, Me,
    Mri, Page, PageCursor, StoredConversation, Timestamp, UserState, is_notes, now_ms,
};

use super::{Gateway, OpFn};
use crate::backend::EventStreamBox;
use crate::irc::Message;
use crate::names::nick_base;

/// First delay before retrying a failed step.
const RETRY_FIRST: Duration = Duration::from_secs(2);
/// Longest delay between retries.
const RETRY_CAP: Duration = Duration::from_secs(120);
/// Most `thread()` fetches in flight at once.
const THREAD_CONCURRENCY: usize = 4;
/// Most names asked for in one `user_names` call.
const NAMES_BATCH: usize = 200;

/// Where the session is.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum Phase {
    /// Checking for a stored session.
    #[default]
    Starting,
    /// No usable session; a sign-in is starting or failed.
    SignInNeeded,
    /// A device code is shown; waiting for the user.
    SigningIn,
    /// Signed in; loading the chat list.
    Syncing,
    /// Everything loaded; events flow.
    Ready,
}

/// Session bookkeeping.
#[derive(Debug, Default)]
pub(crate) struct Session {
    pub(crate) phase: Phase,
    prompt: Option<DeviceCodePrompt>,
    sign_in_error: Option<String>,
    /// Bumped on every new session: results of an older one are ignored.
    generation: u64,
    me_done: bool,
    listing_done: bool,
    csa_done: bool,
    /// When this session started: catch-up delivers nothing older.
    pub(crate) started_at: Timestamp,
    catch_up_running: bool,
    catch_up_again: bool,
    /// Retries so far, per step, for the backoff.
    attempts: std::collections::HashMap<&'static str, u32>,
    /// Shown conversations whose name is not known yet.
    pub(crate) unnamed: BTreeSet<ConversationId>,
    threads_queue: VecDeque<ConversationId>,
    threads_in_flight: HashSet<ConversationId>,
    threads_done: HashSet<ConversationId>,
    pub(crate) names_wanted: BTreeSet<Mri>,
    names_tried: HashSet<Mri>,
    names_in_flight: bool,
    /// Conversations the service refused for good (removed from the chat).
    gone: HashSet<ConversationId>,
    /// Team channels hidden (`true`) or shown (`false`) from IRC this
    /// session, overriding the CSA state loaded at startup.
    pub(crate) channel_hidden: std::collections::HashMap<ConversationId, bool>,
}

impl Session {
    /// One line for the MOTD.
    pub(crate) fn status_text(&self) -> String {
        match self.phase {
            Phase::Starting => "Starting up.".into(),
            Phase::SignInNeeded | Phase::SigningIn => {
                "Not signed in to Teams: see the message from Server.".into()
            }
            Phase::Syncing => "Signed in; loading your chats.".into(),
            Phase::Ready => "Connected to Teams.".into(),
        }
    }

    /// What `Server` tells a client about sign-in, if anything.
    pub(crate) fn sign_in_text(&self) -> Option<String> {
        match (&self.phase, &self.prompt) {
            (Phase::SigningIn, Some(p)) => Some(sign_in_message(p)),
            (Phase::SignInNeeded, _) => Some(match &self.sign_in_error {
                Some(e) => format!("Sign-in to Teams failed: {e}. Send me \"login\" to try again."),
                None => "Signing in to Teams…".into(),
            }),
            _ => None,
        }
    }
}

fn sign_in_message(p: &DeviceCodePrompt) -> String {
    format!(
        "To sign in to Teams, open {} and enter the code {}",
        p.verification_url, p.user_code
    )
}

/// How a conversation is shown.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Shown {
    /// A channel, with the label its name is made from (`None` until known).
    Channel(Option<String>),
    /// A private query (1:1 chats and Notes).
    Query,
    /// Not shown (hidden, archived, too old, or an unsupported kind).
    Hidden,
}

impl Gateway {
    pub(super) fn start(&mut self) {
        self.session.phase = Phase::Starting;
        self.spawn(|b| async move {
            let r = b.needs_login().await;
            Box::new(move |gw: &mut Gateway| gw.on_needs_login(r)) as OpFn
        });
    }

    fn on_needs_login(&mut self, r: ic3_teams::Result<bool>) {
        match r {
            Ok(false) => self.begin_session(),
            Ok(true) => self.begin_login(),
            Err(e) => {
                tracing::warn!(error = %e, "checking the stored session failed");
                self.retry_later("needs_login", |gw| gw.start());
            }
        }
    }

    /// Starts a device-code sign-in (unless one is under way).
    pub(super) fn begin_login(&mut self) {
        if self.session.phase == Phase::SigningIn {
            if let Some(p) = &self.session.prompt {
                let text = sign_in_message(p);
                self.announce(&text);
            }
            return;
        }
        self.session.phase = Phase::SignInNeeded;
        self.session.sign_in_error = None;
        self.events = None;
        self.spawn_named("login", |b| async move {
            let r = b.start_login().await;
            Box::new(move |gw: &mut Gateway| gw.on_login_started(r)) as OpFn
        });
    }

    fn on_login_started(&mut self, r: ic3_teams::Result<DeviceCodePrompt>) {
        match r {
            Ok(p) => {
                self.session.phase = Phase::SigningIn;
                let text = sign_in_message(&p);
                self.session.prompt = Some(p);
                self.announce(&text);
                self.spawn_named("login", |b| async move {
                    let r = b.wait_login().await;
                    Box::new(move |gw: &mut Gateway| gw.on_login_finished(r)) as OpFn
                });
            }
            Err(e) => self.sign_in_failed(e.to_string()),
        }
    }

    fn on_login_finished(&mut self, r: ic3_teams::Result<()>) {
        self.session.prompt = None;
        match r {
            Ok(()) => {
                self.announce("Signed in to Teams. Loading your chats…");
                self.begin_session();
            }
            Err(e) => self.sign_in_failed(e.to_string()),
        }
    }

    fn sign_in_failed(&mut self, error: String) {
        self.session.phase = Phase::SignInNeeded;
        self.announce(&format!(
            "Sign-in to Teams failed: {error}. Send me \"login\" to try again."
        ));
        self.session.sign_in_error = Some(error);
    }

    /// Signs out: forgets the session and everything loaded from it.
    pub(super) fn logout(&mut self) {
        self.spawn(|b| async move {
            let r = b.logout().await;
            Box::new(move |gw: &mut Gateway| {
                if let Err(e) = r {
                    gw.announce(&format!("Signing out failed: {e}"));
                    return;
                }
                gw.reset_session();
                gw.announce("Signed out of Teams. Send me \"login\" to sign in again.");
                gw.session.phase = Phase::SignInNeeded;
            }) as OpFn
        });
    }

    /// Drops everything about the current session and parts every channel.
    fn reset_session(&mut self) {
        let parts: Vec<String> = self
            .joined
            .iter()
            .filter_map(|c| self.dir.existing_channel(c).map(str::to_owned))
            .collect();
        let mask = self.self_mask();
        for ch in parts {
            let m = Message::new("PART", [ch, "Signed out".into()]).with_source(mask.clone());
            self.broadcast(&m);
        }
        for (_, h) in self.tasks.drain() {
            h.abort();
        }
        self.joined.clear();
        self.events = None;
        self.store = ic3_teams::Store::new();
        self.forget_windows();
        self.pending_sends.clear();
        self.typing_shown.clear();
        self.typing_sent.clear();
        let generation = self.session.generation + 1;
        self.session = Session {
            generation,
            ..Session::default()
        };
    }

    /// Profile, CSA state, chat list and push connection, in parallel.
    fn begin_session(&mut self) {
        let generation = self.session.generation + 1;
        self.session = Session {
            generation,
            phase: Phase::Syncing,
            started_at: now_ms(),
            ..Session::default()
        };
        self.store = ic3_teams::Store::new();
        self.spawn_me();
        self.spawn_user_state();
        self.spawn_conversations(None);
        self.spawn_connect();
    }

    /// Wraps a result handler so it only runs for the session that asked.
    fn current<T: Send + 'static>(
        generation: u64,
        f: impl FnOnce(&mut Gateway, T) + Send + 'static,
    ) -> impl FnOnce(T) -> OpFn {
        move |r| {
            Box::new(move |gw: &mut Gateway| {
                if gw.session.generation == generation {
                    f(gw, r);
                }
            }) as OpFn
        }
    }

    fn spawn_me(&mut self) {
        let k = Self::current(self.session.generation, Gateway::on_me);
        self.spawn(|b| async move { k(b.me().await) });
    }

    fn spawn_user_state(&mut self) {
        let k = Self::current(self.session.generation, Gateway::on_user_state);
        self.spawn(|b| async move { k(b.user_state().await) });
    }

    fn spawn_conversations(&mut self, cursor: Option<PageCursor>) {
        let k = Self::current(self.session.generation, Gateway::on_conversations);
        self.spawn(|b| async move { k(b.conversations(None, cursor).await) });
    }

    fn spawn_connect(&mut self) {
        let k = Self::current(self.session.generation, Gateway::on_connected);
        self.spawn_named("connect", |b| async move { k(b.connect().await) });
    }

    fn on_me(&mut self, r: ic3_teams::Result<Me>) {
        match r {
            Ok(me) => {
                self.store.set_me(&me);
                self.session.me_done = true;
                self.set_own_nick(&me);
                self.maybe_ready();
            }
            Err(e) => self.step_failed("me", e, Gateway::spawn_me),
        }
    }

    /// Takes the nick made from our Teams name, telling clients.
    fn set_own_nick(&mut self, me: &Me) {
        let base = nick_base(&me.display_name);
        let nick = if base.is_empty() {
            self.nick.clone()
        } else {
            base
        };
        if let Some(displaced) = self.dir.force_nick(&me.mri, &nick) {
            self.announce_rename(&displaced.old, &displaced.new);
        }
        if nick != self.nick {
            let old = std::mem::replace(&mut self.nick, nick.clone());
            let m = Message::new("NICK", [nick])
                .with_source(format!("{old}!{old}@{}", crate::names::USER_HOST));
            self.broadcast(&m);
        }
    }

    fn on_user_state(&mut self, r: ic3_teams::Result<UserState>) {
        match r {
            Ok(us) => {
                self.store.apply_user_state(&us);
                self.session.attempts.remove("user_state");
                // A late answer (after a failure): show what it adds.
                if self.session.phase == Phase::Ready {
                    let before = self.joined.len();
                    self.refresh_shown();
                    let added = self.joined.len() - before;
                    if added > 0 {
                        self.announce(&format!("Team channels loaded: {added} more channels."));
                    }
                }
            }
            Err(ic3_teams::Error::ReauthRequired) => return self.reauth(),
            // Without CSA there are no team channels (and no older chats),
            // but chats still work: start without it and keep trying.
            Err(e) => {
                tracing::warn!(error = %e, "CSA user state failed; team channels unavailable until it loads");
                let generation = self.session.generation;
                self.retry_later("user_state", move |gw| {
                    if gw.session.generation == generation {
                        gw.spawn_user_state();
                    }
                });
            }
        }
        self.session.csa_done = true;
        self.maybe_ready();
    }

    fn on_conversations(&mut self, r: ic3_teams::Result<Page<Conversation>>) {
        match r {
            Ok(page) => {
                self.store.apply_conversations(page.items);
                if let Some(note) = page.note {
                    tracing::info!(%note, "conversation listing stopped early");
                }
                match page.next {
                    Some(next) => self.spawn_conversations(Some(next)),
                    None => {
                        self.session.listing_done = true;
                        self.maybe_ready();
                    }
                }
            }
            Err(e) => self.step_failed("conversations", e, |gw| gw.spawn_conversations(None)),
        }
    }

    fn on_connected(&mut self, r: ic3_teams::Result<EventStreamBox>) {
        self.tasks.remove("connect");
        match r {
            Ok(events) => {
                self.session.attempts.remove("connect");
                self.events = Some(events);
            }
            Err(e) => self.step_failed("connect", e, Gateway::spawn_connect),
        }
    }

    /// A startup step failed: sign in again, or retry with backoff.
    fn step_failed(&mut self, what: &'static str, e: ic3_teams::Error, retry: fn(&mut Gateway)) {
        if matches!(e, ic3_teams::Error::ReauthRequired) {
            self.reauth();
            return;
        }
        tracing::warn!(step = what, error = %e, "Teams request failed; retrying");
        let generation = self.session.generation;
        self.retry_later(what, move |gw| {
            if gw.session.generation == generation {
                retry(gw);
            }
        });
    }

    fn retry_later(&mut self, what: &'static str, f: impl FnOnce(&mut Gateway) + Send + 'static) {
        let n = self.session.attempts.entry(what).or_insert(0);
        let delay = RETRY_FIRST
            .saturating_mul(2u32.saturating_pow(*n))
            .min(RETRY_CAP);
        *n += 1;
        self.after(delay, f);
    }

    /// The session is no longer usable: start over with a sign-in.
    fn reauth(&mut self) {
        if matches!(self.session.phase, Phase::SignInNeeded | Phase::SigningIn) {
            return;
        }
        tracing::warn!("Teams session expired; signing in again");
        self.reset_session();
        self.begin_login();
    }

    fn maybe_ready(&mut self) {
        let s = &self.session;
        if s.phase != Phase::Syncing || !(s.me_done && s.listing_done && s.csa_done) {
            return;
        }
        self.session.phase = Phase::Ready;
        self.refresh_shown();
        // 1:1 partners' names, so they can be found by name (`Server hide`)
        // and get real nicks; asked for in batches on the tick.
        let mut unnamed = self.store.unresolved_mris();
        unnamed.extend(
            self.one_to_ones()
                .into_iter()
                .map(|(_, p)| p)
                .filter(|p| self.store.display_name(p).is_none()),
        );
        self.session.names_wanted.extend(unnamed);
        self.maybe_request_names();
        self.presence_ready();
        let n = self.joined.len();
        self.announce(&format!("Connected to Teams: {n} channels."));
    }

    // ----- Which conversations are shown -----

    /// How `conv` is shown, judged from the store and the CSA state.
    pub(crate) fn shown(&self, conv: &ConversationId) -> Shown {
        if is_notes(conv) {
            return Shown::Query;
        }
        if self.session.gone.contains(conv) {
            return Shown::Hidden;
        }
        let sc = self.store.conversation(conv);
        let kind = sc.map_or_else(
            || ConversationKind::classify(&conv.0, &serde_json::Map::new()),
            |sc| sc.conversation.kind,
        );
        let csa = self.store.user_state();
        match kind {
            ConversationKind::OneToOne => Shown::Query,
            ConversationKind::System => Shown::Hidden,
            ConversationKind::Channel => match csa.and_then(|us| us.channel(conv)) {
                Some((t, c)) if !t.archived && !c.archived && !self.channel_hidden(c) => {
                    Shown::Channel(Some(format!("{}/{}", t.name, c.name)))
                }
                _ => Shown::Hidden,
            },
            // CSA's own chat `hidden` is not what Teams' list follows (a
            // live account had a 1:1 Teams shows marked hidden there); the
            // chat service's `historyHiddenTime` is.
            ConversationKind::Group => {
                if self.parted(conv) || sc.is_some_and(is_hidden) {
                    return Shown::Hidden;
                }
                Shown::Channel(self.label_of(conv))
            }
        }
    }

    /// Whether a chat was left from IRC and nothing has been said in it
    /// since. Kept across restarts: Teams' own record of the hide
    /// (`historyHiddenTime`) is only known for the chats its listing
    /// returns, and its echo of the hide need not carry it.
    pub(crate) fn parted(&self, conv: &ConversationId) -> bool {
        self.settings
            .parted
            .get(&conv.0)
            .is_some_and(|&at| self.last_said(conv) <= at)
    }

    /// When the newest message of a chat arrived, by the chat service or
    /// CSA; property changes (a hide, a rename) do not count.
    fn last_said(&self, conv: &ConversationId) -> ic3_teams::Timestamp {
        let csa = self
            .store
            .user_state()
            .and_then(|us| us.chats.iter().find(|c| c.id == *conv))
            .and_then(|c| c.last_message_at)
            .unwrap_or(0);
        self.store.last_message_at(conv).max(csa)
    }

    /// Remembers that `conv` was left from IRC now.
    pub(crate) fn note_parted(&mut self, conv: &ConversationId) {
        self.settings.parted.insert(conv.0.clone(), now_ms());
        self.save_settings();
    }

    /// Writes the settings file, if there is one.
    pub(crate) fn save_settings(&self) {
        if let Some(path) = &self.cfg.settings_path
            && let Err(e) = self.settings.save(path)
        {
            tracing::warn!(path = %path.display(), error = %e, "saving settings failed");
        }
    }

    /// Whether a team channel is hidden: as hidden or shown from IRC this
    /// session, else as Teams had it at startup.
    pub(crate) fn channel_hidden(&self, c: &ic3_teams::Channel) -> bool {
        self.session
            .channel_hidden
            .get(&c.id)
            .copied()
            .unwrap_or_else(|| c.hidden())
    }

    /// The Teams name of a channel or group chat: `Team/Channel`, or the
    /// chat's name; `None` for anything else or an unnamed chat.
    pub(crate) fn label_of(&self, conv: &ConversationId) -> Option<String> {
        if let Some((t, c)) = self.store.user_state().and_then(|us| us.channel(conv)) {
            return Some(format!("{}/{}", t.name, c.name));
        }
        if let Some(sc) = self.store.conversation(conv) {
            return (sc.conversation.kind == ConversationKind::Group)
                .then(|| self.store.conversation_name(conv))
                .flatten();
        }
        // Older chats are only in CSA's list (the chat service lists the
        // most recent ones).
        let chat = self
            .store
            .user_state()?
            .chats
            .iter()
            .find(|c| c.id == *conv && c.kind != ic3_teams::ChatKind::OneOnOne)?;
        chat.title.clone().or_else(|| {
            let names: Vec<&str> = chat
                .members
                .iter()
                .filter(|m| self.store.me() != Some(&m.mri))
                .filter_map(|m| self.store.display_name(&m.mri))
                .take(3)
                .collect();
            (!names.is_empty()).then(|| names.join(", "))
        })
    }

    /// The 1:1 chats Teams knows of, from the chat service's listing and
    /// CSA's, with the other person: `(chat, person)`.
    pub(crate) fn one_to_ones(&self) -> Vec<(ConversationId, Mri)> {
        let mut out: Vec<(ConversationId, Mri)> = self
            .store
            .conversations()
            .filter(|sc| sc.conversation.kind == ConversationKind::OneToOne && !is_hidden(sc))
            .filter_map(|sc| {
                let conv = sc.conversation.id.clone();
                self.store.peer_of(&conv).map(|p| (conv, p))
            })
            .collect();
        if let Some(us) = self.store.user_state() {
            for chat in us
                .chats
                .iter()
                .filter(|c| c.kind == ic3_teams::ChatKind::OneOnOne)
            {
                if out.iter().any(|(c, _)| *c == chat.id) {
                    continue;
                }
                if self.store.conversation(&chat.id).is_some_and(is_hidden) {
                    continue;
                }
                if let Some(m) = chat
                    .members
                    .iter()
                    .find(|m| self.store.me() != Some(&m.mri))
                {
                    out.push((chat.id.clone(), m.mri.clone()));
                }
            }
        }
        out
    }

    /// Hands out an IRC name to every team channel and named group chat,
    /// shown or not, so `/join` and `Server` can find them.
    fn name_everything(&mut self) {
        let mut convs: Vec<ConversationId> = self
            .store
            .conversations()
            .filter(|sc| !sc.stub && sc.conversation.kind == ConversationKind::Group)
            .map(|sc| sc.conversation.id.clone())
            .collect();
        if let Some(us) = self.store.user_state() {
            convs.extend(
                us.chats
                    .iter()
                    .filter(|c| c.kind != ic3_teams::ChatKind::OneOnOne)
                    .map(|c| c.id.clone()),
            );
        }
        convs.sort();
        convs.dedup();
        if let Some(us) = self.store.user_state() {
            convs.extend(us.teams.iter().filter(|t| !t.archived).flat_map(|t| {
                t.channels
                    .iter()
                    .filter(|c| !c.archived)
                    .map(|c| c.id.clone())
            }));
        }
        // Most recent first, so a name clash numbers the older one.
        convs.sort_by_key(|c| std::cmp::Reverse(self.store.last_activity(c)));
        for conv in convs {
            if let Some(label) = self.label_of(&conv) {
                self.dir.channel(&conv, &label);
            }
        }
    }

    /// Whether a group chat is recent enough to join at startup.
    fn recent(&self, sc: &StoredConversation) -> bool {
        self.within_cutoff(&sc.conversation.id)
    }

    /// The newest activity of a chat: the chat service's, or CSA's
    /// last-message time for chats only CSA lists.
    pub(crate) fn activity(&self, conv: &ConversationId) -> ic3_teams::Timestamp {
        let csa = self
            .store
            .user_state()
            .and_then(|us| us.chats.iter().find(|c| c.id == *conv))
            .and_then(|c| c.last_message_at)
            .unwrap_or(0);
        self.store.last_activity(conv).max(csa)
    }

    /// Whether a chat was active within the cutoff.
    pub(crate) fn within_cutoff(&self, conv: &ConversationId) -> bool {
        match self.cutoff {
            None => true,
            Some(age) => {
                let since = now_ms() - i64::try_from(age.as_millis()).unwrap_or(i64::MAX);
                self.activity(conv) >= since
            }
        }
    }

    /// Changes the cutoff, keeps it, and applies it: chats now too old are
    /// left (here only: nothing changes in Teams), chats now recent enough
    /// are joined. Returns (left, joined).
    pub(crate) fn set_cutoff(&mut self, cutoff: Option<Duration>) -> (usize, usize) {
        self.cutoff = cutoff;
        self.settings.set_cutoff(cutoff);
        self.save_settings();
        let stale: Vec<ConversationId> = self
            .joined
            .iter()
            .filter(|c| {
                self.store
                    .user_state()
                    .and_then(|us| us.channel(c))
                    .is_none()
                    && !self.within_cutoff(c)
            })
            .cloned()
            .collect();
        for conv in &stale {
            self.force_part(conv, "Older than the cutoff");
        }
        let before = self.joined.len();
        self.refresh_shown();
        (stale.len(), self.joined.len() - before)
    }

    /// Joins every conversation that should be shown and is not yet.
    fn refresh_shown(&mut self) {
        self.name_everything();
        let mut candidates: Vec<ConversationId> = self
            .store
            .conversations()
            .filter(|sc| {
                !sc.stub && sc.conversation.kind == ConversationKind::Group && self.recent(sc)
            })
            .map(|sc| sc.conversation.id.clone())
            .collect();
        // Older chats only CSA lists, by its last-message time.
        if let Some(us) = self.store.user_state() {
            candidates.extend(
                us.chats
                    .iter()
                    .filter(|c| c.kind != ic3_teams::ChatKind::OneOnOne)
                    .filter(|c| self.store.conversation(&c.id).is_none())
                    .filter(|c| self.within_cutoff(&c.id))
                    .map(|c| c.id.clone()),
            );
        }
        if let Some(us) = self.store.user_state() {
            candidates.extend(us.teams.iter().filter(|t| !t.archived).flat_map(|t| {
                t.channels
                    .iter()
                    .filter(|c| !c.archived && !self.channel_hidden(c))
                    .map(|c| c.id.clone())
            }));
        }
        candidates.sort_by_key(|c| std::cmp::Reverse(self.store.last_activity(c)));
        for conv in candidates {
            self.show(&conv);
        }
    }

    /// Joins `conv` if it is shown as a channel and not joined yet; an
    /// unnamed one waits for its name. Returns whether it is joined now.
    pub(crate) fn show(&mut self, conv: &ConversationId) -> bool {
        if self.joined.contains(conv) {
            return true;
        }
        match self.shown(conv) {
            Shown::Channel(Some(label)) => {
                self.force_join(conv, &label);
                true
            }
            Shown::Channel(None) => {
                self.session.unnamed.insert(conv.clone());
                self.want_thread(conv);
                false
            }
            Shown::Query | Shown::Hidden => false,
        }
    }

    /// Joins `conv` even when its name is unknown (a message arrived in it),
    /// naming it after `fallback`. Returns whether it is joined.
    pub(crate) fn show_now(&mut self, conv: &ConversationId, fallback: &str) -> bool {
        if self.show(conv) {
            return true;
        }
        if self.session.unnamed.contains(conv) {
            self.force_join(conv, fallback);
            return true;
        }
        false
    }

    /// Joins `conv` under a name made from `label`, whatever `shown` says.
    pub(crate) fn force_join(&mut self, conv: &ConversationId, label: &str) {
        if self.joined.contains(conv) {
            return;
        }
        self.session.unnamed.remove(conv);
        if self.settings.parted.remove(&conv.0).is_some() {
            self.save_settings();
        }
        let name = self.dir.channel(conv, label);
        self.joined.insert(conv.clone());
        tracing::debug!(conversation = %conv, channel = %name, "joined");
        let ids: Vec<_> = self.clients.keys().copied().collect();
        for id in ids {
            self.join_burst(id, conv);
        }
    }

    /// Parts `conv` for every client (it was hidden or left in Teams).
    pub(crate) fn force_part(&mut self, conv: &ConversationId, reason: &str) {
        if !self.joined.remove(conv) {
            return;
        }
        let Some(name) = self.dir.existing_channel(conv).map(str::to_owned) else {
            return;
        };
        let m = Message::new("PART", [name, reason.to_string()]).with_source(self.self_mask());
        self.broadcast(&m);
    }

    // ----- Rosters and names -----

    pub(crate) fn want_thread(&mut self, conv: &ConversationId) {
        if self.session.threads_done.contains(conv)
            || self.session.threads_in_flight.contains(conv)
            || self.session.threads_queue.contains(conv)
        {
            return;
        }
        self.session.threads_queue.push_back(conv.clone());
        self.pump_threads();
    }

    fn pump_threads(&mut self) {
        while self.session.threads_in_flight.len() < THREAD_CONCURRENCY {
            let Some(conv) = self.session.threads_queue.pop_front() else {
                return;
            };
            self.session.threads_in_flight.insert(conv.clone());
            let c = conv.clone();
            let k = Self::current(self.session.generation, move |gw: &mut Gateway, r| {
                gw.on_thread(c, r)
            });
            self.spawn(|b| async move { k(b.thread(&conv).await) });
        }
    }

    fn on_thread(&mut self, conv: ConversationId, r: ic3_teams::Result<ic3_teams::ThreadInfo>) {
        self.session.threads_in_flight.remove(&conv);
        self.session.threads_done.insert(conv.clone());
        match r {
            Ok(info) => {
                let before = self.store.conversation_name(&conv);
                self.store.apply_thread_info(info);
                if self.session.unnamed.contains(&conv) {
                    self.show(&conv);
                }
                self.roster_changed(&conv);
                if self.joined.contains(&conv) && before != self.store.conversation_name(&conv) {
                    self.send_topic_change(&conv);
                }
            }
            Err(ic3_teams::Error::ReauthRequired) => self.reauth(),
            Err(e) => {
                tracing::debug!(conversation = %conv, error = %e, "thread fetch failed");
                if self.session.unnamed.contains(&conv) {
                    // Name it after its id rather than never show it.
                    let short: String = conv.0.chars().skip(3).take(8).collect();
                    self.show_now(&conv, &format!("chat-{short}"));
                }
            }
        }
        self.pump_threads();
    }

    /// Asks Teams for names not known yet (provisional nicks), in batches.
    pub(crate) fn maybe_request_names(&mut self) {
        if self.session.names_in_flight || self.session.phase != Phase::Ready {
            return;
        }
        let batch: Vec<Mri> = self
            .session
            .names_wanted
            .iter()
            .filter(|m| !self.session.names_tried.contains(*m))
            .take(NAMES_BATCH)
            .cloned()
            .collect();
        if batch.is_empty() {
            return;
        }
        for m in &batch {
            self.session.names_wanted.remove(m);
            self.session.names_tried.insert(m.clone());
        }
        self.session.names_in_flight = true;
        let k = Self::current(self.session.generation, Gateway::on_names);
        self.spawn(|b| async move { k(b.user_names(&batch).await) });
    }

    fn on_names(&mut self, r: ic3_teams::Result<std::collections::HashMap<Mri, String>>) {
        self.session.names_in_flight = false;
        match r {
            Ok(names) => {
                for (mri, name) in names {
                    self.learn_name(&mri, &name);
                }
            }
            Err(ic3_teams::Error::ReauthRequired) => self.reauth(),
            Err(e) => tracing::debug!(error = %e, "name lookup failed"),
        }
    }

    /// A display name became known: store it, and replace a provisional nick.
    pub(crate) fn learn_name(&mut self, mri: &Mri, name: &str) {
        self.store.learn_name(mri, name);
        if let Some(r) = self.dir.learn_name(mri, name) {
            self.announce_rename(&r.old, &r.new);
        }
    }

    pub(crate) fn announce_rename(&mut self, old: &str, new: &str) {
        let m = Message::new("NICK", [new.to_string()]).with_source(Gateway::user_mask(old));
        self.broadcast(&m);
    }

    pub(super) fn session_tick(&mut self) {
        self.maybe_request_names();
    }

    // ----- Events -----

    pub(super) fn handle_stream_item(&mut self, item: Option<Event>) {
        match item {
            Some(ev) => self.handle_event(ev),
            None => {
                self.events = None;
                self.forget_windows();
                if matches!(self.session.phase, Phase::Syncing | Phase::Ready) {
                    tracing::info!("push connection ended; reconnecting");
                    let generation = self.session.generation;
                    self.retry_later("connect", move |gw| {
                        if gw.session.generation == generation {
                            gw.spawn_connect();
                        }
                    });
                }
            }
        }
    }

    fn handle_event(&mut self, ev: Event) {
        match ev {
            Event::Connected => {
                self.request_catch_up();
                self.presence_reconnected();
            }
            // Events may have been lost: no window can be trusted.
            Event::NeedsCatchUp { .. } => {
                self.forget_windows();
                self.request_catch_up();
            }
            Event::Disconnected { reason, retry_in } => {
                tracing::info!(?reason, ?retry_in, "push connection dropped");
                self.forget_windows();
            }
            Event::Presence(list) => {
                tracing::debug!(target: "ic3_irc::gateway::presence", n = list.len(), "presence pushed");
                self.apply_presence(list);
            }
            Event::ReauthRequired => self.reauth(),
            Event::Typing {
                conversation,
                from,
                from_name,
            } => self.on_typing(conversation, from, from_name),
            Event::NewMessage(m) | Event::MessageEdited(m) => {
                self.store.note_event_now();
                self.incoming_message(m, true);
            }
            Event::MessageDeleted {
                conversation,
                message,
                version,
            } => {
                self.store.note_event_now();
                self.incoming_delete(conversation, message, version);
            }
            Event::ReactionsChanged {
                conversation,
                message,
                reactions,
                version,
            } => {
                self.store.note_event_now();
                self.incoming_reactions(conversation, message, reactions, version);
            }
            Event::ReadStateChanged {
                conversation,
                horizon,
            } => {
                self.store.note_event_now();
                let ev = Event::ReadStateChanged {
                    conversation: conversation.clone(),
                    horizon,
                };
                if self.store.apply_event(&ev) != ic3_teams::Change::Unchanged {
                    self.send_read_marker(&conversation, None);
                }
            }
            Event::ConversationUpdated(c) => {
                self.store.note_event_now();
                self.incoming_conversation_update(c);
            }
        }
    }

    fn incoming_conversation_update(&mut self, c: Conversation) {
        let id = c.id.clone();
        let refetch = c.version == 0 && c.topic.is_none();
        let hidden_said = c.hidden_in_payload();
        let before = self.store.conversation_name(&id);
        self.store.apply_event(&Event::ConversationUpdated(c));
        if refetch {
            self.session.threads_done.remove(&id);
            self.want_thread(&id);
        }
        let hidden_now = self
            .store
            .conversation(&id)
            .is_some_and(|sc| sc.conversation.hidden);
        if self.joined.contains(&id) && hidden_now && hidden_said {
            self.force_part(&id, "Hidden in Teams");
        } else if self.joined.contains(&id) {
            if before != self.store.conversation_name(&id) {
                self.send_topic_change(&id);
            }
        } else if self.session.phase == Phase::Ready
            && matches!(self.shown(&id), Shown::Channel(_))
            && self
                .store
                .conversation(&id)
                .is_some_and(|sc| !sc.stub && self.recent(sc))
        {
            self.show(&id);
        }
    }

    // ----- Catch-up -----

    /// Fetches what was missed while the push connection was down: the
    /// conversations changed since the last event, then the messages of
    /// each changed one that is (or may become) shown.
    fn request_catch_up(&mut self) {
        if !matches!(self.session.phase, Phase::Ready | Phase::Syncing) {
            return;
        }
        if self.session.catch_up_running {
            self.session.catch_up_again = true;
            return;
        }
        self.session.catch_up_running = true;
        let since = self.store.catch_up_plan().since;
        let skip: HashSet<ConversationId> = self
            .store
            .conversations()
            .map(|sc| sc.conversation.id.clone())
            .filter(|c| self.session.gone.contains(c) || self.shown(c) == Shown::Hidden)
            .collect();
        let k = Self::current(self.session.generation, Gateway::on_catch_up);
        self.spawn_named("catch_up", |b| async move {
            k(catch_up(b.as_ref(), since, skip).await)
        });
    }

    fn on_catch_up(&mut self, mut r: CatchUp) {
        self.tasks.remove("catch_up");
        self.session.catch_up_running = false;
        self.session.gone.extend(r.gone.iter().cloned());
        let complete = r.errors == 0;
        for c in std::mem::take(&mut r.conversations) {
            self.incoming_conversation_update(c);
        }
        let mut messages = std::mem::take(&mut r.messages);
        messages.sort_by(|a, b| {
            a.arrived_at
                .cmp(&b.arrived_at)
                .then_with(|| a.id.cmp(&b.id))
        });
        for m in messages {
            let live = m.arrived_at >= self.session.started_at;
            self.incoming_message(m, live);
        }
        if complete {
            // Advances the store's event time to when this run started.
            self.store.apply_catch_up(CatchUpResult {
                started_at: r.started_at,
                ..CatchUpResult::default()
            });
        }
        if self.session.catch_up_again || !complete {
            self.session.catch_up_again = false;
            let generation = self.session.generation;
            let delay = if complete {
                Duration::ZERO
            } else {
                RETRY_FIRST
            };
            self.after(delay, move |gw| {
                if gw.session.generation == generation {
                    gw.request_catch_up();
                }
            });
        }
    }
}

/// Most conversation pages one catch-up follows.
const CATCH_UP_MAX_PAGES: usize = 50;

/// What a catch-up fetched.
#[derive(Debug, Default)]
pub(crate) struct CatchUp {
    started_at: Timestamp,
    conversations: Vec<Conversation>,
    messages: Vec<ic3_teams::Message>,
    /// Failed fetches; any makes the run incomplete.
    errors: usize,
    /// Conversations refused for good (a 4xx: removed from the chat).
    gone: Vec<ConversationId>,
}

/// The network half of a catch-up; never fails (errors are counted).
async fn catch_up(
    b: &dyn crate::backend::Backend,
    since: Timestamp,
    skip: HashSet<ConversationId>,
) -> CatchUp {
    let mut r = CatchUp {
        started_at: now_ms(),
        ..CatchUp::default()
    };
    let mut cursor = None;
    for _ in 0..CATCH_UP_MAX_PAGES {
        match b.conversations(Some(since), cursor.take()).await {
            Ok(page) => {
                r.conversations.extend(page.items);
                cursor = page.next;
            }
            Err(e) => {
                tracing::warn!(error = %e, "catch-up: conversation listing failed");
                r.errors += 1;
                break;
            }
        }
        if cursor.is_none() {
            break;
        }
    }
    let changed: Vec<ConversationId> = r
        .conversations
        .iter()
        .filter(|c| !skip.contains(&c.id) && c.kind != ConversationKind::System || is_notes(&c.id))
        .map(|c| c.id.clone())
        .collect();
    for conv in changed {
        match b.messages_since(&conv, since).await {
            Ok(ms) => r.messages.extend(ms),
            Err(e) if is_final(&e) => r.gone.push(conv),
            Err(e) => {
                tracing::warn!(conversation = %conv, error = %e, "catch-up: messages failed");
                r.errors += 1;
            }
        }
    }
    r
}

/// Whether a conversation's fetch will keep failing (a 4xx other than 408
/// and 429: removed from the chat, or it no longer exists).
fn is_final(e: &ic3_teams::Error) -> bool {
    matches!(e, ic3_teams::Error::Http { status, .. }
        if (400..500).contains(status) && !matches!(status, 408 | 429))
}

/// Whether the chat service lists the conversation as hidden.
fn is_hidden(sc: &StoredConversation) -> bool {
    sc.conversation.hidden
}
