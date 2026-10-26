//! Teams → IRC: messages, edits, deletes, reactions, typing, read markers,
//! joins and topics, and the lines a Teams message becomes.

use std::collections::BTreeSet;
use std::time::Duration;

use ic3_teams::{
    Attachment, AttachmentKind, Block, Change, ConversationId, ConversationKind, Event, Inline,
    Message as TeamsMessage, MessageId, Mri, Reaction, ReactionSummary, RichText, Store, Timestamp,
    is_displayable, is_notes, now_ms,
};
use tokio::time::Instant;

use super::{ClientId, Gateway, timestamp_ref};
use crate::irc::{Message, format_time};
use crate::media::MediaTable;
use crate::names::{Directory, NOTES_NICK, nick_base};
use crate::render::{self, RenderCtx};

/// Most bytes of message text per IRC line (the line limit is 512 with
/// the source and command).
const LINE_TEXT_MAX: usize = 400;

/// How often a still-typing user is re-announced (clients expire `active`
/// after 6 s; Teams repeats only every ~20 s).
const TYPING_REPEAT: Duration = Duration::from_secs(4);

/// A send waiting for its echo.
#[derive(Debug)]
pub(crate) struct PendingSend {
    pub(crate) client: ClientId,
    pub(crate) label: Option<String>,
}

/// A Teams message as IRC sees it: who, where, tags and text lines.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct ChatLine {
    pub(crate) source: String,
    pub(crate) target: String,
    pub(crate) tags: Vec<(String, Option<String>)>,
    pub(crate) lines: Vec<String>,
}

/// Rendering context over the gateway's directory, store and media table.
pub(crate) struct Ctx<'a> {
    pub(crate) dir: &'a Directory,
    pub(crate) store: &'a Store,
    pub(crate) media: &'a MediaTable,
    pub(crate) nick: &'a str,
}

impl RenderCtx for Ctx<'_> {
    fn mention_nick(&self, mri: &Mri, name: &str) -> String {
        if self.store.me() == Some(mri) || self.store.me_name() == Some(name.trim()) {
            return self.nick.to_string();
        }
        // A team, channel or everyone (`19:…`), or an unresolved itemid: no
        // nick stands for it, so it reads as Teams shows it.
        if !mri.0.starts_with("8:") {
            return format!("@{}", name.trim());
        }
        if let Some(n) = self.dir.existing_nick(mri) {
            return n.to_string();
        }
        let base = nick_base(name);
        if base.is_empty() {
            name.to_string()
        } else {
            base
        }
    }

    fn media_url(&self, url: &str) -> String {
        self.media.link_for(url, None)
    }

    fn resolve_nick(&self, nick: &str) -> Option<(Mri, String)> {
        if crate::irc::casefold(nick) == crate::irc::casefold(self.nick) {
            let me = self.store.me()?.clone();
            let name = self.store.me_name().unwrap_or(nick).to_string();
            return Some((me, name));
        }
        let mri = self.dir.user_by_nick(nick)?.clone();
        let name = self
            .store
            .display_name(&mri)
            .map_or_else(|| nick.to_string(), str::to_owned);
        Some((mri, name))
    }
}

/// Every mention in `body`, as (MRI, shown name).
fn mentions(body: &RichText) -> Vec<(Mri, String)> {
    fn inlines(v: &[Inline], out: &mut Vec<(Mri, String)>) {
        for i in v {
            match i {
                Inline::Mention { mri, name } => out.push((mri.clone(), name.clone())),
                Inline::Bold(c) | Inline::Italic(c) | Inline::Strike(c) => inlines(c, out),
                Inline::Link { text, .. } => inlines(text, out),
                _ => {}
            }
        }
    }
    fn blocks(v: &[Block], out: &mut Vec<(Mri, String)>) {
        for b in v {
            match b {
                Block::Paragraph(i) => inlines(i, out),
                Block::Quote(b) => blocks(b, out),
                Block::List { items, .. } => items.iter().for_each(|b| blocks(b, out)),
                Block::CodeBlock(_) | Block::Reply(_) => {}
            }
        }
    }
    let mut out = Vec::new();
    blocks(&body.0, &mut out);
    out
}

