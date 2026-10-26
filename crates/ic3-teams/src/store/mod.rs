//! An optional in-memory model for chat UIs (spec §4.8).
//!
//! Everything enters through version-based upserts, so events and sync results
//! can be applied in any order: an event for a conversation the store does not
//! know yet creates a *stub* that a later sync (or `thread()` fetch) fills in.
//! Bots and bridges can ignore this module.

pub mod catch_up;

use std::collections::{BTreeSet, HashMap};
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::watch;

use crate::api::{Page, PageCursor};
use crate::csa::UserState;
use crate::events::Event;
use crate::format::{ReplyQuote, RichText};
use crate::store::catch_up::{CATCH_UP_OVERLAP_MS, CATCH_UP_PAGE_SIZE, CatchUpPlan, CatchUpResult};
use crate::types::{
    Conversation, ConversationId, ConversationKind, Me, Member, Message, MessageId, MessageRef,
    Mri, Reaction, ReactionSummary, ReadHorizon, ThreadInfo, Timestamp, is_displayable, is_notes,
    now_ms, parse_iso_ms,
};

/// How long a typing signal shows (spec D.1): Teams re-sends about every
/// 20 s while someone keeps typing and sends nothing when they stop.
pub const TYPING_EXPIRY: Duration = Duration::from_secs(25);

/// A message ends its sender's typing only when it arrived at most this
/// long before the typing was seen: history pages and catch-up must not
/// wipe live typing.
const TYPING_CLEAR_WINDOW_MS: i64 = 60_000;

/// [`TYPING_EXPIRY`] in milliseconds.
fn typing_expiry_ms() -> i64 {
    i64::try_from(TYPING_EXPIRY.as_millis()).unwrap_or(i64::MAX)
}

/// Identifies a stored message: by server id, or by `client_message_id` while
/// a local send has not been echoed back yet.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum MessageKey {
    Server(MessageId),
    Pending(String),
}

/// Send state of a message sent from this client.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SendState {
    Pending,
    Sent,
    Failed(String),
}

/// A message as held by the store (spec §6.0.2).
///
/// Every mutation replaces the `Arc` and increments `local_rev`; the server
/// `version` is only used for dedup/ordering and never bumped locally.
#[derive(Clone, Debug, PartialEq)]
pub struct StoredMessage {
    pub key: MessageKey,
    pub message: Message,
    pub local_rev: u64,
    /// Only for own messages sent from this client.
    pub send_state: Option<SendState>,
    /// `false` while an optimistic reaction awaits the server's confirmation.
    pub reactions_confirmed: bool,
}

/// What an upsert changed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Change {
    Inserted,
    Edited,
    Deleted,
    ReactionsChanged,
    Unchanged,
    ReadStateChanged,
    ConversationChanged,
    /// Someone started (or is still) typing; no message changed.
    Typing,
}

/// Unread state of a conversation (spec §4.8 "Unread").
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Unread {
    None,
    /// Unread, count unknown (no messages loaded).
    Some,
    Count(usize),
    /// Every loaded message is unread; there may be more.
    AtLeast(usize),
}

/// A conversation with its loaded messages.
#[derive(Clone, Debug)]
pub struct StoredConversation {
    pub conversation: Conversation,
    /// Created by an event before any sync described the conversation.
    pub stub: bool,
    /// `Server` keys in `MessageId` order, then `Pending` keys in insertion order.
    pub messages: Vec<Arc<StoredMessage>>,
    /// At least one page of messages has been applied.
    pub loaded: bool,
    pub has_older: bool,
    pub older_cursor: Option<PageCursor>,
    pub thread_info: Option<ThreadInfo>,
}

impl StoredConversation {
    fn stub(id: ConversationId) -> StoredConversation {
        let kind = ConversationKind::classify(&id.0, &serde_json::Map::new());
        StoredConversation {
            conversation: Conversation {
                id,
                kind,
                topic: None,
                members: None,
                last_message: None,
                read_horizon: None,
                version: 0,
                hidden: false,
                extra: serde_json::Map::new(),
            },
            stub: true,
            messages: Vec::new(),
            loaded: false,
            has_older: false,
            older_cursor: None,
            thread_info: None,
        }
    }

    /// Number of `Server`-keyed entries (they precede every `Pending` one).
    fn server_len(&self) -> usize {
        self.messages
            .partition_point(|m| matches!(m.key, MessageKey::Server(_)))
    }

    /// `Ok(index)` of the `Server(id)` entry, or `Err(insertion point)`.
    fn search_server(&self, id: &MessageId) -> Result<usize, usize> {
        let n = self.server_len();
        self.messages[..n].binary_search_by(|m| m.message.id.cmp(id))
    }

    fn newest_server_id(&self) -> Option<i64> {
        self.messages[..self.server_len()]
            .iter()
            .rev()
            .find_map(|m| m.message.id.as_i64())
    }
}

/// The in-memory model. All access goes through methods.
#[derive(Debug)]
pub struct Store {
    me: Option<Me>,
    conversations: HashMap<ConversationId, StoredConversation>,
    names: HashMap<Mri, String>,
    cmid_index: HashMap<String, (ConversationId, MessageKey)>,
    revision: watch::Sender<u64>,
    next_rev: u64,
    /// Newest time up to which events are known to have been seen (spec §5.1).
    last_event_time: Timestamp,
    /// The last CSA user state applied (spec A.5).
    user_state: Option<UserState>,
    /// Who is typing where: conversation → sender → (name, local time
    /// seen). Never part of `conversations` (spec D.1).
    typing: HashMap<ConversationId, HashMap<Mri, (Option<String>, Timestamp)>>,
}

impl Default for Store {
    fn default() -> Self {
        Store::new()
    }
}

impl Store {
    /// An empty store whose `last_event_time` starts now (spec §5.1 step 3:
    /// `sync_start`).
    pub fn new() -> Store {
        Store::with_start(now_ms())
    }

    /// An empty store whose `last_event_time` starts at `start` (ms).
    pub fn with_start(start: Timestamp) -> Store {
        let (revision, _) = watch::channel(0);
        Store {
            me: None,
            conversations: HashMap::new(),
            names: HashMap::new(),
            cmid_index: HashMap::new(),
            revision,
            next_rev: 0,
            last_event_time: start,
            user_state: None,
            typing: HashMap::new(),
        }
    }

    pub fn set_me(&mut self, me: &Me) {
        self.learn_name_quiet(&me.mri, &me.display_name);
        self.me = Some(me.clone());
        self.bump();
    }

    pub fn me(&self) -> Option<&Mri> {
        self.me.as_ref().map(|m| &m.mri)
    }

    /// The signed-in user's display name.
    pub fn me_name(&self) -> Option<&str> {
        self.me.as_ref().map(|m| m.display_name.as_str())
    }

    fn is_me(&self, mri: &Mri) -> bool {
        self.me() == Some(mri)
    }

    /// Increments on every change; see [`Store::subscribe`].
    pub fn revision(&self) -> u64 {
        *self.revision.borrow()
    }

    /// A change notification: the receiver sees the new revision after every change.
    pub fn subscribe(&self) -> watch::Receiver<u64> {
        self.revision.subscribe()
    }

    fn bump(&mut self) {
        self.next_rev += 1;
        self.revision.send_replace(self.next_rev);
    }

    fn ensure(&mut self, id: &ConversationId) -> &mut StoredConversation {
        self.conversations
            .entry(id.clone())
            .or_insert_with(|| StoredConversation::stub(id.clone()))
    }

    /// Merges a conversations listing (sync). Returns how many were applied.
    pub fn apply_conversations(&mut self, convs: Vec<Conversation>) -> usize {
        let n = convs.len();
        for c in convs {
            self.merge_conversation(c);
        }
        if n > 0 {
            self.bump();
        }
        n
    }

    /// Merges a conversation by `version`; never erases known fields with absent ones.
    fn merge_conversation(&mut self, c: Conversation) {
        if let Some(lm) = &c.last_message
            && let Some(name) = &lm.from_name
        {
            self.learn_name_quiet(&lm.from, name);
        }
        for m in c.members.iter().flatten() {
            if let Some(name) = &m.friendly_name {
                self.learn_name_quiet(&m.mri, name);
            }
        }
        let sc = self.ensure(&c.id);
        if !(sc.stub || c.version >= sc.conversation.version) {
            return;
        }
        let fills = c.version > 0 || c.topic.is_some() || c.last_message.is_some();
        let old = &mut sc.conversation;
        old.kind = c.kind;
        old.version = c.version;
        // Only a payload that says so hides or unhides.
        if c.hidden_in_payload() {
            old.hidden = c.hidden;
        }
        if !c.extra.is_empty() {
            old.extra = c.extra;
        }
        if c.topic.is_some() {
            old.topic = c.topic;
        }
        // `Some(vec![])` means "no member info", not "no members".
        if c.members.as_ref().is_some_and(|m| !m.is_empty()) {
            old.members = c.members;
        }
        if let Some(h) = c.read_horizon
            && old
                .read_horizon
                .as_ref()
                .is_none_or(|cur| h.message_id >= cur.message_id)
        {
            old.read_horizon = Some(h);
        }
        if c.last_message.is_some() {
            old.last_message = c.last_message;
        }
        if fills {
            sc.stub = false;
        }
    }

    /// Upserts a message by `version` (spec §4.7) and reports what changed.
    /// REST pages and events both enter here.
    /// An equal-version copy of a message whose reactions await confirmation
    /// settles just its reactions (Teams may not bump `version` for them).
    /// An inserted message ends its sender's typing in that conversation,
    /// unless it arrived long before the typing was seen (a history page).
    pub fn apply_message(&mut self, m: Message) -> Change {
        let (conv, from, at) = (m.conversation.clone(), m.from.clone(), m.arrived_at);
        let change = self.apply_message_inner(m);
        if change == Change::Inserted
            && let Some(senders) = self.typing.get_mut(&conv)
            && senders
                .get(&from)
                .is_some_and(|(_, seen)| at >= seen.saturating_sub(TYPING_CLEAR_WINDOW_MS))
        {
            senders.remove(&from);
        }
        change
    }

    /// The upsert behind [`Store::apply_message`].
    fn apply_message_inner(&mut self, m: Message) -> Change {
        let learned = m
            .from_name
            .as_ref()
            .is_some_and(|name| self.learn_name_quiet(&m.from, name));
        let conv_id = m.conversation.clone();

        // Echo (or refetch) of a pending local send: re-key it to the server id.
        let mut dropped_pending = false;
        if let Some(cmid) = m.client_message_id.clone()
            && let Some((c, MessageKey::Pending(_))) = self.cmid_index.get(&cmid)
            && *c == conv_id
        {
            let pending_key = MessageKey::Pending(cmid.clone());
            let sc = self.ensure(&conv_id);
            if let Some(pos) = sc.messages.iter().position(|e| e.key == pending_key) {
                let pending = sc.messages.remove(pos);
                let id = m.id.clone();
                self.cmid_index
                    .insert(cmid, (conv_id.clone(), MessageKey::Server(id.clone())));
                let sc = self.ensure(&conv_id);
                match sc.search_server(&id) {
                    Err(i) => {
                        let entry = StoredMessage {
                            key: MessageKey::Server(id),
                            message: m,
                            local_rev: pending.local_rev + 1,
                            send_state: Some(SendState::Sent),
                            reactions_confirmed: true,
                        };
                        sc.messages.insert(i, Arc::new(entry));
                        self.bump();
                        return Change::Inserted;
                    }
                    // The server copy was already stored: the pending duplicate
                    // is gone, the survivor is ours and sent; upsert it below.
                    Ok(i) => {
                        let mut survivor = StoredMessage::clone(&sc.messages[i]);
                        survivor.send_state = Some(SendState::Sent);
                        survivor.local_rev += 1;
                        sc.messages[i] = Arc::new(survivor);
                        dropped_pending = true;
                    }
                }
            }
        }

        let sc = self.ensure(&conv_id);
        let change = match sc.search_server(&m.id) {
            Err(i) => {
                let entry = StoredMessage {
                    key: MessageKey::Server(m.id.clone()),
                    message: m,
                    local_rev: 0,
                    send_state: None,
                    reactions_confirmed: true,
                };
                sc.messages.insert(i, Arc::new(entry));
                Change::Inserted
            }
            Ok(i) => {
                let old = &sc.messages[i];
                let same_version = m.version == old.message.version;
                if m.version < old.message.version || (same_version && old.reactions_confirmed) {
                    if learned || dropped_pending {
                        self.bump();
                    }
                    return Change::Unchanged;
                }
                if same_version {
                    // An optimistic reaction awaits confirmation and Teams may
                    // not bump `version` for reactions (spec §5.3 refetch): an
                    // equal-version server copy settles the reactions only;
                    // body and version stay as they are.
                    let mut sm = StoredMessage::clone(old);
                    sm.message.reactions = m.reactions;
                    sm.reactions_confirmed = true;
                    sm.local_rev += 1;
                    sc.messages[i] = Arc::new(sm);
                    self.bump();
                    return Change::ReactionsChanged;
                }
                let o = &old.message;
                let change = if m.deleted && !o.deleted {
                    Change::Deleted
                } else if m.reactions != o.reactions && m.body == o.body && m.edited == o.edited {
                    Change::ReactionsChanged
                } else {
                    Change::Edited
                };
                sc.messages[i] = Arc::new(StoredMessage {
                    key: old.key.clone(),
                    message: m,
                    local_rev: old.local_rev + 1,
                    send_state: old.send_state.clone(),
                    reactions_confirmed: true,
                });
                change
            }
        };
        self.bump();
        change
    }

    /// Applies a live event and reports what changed.
    pub fn apply_event(&mut self, ev: &Event) -> Change {
        let change = match ev {
            Event::NewMessage(m) | Event::MessageEdited(m) => return self.apply_message(m.clone()),
            // A delete is final whatever its version; taking the version
            // keeps an older server copy (a catch-up page) from undoing it.
            Event::MessageDeleted {
                conversation,
                message,
                version,
            } => self.update_message(conversation, message, |sm| {
                if sm.message.deleted {
                    sm.message.version = sm.message.version.max(*version);
                    return None;
                }
                sm.message.deleted = true;
                sm.message.body = RichText::default();
                sm.message.reply_to = None;
                sm.message.version = sm.message.version.max(*version);
                Some(Change::Deleted)
            }),
            // Applied unless the stored copy is newer (an unknown version,
            // 0, always applies); like a delete it takes the version.
            Event::ReactionsChanged {
                conversation,
                message,
                reactions,
                version,
            } => self.update_message(conversation, message, |sm| {
                if *version > 0 && *version < sm.message.version {
                    return None;
                }
                sm.message.reactions = reactions.clone();
                sm.message.version = sm.message.version.max(*version);
                sm.reactions_confirmed = true;
                Some(Change::ReactionsChanged)
            }),
            Event::ReadStateChanged {
                conversation,
                horizon,
            } => {
                if self.advance_horizon(conversation, horizon.clone()) {
                    Change::ReadStateChanged
                } else {
                    Change::Unchanged
                }
            }
            Event::ConversationUpdated(c) => {
                self.merge_conversation(c.clone());
                Change::ConversationChanged
            }
            // A side map only (spec D.1): no stub, no unread or activity
            // change; the user's own typing is ignored.
            Event::Typing {
                conversation,
                from,
                from_name,
            } => {
                if self.is_me(from) {
                    Change::Unchanged
                } else {
                    let now = now_ms();
                    // Nobody may ever read some conversations' entries.
                    self.prune_typing(now);
                    self.typing
                        .entry(conversation.clone())
                        .or_default()
                        .insert(from.clone(), (from_name.clone(), now));
                    Change::Typing
                }
            }
            Event::Presence(_)
            | Event::Connected
            | Event::NeedsCatchUp { .. }
            | Event::Disconnected { .. }
            | Event::ReauthRequired => Change::Unchanged,
        };
        if change != Change::Unchanged {
            self.bump();
        }
        change
    }

    /// Moves the read horizon forward (never backwards). Does not bump.
    fn advance_horizon(&mut self, conv: &ConversationId, horizon: ReadHorizon) -> bool {
        let conv = &mut self.ensure(conv).conversation;
        if conv
            .read_horizon
            .as_ref()
            .is_none_or(|cur| horizon.message_id > cur.message_id)
        {
            conv.read_horizon = Some(horizon);
            true
        } else {
            false
        }
    }

    /// Applies `f` to a copy of the `Server(id)` message; when it reports a
    /// change, replaces the `Arc` with `local_rev + 1`. Does not bump.
    fn update_message(
        &mut self,
        conv: &ConversationId,
        id: &MessageId,
        f: impl FnOnce(&mut StoredMessage) -> Option<Change>,
    ) -> Change {
        let Some(sc) = self.conversations.get_mut(conv) else {
            return Change::Unchanged;
        };
        let Ok(i) = sc.search_server(id) else {
            return Change::Unchanged;
        };
        let mut sm = StoredMessage::clone(&sc.messages[i]);
        let Some(change) = f(&mut sm) else {
            return Change::Unchanged;
        };
        sm.local_rev += 1;
        sc.messages[i] = Arc::new(sm);
        change
    }

    /// Applies a page of messages. `older` is true for a page fetched by
    /// scrolling back; the first page of a conversation also sets the cursor.
    /// Returns how many messages were inserted.
    pub fn apply_page(&mut self, conv: &ConversationId, page: Page<Message>, older: bool) -> usize {
        self.ensure(conv);
        let mut inserted = 0;
        for m in page.items {
            if self.apply_message(m) == Change::Inserted {
                inserted += 1;
            }
        }
        let sc = self.ensure(conv);
        if older || !sc.loaded {
            sc.has_older = page.next.is_some();
            sc.older_cursor = page.next;
        }
        sc.loaded = true;
        self.bump();
        inserted
    }

    /// Applies a `thread()` result: names, members and topic; fills a stub in.
    pub fn apply_thread_info(&mut self, info: ThreadInfo) {
        for m in &info.members {
            if let Some(name) = &m.friendly_name {
                self.learn_name_quiet(&m.mri, name);
            }
        }
        let sc = self.ensure(&info.id);
        // A fresh `thread()` result is the newest source (it follows a
        // `ConversationUpdated` refetch); without a topic, keep the known one.
        if let Some(t) = info.topic.clone() {
            sc.conversation.topic = Some(t);
        }
        if !info.members.is_empty() {
            sc.conversation.members = Some(info.members.clone());
        }
        // Spec §4.8: a `thread()` fetch fills a stub in.
        sc.stub = false;
        sc.thread_info = Some(info);
        self.bump();
    }

    /// Applies a CSA user state (spec A.5). For every chat the store already
    /// holds as a non-stub conversation, a non-empty CSA member list becomes
    /// `conversation.members` (its `friendlyName`s fill names not known
    /// yet; they never overwrite one). Nothing else changes: no conversation
    /// is created (CSA lists far more chats than chatsvc, and the chat list
    /// stays chatsvc's), stubs and `thread_info` are left alone (a `thread()`
    /// roster keeps priority). The whole state is kept for
    /// [`Store::user_state`]. Always notifies.
    pub fn apply_user_state(&mut self, us: &UserState) {
        for chat in &us.chats {
            if chat.members.is_empty() {
                continue;
            }
            let Some(sc) = self.conversations.get_mut(&chat.id) else {
                continue;
            };
            if sc.stub {
                continue;
            }
            sc.conversation.members = Some(chat.members.iter().map(Member::from).collect());
            // A CSA friendlyName fills a gap only; it never overwrites a
            // name from another source.
            for m in &chat.members {
                if let Some(name) = &m.friendly_name
                    && self.display_name(&m.mri).is_none()
                {
                    self.learn_name_quiet(&m.mri, name);
                }
            }
        }
        self.user_state = Some(us.clone());
        self.bump();
    }

    /// The last CSA user state applied, if any.
    pub fn user_state(&self) -> Option<&UserState> {
        self.user_state.as_ref()
    }

    pub fn conversation(&self, id: &ConversationId) -> Option<&StoredConversation> {
        self.conversations.get(id)
    }

    pub fn conversations(&self) -> impl Iterator<Item = &StoredConversation> {
        self.conversations.values()
    }

    /// Visible, non-stub, named conversations, newest activity first (ties by id).
    pub fn chat_list(&self) -> Vec<&StoredConversation> {
        let mut list: Vec<(Timestamp, &StoredConversation)> = self
            .conversations
            .values()
            .filter(|sc| {
                !sc.stub
                    && sc.conversation.kind.is_v1_visible(&sc.conversation.id.0)
                    && self.conversation_name(&sc.conversation.id).is_some()
            })
            .map(|sc| (Self::activity(sc), sc))
            .collect();
        list.sort_by(|(a, x), (b, y)| {
            b.cmp(a)
                .then_with(|| x.conversation.id.cmp(&y.conversation.id))
        });
        list.into_iter().map(|(_, sc)| sc).collect()
    }

    /// Newest of the listing's last message, the newest loaded message and the
    /// conversation version; 0 for an unknown conversation.
    pub fn last_activity(&self, id: &ConversationId) -> Timestamp {
        self.conversations.get(id).map_or(0, Self::activity)
    }

    /// When the newest message known in `id` arrived (its server id), not
    /// counting property changes as [`last_activity`](Self::last_activity)
    /// does; 0 when none is known.
    pub fn last_message_at(&self, id: &ConversationId) -> Timestamp {
        self.conversations.get(id).map_or(0, Self::message_activity)
    }

    fn message_activity(sc: &StoredConversation) -> Timestamp {
        let last = sc
            .conversation
            .last_message
            .as_ref()
            .and_then(|lm| lm.id.as_i64())
            .unwrap_or(0);
        last.max(sc.newest_server_id().unwrap_or(0))
    }

    fn activity(sc: &StoredConversation) -> Timestamp {
        Self::message_activity(sc).max(sc.conversation.version)
    }

    /// The conversation's messages in display order (empty if unknown).
    pub fn messages(&self, id: &ConversationId) -> &[Arc<StoredMessage>] {
        self.conversations
            .get(id)
            .map_or(&[], |sc| sc.messages.as_slice())
    }

    pub fn find(&self, id: &ConversationId, key: &MessageKey) -> Option<&Arc<StoredMessage>> {
        let sc = self.conversations.get(id)?;
        match key {
            MessageKey::Server(mid) => sc.search_server(mid).ok().map(|i| &sc.messages[i]),
            MessageKey::Pending(_) => sc.messages[sc.server_len()..]
                .iter()
                .find(|m| m.key == *key),
        }
    }

    // ----- Unread (spec §4.8) -----

    /// Unread state of a conversation (spec §4.8, decision #23).
    ///
    /// Before a page is loaded: unread iff the last message (the newer of the
    /// listing's `last_message` and any message received via events) is from
    /// someone else, displayable, and newer than the read horizon. Once a page
    /// is loaded: the number of displayable messages from others after the
    /// horizon; `AtLeast` when every loaded one is unread and older pages exist.
    pub fn unread(&self, id: &ConversationId) -> Unread {
        let Some(sc) = self.conversations.get(id) else {
            return Unread::None;
        };
        let horizon = sc.conversation.read_horizon.as_ref().map(|h| &h.message_id);
        let server = &sc.messages[..sc.server_len()];
        if sc.loaded {
            let n = server
                .iter()
                .filter(|m| {
                    let m = &m.message;
                    !m.deleted
                        && is_displayable(&m.message_type)
                        && !self.is_me(&m.from)
                        && horizon.is_none_or(|h| m.id > *h)
                })
                .count();
            let oldest = server.first().map(|m| &m.message.id);
            let all_unread = match (horizon, oldest) {
                (None, _) => true,
                (Some(h), Some(o)) => h < o,
                (Some(_), None) => false,
            };
            return match n {
                0 => Unread::None,
                n if sc.has_older && all_unread => Unread::AtLeast(n),
                n => Unread::Count(n),
            };
        }
        let listed = sc
            .conversation
            .last_message
            .as_ref()
            .map(|lm| (&lm.id, &lm.from, lm.message_type.as_str()));
        let received = server.iter().rev().find(|m| !m.message.deleted).map(|m| {
            (
                &m.message.id,
                &m.message.from,
                m.message.message_type.as_str(),
            )
        });
        let last = match (listed, received) {
            (Some(l), Some(r)) => Some(if r.0 > l.0 { r } else { l }),
            (l, r) => l.or(r),
        };
        match last {
            Some((id, from, message_type))
                if !self.is_me(from)
                    && is_displayable(message_type)
                    && horizon.is_none_or(|h| id > h) =>
            {
                Unread::Some
            }
            _ => Unread::None,
        }
    }

    /// Advances the read horizon after a local `mark_read` (never backwards).
    /// Only a stored server message qualifies: a pending send's local id
    /// (`pending:…`) would outrank every real id. Returns whether it moved.
    pub fn advance_horizon_local(&mut self, conv: &ConversationId, msg: &MessageRef) -> bool {
        if !self
            .conversations
            .get(conv)
            .is_some_and(|sc| sc.search_server(&msg.id).is_ok())
        {
            return false;
        }
        let horizon = ReadHorizon {
            message_id: msg.id.clone(),
            at: now_ms(),
            client_message_id: msg.client_message_id.clone(),
        };
        let moved = self.advance_horizon(conv, horizon);
        if moved {
            self.bump();
        }
        moved
    }

    // ----- Pending sends (spec §4.8) -----

    /// Inserts a local send as `Pending(client_message_id)` at the end of the
    /// conversation. It keeps that key until the server copy arrives.
    pub fn insert_pending(
        &mut self,
        conv: &ConversationId,
        client_message_id: String,
        body: RichText,
        reply_to: Option<ReplyQuote>,
    ) -> MessageKey {
        let message = Message {
            id: MessageId(format!("pending:{client_message_id}")),
            client_message_id: Some(client_message_id.clone()),
            conversation: conv.clone(),
            thread_root: None,
            from: self.me().cloned().unwrap_or_else(|| Mri(String::new())),
            from_name: self.me_name().map(str::to_owned),
            arrived_at: now_ms(),
            version: 0,
            edited: false,
            deleted: false,
            body,
            reply_to,
            reactions: vec![],
            attachments: vec![],
            message_type: "RichText/Html".into(),
            extra: serde_json::Map::new(),
        };
        let key = MessageKey::Pending(client_message_id.clone());
        self.ensure(conv).messages.push(Arc::new(StoredMessage {
            key: key.clone(),
            message,
            local_rev: 0,
            send_state: Some(SendState::Pending),
            reactions_confirmed: true,
        }));
        self.cmid_index
            .insert(client_message_id, (conv.clone(), key.clone()));
        self.bump();
        key
    }

    /// The `201` arrived: `Pending`/`Failed` → `Sent`, keeping the pending key.
    /// An entry already re-keyed by its echo stays `Sent`. `false` if unknown.
    pub fn mark_sent(&mut self, client_message_id: &str) -> bool {
        self.update_send_state(client_message_id, |_, state| match state {
            Some(SendState::Sent) => Some(None),
            _ => Some(Some(SendState::Sent)),
        })
    }

    /// The send failed. An entry whose echo already arrived is sent and stays so.
    pub fn mark_failed(&mut self, client_message_id: &str, reason: String) -> bool {
        self.update_send_state(client_message_id, |key, _| match key {
            MessageKey::Pending(_) => Some(Some(SendState::Failed(reason))),
            MessageKey::Server(_) => None,
        })
    }

    /// A retry: `Failed` → `Pending`. `false` for anything else.
    pub fn mark_pending(&mut self, client_message_id: &str) -> bool {
        self.update_send_state(client_message_id, |key, state| match (key, state) {
            (MessageKey::Pending(_), Some(SendState::Failed(_))) => Some(Some(SendState::Pending)),
            _ => None,
        })
    }