impl Gateway {
    /// The rendering context.
    fn ctx(&self) -> Ctx<'_> {
        Ctx {
            dir: &self.dir,
            store: &self.store,
            media: &self.media,
            nick: &self.nick,
        }
    }

    /// Hands out nicks for everyone `m` mentions or is from, so rendering
    /// (which only reads the directory) finds them.
    fn prepare_nicks(&mut self, m: &TeamsMessage) {
        self.nick_of(&m.from, m.from_name.as_deref());
        for (mri, name) in mentions(&m.body) {
            if mri.0.contains(':') {
                self.nick_of(&mri, Some(&name));
            }
        }
        if !m.from.0.is_empty()
            && self.store.display_name(&m.from).is_none()
            && m.from_name.is_none()
            && self.store.me() != Some(&m.from)
        {
            self.session.names_wanted.insert(m.from.clone());
        }
    }

    /// The IRC target a message in `conv` goes to, from `from`'s point of
    /// view; `None` when the conversation is not shown.
    fn target_for(&mut self, conv: &ConversationId, from: &Mri) -> Option<String> {
        if is_notes(conv) {
            return Some(NOTES_NICK.to_string());
        }
        if let Some(ch) = self.dir.existing_channel(conv)
            && self.joined.contains(conv)
        {
            return Some(ch.to_string());
        }
        let kind = self
            .store
            .conversation(conv)
            .map(|sc| sc.conversation.kind)?;
        if kind != ConversationKind::OneToOne {
            return None;
        }
        if self.store.me() == Some(from) {
            let peer = self.store.peer_of(conv)?;
            Some(self.nick_of(&peer, None))
        } else {
            Some(self.nick.clone())
        }
    }

    /// The lines of a Teams message; `None` when it is not shown.
    pub(crate) fn chat_line(&mut self, m: &TeamsMessage) -> Option<ChatLine> {
        if m.deleted || !is_displayable(&m.message_type) {
            return None;
        }
        self.prepare_nicks(m);
        let target = self.target_for(&m.conversation, &m.from)?;
        let source = if self.store.me() == Some(&m.from) {
            self.self_mask()
        } else {
            Gateway::user_mask(&self.nick_of(&m.from, m.from_name.as_deref()))
        };
        let mut tags = vec![
            ("msgid".to_string(), Some(m.id.0.clone())),
            ("time".to_string(), Some(format_time(m.arrived_at))),
        ];
        let replied = m
            .reply_to
            .as_ref()
            .map(|q| q.message_id.clone())
            .or_else(|| m.thread_root.clone());
        if let Some(id) = replied {
            tags.push(("+reply".into(), Some(id.0.clone())));
            tags.push(("+draft/reply".into(), Some(id.0)));
        }
        let ctx = self.ctx();
        let text = render::to_irc(&m.body, &ctx);
        let mut lines: Vec<String> = text
            .split('\n')
            .filter(|l| !l.trim().is_empty())
            .map(str::to_owned)
            .collect();
        lines.extend(
            m.attachments
                .iter()
                .filter_map(|a| attachment_line(a, &self.media)),
        );
        if lines.is_empty() {
            return None;
        }
        Some(ChatLine {
            source,
            target,
            tags,
            lines,
        })
    }

    /// The IRC messages `line` becomes for client `id`: one `PRIVMSG`, a
    /// `draft/multiline` batch, or (without multiline) one `PRIVMSG` per
    /// line with the message's tags on the first. `parent` nests it in a
    /// batch.
    pub(crate) fn chat_messages(
        &mut self,
        id: ClientId,
        line: &ChatLine,
        parent: Option<&str>,
    ) -> Vec<Message> {
        let multiline = self.caps(id).is_some_and(|c| c.multiline());
        let privmsg = |text: &str| {
            Message::new("PRIVMSG", [line.target.clone(), text.to_string()])
                .with_source(line.source.clone())
        };
        // (text, continues the previous line)
        let pieces: Vec<(&str, bool)> = line
            .lines
            .iter()
            .flat_map(|l| {
                split_text(l, LINE_TEXT_MAX)
                    .into_iter()
                    .enumerate()
                    .map(|(i, p)| (p, i > 0))
            })
            .collect();
        let mut out = Vec::new();
        if pieces.len() == 1 || !multiline {
            for (i, (text, _)) in pieces.iter().enumerate() {
                let mut m = privmsg(text);
                if i == 0 {
                    m.tags = line.tags.clone();
                } else if let Some(t) = line.tags.iter().find(|(k, _)| k == "time") {
                    m.tags.push(t.clone());
                }
                if let Some(p) = parent {
                    m.set_tag("batch", Some(p.to_string()));
                }
                out.push(m);
            }
            return out;
        }
        let r = self.batch_ref(id);
        let mut start = Message::new(
            "BATCH",
            [
                format!("+{r}"),
                "draft/multiline".into(),
                line.target.clone(),
            ],
        )
        .with_source(line.source.clone());
        start.tags = line.tags.clone();
        if let Some(p) = parent {
            start.set_tag("batch", Some(p.to_string()));
        }
        out.push(start);
        for (text, concat) in pieces {
            let mut m = privmsg(text).with_tag("batch", r.clone());
            if concat {
                m.set_tag("draft/multiline-concat", None);
            }
            out.push(m);
        }
        out.push(Message::new("BATCH", [format!("-{r}")]).with_source(line.source.clone()));
        out
    }

    /// Sends `line` to every client; the client whose send it echoes gets
    /// it under its label (or not at all, without `echo-message`).
    fn deliver(&mut self, line: &ChatLine, echo_of: Option<PendingSend>) {
        let ids: Vec<ClientId> = self.clients.keys().copied().collect();
        for id in ids {
            match &echo_of {
                Some(p) if p.client == id => {
                    if self.caps(id).is_some_and(|c| c.echo()) {
                        let msgs = self.chat_messages(id, line, None);
                        self.reply(id, p.label.as_deref(), msgs);
                    } else if p.label.is_some() {
                        self.reply(id, p.label.as_deref(), Vec::new());
                    }
                }
                _ => {
                    for m in self.chat_messages(id, line, None) {
                        self.send_to(id, m);
                    }
                }
            }
        }
    }

    /// A message from Teams (new or edited). `live` delivers a new one to
    /// clients; otherwise it is only stored.
    pub(crate) fn incoming_message(&mut self, m: TeamsMessage, live: bool) {
        let conv = m.conversation.clone();
        let echo = m
            .client_message_id
            .as_ref()
            .and_then(|c| self.pending_sends.remove(c));
        let old_text = self.stored_text(&conv, &m.id);
        let change = self.store.apply_message(m.clone());
        if self.session.phase != super::Phase::Ready {
            return;
        }
        let new = change == Change::Inserted && (live || echo.is_some());
        let shown = is_displayable(&m.message_type) && !m.deleted;
        if (new || echo.is_some()) && shown {
            if new {
                self.join_for_message(&m);
                if self
                    .store
                    .conversation(&m.conversation)
                    .is_some_and(|sc| sc.conversation.kind != ic3_teams::ConversationKind::Channel)
                {
                    self.follow_presence(&m.from);
                }
            }
            if let Some(line) = self.chat_line(&m) {
                if new {
                    self.deliver(&line, echo);
                } else if let Some(p) = echo {
                    // Already stored (a history page got there first): only
                    // the sender still waits for it.
                    self.deliver_echo(&line, p);
                }
                return;
            }
        }
        if let Some(p) = echo {
            self.reply(p.client, p.label.as_deref(), Vec::new());
        }
        match change {
            Change::Edited => {
                let new_text = self.stored_text(&conv, &m.id);
                if new_text.is_some() && new_text != old_text {
                    self.send_edit(&m);
                }
            }
            Change::Deleted => self.send_redact(&conv, &m.id, Some(&m.from)),
            _ => {}
        }
    }

    /// Joins the conversation a new message arrived in, when it is not
    /// joined: a hidden chat comes back (as in Teams), an unnamed one is
    /// named after the chat or the sender.
    fn join_for_message(&mut self, m: &TeamsMessage) {
        let conv = &m.conversation;
        if self.joined.contains(conv) || is_notes(conv) {
            return;
        }
        let kind = self.store.conversation(conv).map(|sc| sc.conversation.kind);
        match kind {
            Some(ConversationKind::Group) => {
                let label = self.fallback_label(m);
                self.force_join(conv, &label);
            }
            Some(ConversationKind::Channel) => {
                if let super::session::Shown::Channel(Some(label)) = self.shown(conv) {
                    self.force_join(conv, &label);
                }
            }
            _ => {}
        }
    }

    /// Sends the echo of a send to its sender only.
    fn deliver_echo(&mut self, line: &ChatLine, p: PendingSend) {
        let msgs = if self.caps(p.client).is_some_and(|c| c.echo()) {
            self.chat_messages(p.client, line, None)
        } else {
            Vec::new()
        };
        self.reply(p.client, p.label.as_deref(), msgs);
    }

    /// A label for a conversation shown only because a message arrived:
    /// its name, else the sender's.
    fn fallback_label(&self, m: &TeamsMessage) -> String {
        self.store
            .conversation_name(&m.conversation)
            .or_else(|| m.from_name.clone())
            .unwrap_or_else(|| "chat".into())
    }

    /// The rendered text of a stored message.
    fn stored_text(&self, conv: &ConversationId, id: &MessageId) -> Option<String> {
        let sm = self
            .store
            .find(conv, &ic3_teams::MessageKey::Server(id.clone()))?;
        (!sm.message.deleted).then(|| render::to_irc(&sm.message.body, &self.ctx()))
    }

    /// Tells clients a message's text changed (`draft/message-edit`).
    pub(crate) fn send_edit(&mut self, m: &TeamsMessage) {
        if let Some((msg, true)) = self.edit_message(m) {
            self.broadcast(&msg);
        }
    }

    /// The `EDIT` announcing `m`'s current text, and whether that text is
    /// new: our own edit is seen twice (the request's success and Teams'
    /// edit event, in either order), and clients get it once.
    pub(crate) fn edit_message(&mut self, m: &TeamsMessage) -> Option<(Message, bool)> {
        let line = self.chat_line(m)?;
        let text = line.lines.join("\n");
        let fresh = self.edits_sent.get(&m.id) != Some(&text);
        if fresh {
            self.edits_sent.insert(m.id.clone(), text.clone());
        }
        let msg = Message::new("EDIT", [line.target])
            .with_source(line.source)
            .with_tag("msgid", format!("{}-edit-{}", m.id, now_ms()))
            .with_tag("time", format_time(now_ms()))
            .with_tag("draft/target-msgid", m.id.0.clone())
            .with_tag("draft/edit-text", text);
        Some((msg, fresh))
    }

    /// Tells clients a message was deleted (`draft/message-redaction`).
    pub(crate) fn send_redact(&mut self, conv: &ConversationId, id: &MessageId, by: Option<&Mri>) {
        let by = by.cloned().or_else(|| {
            self.store
                .find(conv, &ic3_teams::MessageKey::Server(id.clone()))
                .map(|sm| sm.message.from.clone())
        });
        let Some(by) = by else {
            return;
        };
        let Some(target) = self.target_for(conv, &by) else {
            return;
        };
        let source = if self.store.me() == Some(&by) {
            self.self_mask()
        } else {
            Gateway::user_mask(&self.nick_of(&by, None))
        };
        let msg = Message::new("REDACT", [target, id.0.clone()])
            .with_source(source)
            .with_tag("time", format_time(now_ms()));
        self.broadcast(&msg);
    }

    pub(crate) fn incoming_delete(&mut self, conv: ConversationId, id: MessageId, version: i64) {
        let from = self
            .store
            .find(&conv, &ic3_teams::MessageKey::Server(id.clone()))
            .map(|sm| sm.message.from.clone());
        let ev = Event::MessageDeleted {
            conversation: conv.clone(),
            message: id.clone(),
            version,
        };
        if self.store.apply_event(&ev) == Change::Deleted
            && self.session.phase == super::Phase::Ready
        {
            self.send_redact(&conv, &id, from.as_ref());
        }
    }

    pub(crate) fn incoming_reactions(
        &mut self,
        conv: ConversationId,
        id: MessageId,
        reactions: Vec<ReactionSummary>,
        version: i64,
    ) {
        let before = self
            .store
            .find(&conv, &ic3_teams::MessageKey::Server(id.clone()))
            .map(|sm| sm.message.reactions.clone());
        let ev = Event::ReactionsChanged {
            conversation: conv.clone(),
            message: id.clone(),
            reactions: reactions.clone(),
            version,
        };
        let change = self.store.apply_event(&ev);
        if self.session.phase != super::Phase::Ready {
            return;
        }
        // Unknown message: nothing to diff against, so every reaction is new
        // (clients ignore a repeated one).
        if before.is_some() && change == Change::Unchanged {
            return;
        }
        let before = before.unwrap_or_default();
        let (added, removed) = diff_reactions(&before, &reactions);
        for (r, user) in added {
            self.send_reaction(&conv, &id, &r, &user, true, None);
        }
        for (r, user) in removed {
            self.send_reaction(&conv, &id, &r, &user, false, None);
        }
    }

    /// Sends a `+draft/react` / `+draft/unreact` TAGMSG from `user` to every
    /// client; the one that asked for it (`echo`) gets its label.
    pub(crate) fn send_reaction(
        &mut self,
        conv: &ConversationId,
        id: &MessageId,
        r: &Reaction,
        user: &Mri,
        add: bool,
        echo: Option<(ClientId, Option<String>)>,
    ) {
        let Some(target) = self.target_for(conv, user) else {
            return;
        };
        let source = if self.store.me() == Some(user) {
            self.self_mask()
        } else {
            Gateway::user_mask(&self.nick_of(user, None))
        };
        let key = if add {
            "+draft/react"
        } else {
            "+draft/unreact"
        };
        let msg = Message::new("TAGMSG", [target])
            .with_source(source)
            .with_tag(
                "msgid",
                format!("{id}-{}-{}", if add { "r" } else { "u" }, now_ms()),
            )
            .with_tag("time", format_time(now_ms()))
            .with_tag("+draft/reply", id.0.clone())
            .with_tag("+reply", id.0.clone())
            .with_tag(key, r.glyph());
        let ids: Vec<ClientId> = self.clients.keys().copied().collect();
        for cid in ids {
            match &echo {
                Some((eid, label)) if *eid == cid => {
                    let msgs = if self.caps(cid).is_some_and(|c| c.echo()) {
                        vec![msg.clone()]
                    } else {
                        Vec::new()
                    };
                    self.reply(cid, label.as_deref(), msgs);
                }
                _ => self.send_to(cid, msg.clone()),
            }
        }
    }

    // ----- Typing -----

    pub(crate) fn on_typing(&mut self, conv: ConversationId, from: Mri, name: Option<String>) {
        if self.store.me() == Some(&from) || self.session.phase != super::Phase::Ready {
            return;
        }
        if let Some(n) = &name
            && self.store.display_name(&from).is_none()
        {
            self.learn_name(&from, n);
        }
        self.store.apply_event(&Event::Typing {
            conversation: conv.clone(),
            from: from.clone(),
            from_name: name,
        });
        self.send_typing_tag(&conv, &from, "active");
        self.typing_shown.insert((conv, from), Instant::now());
    }

    fn send_typing_tag(&mut self, conv: &ConversationId, from: &Mri, state: &str) {
        let Some(target) = self.target_for(conv, from) else {
            return;
        };
        let nick = self.nick_of(from, None);
        let msg = Message::new("TAGMSG", [target])
            .with_source(Gateway::user_mask(&nick))
            .with_tag("+typing", state)
            .with_tag("+draft/typing", state);
        self.broadcast(&msg);
    }

    /// Re-announces users still typing and ends the ones that stopped.
    pub(super) fn typing_tick(&mut self) {
        let now = Instant::now();
        let shown: Vec<(ConversationId, Mri, Instant)> = self
            .typing_shown
            .iter()
            .map(|((c, m), t)| (c.clone(), m.clone(), *t))
            .collect();
        for (conv, mri, last) in shown {
            let still = self
                .store
                .typing(&conv, now_ms())
                .iter()
                .any(|(m, _)| *m == mri);
            if !still {
                self.typing_shown.remove(&(conv.clone(), mri.clone()));
                self.send_typing_tag(&conv, &mri, "done");
            } else if now.duration_since(last) >= TYPING_REPEAT {
                self.typing_shown.insert((conv.clone(), mri.clone()), now);
                self.send_typing_tag(&conv, &mri, "active");
            }
        }
    }

    // ----- Read markers -----

    /// The read marker of `conv` as a `timestamp=` value (`*` when unknown).
    pub(crate) fn read_marker_value(&self, conv: &ConversationId) -> String {
        self.store
            .conversation(conv)
            .and_then(|sc| sc.conversation.read_horizon.as_ref())
            .and_then(|h| h.message_id.as_i64())
            .map_or_else(|| "*".to_string(), timestamp_ref)
    }

    /// Sends `MARKREAD` for `conv` to client `only`, or to everyone.
    pub(crate) fn send_read_marker(&mut self, conv: &ConversationId, only: Option<ClientId>) {
        let Some(target) = self.query_or_channel(conv) else {
            return;
        };
        let msg = Message::new("MARKREAD", [target, self.read_marker_value(conv)])
            .with_source(self.cfg.server_name.clone());
        match only {
            Some(id) => self.send_to(id, msg),
            None => self.broadcast(&msg),
        }
    }

    /// The IRC name of a conversation from our side: its channel, the 1:1
    /// peer's nick, or `Notes`.
    pub(crate) fn query_or_channel(&mut self, conv: &ConversationId) -> Option<String> {
        if is_notes(conv) {
            return Some(NOTES_NICK.into());
        }
        if self.joined.contains(conv) {
            return self.dir.existing_channel(conv).map(str::to_owned);
        }
        let peer = self.store.peer_of(conv)?;
        Some(self.nick_of(&peer, None))
    }

    // ----- Channels -----

    /// The members' nicks of a joined conversation (us first).
    pub(crate) fn member_nicks(&mut self, conv: &ConversationId) -> Vec<String> {
        let members: Vec<(Mri, Option<String>)> = self
            .store
            .conversation(conv)
            .map(|sc| {
                sc.thread_info
                    .as_ref()
                    .map(|t| &t.members)
                    .filter(|m| !m.is_empty())
                    .or(sc.conversation.members.as_ref())
                    .map(|ms| {
                        ms.iter()
                            .map(|m| (m.mri.clone(), m.friendly_name.clone()))
                            .collect()
                    })
                    .unwrap_or_default()
            })
            .unwrap_or_default();
        let mut nicks = BTreeSet::new();
        for (mri, name) in members {
            if self.store.me() == Some(&mri) {
                continue;
            }
            nicks.insert(self.nick_of(&mri, name.as_deref()));
        }
        let mut out = vec![self.nick.clone()];
        out.extend(nicks);
        out
    }

    /// The channel's topic: the Teams name of the chat.
    pub(crate) fn topic_of(&self, conv: &ConversationId) -> Option<String> {
        if let super::session::Shown::Channel(Some(label)) = self.shown(conv)
            && self
                .store
                .conversation(conv)
                .is_some_and(|sc| sc.conversation.kind == ConversationKind::Channel)
        {
            return Some(label);
        }
        self.store.conversation_name(conv)
    }

    /// JOIN, topic, names and read marker of `conv` for client `id`.
    pub(crate) fn join_burst(&mut self, id: ClientId, conv: &ConversationId) {
        let Some(ch) = self.dir.existing_channel(conv).map(str::to_owned) else {
            return;
        };
        let join = Message::new("JOIN", [ch.clone()]).with_source(self.self_mask());
        self.send_to(id, join);
        if let Some(topic) = self.topic_of(conv) {
            let m = self.numeric("332", [ch.clone(), topic]);
            self.send_to(id, m);
        }
        for m in self.names_reply(id, conv, &ch) {
            self.send_to(id, m);
        }
        self.away_burst(id, conv);
        self.send_read_marker(conv, Some(id));
    }

    /// `353` lines and the closing `366` for a channel.
    pub(crate) fn names_reply(
        &mut self,
        id: ClientId,
        conv: &ConversationId,
        ch: &str,
    ) -> Vec<Message> {
        let full = self.caps(id).is_some_and(|c| c.userhost_in_names());
        let nicks: Vec<String> = self
            .member_nicks(conv)
            .into_iter()
            .map(|n| if full { Gateway::user_mask(&n) } else { n })
            .collect();
        let mut out = Vec::new();
        for chunk in nicks.chunks(40) {
            out.push(self.numeric("353", ["=".into(), ch.to_string(), chunk.join(" ")]));
        }
        out.push(self.numeric("366", [ch.to_string(), "End of /NAMES list.".into()]));
        out
    }

    pub(crate) fn send_topic_change(&mut self, conv: &ConversationId) {
        let (Some(ch), Some(topic)) = (
            self.dir.existing_channel(conv).map(str::to_owned),
            self.topic_of(conv),
        ) else {
            return;
        };
        let m = Message::new("TOPIC", [ch, topic]).with_source(self.cfg.server_name.clone());
        self.broadcast(&m);
    }

    /// A roster changed: JOIN / PART the difference for clients.
    pub(crate) fn roster_changed(&mut self, _conv: &ConversationId) {
        // Clients ask for NAMES when they need it; membership churn is not
        // replayed as JOIN/PART noise.
    }

    // ----- History -----

    /// The lines of a history batch for `conv`: messages oldest first, each
    /// followed (with `event-playback`) by its reactions.
    pub(crate) fn history_messages(
        &mut self,
        id: ClientId,
        batch: &str,
        messages: &[TeamsMessage],
    ) -> Vec<Message> {
        let playback = self
            .caps(id)
            .is_some_and(|c| c.event_playback() && c.tags());
        let mut out = Vec::new();
        for m in messages {
            let Some(line) = self.chat_line(m) else {
                continue;
            };
            out.extend(self.chat_messages(id, &line, Some(batch)));
            if !playback {
                continue;
            }
            for r in &m.reactions {
                for user in &r.users {
                    let source = if self.store.me() == Some(user) {
                        self.self_mask()
                    } else {
                        Gateway::user_mask(&self.nick_of(user, None))
                    };
                    let msg = Message::new("TAGMSG", [line.target.clone()])
                        .with_source(source)
                        .with_tag("batch", batch.to_string())
                        .with_tag("msgid", format!("{}-r-{}", m.id, out.len()))
                        .with_tag("time", format_time(m.arrived_at))
                        .with_tag("+draft/reply", m.id.0.clone())
                        .with_tag("+reply", m.id.0.clone())
                        .with_tag("+draft/react", r.reaction.glyph());
                    out.push(msg);
                }
            }
        }
        out
    }

    /// The newest stored message in `conv` that arrived at or before `at`.
    pub(crate) fn message_at_or_before(
        &self,
        conv: &ConversationId,
        at: Timestamp,
    ) -> Option<TeamsMessage> {
        self.store
            .messages(conv)
            .iter()
            .rev()
            .filter(|sm| matches!(sm.key, ic3_teams::MessageKey::Server(_)))
            .find(|sm| sm.message.arrived_at <= at)
            .map(|sm| sm.message.clone())
    }
}