    /// Looks the entry up via the `client_message_id` index and asks `f` for
    /// its new send state: `None` = refuse (`false`), `Some(None)` = accept
    /// without change (`true`), `Some(Some(s))` = set `s` (`true`, bumps).
    fn update_send_state(
        &mut self,
        client_message_id: &str,
        f: impl FnOnce(&MessageKey, &Option<SendState>) -> Option<Option<SendState>>,
    ) -> bool {
        let Some((conv, key)) = self.cmid_index.get(client_message_id).cloned() else {
            return false;
        };
        let Some(sc) = self.conversations.get_mut(&conv) else {
            return false;
        };
        let Some(i) = sc.messages.iter().position(|m| m.key == key) else {
            return false;
        };
        match f(&key, &sc.messages[i].send_state) {
            None => false,
            Some(None) => true,
            Some(Some(state)) => {
                let mut sm = StoredMessage::clone(&sc.messages[i]);
                sm.send_state = Some(state);
                sm.local_rev += 1;
                sc.messages[i] = Arc::new(sm);
                self.bump();
                true
            }
        }
    }

    /// What a retry needs to resend: only for a local send not yet confirmed
    /// by its echo (`Pending` or `Failed`).
    pub fn pending_body(
        &self,
        client_message_id: &str,
    ) -> Option<(ConversationId, RichText, Option<ReplyQuote>)> {
        let (conv, key) = self.cmid_index.get(client_message_id)?;
        let sm = self.find(conv, key)?;
        match sm.send_state {
            Some(SendState::Pending | SendState::Failed(_)) => Some((
                conv.clone(),
                sm.message.body.clone(),
                sm.message.reply_to.clone(),
            )),
            _ => None,
        }
    }

    /// The current key of a message sent from this client.
    pub fn key_for_client_message_id(
        &self,
        client_message_id: &str,
    ) -> Option<(ConversationId, MessageKey)> {
        self.cmid_index.get(client_message_id).cloned()
    }

    // ----- Typing (spec D.1) -----

    /// Who is typing in `conv` at `now` (local ms): senders seen less than
    /// [`TYPING_EXPIRY`] ago, sorted by MRI. Expired entries are pruned
    /// (without notifying).
    pub fn typing(&mut self, conv: &ConversationId, now: Timestamp) -> Vec<(Mri, Option<String>)> {
        let expiry = typing_expiry_ms();
        let Some(senders) = self.typing.get_mut(conv) else {
            return Vec::new();
        };
        senders.retain(|_, (_, seen)| now.saturating_sub(*seen) < expiry);
        let mut out: Vec<(Mri, Option<String>)> = senders
            .iter()
            .map(|(m, (name, _))| (m.clone(), name.clone()))
            .collect();
        if senders.is_empty() {
            self.typing.remove(conv);
        }
        out.sort();
        out
    }

    /// Drops expired typing entries in every conversation (no notification).
    fn prune_typing(&mut self, now: Timestamp) {
        let expiry = typing_expiry_ms();
        self.typing.retain(|_, senders| {
            senders.retain(|_, (_, seen)| now.saturating_sub(*seen) < expiry);
            !senders.is_empty()
        });
    }

    // ----- Reactions (spec §5.3) -----

    /// Adds or removes `me`'s `reaction` before the server confirms it and
    /// marks the reactions unconfirmed. Returns the previous reactions for
    /// [`Store::revert_reactions`]; `None` if the message (or `me`) is unknown
    /// or the toggle would change nothing.
    pub fn set_reaction_optimistic(
        &mut self,
        conv: &ConversationId,
        msg: &MessageId,
        reaction: &Reaction,
        add: bool,
    ) -> Option<Vec<ReactionSummary>> {
        let me = self.me()?.clone();
        let mut prev = None;
        self.update_message(conv, msg, |sm| {
            let before = sm.message.reactions.clone();
            let reactions = &mut sm.message.reactions;
            let pos = reactions.iter().position(|r| r.reaction == *reaction);
            let mine = pos.is_some_and(|i| reactions[i].users.contains(&me));
            match (add, pos) {
                // Already in the requested state: no request, nothing to confirm.
                _ if add == mine => return None,
                (true, Some(i)) => reactions[i].users.push(me),
                (true, None) => reactions.push(ReactionSummary {
                    reaction: reaction.clone(),
                    users: vec![me],
                }),
                (false, Some(i)) => {
                    reactions[i].users.retain(|u| *u != me);
                    if reactions[i].users.is_empty() {
                        reactions.remove(i);
                    }
                }
                (false, None) => return None,
            }
            prev = Some(before);
            sm.reactions_confirmed = false;
            Some(Change::ReactionsChanged)
        });
        if prev.is_some() {
            self.bump();
        }
        prev
    }

    /// Undoes an optimistic reaction change after the request failed. A no-op
    /// once the server has confirmed the reactions (its state wins).
    pub fn revert_reactions(
        &mut self,
        conv: &ConversationId,
        msg: &MessageId,
        previous: Vec<ReactionSummary>,
    ) {
        let change = self.update_message(conv, msg, |sm| {
            if sm.reactions_confirmed {
                return None;
            }
            sm.message.reactions = previous;
            sm.reactions_confirmed = true;
            Some(Change::ReactionsChanged)
        });
        if change != Change::Unchanged {
            self.bump();
        }
    }

    // ----- Local edits (spec C) -----

    /// Shows a successful [`Api::edit`](crate::Api::edit) before the
    /// server's edit event (spec C.1): replaces the body and marks the
    /// message edited, **keeping `version`**, so that event (whose version
    /// is the server's edit time) always replaces it, whatever the clock
    /// skew. A late server copy older than the edit but with a higher
    /// version than the original can briefly show the previous body until
    /// the event arrives. Apply only after the request succeeded.
    /// `Unchanged` (no notification) for an unknown or deleted message.
    pub fn apply_local_edit(
        &mut self,
        conv: &ConversationId,
        id: &MessageId,
        body: RichText,
    ) -> Change {
        let change = self.update_message(conv, id, |sm| {
            if sm.message.deleted {
                return None;
            }
            sm.message.body = body;
            sm.message.edited = true;
            Some(Change::Edited)
        });
        if change != Change::Unchanged {
            self.bump();
        }
        change
    }

    /// Shows a successful [`Api::delete`](crate::Api::delete) before the
    /// server's delete event (spec C.2): marks the message deleted and
    /// clears its body and reply quote, keeping `version` (the same
    /// late-copy consequence as [`Store::apply_local_edit`]). `Unchanged`
    /// for an unknown or already deleted message.
    pub fn apply_local_delete(&mut self, conv: &ConversationId, id: &MessageId) -> Change {
        let change = self.update_message(conv, id, |sm| {
            if sm.message.deleted {
                return None;
            }
            sm.message.deleted = true;
            sm.message.body = RichText::default();
            sm.message.reply_to = None;
            Some(Change::Deleted)
        });
        if change != Change::Unchanged {
            self.bump();
        }
        change
    }

    // ----- Names (spec §5.2) -----

    /// The other participant of a 1:1 chat: from a `19:{a}_{b}@unq.gbl.spaces`
    /// id, else `last_message.from` when it is not `me`, else the first
    /// member that is not `me`. `None` for other kinds.
    pub fn peer_of(&self, id: &ConversationId) -> Option<Mri> {
        let sc = self.conversations.get(id);
        let kind = sc.map_or_else(
            || ConversationKind::classify(&id.0, &serde_json::Map::new()),
            |sc| sc.conversation.kind,
        );
        if kind != ConversationKind::OneToOne {
            return None;
        }
        if let Some(me_guid) = self.me().and_then(Mri::orgid_guid)
            && let Some((a, b)) =
                id.0.strip_prefix("19:")
                    .and_then(|r| r.strip_suffix("@unq.gbl.spaces"))
                    .and_then(|r| r.split_once('_'))
        {
            if a == me_guid {
                return Some(Mri(format!("8:orgid:{b}")));
            }
            if b == me_guid {
                return Some(Mri(format!("8:orgid:{a}")));
            }
        }
        let sc = sc?;
        if let Some(lm) = &sc.conversation.last_message
            && !lm.from.0.is_empty()
            && !self.is_me(&lm.from)
        {
            return Some(lm.from.clone());
        }
        Self::members_of(sc)
            .iter()
            .find(|m| !self.is_me(&m.mri))
            .map(|m| m.mri.clone())
    }

    /// The roster: from `thread()` when fetched, else from the listing.
    fn members_of(sc: &StoredConversation) -> &[Member] {
        match &sc.thread_info {
            Some(t) if !t.members.is_empty() => &t.members,
            _ => sc.conversation.members.as_deref().unwrap_or(&[]),
        }
    }

    /// A member's name: the cache, else their `friendlyName`.
    fn member_name<'a>(&'a self, m: &'a Member) -> Option<&'a str> {
        self.display_name(&m.mri).or_else(|| {
            m.friendly_name
                .as_deref()
                .map(str::trim)
                .filter(|n| !n.is_empty())
        })
    }

    /// The chat's display name (spec §5.2): "Notes", the topic, the 1:1 peer's
    /// name, or the first three other members' names; `None` until known.
    pub fn conversation_name(&self, id: &ConversationId) -> Option<String> {
        if is_notes(id) {
            return Some("Notes".into());
        }
        let sc = self.conversations.get(id)?;
        if let Some(topic) = sc.conversation.topic.as_deref().map(str::trim)
            && !topic.is_empty()
        {
            return Some(topic.to_owned());
        }
        match sc.conversation.kind {
            ConversationKind::OneToOne => self
                .peer_of(id)
                .and_then(|p| self.display_name(&p))
                .map(str::to_owned),
            ConversationKind::Group => {
                let names: Vec<&str> = Self::members_of(sc)
                    .iter()
                    .filter(|m| !self.is_me(&m.mri))
                    .filter_map(|m| self.member_name(m))
                    .take(3)
                    .collect();
                (!names.is_empty()).then(|| names.join(", "))
            }
            ConversationKind::Channel | ConversationKind::System => None,
        }
    }

    /// A visible chat that has no name yet: the engine should call `thread()`.
    pub fn needs_thread_fetch(&self, id: &ConversationId) -> bool {
        self.conversations
            .get(id)
            .is_some_and(|sc| sc.conversation.kind.is_v1_visible(&id.0))
            && self.conversation_name(id).is_none()
    }

    /// MRIs whose names feed a visible chat's name but are not cached: 1:1
    /// peers and the members of untitled groups (the `user_names` fallback).
    /// Sorted, deduplicated, never `me`.
    pub fn unresolved_mris(&self) -> Vec<Mri> {
        let mut out = BTreeSet::new();
        for sc in self.conversations.values() {
            let c = &sc.conversation;
            if !c.kind.is_v1_visible(&c.id.0) || is_notes(&c.id) {
                continue;
            }
            match c.kind {
                ConversationKind::OneToOne => {
                    if let Some(peer) = self.peer_of(&c.id)
                        && self.display_name(&peer).is_none()
                    {
                        out.insert(peer);
                    }
                }
                _ if c.topic.as_deref().is_some_and(|t| !t.trim().is_empty()) => {}
                _ => out.extend(
                    Self::members_of(sc)
                        .iter()
                        .filter(|m| !self.is_me(&m.mri) && self.member_name(m).is_none())
                        .map(|m| m.mri.clone()),
                ),
            }
        }
        out.into_iter().collect()
    }

    pub fn display_name(&self, mri: &Mri) -> Option<&str> {
        self.names.get(mri).map(String::as_str)
    }

    /// Records a display name (e.g. from the `user_names` fallback).
    pub fn learn_name(&mut self, mri: &Mri, name: &str) {
        if self.learn_name_quiet(mri, name) {
            self.bump();
        }
    }

    /// Newest time (ms) up to which events are known to have been received;
    /// catch-up fetches from here minus an overlap. Never moves backwards.
    pub fn last_event_time(&self) -> Timestamp {
        self.last_event_time
    }

    /// Advances `last_event_time` to an EventMessage's ISO-8601 `time`; a
    /// missing or unparseable time keeps the previous value (spec §5.1).
    pub fn note_event_time(&mut self, iso: Option<&str>) {
        if let Some(t) = iso.and_then(parse_iso_ms) {
            self.last_event_time = self.last_event_time.max(t);
        }
    }

    /// Advances `last_event_time` to the current time, for consumers that see
    /// only [`Event`]s (which carry no EventMessage time): receive time.
    pub fn note_event_now(&mut self) {
        self.last_event_time = self.last_event_time.max(now_ms());
    }

    /// What [`catch_up`](crate::catch_up()) should fetch (spec §5.4): changes
    /// since `last_event_time` minus the 60 s overlap (never below 0), for
    /// every conversation with loaded messages, sorted by id.
    pub fn catch_up_plan(&self) -> CatchUpPlan {
        let mut loaded_conversations: Vec<ConversationId> = self
            .conversations
            .values()
            .filter(|sc| sc.loaded)
            .map(|sc| sc.conversation.id.clone())
            .collect();
        loaded_conversations.sort();
        CatchUpPlan {
            since: self
                .last_event_time
                .saturating_sub(CATCH_UP_OVERLAP_MS)
                .max(0),
            loaded_conversations,
            page_size: CATCH_UP_PAGE_SIZE,
        }
    }

    /// Applies a [`catch_up`](crate::catch_up()) result: conversations, then
    /// every message through [`Store::apply_message`]; advances
    /// `last_event_time` to `started_at` only if the result is
    /// [complete](CatchUpResult::complete) (otherwise the gap stays to be
    /// caught up again), logs the collected errors and notes and fires the
    /// "resynced" notification (a revision bump, even if nothing changed).
    /// Returns how many messages were inserted.
    pub fn apply_catch_up(&mut self, r: CatchUpResult) -> usize {
        for e in &r.errors {
            tracing::warn!(error = %e, "catch-up fetch failed");
        }
        for n in &r.notes {
            tracing::info!(note = %n, "catch-up paging stopped early");
        }
        let complete = r.complete();
        self.apply_conversations(r.conversations);
        let mut inserted = 0;
        for m in r.messages {
            if self.apply_message(m) == Change::Inserted {
                inserted += 1;
            }
        }
        if complete {
            self.last_event_time = self.last_event_time.max(r.started_at);
        }
        self.bump();
        inserted
    }