/// Splits `text` into pieces of at most `max` bytes, after a space when
/// there is one (the space stays at the end of the piece, so joining the
/// pieces gives `text` back), else at a character boundary.
fn split_text(text: &str, max: usize) -> Vec<&str> {
    let mut out = Vec::new();
    let mut rest = text;
    while rest.len() > max {
        let mut cut = max;
        while !rest.is_char_boundary(cut) {
            cut -= 1;
        }
        if let Some(sp) = rest[..cut].rfind(' ').filter(|&i| i > 0) {
            cut = sp + 1;
        }
        out.push(&rest[..cut]);
        rest = &rest[cut..];
    }
    out.push(rest);
    out
}

/// The line for an attachment: an image's link alone (clients show it
/// inline), a file's name and link.
fn attachment_line(a: &Attachment, media: &MediaTable) -> Option<String> {
    let url = a.url.as_deref()?;
    let link = media.link_for(url, Some(&a.name));
    Some(match a.kind {
        AttachmentKind::Image => link,
        AttachmentKind::File | AttachmentKind::Other => format!("📎 {} {link}", a.name),
    })
}

/// Reactions as (reaction, user) pairs.
type ReactionPairs = Vec<(Reaction, Mri)>;

/// Reactions added and removed between two sets, as (reaction, user).
pub(crate) fn diff_reactions(
    before: &[ReactionSummary],
    after: &[ReactionSummary],
) -> (ReactionPairs, ReactionPairs) {
    let pairs = |v: &[ReactionSummary]| -> BTreeSet<(String, Mri)> {
        v.iter()
            .flat_map(|r| {
                r.users
                    .iter()
                    .map(move |u| (r.reaction.key().to_string(), u.clone()))
            })
            .collect()
    };
    let (b, a) = (pairs(before), pairs(after));
    let to_vec = |s: Vec<&(String, Mri)>| {
        s.into_iter()
            .map(|(k, u)| (Reaction::from_key(k), u.clone()))
            .collect::<Vec<_>>()
    };
    (
        to_vec(a.difference(&b).collect()),
        to_vec(b.difference(&a).collect()),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mri(s: &str) -> Mri {
        Mri(s.into())
    }

    #[test]
    fn long_lines_split_at_spaces_and_rejoin() {
        let text = "word ".repeat(200);
        let pieces = split_text(&text, 400);
        assert!(pieces.iter().all(|p| p.len() <= 400), "{pieces:?}");
        assert!(pieces[0].ends_with(' '));
        assert_eq!(pieces.concat(), text);
        let cjk = "漢".repeat(300);
        let pieces = split_text(&cjk, 400);
        assert!(pieces.iter().all(|p| p.len() <= 400));
        assert_eq!(pieces.concat(), cjk);
        assert_eq!(split_text("short", 400), vec!["short"]);
    }

    #[test]
    fn reaction_diff() {
        let before = vec![ReactionSummary {
            reaction: Reaction::Like,
            users: vec![mri("a"), mri("b")],
        }];
        let after = vec![
            ReactionSummary {
                reaction: Reaction::Like,
                users: vec![mri("a")],
            },
            ReactionSummary {
                reaction: Reaction::Heart,
                users: vec![mri("c")],
            },
        ];
        let (added, removed) = diff_reactions(&before, &after);
        assert_eq!(added, vec![(Reaction::Heart, mri("c"))]);
        assert_eq!(removed, vec![(Reaction::Like, mri("b"))]);
    }
}