    /// Records a non-blank name that is not just the MRI (messages sent
    /// before ic3-teams resolved its own name carry `orgid:{guid}`); returns
    /// whether the cache changed. Does not bump.
    fn learn_name_quiet(&mut self, mri: &Mri, name: &str) -> bool {
        let name = name.trim();
        if name.is_empty()
            || mri.is_bare_name(name)
            || self.names.get(mri).is_some_and(|n| n == name)
        {
            return false;
        }
        self.names.insert(mri.clone(), name.to_owned());
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::paging::CursorInner;
    use crate::csa::{ChatKind, ChatMember, ChatSummary, UserState};
    use crate::types::{
        Member, MessageRef, MessageSummary, Reaction, ReactionSummary, ReadHorizon,
    };
    use crate::{Event, Page, PageCursor, RichText};

    #[test]
    fn a_bare_mri_name_never_replaces_a_known_name() {
        let mut s = Store::new();
        let ada = Mri("8:orgid:a".into());
        s.set_me(&Me {
            mri: ada.clone(),
            display_name: "Ada".into(),
            upn: None,
        });
        for name in ["orgid:a", "8:orgid:a"] {
            let mut m = msg("19:g@thread.v2", "100", "8:orgid:a", 100, "x");
            m.from_name = Some(name.into());
            s.apply_message(m);
            assert_eq!(s.display_name(&ada), Some("Ada"), "{name}");
        }
        let b = Mri("8:orgid:b".into());
        let mut m = msg("19:g@thread.v2", "101", "8:orgid:b", 101, "y");
        m.from_name = Some("orgid:b".into());
        s.apply_message(m);
        assert_eq!(s.display_name(&b), None, "not learnt as a name either");
    }

    fn msg(conv: &str, id: &str, from: &str, version: i64, text: &str) -> Message {
        Message {
            id: MessageId(id.into()),
            client_message_id: None,
            conversation: ConversationId(conv.into()),
            thread_root: None,
            from: Mri(from.into()),
            from_name: None,
            arrived_at: id.parse().unwrap_or(0),
            version,
            edited: false,
            deleted: false,
            body: RichText::text(text),
            reply_to: None,
            reactions: vec![],
            attachments: vec![],
            message_type: "RichText/Html".into(),
            extra: Default::default(),
        }
    }

    fn conv(id: &str, kind: ConversationKind) -> Conversation {
        Conversation {
            id: ConversationId(id.into()),
            kind,
            topic: None,
            members: None,
            last_message: None,
            read_horizon: None,
            version: 0,
            hidden: false,
            extra: Default::default(),
        }
    }

    /// Channel replies, from a page or an event, land in the channel's
    /// conversation with their thread root; no per-thread conversation
    /// appears, and a quote of the post finds it.
    #[test]
    fn channel_replies_land_in_the_channel() {
        let chan = ConversationId("19:team-general@thread.tacv2".into());
        let mut s = store_with_me();
        s.apply_conversations(vec![Conversation {
            version: 10,
            ..conv(&chan.0, ConversationKind::Channel)
        }]);
        let page: serde_json::Value =
            serde_json::from_str(fixture!("9.20-channel-threads/channel-messages.json")).unwrap();
        for m in page["messages"].as_array().unwrap() {
            let m = crate::api::wire::parse_message(m, Some(&chan)).unwrap();
            assert_eq!(s.apply_message(m), Change::Inserted);
        }
        let ev: serde_json::Value =
            serde_json::from_str(fixture!("9.20-channel-threads/channel-reply-event.json"))
                .unwrap();
        let mut events =
            crate::events::map_event_message(&crate::events::EventMessage::parse(&ev).unwrap());
        let Some(Event::NewMessage(mut reply)) = events.pop() else {
            panic!("not a NewMessage")
        };
        // a reply that also quotes the post
        reply.reply_to = Some(ReplyQuote {
            message_id: MessageId("1760346000000".into()),
            ..quote()
        });
        s.apply_event(&Event::NewMessage(reply));
        assert_eq!(
            s.conversations()
                .map(|sc| &sc.conversation.id)
                .collect::<Vec<_>>(),
            [&chan],
            "no per-thread conversation"
        );
        let got: Vec<(&str, Option<&str>)> = s
            .messages(&chan)
            .iter()
            .map(|sm| {
                (
                    sm.message.id.0.as_str(),
                    sm.message.thread_root.as_ref().map(|r| r.0.as_str()),
                )
            })
            .collect();
        assert_eq!(
            got,
            [
                ("1760346000000", None),
                ("1760346030000", Some("1760346000000")),
                ("1760346060000", None),
                ("1760346120000", Some("1760346000000")),
                ("1760346180000", Some("1760346000000")),
            ]
        );
        let last = s.messages(&chan).last().unwrap();
        let quoted = last.message.reply_to.as_ref().unwrap();
        assert!(
            s.find(&chan, &MessageKey::Server(quoted.message_id.clone()))
                .is_some_and(|sm| sm.message.body.plain_text() == "the post")
        );
    }

    /// Only a payload that carries `properties.hidden` hides or unhides.
    #[test]
    fn hidden_follows_payloads_that_say_so() {
        let id = ConversationId("19:g@thread.v2".into());
        let with = |version: i64, props: serde_json::Value| {
            let mut c = Conversation {
                version,
                ..conv(&id.0, ConversationKind::Group)
            };
            c.hidden = props.get("hidden").is_some_and(|h| h == "true");
            c.extra.insert("properties".into(), props);
            c
        };
        let mut s = store_with_me();
        s.apply_conversations(vec![with(10, serde_json::json!({"hidden": "true"}))]);
        assert!(s.conversation(&id).unwrap().conversation.hidden);
        s.apply_conversations(vec![with(11, serde_json::json!({}))]);
        assert!(
            s.conversation(&id).unwrap().conversation.hidden,
            "an update that does not say keeps it hidden"
        );
        s.apply_event(&Event::ConversationUpdated(with(
            12,
            serde_json::json!({"hidden": "false"}),
        )));
        assert!(!s.conversation(&id).unwrap().conversation.hidden);
    }

    fn quote() -> ReplyQuote {
        ReplyQuote {
            message_id: MessageId("50".into()),
            author: None,
            author_name: Some("Bea".into()),
            excerpt: RichText::text("earlier"),
        }
    }

    #[test]
    fn a_local_edit_keeps_the_version_so_the_server_edit_replaces_it() {
        let mut s = Store::new();
        let c = ConversationId("c".into());
        let id = MessageId("100".into());
        let mut m = msg("c", "100", "8:orgid:a", 100, "one");
        m.reply_to = Some(quote());
        s.apply_message(m);
        let rev = s.revision();
        assert_eq!(
            s.apply_local_edit(&c, &id, RichText::text("two")),
            Change::Edited
        );
        let sm = &s.messages(&c)[0];
        assert_eq!(sm.message.body.plain_text(), "two");
        assert!(sm.message.edited);
        assert_eq!(sm.message.version, 100, "version kept");
        assert_eq!(sm.message.reply_to, Some(quote()), "the quote stays");
        assert_eq!(sm.local_rev, 1);
        assert!(s.revision() > rev);
        // The server's edit event (version = its edit time) replaces it.
        let mut server = msg("c", "100", "8:orgid:a", 200, "two (server)");
        server.edited = true;
        assert_eq!(s.apply_event(&Event::MessageEdited(server)), Change::Edited);
        assert_eq!(s.messages(&c)[0].message.body.plain_text(), "two (server)");
        // Unknown messages or conversations: nothing changes.
        let rev = s.revision();
        assert_eq!(
            s.apply_local_edit(&c, &MessageId("404".into()), RichText::text("x")),
            Change::Unchanged
        );
        let nope = ConversationId("nope".into());
        assert_eq!(
            s.apply_local_edit(&nope, &id, RichText::text("x")),
            Change::Unchanged
        );
        assert_eq!(s.revision(), rev);
        assert!(
            s.conversation(&nope).is_none(),
            "never creates a conversation"
        );
    }

    #[test]
    fn a_local_edit_of_a_deleted_message_changes_nothing() {
        let mut s = Store::new();
        let c = ConversationId("c".into());
        let id = MessageId("100".into());
        s.apply_message(msg("c", "100", "8:orgid:a", 100, "one"));
        s.apply_local_delete(&c, &id);
        let rev = s.revision();
        assert_eq!(
            s.apply_local_edit(&c, &id, RichText::text("two")),
            Change::Unchanged
        );
        let sm = &s.messages(&c)[0];
        assert!(sm.message.deleted && sm.message.body.is_empty() && !sm.message.edited);
        assert_eq!(s.revision(), rev);
    }

    #[test]
    fn a_late_older_copy_can_briefly_undo_a_local_edit_until_the_edit_event() {
        // Spec C.1's documented consequence: the local edit keeps v100, so a
        // copy from before the edit with v150 (a reaction bump, say) wins.
        let mut s = Store::new();
        let c = ConversationId("c".into());
        s.apply_message(msg("c", "100", "8:orgid:a", 100, "one"));
        s.apply_local_edit(&c, &MessageId("100".into()), RichText::text("two"));
        assert_eq!(
            s.apply_message(msg("c", "100", "8:orgid:a", 150, "one")),
            Change::Edited
        );
        assert_eq!(
            s.messages(&c)[0].message.body.plain_text(),
            "one",
            "the previous body shows again"
        );
        let mut edit = msg("c", "100", "8:orgid:a", 200, "two");
        edit.edited = true;
        s.apply_event(&Event::MessageEdited(edit));
        assert_eq!(s.messages(&c)[0].message.body.plain_text(), "two");
    }

    #[test]
    fn a_local_delete_clears_body_and_quote_and_keeps_the_version() {
        let mut s = Store::new();
        let c = ConversationId("c".into());
        let id = MessageId("100".into());
        let mut m = msg("c", "100", "8:orgid:a", 100, "one");
        m.reply_to = Some(quote());
        s.apply_message(m);
        assert_eq!(s.apply_local_delete(&c, &id), Change::Deleted);
        let sm = &s.messages(&c)[0];
        assert!(sm.message.deleted && sm.message.body.is_empty() && sm.message.reply_to.is_none());
        assert_eq!(sm.message.version, 100, "version kept");
        let rev = s.revision();
        assert_eq!(
            s.apply_local_delete(&c, &id),
            Change::Unchanged,
            "already deleted"
        );
        assert_eq!(
            s.apply_local_delete(&ConversationId("nope".into()), &id),
            Change::Unchanged
        );
        assert_eq!(s.revision(), rev);
        // The same late-copy consequence: v150 from before the delete
        // undeletes it until the delete event arrives.
        assert_eq!(
            s.apply_message(msg("c", "100", "8:orgid:a", 150, "one")),
            Change::Edited
        );
        assert!(!s.messages(&c)[0].message.deleted);
        s.apply_event(&Event::MessageDeleted {
            conversation: c.clone(),
            message: id.clone(),
            version: 300,
        });
        assert!(s.messages(&c)[0].message.deleted);
    }

    #[test]
    fn a_delete_event_clears_the_reply_quote() {
        let mut s = Store::new();
        let c = ConversationId("c".into());
        let mut m = msg("c", "5", "8:orgid:b", 5, "x");
        m.reply_to = Some(quote());
        s.apply_message(m);
        s.apply_event(&Event::MessageDeleted {
            conversation: c.clone(),
            message: MessageId("5".into()),
            version: 6,
        });
        let sm = &s.messages(&c)[0];
        assert!(sm.message.deleted && sm.message.body.is_empty());
        assert!(sm.message.reply_to.is_none());
    }

    #[test]
    fn apply_message_inserts_sorted_by_numeric_id() {
        let mut s = Store::new();
        assert_eq!(
            s.apply_message(msg("c", "200", "8:orgid:b", 200, "b")),
            Change::Inserted
        );
        assert_eq!(
            s.apply_message(msg("c", "1000", "8:orgid:b", 1000, "c")),
            Change::Inserted
        );
        assert_eq!(
            s.apply_message(msg("c", "30", "8:orgid:b", 30, "a")),
            Change::Inserted
        );
        let ids: Vec<&str> = s
            .messages(&ConversationId("c".into()))
            .iter()
            .map(|m| m.message.id.0.as_str())
            .collect();
        assert_eq!(ids, vec!["30", "200", "1000"]);
        assert!(
            s.conversation(&ConversationId("c".into())).unwrap().stub,
            "event for an unknown conversation creates a stub"
        );
        assert!(s.chat_list().is_empty(), "stubs stay out of the chat list");
    }

    #[test]
    fn version_rule_dedup_edit_delete_reactions() {
        let mut s = Store::new();
        let c = ConversationId("c".into());
        s.apply_message(msg("c", "1", "8:orgid:b", 1, "hi"));
        let rev0 = s.messages(&c)[0].local_rev;
        assert_eq!(
            s.apply_message(msg("c", "1", "8:orgid:b", 1, "hi")),
            Change::Unchanged,
            "same version is a duplicate"
        );
        assert_eq!(
            s.messages(&c)[0].local_rev,
            rev0,
            "duplicates do not bump local_rev"
        );
        assert_eq!(
            s.apply_message(msg("c", "1", "8:orgid:b", 0, "older")),
            Change::Unchanged,
            "older version ignored"
        );
        let mut e = msg("c", "1", "8:orgid:b", 2, "hi!");
        e.edited = true;
        assert_eq!(s.apply_message(e), Change::Edited);
        assert!(s.messages(&c)[0].message.edited);
        assert!(s.messages(&c)[0].local_rev > rev0);
        let mut r = msg("c", "1", "8:orgid:b", 3, "hi!");
        r.edited = true;
        r.reactions = vec![ReactionSummary {
            reaction: Reaction::Like,
            users: vec![Mri("8:orgid:a".into())],
        }];
        assert_eq!(s.apply_message(r), Change::ReactionsChanged);
        assert!(s.messages(&c)[0].reactions_confirmed);
        let mut d = msg("c", "1", "8:orgid:b", 4, "");
        d.deleted = true;
        assert_eq!(s.apply_message(d), Change::Deleted);
        assert!(s.messages(&c)[0].message.deleted);
    }

    #[test]
    fn an_older_server_copy_never_undoes_a_delete_or_reaction_event() {
        // Events newer than a catch-up page's copy (v200) arrive first.
        let mut s = Store::new();
        let c = ConversationId("c".into());
        s.apply_message(msg("c", "1", "8:orgid:b", 100, "one"));
        s.apply_message(msg("c", "2", "8:orgid:b", 100, "two"));
        s.apply_event(&Event::MessageDeleted {
            conversation: c.clone(),
            message: MessageId("1".into()),
            version: 300,
        });
        let like = vec![ReactionSummary {
            reaction: Reaction::Like,
            users: vec![Mri("8:orgid:b".into())],
        }];
        s.apply_event(&Event::ReactionsChanged {
            conversation: c.clone(),
            message: MessageId("2".into()),
            reactions: like.clone(),
            version: 300,
        });
        s.apply_message(msg("c", "1", "8:orgid:b", 200, "one"));
        s.apply_message(msg("c", "2", "8:orgid:b", 200, "two"));
        assert!(s.messages(&c)[0].message.deleted, "not resurrected");
        assert_eq!(s.messages(&c)[1].message.reactions, like, "not rolled back");
    }

    #[test]
    fn apply_event_variants() {
        let mut s = Store::new();
        let c = ConversationId("c".into());
        assert_eq!(
            s.apply_event(&Event::NewMessage(msg("c", "5", "8:orgid:b", 5, "x"))),
            Change::Inserted
        );
        assert_eq!(
            s.apply_event(&Event::ReactionsChanged {
                conversation: c.clone(),
                message: MessageId("5".into()),
                reactions: vec![],
                version: 0,
            }),
            Change::ReactionsChanged
        );
        assert_eq!(
            s.apply_event(&Event::MessageDeleted {
                conversation: c.clone(),
                message: MessageId("5".into()),
                version: 0,
            }),
            Change::Deleted
        );
        assert_eq!(
            s.apply_event(&Event::MessageDeleted {
                conversation: c.clone(),
                message: MessageId("404".into()),
                version: 0,
            }),
            Change::Unchanged
        );
        let h = ReadHorizon {
            message_id: MessageId("5".into()),
            at: 1,
            client_message_id: None,
        };
        assert_eq!(
            s.apply_event(&Event::ReadStateChanged {
                conversation: c.clone(),
                horizon: h.clone()
            }),
            Change::ReadStateChanged
        );
        assert_eq!(
            s.conversation(&c).unwrap().conversation.read_horizon,
            Some(h)
        );
        let older = ReadHorizon {
            message_id: MessageId("3".into()),
            at: 2,
            client_message_id: None,
        };
        assert_eq!(
            s.apply_event(&Event::ReadStateChanged {
                conversation: c.clone(),
                horizon: older
            }),
            Change::Unchanged,
            "horizon never moves backwards"
        );
        assert_eq!(s.apply_event(&Event::Connected), Change::Unchanged);
        assert_eq!(
            s.apply_event(&Event::ConversationUpdated(conv(
                "c",
                ConversationKind::Group
            ))),
            Change::ConversationChanged
        );
    }

    #[test]
    fn sync_fills_stub_and_chat_list_sorts_by_activity() {
        let mut s = Store::new();
        s.set_me(&Me {
            mri: Mri("8:orgid:a".into()),
            display_name: "Ada".into(),
            upn: None,
        });
        s.apply_message(msg("19:g@thread.v2", "100", "8:orgid:b", 100, "x"));
        let mut g = conv("19:g@thread.v2", ConversationKind::Group);
        g.topic = Some("Team".into());
        g.version = 100;
        let mut n = conv("48:notes", ConversationKind::System);
        n.version = 50;
        n.last_message = Some(MessageSummary {
            id: MessageId("50".into()),
            from: Mri("8:orgid:a".into()),
            from_name: Some("Ada".into()),
            message_type: "Text".into(),
        });
        let ch = conv("19:ch@thread.tacv2", ConversationKind::Channel);
        let cl = conv("48:calllogs", ConversationKind::System);
        assert_eq!(s.apply_conversations(vec![n, g, ch, cl]), 4);
        let list: Vec<&str> = s
            .chat_list()
            .iter()
            .map(|c| c.conversation.id.0.as_str())
            .collect();
        assert_eq!(
            list,
            vec!["19:g@thread.v2", "48:notes"],
            "visible only, newest activity first; stub filled in"
        );
        assert!(
            !s.conversation(&ConversationId("19:g@thread.v2".into()))
                .unwrap()
                .stub
        );
        assert_eq!(
            s.messages(&ConversationId("19:g@thread.v2".into())).len(),
            1,
            "messages survive the sync"
        );
        let mut g2 = conv("19:g@thread.v2", ConversationKind::Group);
        g2.version = 1;
        g2.topic = Some("Stale".into());
        s.apply_conversations(vec![g2]);
        assert_eq!(
            s.conversation(&ConversationId("19:g@thread.v2".into()))
                .unwrap()
                .conversation
                .topic
                .as_deref(),
            Some("Team"),
            "older version does not overwrite"
        );
        let mut g3 = conv("19:g@thread.v2", ConversationKind::Group);
        g3.version = 0; // refetch marker from ThreadActivity
        s.apply_event(&Event::ConversationUpdated(g3));
        assert_eq!(
            s.conversation(&ConversationId("19:g@thread.v2".into()))
                .unwrap()
                .conversation
                .topic
                .as_deref(),
            Some("Team"),
            "version-0 marker never clears a topic"
        );
    }

    #[test]
    fn apply_page_sets_loaded_and_cursor() {
        let mut s = Store::new();
        let c = ConversationId("c".into());
        let page = Page {
            items: vec![
                msg("c", "3", "8:orgid:b", 3, ""),
                msg("c", "2", "8:orgid:b", 2, ""),
            ],
            next: Some(PageCursor(CursorInner::EndTime(1))),
            meta: Default::default(),
            note: None,
        };
        assert_eq!(s.apply_page(&c, page, false), 2);
        let sc = s.conversation(&c).unwrap();
        assert!(sc.loaded && sc.has_older);
        assert!(sc.older_cursor.is_some());
        let last = Page {
            items: vec![msg("c", "1", "8:orgid:b", 1, "")],
            next: None,
            meta: Default::default(),
            note: None,
        };
        s.apply_page(&c, last, true);
        let sc = s.conversation(&c).unwrap();
        assert!(!sc.has_older);
        assert_eq!(sc.messages.len(), 3);
    }

    #[test]
    fn empty_members_never_erase_known_members() {
        let mut s = Store::new();
        let id = ConversationId("19:g@thread.v2".into());
        let mut c1 = conv("19:g@thread.v2", ConversationKind::Group);
        c1.version = 1;
        c1.members = Some(vec![Member {
            mri: Mri("8:orgid:b".into()),
            role: None,
            friendly_name: Some("Bea".into()),
        }]);
        s.apply_conversations(vec![c1]);
        let mut c2 = conv("19:g@thread.v2", ConversationKind::Group);
        c2.version = 2;
        c2.members = Some(vec![]);
        s.apply_event(&Event::ConversationUpdated(c2));
        let sc = s.conversation(&id).unwrap();
        assert_eq!(sc.conversation.version, 2);
        assert_eq!(
            sc.conversation.members.as_ref().map(Vec::len),
            Some(1),
            "Some(vec![]) means no member info"
        );
        assert_eq!(s.display_name(&Mri("8:orgid:b".into())), Some("Bea"));
    }

    #[test]
    fn names_cache_and_revision_notify() {
        let mut s = Store::new();
        let rx = s.subscribe();
        let r0 = s.revision();
        let mut m = msg("c", "1", "8:orgid:b", 1, "x");
        m.from_name = Some("Bea".into());
        s.apply_message(m);
        assert_eq!(s.display_name(&Mri("8:orgid:b".into())), Some("Bea"));
        assert!(s.revision() > r0);
        assert!(rx.has_changed().unwrap());
        s.apply_thread_info(ThreadInfo {
            id: ConversationId("c".into()),
            topic: None,
            members: vec![Member {
                mri: Mri("8:orgid:c".into()),
                role: None,
                friendly_name: Some("Cy".into()),
            }],
        });
        assert_eq!(s.display_name(&Mri("8:orgid:c".into())), Some("Cy"));
        let sc = s.conversation(&ConversationId("c".into())).unwrap();
        assert!(sc.thread_info.is_some());
        assert!(!sc.stub, "a thread() fetch fills a stub in (spec §4.8)");
    }

    #[test]
    fn thread_refetch_after_marker_applies_a_rename() {
        let mut s = Store::new();
        let id = ConversationId("19:g@thread.v2".into());
        let mut g = conv("19:g@thread.v2", ConversationKind::Group);
        g.topic = Some("Team".into());
        g.version = 100;
        s.apply_conversations(vec![g]);
        // ThreadActivity/TopicUpdate → version-0 marker; the engine refetches thread().
        s.apply_event(&Event::ConversationUpdated(conv(
            "19:g@thread.v2",
            ConversationKind::Group,
        )));
        s.apply_thread_info(ThreadInfo {
            id: id.clone(),
            topic: Some("Renamed".into()),
            members: vec![],
        });
        assert_eq!(
            s.conversation(&id).unwrap().conversation.topic.as_deref(),
            Some("Renamed")
        );
        s.apply_thread_info(ThreadInfo {
            id: id.clone(),
            topic: None,
            members: vec![],
        });
        assert_eq!(
            s.conversation(&id).unwrap().conversation.topic.as_deref(),
            Some("Renamed"),
            "a thread() result without a topic keeps the known one"
        );
    }

    #[test]
    fn empty_extra_never_erases_retained_properties() {
        let mut s = Store::new();
        let id = ConversationId("19:o@thread.v2".into());
        let mut c1 = conv("19:o@thread.v2", ConversationKind::OneToOne);
        c1.version = 1;
        c1.extra.insert(
            "threadProperties".into(),
            serde_json::json!({ "uniquerosterthread": "true" }),
        );
        s.apply_conversations(vec![c1]);
        let mut c2 = conv("19:o@thread.v2", ConversationKind::OneToOne);
        c2.version = 2; // a ThreadUpdate carries no extra
        s.apply_event(&Event::ConversationUpdated(c2));
        let sc = s.conversation(&id).unwrap();
        assert_eq!(sc.conversation.version, 2);
        assert!(sc.conversation.extra.contains_key("threadProperties"));
    }

    fn store_with_me() -> Store {
        store_with_me_from(now_ms())
    }

    #[test]
    fn unread_from_conversation_list_only() {
        let mut s = store_with_me();
        let mut c = conv("19:g@thread.v2", ConversationKind::Group);
        c.version = 10;
        c.last_message = Some(MessageSummary {
            id: MessageId("10".into()),
            from: Mri("8:orgid:b".into()),
            from_name: None,
            message_type: "Text".into(),
        });
        s.apply_conversations(vec![c.clone()]);
        let id = ConversationId("19:g@thread.v2".into());
        assert_eq!(s.unread(&id), Unread::Some, "no horizon = never read");
        s.apply_event(&Event::ReadStateChanged {
            conversation: id.clone(),
            horizon: ReadHorizon {
                message_id: MessageId("10".into()),
                at: 1,
                client_message_id: None,
            },
        });
        assert_eq!(s.unread(&id), Unread::None);
        c.last_message.as_mut().unwrap().id = MessageId("11".into());
        c.version = 11;
        s.apply_conversations(vec![c.clone()]);
        assert_eq!(s.unread(&id), Unread::Some);
        c.last_message.as_mut().unwrap().from = Mri("8:orgid:a".into());
        c.version = 12;
        s.apply_conversations(vec![c.clone()]);
        assert_eq!(s.unread(&id), Unread::None, "own last message is read");
        c.last_message.as_mut().unwrap().from = Mri("8:orgid:b".into());
        c.last_message.as_mut().unwrap().message_type = "Event/Call".into();
        c.version = 13;
        s.apply_conversations(vec![c]);
        assert_eq!(s.unread(&id), Unread::None, "non-displayable is not unread");
    }

    #[test]
    fn unread_never_opened_chat_uses_event_messages() {
        // Decision #23: the newer of lastMessage and any event-received message.
        let mut s = store_with_me();
        let id = ConversationId("19:g@thread.v2".into());
        let mut c = conv("19:g@thread.v2", ConversationKind::Group);
        c.version = 10;
        c.last_message = Some(MessageSummary {
            id: MessageId("10".into()),
            from: Mri("8:orgid:a".into()),
            from_name: None,
            message_type: "Text".into(),
        });
        s.apply_conversations(vec![c]);
        assert_eq!(s.unread(&id), Unread::None, "own last message");
        s.apply_event(&Event::NewMessage(msg(
            "19:g@thread.v2",
            "20",
            "8:orgid:b",
            20,
            "hey",
        )));
        assert_eq!(
            s.unread(&id),
            Unread::Some,
            "a live message from someone else"
        );
        assert!(s.advance_horizon_local(
            &id,
            &MessageRef {
                id: MessageId("20".into()),
                client_message_id: None
            }
        ));
        assert_eq!(s.unread(&id), Unread::None);
        assert_eq!(s.unread(&ConversationId("unknown".into())), Unread::None);
    }

    #[test]
    fn unread_counts_once_loaded() {
        let mut s = store_with_me();
        let id = ConversationId("c".into());
        let page = Page {
            items: vec![
                msg("c", "5", "8:orgid:b", 5, ""),
                msg("c", "4", "8:orgid:a", 4, ""),
                msg("c", "3", "8:orgid:b", 3, ""),
                msg("c", "2", "8:orgid:b", 2, ""),
            ],
            next: Some(PageCursor(CursorInner::EndTime(1))),
            meta: Default::default(),
            note: None,
        };
        s.apply_page(&id, page, false);
        assert_eq!(
            s.unread(&id),
            Unread::AtLeast(3),
            "no horizon, older pages exist → N+"
        );
        s.apply_event(&Event::ReadStateChanged {
            conversation: id.clone(),
            horizon: ReadHorizon {
                message_id: MessageId("3".into()),
                at: 1,
                client_message_id: None,
            },
        });
        assert_eq!(s.unread(&id), Unread::Count(1));
        s.apply_event(&Event::ReadStateChanged {
            conversation: id.clone(),
            horizon: ReadHorizon {
                message_id: MessageId("5".into()),
                at: 2,
                client_message_id: None,
            },
        });
        assert_eq!(s.unread(&id), Unread::None);
    }

    #[test]
    fn unread_count_skips_deleted_and_non_displayable() {
        let mut s = store_with_me();
        let id = ConversationId("c".into());
        let mut call = msg("c", "6", "8:orgid:b", 6, "");
        call.message_type = "Event/Call".into();
        let mut gone = msg("c", "5", "8:orgid:b", 5, "");
        gone.deleted = true;
        let page = Page {
            items: vec![
                call,
                gone,
                msg("c", "4", "8:orgid:b", 4, ""),
                msg("c", "3", "8:orgid:b", 3, ""),
            ],
            next: None,
            meta: Default::default(),
            note: None,
        };
        s.apply_page(&id, page, false);
        assert_eq!(
            s.unread(&id),
            Unread::Count(2),
            "no older pages → an exact count"
        );
        s.insert_pending(&id, "p".into(), RichText::text("mine"), None);
        assert_eq!(
            s.unread(&id),
            Unread::Count(2),
            "pending sends are not counted"
        );
    }

    #[test]
    fn pending_send_lifecycle() {
        let mut s = store_with_me();
        let id = ConversationId("c".into());
        s.apply_message(msg("c", "100", "8:orgid:b", 100, "hi"));
        let key = s.insert_pending(&id, "cmid1".into(), RichText::text("mine"), None);
        assert_eq!(key, MessageKey::Pending("cmid1".into()));
        let list = s.messages(&id);
        assert_eq!(list.len(), 2);
        assert_eq!(list[1].key, key);
        assert_eq!(list[1].send_state, Some(SendState::Pending));
        assert_eq!(list[1].message.from.0, "8:orgid:a");
        assert!(s.mark_sent("cmid1"));
        assert_eq!(s.messages(&id)[1].send_state, Some(SendState::Sent));
        assert_eq!(s.messages(&id)[1].key, key, "201 keeps the pending key");
        let mut echo = msg("c", "101", "8:orgid:a", 101, "mine");
        echo.client_message_id = Some("cmid1".into());
        assert_eq!(s.apply_message(echo), Change::Inserted);
        assert_eq!(s.messages(&id).len(), 2, "echo replaces the pending entry");
        assert_eq!(
            s.messages(&id)[1].key,
            MessageKey::Server(MessageId("101".into()))
        );
        assert_eq!(s.messages(&id)[1].send_state, Some(SendState::Sent));
        assert_eq!(
            s.key_for_client_message_id("cmid1"),
            Some((id.clone(), MessageKey::Server(MessageId("101".into()))))
        );
        assert!(
            s.apply_message({
                let mut m = msg("c", "101", "8:orgid:a", 101, "mine");
                m.client_message_id = Some("cmid1".into());
                m
            }) == Change::Unchanged,
            "refetch of the same version is a no-op"
        );
    }

    #[test]
    fn failed_send_retry_and_late_echo() {
        let mut s = store_with_me();
        let id = ConversationId("c".into());
        s.insert_pending(&id, "cmid2".into(), RichText::text("x"), None);
        assert!(s.mark_failed("cmid2", "403".into()));
        assert_eq!(
            s.messages(&id)[0].send_state,
            Some(SendState::Failed("403".into()))
        );
        assert_eq!(s.pending_body("cmid2").unwrap().1, RichText::text("x"));
        assert!(!s.mark_sent("nope"));
        let mut echo = msg("c", "7", "8:orgid:a", 7, "x");
        echo.client_message_id = Some("cmid2".into());
        s.apply_message(echo);
        assert_eq!(s.messages(&id)[0].send_state, Some(SendState::Sent));
        assert_eq!(
            s.pending_body("cmid2"),
            None,
            "retry after the echo is a no-op"
        );
        assert!(!s.mark_pending("cmid2"));
        assert!(
            !s.mark_failed("cmid2", "late".into()),
            "an echoed message cannot fail"
        );
        assert_eq!(s.messages(&id)[0].send_state, Some(SendState::Sent));
    }

    #[test]
    fn retry_flips_failed_back_to_pending() {
        let mut s = store_with_me();
        let id = ConversationId("c".into());
        s.insert_pending(&id, "cmid3".into(), RichText::text("y"), None);
        assert!(
            !s.mark_pending("cmid3"),
            "only a failed send can be retried"
        );
        assert!(s.mark_failed("cmid3", "500".into()));
        let rev = s.messages(&id)[0].local_rev;
        assert!(s.mark_pending("cmid3"));
        assert_eq!(s.messages(&id)[0].send_state, Some(SendState::Pending));
        assert!(s.messages(&id)[0].local_rev > rev);
        assert_eq!(
            s.pending_body("cmid3"),
            Some((id.clone(), RichText::text("y"), None))
        );
        assert!(s.mark_sent("cmid3"));
        assert_eq!(s.messages(&id)[0].key, MessageKey::Pending("cmid3".into()));
    }

    #[test]
    fn echo_before_201_keeps_sent() {
        let mut s = store_with_me();
        let id = ConversationId("c".into());
        s.insert_pending(&id, "cmid4".into(), RichText::text("z"), None);
        let mut echo = msg("c", "9", "8:orgid:a", 9, "z");
        echo.client_message_id = Some("cmid4".into());
        assert_eq!(s.apply_message(echo), Change::Inserted);
        assert!(
            s.mark_sent("cmid4"),
            "the 201 after the echo still succeeds"
        );
        assert_eq!(s.messages(&id).len(), 1);
        assert_eq!(
            s.messages(&id)[0].key,
            MessageKey::Server(MessageId("9".into()))
        );
        assert_eq!(s.messages(&id)[0].send_state, Some(SendState::Sent));
    }

    #[test]
    fn echo_for_an_already_stored_server_copy_drops_the_pending_entry() {
        let mut s = store_with_me();
        let id = ConversationId("c".into());
        // The server copy arrived first without its client message id.
        s.apply_message(msg("c", "50", "8:orgid:a", 50, "dup"));
        s.insert_pending(&id, "cmid5".into(), RichText::text("dup"), None);
        assert!(s.mark_failed("cmid5", "timeout".into()));
        assert_eq!(s.messages(&id).len(), 2);
        let rev = s.revision();
        let mut echo = msg("c", "50", "8:orgid:a", 50, "dup");
        echo.client_message_id = Some("cmid5".into());
        s.apply_message(echo);
        let list = s.messages(&id);
        assert_eq!(list.len(), 1, "the pending duplicate is gone");
        assert_eq!(list[0].key, MessageKey::Server(MessageId("50".into())));
        assert_eq!(
            list[0].send_state,
            Some(SendState::Sent),
            "the survivor is marked sent"
        );
        assert!(s.revision() > rev);
        assert_eq!(
            s.key_for_client_message_id("cmid5"),
            Some((id.clone(), MessageKey::Server(MessageId("50".into()))))
        );
        assert_eq!(s.pending_body("cmid5"), None);
    }

    #[test]
    fn insert_pending_fills_the_local_message() {
        let mut s = store_with_me();
        let id = ConversationId("c".into());
        let rev = s.revision();
        s.insert_pending(&id, "cmid6".into(), RichText::text("hello"), None);
        assert!(s.revision() > rev);
        let m = &s.messages(&id)[0];
        assert!(m.reactions_confirmed);
        assert_eq!(m.message.id, MessageId("pending:cmid6".into()));
        assert_eq!(m.message.client_message_id.as_deref(), Some("cmid6"));
        assert_eq!(m.message.from_name.as_deref(), Some("Ada"));
        assert_eq!(m.message.version, 0);
        assert!(m.message.arrived_at > 0);
        assert_eq!(s.me_name(), Some("Ada"));
        assert_eq!(
            s.find(&id, &MessageKey::Pending("cmid6".into()))
                .map(|m| m.key.clone()),
            Some(MessageKey::Pending("cmid6".into()))
        );
    }

    #[test]
    fn optimistic_reactions_and_revert() {
        let mut s = store_with_me();
        let id = ConversationId("c".into());
        let mid = MessageId("1".into());
        s.apply_message(msg("c", "1", "8:orgid:b", 1, "x"));
        let prev = s
            .set_reaction_optimistic(&id, &mid, &Reaction::Like, true)
            .unwrap();
        assert!(prev.is_empty());
        let m = &s.messages(&id)[0];
        assert_eq!(m.message.reactions[0].users, vec![Mri("8:orgid:a".into())]);
        assert!(!m.reactions_confirmed);
        let prev2 = s
            .set_reaction_optimistic(&id, &mid, &Reaction::Like, false)
            .unwrap();
        assert_eq!(prev2.len(), 1);
        assert!(s.messages(&id)[0].message.reactions.is_empty());
        s.revert_reactions(&id, &mid, prev2);
        assert_eq!(s.messages(&id)[0].message.reactions.len(), 1);
        assert!(s.messages(&id)[0].reactions_confirmed);
        assert!(
            s.set_reaction_optimistic(&id, &MessageId("404".into()), &Reaction::Like, true)
                .is_none()
        );
    }

    #[test]
    fn optimistic_reaction_joins_existing_summary_and_server_confirms() {
        let mut s = store_with_me();
        let id = ConversationId("c".into());
        let mid = MessageId("1".into());
        let mut m = msg("c", "1", "8:orgid:b", 1, "x");
        m.reactions = vec![ReactionSummary {
            reaction: Reaction::Heart,
            users: vec![Mri("8:orgid:b".into())],
        }];
        s.apply_message(m);
        let rev = s.messages(&id)[0].local_rev;
        s.set_reaction_optimistic(&id, &mid, &Reaction::Heart, true)
            .unwrap();
        let sm = &s.messages(&id)[0];
        assert_eq!(sm.message.reactions.len(), 1);
        assert_eq!(
            sm.message.reactions[0].users,
            vec![Mri("8:orgid:b".into()), Mri("8:orgid:a".into())]
        );
        assert!(sm.local_rev > rev);
        s.set_reaction_optimistic(&id, &mid, &Reaction::Heart, false)
            .unwrap();
        assert_eq!(
            s.messages(&id)[0].message.reactions[0].users,
            vec![Mri("8:orgid:b".into())],
            "summary with other users survives"
        );
        s.apply_event(&Event::ReactionsChanged {
            conversation: id.clone(),
            message: mid.clone(),
            reactions: vec![],
            version: 0,
        });
        assert!(
            s.messages(&id)[0].reactions_confirmed,
            "the server's ReactionsChanged confirms"
        );
    }

    #[test]
    fn local_horizon_never_moves_backwards() {
        let mut s = store_with_me();
        let id = ConversationId("c".into());
        s.apply_message(msg("c", "9", "8:orgid:b", 9, "x"));
        assert!(s.advance_horizon_local(
            &id,
            &MessageRef {
                id: MessageId("9".into()),
                client_message_id: None
            }
        ));
        assert!(!s.advance_horizon_local(
            &id,
            &MessageRef {
                id: MessageId("8".into()),
                client_message_id: None
            }
        ));
        assert_eq!(s.unread(&id), Unread::None);
    }

    #[test]
    fn naming_rules() {
        let mut s = store_with_me();
        s.set_me(&Me {
            mri: Mri("8:orgid:aaaaaaaa-1111-1111-1111-111111111111".into()),
            display_name: "Ada".into(),
            upn: None,
        });
        let id1 = "19:aaaaaaaa-1111-1111-1111-111111111111_bbbbbbbb-2222-2222-2222-222222222222@unq.gbl.spaces";
        let mut c1 = conv(id1, ConversationKind::OneToOne);
        c1.version = 1;
        s.apply_conversations(vec![c1]);
        assert_eq!(
            s.peer_of(&ConversationId(id1.into())),
            Some(Mri("8:orgid:bbbbbbbb-2222-2222-2222-222222222222".into())),
            "rule 1: from the id"
        );
        assert_eq!(
            s.conversation_name(&ConversationId(id1.into())),
            None,
            "peer known but unnamed"
        );
        assert!(s.needs_thread_fetch(&ConversationId(id1.into())));
        assert_eq!(
            s.unresolved_mris(),
            vec![Mri("8:orgid:bbbbbbbb-2222-2222-2222-222222222222".into())]
        );
        s.learn_name(
            &Mri("8:orgid:bbbbbbbb-2222-2222-2222-222222222222".into()),
            "Bea",
        );
        assert_eq!(
            s.conversation_name(&ConversationId(id1.into())).as_deref(),
            Some("Bea")
        );
        assert!(!s.needs_thread_fetch(&ConversationId(id1.into())));
        let mut c2 = conv("19:uni01_x@thread.v2", ConversationKind::OneToOne);
        c2.version = 1;
        c2.last_message = Some(MessageSummary {
            id: MessageId("1".into()),
            from: Mri("8:orgid:cccc".into()),
            from_name: Some("Cy".into()),
            message_type: "Text".into(),
        });
        s.apply_conversations(vec![c2]);
        assert_eq!(
            s.conversation_name(&ConversationId("19:uni01_x@thread.v2".into()))
                .as_deref(),
            Some("Cy"),
            "rule 2: last message from the peer"
        );
        let mut c3 = conv("19:uni01_y@thread.v2", ConversationKind::OneToOne);
        c3.version = 1;
        c3.last_message = Some(MessageSummary {
            id: MessageId("1".into()),
            from: Mri("8:orgid:aaaaaaaa-1111-1111-1111-111111111111".into()),
            from_name: Some("Ada".into()),
            message_type: "Text".into(),
        });
        s.apply_conversations(vec![c3]);
        assert!(
            s.needs_thread_fetch(&ConversationId("19:uni01_y@thread.v2".into())),
            "rule 3 needs thread()"
        );
        s.apply_thread_info(ThreadInfo {
            id: ConversationId("19:uni01_y@thread.v2".into()),
            topic: None,
            members: vec![
                Member {
                    mri: Mri("8:orgid:aaaaaaaa-1111-1111-1111-111111111111".into()),
                    role: None,
                    friendly_name: Some("Ada".into()),
                },
                Member {
                    mri: Mri("8:orgid:dddd".into()),
                    role: None,
                    friendly_name: Some("Dee".into()),
                },
            ],
        });
        assert_eq!(
            s.conversation_name(&ConversationId("19:uni01_y@thread.v2".into()))
                .as_deref(),
            Some("Dee")
        );
        let mut g = conv("19:g@thread.v2", ConversationKind::Group);
        g.version = 1;
        s.apply_conversations(vec![g]);
        assert!(
            s.needs_thread_fetch(&ConversationId("19:g@thread.v2".into())),
            "untitled group"
        );
        s.apply_thread_info(ThreadInfo {
            id: ConversationId("19:g@thread.v2".into()),
            topic: None,
            members: vec![
                Member {
                    mri: Mri("8:orgid:aaaaaaaa-1111-1111-1111-111111111111".into()),
                    role: None,
                    friendly_name: Some("Ada".into()),
                },
                Member {
                    mri: Mri("8:orgid:1".into()),
                    role: None,
                    friendly_name: Some("One".into()),
                },
                Member {
                    mri: Mri("8:orgid:2".into()),
                    role: None,
                    friendly_name: Some("Two".into()),
                },
                Member {
                    mri: Mri("8:orgid:3".into()),
                    role: None,
                    friendly_name: Some("Three".into()),
                },
                Member {
                    mri: Mri("8:orgid:4".into()),
                    role: None,
                    friendly_name: Some("Four".into()),
                },
            ],
        });
        assert_eq!(
            s.conversation_name(&ConversationId("19:g@thread.v2".into()))
                .as_deref(),
            Some("One, Two, Three")
        );
        let mut t = conv("19:t@thread.v2", ConversationKind::Group);
        t.version = 1;
        t.topic = Some("Topic".into());
        s.apply_conversations(vec![t]);
        assert_eq!(
            s.conversation_name(&ConversationId("19:t@thread.v2".into()))
                .as_deref(),
            Some("Topic")
        );
        let mut n = conv("48:notes", ConversationKind::System);
        n.version = 1;
        s.apply_conversations(vec![n]);
        assert_eq!(
            s.conversation_name(&ConversationId("48:notes".into()))
                .as_deref(),
            Some("Notes")
        );
        let names: Vec<&str> = s
            .chat_list()
            .iter()
            .map(|c| c.conversation.id.0.as_str())
            .collect();
        assert!(
            names.contains(&"48:notes") && names.contains(&"19:t@thread.v2"),
            "named conversations are listed; unnamed are not"
        );
    }

    #[test]
    fn unnamed_conversations_stay_out_of_the_chat_list() {
        let mut s = store_with_me();
        let mut g = conv("19:u@thread.v2", ConversationKind::Group);
        g.version = 1;
        s.apply_conversations(vec![g]);
        assert!(
            s.chat_list().is_empty(),
            "an untitled group without members has no name"
        );
        s.apply_thread_info(ThreadInfo {
            id: ConversationId("19:u@thread.v2".into()),
            topic: None,
            members: vec![
                Member {
                    mri: Mri("8:orgid:a".into()),
                    role: None,
                    friendly_name: None,
                },
                Member {
                    mri: Mri("8:orgid:x".into()),
                    role: None,
                    friendly_name: None,
                },
                Member {
                    mri: Mri("8:orgid:y".into()),
                    role: None,
                    friendly_name: Some("Yan".into()),
                },
            ],
        });
        assert_eq!(
            s.unresolved_mris(),
            vec![Mri("8:orgid:x".into())],
            "me is never unresolved"
        );
        assert_eq!(
            s.conversation_name(&ConversationId("19:u@thread.v2".into()))
                .as_deref(),
            Some("Yan")
        );
        assert_eq!(s.chat_list().len(), 1);
        s.learn_name(&Mri("8:orgid:x".into()), "Xi");
        assert_eq!(
            s.conversation_name(&ConversationId("19:u@thread.v2".into()))
                .as_deref(),
            Some("Xi, Yan")
        );
        assert!(s.unresolved_mris().is_empty());
        // A stub from an event stays hidden until a sync or thread() names it.
        s.apply_message(msg("19:ab_cd@unq.gbl.spaces", "1", "8:orgid:cd", 1, "x"));
        assert_eq!(s.chat_list().len(), 1);
    }

    #[test]
    fn local_horizon_ignores_pending_ids() {
        let mut s = store_with_me();
        let id = ConversationId("c".into());
        s.apply_message(msg("c", "9", "8:orgid:b", 9, "x"));
        s.insert_pending(&id, "x".into(), RichText::text("mine"), None);
        let rev = s.revision();
        assert!(!s.advance_horizon_local(
            &id,
            &MessageRef {
                id: MessageId("pending:x".into()),
                client_message_id: Some("x".into())
            }
        ));
        assert!(
            !s.advance_horizon_local(
                &id,
                &MessageRef {
                    id: MessageId("10".into()),
                    client_message_id: None
                }
            ),
            "not a stored message"
        );
        assert!(!s.advance_horizon_local(
            &ConversationId("unknown".into()),
            &MessageRef {
                id: MessageId("9".into()),
                client_message_id: None
            }
        ));
        assert_eq!(s.revision(), rev);
        assert_eq!(s.conversation(&id).unwrap().conversation.read_horizon, None);
        assert_eq!(s.unread(&id), Unread::Some);
        assert!(
            s.advance_horizon_local(
                &id,
                &MessageRef {
                    id: MessageId("9".into()),
                    client_message_id: None
                }
            ),
            "a numeric horizon still advances"
        );
        assert_eq!(s.unread(&id), Unread::None);
        s.apply_message(msg("c", "11", "8:orgid:b", 11, "y"));
        assert_eq!(
            s.apply_event(&Event::ReadStateChanged {
                conversation: id.clone(),
                horizon: ReadHorizon {
                    message_id: MessageId("11".into()),
                    at: 2,
                    client_message_id: None
                }
            }),
            Change::ReadStateChanged
        );
    }

    #[test]
    fn equal_version_server_copy_confirms_unconfirmed_reactions() {
        let mut s = store_with_me();
        let id = ConversationId("c".into());
        let mid = MessageId("1".into());
        s.apply_message(msg("c", "1", "8:orgid:b", 1, "x"));
        // A confirmed message ignores an equal-version copy (version rule).
        let mut same = msg("c", "1", "8:orgid:b", 1, "x");
        same.reactions = vec![ReactionSummary {
            reaction: Reaction::Heart,
            users: vec![Mri("8:orgid:b".into())],
        }];
        assert_eq!(s.apply_message(same), Change::Unchanged);
        assert!(s.messages(&id)[0].message.reactions.is_empty());

        s.set_reaction_optimistic(&id, &mid, &Reaction::Like, true)
            .unwrap();
        let rev = s.messages(&id)[0].local_rev;
        let r = s.revision();
        // Teams may not bump `version` for reactions: the refetched copy at the
        // same version is the server's truth for them.
        let server = vec![ReactionSummary {
            reaction: Reaction::Like,
            users: vec![Mri("8:orgid:a".into()), Mri("8:orgid:c".into())],
        }];
        let mut copy = msg("c", "1", "8:orgid:b", 1, "x");
        copy.reactions = server.clone();
        assert_eq!(s.apply_message(copy), Change::ReactionsChanged);
        let sm = &s.messages(&id)[0];
        assert_eq!(sm.message.reactions, server);
        assert!(sm.reactions_confirmed);
        assert_eq!(sm.local_rev, rev + 1);
        assert_eq!(sm.message.version, 1);
        assert!(s.revision() > r);

        // An older copy still changes nothing, confirmed or not.
        s.set_reaction_optimistic(&id, &mid, &Reaction::Heart, true)
            .unwrap();
        let older = msg("c", "1", "8:orgid:b", 0, "x");
        assert_eq!(s.apply_message(older), Change::Unchanged);
        assert!(!s.messages(&id)[0].reactions_confirmed);
    }

    #[test]
    fn revert_never_overwrites_confirmed_reactions() {
        let mut s = store_with_me();
        let id = ConversationId("c".into());
        let mid = MessageId("1".into());
        s.apply_message(msg("c", "1", "8:orgid:b", 1, "x"));
        let prev = s
            .set_reaction_optimistic(&id, &mid, &Reaction::Like, true)
            .unwrap();
        let server = vec![ReactionSummary {
            reaction: Reaction::Like,
            users: vec![Mri("8:orgid:a".into()), Mri("8:orgid:c".into())],
        }];
        s.apply_event(&Event::ReactionsChanged {
            conversation: id.clone(),
            message: mid.clone(),
            reactions: server.clone(),
            version: 0,
        });
        let rev = s.messages(&id)[0].local_rev;
        let r = s.revision();
        s.revert_reactions(&id, &mid, prev);
        assert_eq!(
            s.messages(&id)[0].message.reactions,
            server,
            "server truth wins"
        );
        assert!(s.messages(&id)[0].reactions_confirmed);
        assert_eq!(s.messages(&id)[0].local_rev, rev);
        assert_eq!(s.revision(), r);
    }

    #[test]
    fn no_op_reaction_toggles_change_nothing() {
        let mut s = store_with_me();
        let id = ConversationId("c".into());
        let mid = MessageId("1".into());
        let mut m = msg("c", "1", "8:orgid:b", 1, "x");
        m.reactions = vec![
            ReactionSummary {
                reaction: Reaction::Like,
                users: vec![Mri("8:orgid:a".into())],
            },
            ReactionSummary {
                reaction: Reaction::Heart,
                users: vec![Mri("8:orgid:b".into())],
            },
        ];
        s.apply_message(m);
        let rev = s.messages(&id)[0].local_rev;
        let r = s.revision();
        assert!(
            s.set_reaction_optimistic(&id, &mid, &Reaction::Like, true)
                .is_none(),
            "already mine"
        );
        assert!(
            s.set_reaction_optimistic(&id, &mid, &Reaction::Sad, false)
                .is_none(),
            "no such summary"
        );
        assert!(
            s.set_reaction_optimistic(&id, &mid, &Reaction::Heart, false)
                .is_none(),
            "summary without me"
        );
        let sm = &s.messages(&id)[0];
        assert!(sm.reactions_confirmed);
        assert_eq!(sm.local_rev, rev);
        assert_eq!(s.revision(), r);
    }

    #[test]
    fn peer_of_ignores_group_chats() {
        let mut s = store_with_me();
        let mut g = conv("19:g@thread.v2", ConversationKind::Group);
        g.version = 1;
        g.last_message = Some(MessageSummary {
            id: MessageId("1".into()),
            from: Mri("8:orgid:b".into()),
            from_name: Some("Bea".into()),
            message_type: "Text".into(),
        });
        s.apply_conversations(vec![g]);
        assert_eq!(s.peer_of(&ConversationId("19:g@thread.v2".into())), None);
        assert_eq!(
            s.conversation_name(&ConversationId("19:g@thread.v2".into())),
            None
        );
    }

    /// `store_with_me`, but with `last_event_time` starting at `start`
    /// (ruling D3) instead of the wall clock.
    fn store_with_me_from(start: Timestamp) -> Store {
        let mut s = Store::with_start(start);
        s.set_me(&Me {
            mri: Mri("8:orgid:a".into()),
            display_name: "Ada".into(),
            upn: None,
        });
        s
    }

    #[test]
    fn new_store_starts_last_event_time_at_now() {
        let before = now_ms();
        let s = Store::new();
        assert!(s.last_event_time() >= before);
        assert!(s.last_event_time() <= now_ms());
        assert_eq!(Store::with_start(0).last_event_time(), 0);
    }

    #[test]
    fn note_event_now_moves_forward_only() {
        let mut s = store_with_me_from(0);
        let before = now_ms();
        s.note_event_now();
        assert!(s.last_event_time() >= before);
        let mut future = store_with_me_from(i64::MAX - 1);
        future.note_event_now();
        assert_eq!(
            future.last_event_time(),
            i64::MAX - 1,
            "never moves backwards"
        );
    }

    #[test]
    fn plan_uses_last_event_time_minus_overlap_and_loaded_conversations() {
        let mut s = store_with_me_from(0);
        s.note_event_time(Some("2025-10-13T09:00:00.000Z"));
        assert_eq!(s.last_event_time(), 1_760_346_000_000);
        s.note_event_time(Some("garbage"));
        assert_eq!(
            s.last_event_time(),
            1_760_346_000_000,
            "unparseable keeps the previous value"
        );
        s.note_event_time(None);
        assert_eq!(s.last_event_time(), 1_760_346_000_000, "no time keeps it");
        s.note_event_time(Some("2025-10-13T08:00:00.000Z"));
        assert_eq!(
            s.last_event_time(),
            1_760_346_000_000,
            "never moves backwards"
        );
        s.apply_page(
            &ConversationId("c".into()),
            Page {
                items: vec![msg("c", "1", "8:orgid:b", 1, "")],
                next: None,
                meta: Default::default(),
                note: None,
            },
            false,
        );
        s.apply_message(msg("d", "2", "8:orgid:b", 2, "")); // event only, not loaded
        let p = s.catch_up_plan();
        assert_eq!(
            p,
            CatchUpPlan {
                since: 1_760_346_000_000 - 60_000,
                loaded_conversations: vec![ConversationId("c".into())],
                page_size: 50
            }
        );
    }

    #[test]
    fn plan_lists_loaded_conversations_sorted_and_never_negative() {
        let mut s = store_with_me_from(0);
        for c in ["z", "b", "m"] {
            let id = ConversationId(c.into());
            s.apply_page(
                &id,
                Page {
                    items: vec![],
                    next: None,
                    meta: Default::default(),
                    note: None,
                },
                false,
            );
        }
        let p = s.catch_up_plan();
        assert_eq!(p.since, 0, "start 0 minus the overlap clamps at 0");
        assert_eq!(
            p.loaded_conversations,
            vec![
                ConversationId("b".into()),
                ConversationId("m".into()),
                ConversationId("z".into())
            ]
        );
    }

    #[test]
    fn apply_catch_up_upserts_and_advances_time() {
        let mut s = store_with_me_from(0);
        let rx = s.subscribe();
        let r = CatchUpResult {
            started_at: 1_760_346_500_000,
            conversations: vec![{
                let mut c = conv("19:g@thread.v2", ConversationKind::Group);
                c.version = 1;
                c.topic = Some("T".into());
                c
            }],
            messages: vec![msg("19:g@thread.v2", "5", "8:orgid:b", 5, "x")],
            ..Default::default()
        };
        assert_eq!(s.apply_catch_up(r), 1);
        assert_eq!(s.last_event_time(), 1_760_346_500_000);
        assert!(rx.has_changed().unwrap(), "resynced notification");
        let id = ConversationId("19:g@thread.v2".into());
        assert_eq!(s.conversation_name(&id).as_deref(), Some("T"));
        assert_eq!(s.messages(&id).len(), 1);
    }

    #[test]
    fn incomplete_catch_up_applies_but_keeps_last_event_time() {
        let mut s = store_with_me_from(1_760_346_000_000);
        let n = s.apply_catch_up(CatchUpResult {
            started_at: 1_760_346_500_000,
            messages: vec![msg("c", "1", "8:orgid:b", 1, "x")],
            errors: vec!["conversations: http 500".into()],
            ..Default::default()
        });
        assert_eq!(n, 1, "what did arrive is applied");
        assert_eq!(
            s.last_event_time(),
            1_760_346_000_000,
            "the gap stays open for the next catch-up"
        );
        s.apply_catch_up(CatchUpResult {
            started_at: 1_760_346_500_000,
            notes: vec!["conversations: cap".into()],
            ..Default::default()
        });
        assert_eq!(
            s.last_event_time(),
            1_760_346_500_000,
            "a guard note alone does not make it incomplete"
        );
    }

    #[test]
    fn apply_catch_up_counts_only_inserts_and_always_notifies() {
        let mut s = store_with_me_from(1_760_346_000_000);
        s.apply_message(msg("c", "1", "8:orgid:b", 1, "old"));
        let r = s.revision();
        let n = s.apply_catch_up(CatchUpResult {
            started_at: 1_760_000_000_000,
            messages: vec![
                msg("c", "1", "8:orgid:b", 2, "edited"),
                msg("c", "1", "8:orgid:b", 2, "edited"),
                msg("c", "2", "8:orgid:b", 1, "new"),
            ],
            errors: vec!["c: latest page: boom".into()],
            ..Default::default()
        });
        assert_eq!(n, 1, "an edit and a duplicate are not inserts");
        assert_eq!(
            s.last_event_time(),
            1_760_346_000_000,
            "an older started_at never moves it backwards"
        );
        assert!(s.revision() > r);
        let r = s.revision();
        assert_eq!(s.apply_catch_up(CatchUpResult::default()), 0);
        assert!(s.revision() > r, "resynced fires even when nothing changed");
    }

    /// A CSA chat entry with `members` (MRI, friendlyName).
    fn csa_chat(id: &str, members: &[(&str, Option<&str>)]) -> ChatSummary {
        ChatSummary {
            id: ConversationId(id.into()),
            title: None,
            kind: ChatKind::Group,
            hidden: false,
            muted: false,
            members: members
                .iter()
                .map(|(m, n)| ChatMember {
                    mri: Mri((*m).into()),
                    role: Some("User".into()),
                    friendly_name: n.map(str::to_owned),
                })
                .collect(),
            last_message_at: None,
        }
    }

    #[test]
    fn user_state_rosters_go_only_to_known_non_stub_chats() {
        let mut s = store_with_me();
        s.apply_conversations(vec![Conversation {
            version: 10,
            ..conv("19:known@thread.v2", ConversationKind::Group)
        }]);
        // An event for an unknown chat leaves a stub.
        s.apply_message(msg("19:stub@thread.v2", "5", "8:orgid:b", 5, "x"));
        let us = UserState {
            chats: vec![
                csa_chat(
                    "19:known@thread.v2",
                    &[
                        ("8:orgid:a", None),
                        ("8:orgid:b", Some("Bea")),
                        ("8:orgid:e", None),
                    ],
                ),
                csa_chat("19:stub@thread.v2", &[("8:orgid:c", None)]),
                csa_chat("19:csa-only@thread.v2", &[("8:orgid:d", None)]),
            ],
            ..UserState::default()
        };
        let rev = s.revision();
        s.apply_user_state(&us);
        assert!(s.revision() > rev, "notifies");
        let known = s
            .conversation(&ConversationId("19:known@thread.v2".into()))
            .unwrap();
        let mris: Vec<&str> = known
            .conversation
            .members
            .as_ref()
            .unwrap()
            .iter()
            .map(|m| m.mri.0.as_str())
            .collect();
        assert_eq!(mris, ["8:orgid:a", "8:orgid:b", "8:orgid:e"]);
        assert_eq!(
            s.display_name(&Mri("8:orgid:b".into())),
            Some("Bea"),
            "friendlyName learned"
        );
        let stub = s
            .conversation(&ConversationId("19:stub@thread.v2".into()))
            .unwrap();
        assert!(
            stub.stub && stub.conversation.members.is_none(),
            "stubs are left alone"
        );
        assert!(
            s.conversation(&ConversationId("19:csa-only@thread.v2".into()))
                .is_none(),
            "never creates a conversation"
        );
        // The known untitled group now has a roster: its unnamed member is
        // unresolved; the CSA-only chat's member is not.
        assert_eq!(
            s.unresolved_mris(),
            [Mri("8:orgid:e".into())],
            "known chats' nameless members only; CSA-only chats stay out"
        );
        assert_eq!(s.user_state(), Some(&us), "the whole state is kept");
    }

    #[test]
    fn user_state_never_erases_members_or_touches_thread_info() {
        let mut s = store_with_me();
        let id = ConversationId("19:g@thread.v2".into());
        s.apply_conversations(vec![Conversation {
            version: 10,
            ..conv("19:g@thread.v2", ConversationKind::Group)
        }]);
        s.apply_thread_info(ThreadInfo {
            id: id.clone(),
            topic: None,
            members: vec![Member {
                mri: Mri("8:orgid:c".into()),
                role: None,
                friendly_name: Some("Cy".into()),
            }],
        });
        let info = s.conversation(&id).unwrap().thread_info.clone();
        s.apply_user_state(&UserState {
            chats: vec![csa_chat("19:g@thread.v2", &[])],
            ..UserState::default()
        });
        assert_eq!(
            s.conversation(&id)
                .unwrap()
                .conversation
                .members
                .as_ref()
                .unwrap()
                .len(),
            1,
            "an empty CSA list erases nothing"
        );
        s.apply_user_state(&UserState {
            chats: vec![csa_chat("19:g@thread.v2", &[("8:orgid:d", Some("Dan"))])],
            ..UserState::default()
        });
        let sc = s.conversation(&id).unwrap();
        assert_eq!(sc.thread_info, info, "thread_info untouched");
        assert_eq!(
            sc.conversation.members.as_ref().unwrap()[0].mri.0,
            "8:orgid:d"
        );
        assert_eq!(
            s.conversation_name(&id).as_deref(),
            Some("Cy"),
            "a thread() roster keeps priority"
        );
        assert!(Store::new().user_state().is_none());
    }

    #[test]
    fn a_csa_friendly_name_never_overwrites_a_known_name() {
        let mut s = store_with_me();
        s.apply_conversations(vec![Conversation {
            version: 10,
            ..conv("19:g@thread.v2", ConversationKind::Group)
        }]);
        s.learn_name(&Mri("8:orgid:d".into()), "Dan Known");
        s.apply_user_state(&UserState {
            chats: vec![csa_chat("19:g@thread.v2", &[("8:orgid:d", Some("Dan"))])],
            ..UserState::default()
        });
        assert_eq!(s.display_name(&Mri("8:orgid:d".into())), Some("Dan Known"));
    }

    fn typing(conv: &ConversationId, from: &str, name: Option<&str>) -> Event {
        Event::Typing {
            conversation: conv.clone(),
            from: Mri(from.into()),
            from_name: name.map(str::to_owned),
        }
    }

    /// Review Focus #5: typing never creates a conversation and expires.
    #[test]
    fn typing_lives_in_a_side_map_and_expires() {
        assert_eq!(TYPING_EXPIRY, std::time::Duration::from_secs(25));
        let mut s = store_with_me();
        let c = ConversationId("19:g@thread.v2".into());
        let rev = s.revision();
        let t0 = now_ms();
        assert_eq!(
            s.apply_event(&typing(&c, "8:orgid:b", Some("Bea"))),
            Change::Typing
        );
        let t1 = now_ms();
        assert!(s.revision() > rev, "notifies");
        assert!(s.conversation(&c).is_none(), "never creates a conversation");
        assert!(s.chat_list().is_empty());
        assert_eq!(
            s.typing(&c, t0 + 24_000),
            vec![(Mri("8:orgid:b".into()), Some("Bea".to_owned()))]
        );
        assert!(s.typing(&c, t1 + 25_000).is_empty(), "expired after 25 s");
        assert!(
            s.typing(&c, t0).is_empty(),
            "pruned on access: gone for good"
        );
        assert!(s.typing(&ConversationId("nope".into()), t0).is_empty());
        // The user's own typing is ignored.
        let rev = s.revision();
        assert_eq!(
            s.apply_event(&typing(&c, "8:orgid:a", None)),
            Change::Unchanged
        );
        assert_eq!(s.revision(), rev);
        assert!(s.typing(&c, now_ms()).is_empty());
    }

    #[test]
    fn a_new_message_from_the_typist_clears_their_typing() {
        let mut s = store_with_me();
        let c = ConversationId("19:g@thread.v2".into());
        s.apply_event(&typing(&c, "8:orgid:c", None));
        s.apply_event(&typing(&c, "8:orgid:b", Some("Bea")));
        assert_eq!(
            s.typing(&c, now_ms())
                .iter()
                .map(|(m, _)| m.0.as_str())
                .collect::<Vec<_>>(),
            ["8:orgid:b", "8:orgid:c"],
            "sorted by MRI"
        );
        // `msg` takes arrived_at from the id: a live message has a recent one.
        let live = now_ms().to_string();
        s.apply_message(msg("19:g@thread.v2", &live, "8:orgid:b", 5, "hi"));
        assert_eq!(
            s.typing(&c, now_ms()),
            vec![(Mri("8:orgid:c".into()), None)]
        );
        // A duplicate (not an insert) clears nothing.
        s.apply_event(&typing(&c, "8:orgid:b", Some("Bea")));
        s.apply_message(msg("19:g@thread.v2", &live, "8:orgid:b", 5, "hi"));
        assert_eq!(s.typing(&c, now_ms()).len(), 2);
        // An old message (a history page, a catch-up) clears nothing.
        s.apply_message(msg("19:g@thread.v2", "5", "8:orgid:c", 5, "old"));
        assert_eq!(
            s.typing(&c, now_ms()).len(),
            2,
            "history never wipes live typing"
        );
        // A message elsewhere clears nothing here.
        let elsewhere = (now_ms() + 1).to_string();
        s.apply_message(msg("19:other@thread.v2", &elsewhere, "8:orgid:c", 6, "hi"));
        assert_eq!(s.typing(&c, now_ms()).len(), 2);
    }

    #[test]
    fn a_typing_event_prunes_expired_entries_everywhere() {
        let mut s = store_with_me();
        let old = ConversationId("19:old@thread.v2".into());
        let c = ConversationId("19:g@thread.v2".into());
        s.typing
            .entry(old.clone())
            .or_default()
            .insert(Mri("8:orgid:b".into()), (None, now_ms() - 30_000));
        s.apply_event(&typing(&c, "8:orgid:c", None));
        assert!(
            !s.typing.contains_key(&old),
            "pruned without anyone reading it"
        );
        assert!(s.typing.contains_key(&c));
    }

    /// Review Focus #5: typing in a known chat moves nothing.
    #[test]
    fn typing_in_a_known_chat_changes_no_order_unread_or_messages() {
        let mut s = store_with_me();
        let a = ConversationId("19:a@thread.v2".into());
        let b = ConversationId("19:b@thread.v2".into());
        s.apply_conversations(vec![
            Conversation {
                topic: Some("A".into()),
                version: 10,
                ..conv("19:a@thread.v2", ConversationKind::Group)
            },
            Conversation {
                topic: Some("B".into()),
                version: 50,
                ..conv("19:b@thread.v2", ConversationKind::Group)
            },
        ]);
        s.apply_message(msg("19:a@thread.v2", "100", "8:orgid:b", 100, "unread"));
        let ids = |s: &Store| {
            s.chat_list()
                .iter()
                .map(|sc| sc.conversation.id.0.clone())
                .collect::<Vec<_>>()
        };
        let order = ids(&s);
        assert_eq!(order, ["19:a@thread.v2", "19:b@thread.v2"]);
        let unread = (s.unread(&a), s.unread(&b));
        assert_eq!(unread.0, Unread::Some);
        s.apply_event(&typing(&b, "8:orgid:b", Some("Bea")));
        s.apply_event(&typing(&a, "8:orgid:c", None));
        assert_eq!(ids(&s), order);
        assert_eq!((s.unread(&a), s.unread(&b)), unread);
        assert_eq!(s.messages(&a).len(), 1);
        assert!(s.messages(&b).is_empty());
    }
}
