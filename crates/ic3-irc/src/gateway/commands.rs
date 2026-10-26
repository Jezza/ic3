//! IRC → Teams: the commands a registered client sends.

use std::time::Duration;

use ic3_teams::{
    Block, ConversationId, ConversationKind, Message as TeamsMessage, MessageId, MessageKey,
    MessageRef, Mri, Outgoing, Reaction, ReplyTarget, Timestamp, is_notes, now_ms,
};
use tokio::time::Instant;

use super::deliver::PendingSend;
use super::history::{HistoryJob, Query};
use super::session::Phase;
use super::{ClientId, Gateway, InboundBatch, OpFn};
use crate::caps::{CHATHISTORY_MAX, MULTILINE_MAX_BYTES, MULTILINE_MAX_LINES};
use crate::irc::{Message, casefold, format_time, parse_time};
use crate::media::Upload;
use crate::names::{NOTES_NICK, SERVICE_NICK};
use crate::render;

/// How long a sent message waits for its echo before the latest page is
/// fetched to find it.
const ECHO_TIMEOUT: Duration = Duration::from_secs(5);
/// Excerpt length of a reply quote.
const REPLY_EXCERPT: usize = 120;

/// What `/part` hides in Teams.
#[derive(Debug)]
enum HideWhat {
    Chat,
    Channel(ic3_teams::Channel),
}

/// What an IRC target names.
#[derive(Clone, Debug, PartialEq)]
pub(crate) enum Target {
    Conversation(ConversationId),
    Service,
    /// A known person with no 1:1 chat yet.
    NoChat(String),
    Unknown,
}

/// A `CHATHISTORY` message reference, resolved to a time.
#[derive(Clone, Copy, Debug, PartialEq)]
enum Ref {
    Star,
    At(Timestamp),
}

impl Gateway {
    pub(super) fn handle_command(&mut self, id: ClientId, msg: Message) {
        let label = msg.tag("label").map(str::to_owned);
        // Lines of a multiline batch are collected, not run.
        if let Some(b) = msg.tag("batch").map(str::to_owned) {
            self.batch_line(id, &b, msg);
            return;
        }
        let ready = self.session.phase == Phase::Ready;
        match msg.command.as_str() {
            "PRIVMSG" | "NOTICE" => {
                let (target, text) = (msg.param(0).to_string(), msg.param(1).to_string());
                if target.is_empty() || text.is_empty() {
                    let m = self.numeric("411", ["No recipient or text given".to_string()]);
                    return self.reply(id, label.as_deref(), vec![m]);
                }
                let text = match ctcp_action(&text) {
                    Some(a) => format!("_{a}_"),
                    None if text.starts_with('\x01') => return,
                    None => text,
                };
                self.send_text(id, label, &target, text, &msg);
            }
            "BATCH" => self.batch_control(id, msg),
            "TAGMSG" => self.tagmsg(id, label, &msg),
            "EDIT" if ready => self.edit(id, label, &msg),
            "REDACT" if ready => self.redact(id, label, &msg),
            "MARKREAD" => self.markread(id, label, &msg),
            "CHATHISTORY" => self.chathistory(id, label, &msg),
            "JOIN" => self.join(id, label, &msg),
            "PART" => self.part(id, label, &msg),
            "NAMES" => self.names(id, label, &msg),
            "TOPIC" => self.topic(id, label, &msg),
            "WHO" => self.who(id, label, &msg),
            "WHOIS" => self.whois(id, label, &msg),
            "MONITOR" => self.monitor(id, label, &msg),
            "LIST" => self.list(id, label),
            "MODE" => self.mode(id, label, &msg),
            "AWAY" => {
                let m = if msg.param(0).is_empty() {
                    self.numeric(
                        "305",
                        ["You are no longer marked as being away".to_string()],
                    )
                } else {
                    self.numeric("306", ["You have been marked as being away".to_string()])
                };
                self.reply(id, label.as_deref(), vec![m]);
            }
            "NICK" => self.nick_change(id, label, &msg),
            // `/cutoff 30d`: clients send unknown commands as they are.
            "CUTOFF" => {
                self.reply(id, label.as_deref(), Vec::new());
                let arg = msg.params.join(" ");
                self.cutoff_command(id, &arg);
            }
            "METADATA" => self.metadata(id, label, &msg),
            "USERHOST" => {
                let masks: Vec<String> = msg
                    .params
                    .iter()
                    .filter(|n| {
                        self.target(n) != Target::Unknown || casefold(n) == casefold(&self.nick)
                    })
                    .map(|n| format!("{n}=+{n}@{}", crate::names::USER_HOST))
                    .collect();
                let m = self.numeric("302", [masks.join(" ")]);
                self.reply(id, label.as_deref(), vec![m]);
            }
            "EDIT" | "REDACT" => {
                let f = self.fail(
                    &msg.command,
                    "TEMPORARILY_UNAVAILABLE",
                    &[],
                    "Not connected to Teams yet",
                );
                self.reply(id, label.as_deref(), vec![f]);
            }
            other => {
                let m = self.numeric("421", [other.to_string(), "Unknown command".into()]);
                self.reply(id, label.as_deref(), vec![m]);
            }
        }
    }

    /// What an IRC target names.
    pub(crate) fn target(&self, name: &str) -> Target {
        if casefold(name) == casefold(SERVICE_NICK) {
            return Target::Service;
        }
        if casefold(name) == casefold(NOTES_NICK) {
            return Target::Conversation(ConversationId("48:notes".into()));
        }
        if name.starts_with('#') {
            return self
                .dir
                .conversation_by_channel(name)
                .cloned()
                .map_or(Target::Unknown, Target::Conversation);
        }
        let Some(mri) = self.dir.user_by_nick(name) else {
            return Target::Unknown;
        };
        self.one_to_one_with(mri)
            .map_or_else(|| Target::NoChat(name.to_string()), Target::Conversation)
    }

    /// The 1:1 chat with `mri`, the most recently active if several.
    fn one_to_one_with(&self, mri: &Mri) -> Option<ConversationId> {
        self.store
            .conversations()
            .filter(|sc| sc.conversation.kind == ConversationKind::OneToOne)
            .filter(|sc| self.store.peer_of(&sc.conversation.id).as_ref() == Some(mri))
            .max_by_key(|sc| self.store.last_activity(&sc.conversation.id))
            .map(|sc| sc.conversation.id.clone())
    }

    /// The conversation `name` names, or a reply explaining why not.
    fn conversation_or_fail(&self, command: &str, name: &str) -> Result<ConversationId, Message> {
        match self.target(name) {
            Target::Conversation(c) => Ok(c),
            Target::NoChat(n) => Err(self.fail(
                command,
                "INVALID_TARGET",
                &[name],
                &format!("You have no chat with {n} yet; start one in Teams"),
            )),
            Target::Service | Target::Unknown => {
                Err(self.numeric("401", [name.to_string(), "No such nick/channel".into()]))
            }
        }
    }

    // ----- Sending -----

    /// Sends a message typed by client `id` to `target`. `msg` carries the
    /// client tags (`+reply`).
    fn send_text(
        &mut self,
        id: ClientId,
        label: Option<String>,
        target: &str,
        text: String,
        msg: &Message,
    ) {
        let conv = match self.target(target) {
            Target::Service => return self.service_command(id, label, &text),
            _ if self.session.phase != Phase::Ready => {
                let f = self.fail(
                    "PRIVMSG",
                    "TEMPORARILY_UNAVAILABLE",
                    &[target],
                    "Not connected to Teams yet",
                );
                return self.reply(id, label.as_deref(), vec![f]);
            }
            _ => match self.conversation_or_fail("PRIVMSG", target) {
                Ok(c) => c,
                Err(m) => return self.reply(id, label.as_deref(), vec![m]),
            },
        };
        let (text, uploads) = self.take_uploads(&text);
        let body = render::from_irc(&text, &self.deliver_ctx());
        if body.is_empty() && uploads.is_empty() {
            return self.reply(id, label.as_deref(), Vec::new());
        }
        let mut out = Outgoing {
            body,
            client_message_id: ic3_teams::api::new_client_message_id(),
            ..Outgoing::default()
        };
        if let Some(reply_id) = msg.tag_any(&["+reply", "+draft/reply"]) {
            self.attach_reply(&conv, &MessageId(reply_id.to_string()), &mut out);
        }
        self.pending_sends.insert(
            out.client_message_id.clone(),
            PendingSend {
                client: id,
                label: label.clone(),
            },
        );
        let cmid = out.client_message_id.clone();
        let target = target.to_string();
        self.spawn(|b| async move {
            let r = async {
                for u in uploads {
                    if u.content_type.starts_with("image/") {
                        let img = b
                            .upload_image(&conv, &u.name, &u.content_type, u.bytes)
                            .await?;
                        out.body.0.push(Block::Paragraph(vec![img]));
                    } else {
                        out.files
                            .push(b.upload_file(&conv, &u.name, u.bytes).await?);
                    }
                }
                b.send(&conv, &out).await
            }
            .await;
            Box::new(move |gw: &mut Gateway| gw.on_sent(conv, cmid, target, r)) as OpFn
        });
    }

    /// Takes links to files the client uploaded (`soju.im/FILEHOST`) out of
    /// `text`: they are sent to Teams as images and attachments instead.
    fn take_uploads(&self, text: &str) -> (String, Vec<Upload>) {
        let mut uploads = Vec::new();
        let kept: Vec<String> = text
            .split('\n')
            .map(|line| {
                line.split(' ')
                    .filter(|word| match self.media.upload_for_link(word) {
                        Some((_, u)) => {
                            uploads.push(u);
                            false
                        }
                        None => true,
                    })
                    .collect::<Vec<_>>()
                    .join(" ")
            })
            .collect();
        (kept.join("\n"), uploads)
    }

    /// A rendering context for parsing what clients type.
    fn deliver_ctx(&self) -> super::deliver::Ctx<'_> {
        super::deliver::Ctx {
            dir: &self.dir,
            store: &self.store,
            media: &self.media,
            nick: &self.nick,
        }
    }

    /// Turns `+reply` into a quote (chats) or a thread reply (channels).
    fn attach_reply(&self, conv: &ConversationId, reply_id: &MessageId, out: &mut Outgoing) {
        let sm = self.store.find(conv, &MessageKey::Server(reply_id.clone()));
        let channel = self
            .store
            .conversation(conv)
            .is_some_and(|sc| sc.conversation.kind == ConversationKind::Channel);
        if channel {
            let root = sm
                .and_then(|sm| sm.message.thread_root.clone())
                .unwrap_or_else(|| reply_id.clone());
            out.thread_root = Some(root);
            return;
        }
        let Some(sm) = sm else {
            tracing::debug!(message = %reply_id, "reply target not loaded; sending without a quote");
            return;
        };
        let m = &sm.message;
        out.reply_to = Some(ReplyTarget {
            message: MessageRef {
                id: m.id.clone(),
                client_message_id: m.client_message_id.clone(),
            },
            author: m.from.clone(),
            author_name: self
                .store
                .display_name(&m.from)
                .map(str::to_owned)
                .or_else(|| m.from_name.clone())
                .unwrap_or_default(),
            excerpt: render::excerpt(&m.body, REPLY_EXCERPT),
            sent_at: m.arrived_at,
        });
    }

    fn on_sent(
        &mut self,
        conv: ConversationId,
        cmid: String,
        target: String,
        r: ic3_teams::Result<ic3_teams::SendReceipt>,
    ) {
        match r {
            Err(e) => {
                let Some(p) = self.pending_sends.remove(&cmid) else {
                    return;
                };
                if matches!(e, ic3_teams::Error::ReauthRequired) {
                    self.reauth_needed();
                }
                let f = self.fail(
                    "PRIVMSG",
                    "CANNOT_SEND",
                    &[&target],
                    &format!("Teams refused the message: {e}"),
                );
                self.reply(p.client, p.label.as_deref(), vec![f]);
            }
            Ok(_) => {
                if !self.pending_sends.contains_key(&cmid) {
                    return; // the echo came first
                }
                self.after(ECHO_TIMEOUT, move |gw| gw.echo_overdue(conv, cmid));
            }
        }
    }

    /// The echo of a send never came: look for it in the latest page.
    fn echo_overdue(&mut self, conv: ConversationId, cmid: String) {
        if !self.pending_sends.contains_key(&cmid) {
            return;
        }
        let page_size = self.cfg.page_size;
        let c = conv.clone();
        self.spawn(|b| async move {
            let r = b.messages(&c, None, page_size).await;
            Box::new(move |gw: &mut Gateway| {
                let found = r.ok().and_then(|p| {
                    p.items
                        .into_iter()
                        .find(|m| m.client_message_id.as_deref() == Some(cmid.as_str()))
                });
                match found {
                    Some(m) => gw.incoming_message(m, true),
                    None => {
                        if let Some(p) = gw.pending_sends.remove(&cmid) {
                            tracing::warn!(conversation = %conv, "sent message never came back");
                            gw.reply(p.client, p.label.as_deref(), Vec::new());
                        }
                    }
                }
            }) as OpFn
        });
    }

    /// The session needs a new sign-in (a request was refused for good).
    pub(crate) fn reauth_needed(&mut self) {
        if self.session.phase == Phase::Ready || self.session.phase == Phase::Syncing {
            self.begin_login();
        }
    }

    // ----- Multiline batches -----

    fn batch_control(&mut self, id: ClientId, msg: Message) {
        let reference = msg.param(0);
        if let Some(r) = reference.strip_prefix('+') {
            let kind = msg.param(1);
            if kind != "draft/multiline" && kind != "multiline" {
                let f = self.fail(
                    "BATCH",
                    "INVALID_BATCH",
                    &[kind],
                    "Only multiline batches are accepted",
                );
                return self.send_to(id, f);
            }
            let b = InboundBatch {
                reference: r.to_string(),
                target: msg.param(2).to_string(),
                tags: msg.tags.clone(),
                text: String::new(),
                lines: 0,
            };
            if let Some(c) = self.clients.get_mut(&id) {
                c.inbound_batch = Some(b);
            }
        } else if let Some(r) = reference.strip_prefix('-') {
            let Some(b) = self
                .clients
                .get_mut(&id)
                .and_then(|c| c.inbound_batch.take_if(|b| b.reference == r))
            else {
                return;
            };
            let mut carrier = Message::new("PRIVMSG", [b.target.clone(), b.text.clone()]);
            carrier.tags = b.tags;
            let label = carrier.tag("label").map(str::to_owned);
            if b.text.trim().is_empty() {
                return self.reply(id, label.as_deref(), Vec::new());
            }
            self.send_text(id, label, &b.target, b.text, &carrier);
        }
    }

    /// A line inside a client's batch.
    fn batch_line(&mut self, id: ClientId, reference: &str, msg: Message) {
        let mut too_long = false;
        if let Some(c) = self.clients.get_mut(&id)
            && let Some(b) = c
                .inbound_batch
                .as_mut()
                .filter(|b| b.reference == reference)
        {
            if msg.command == "PRIVMSG" || msg.command == "NOTICE" {
                let concat = msg
                    .tag_any(&["draft/multiline-concat", "multiline-concat"])
                    .is_some();
                if b.lines > 0 && !concat {
                    b.text.push('\n');
                }
                b.text.push_str(msg.param(1));
                b.lines += 1;
                too_long = b.text.len() > MULTILINE_MAX_BYTES || b.lines > MULTILINE_MAX_LINES;
            }
            if too_long {
                let target = b.target.clone();
                c.inbound_batch = None;
                let f = self.fail(
                    "BATCH",
                    "MULTILINE_MAX_BYTES",
                    &[&target],
                    "Message too long",
                );
                self.send_to(id, f);
            }
        }
    }

    // ----- TAGMSG: typing and reactions -----

    fn tagmsg(&mut self, id: ClientId, label: Option<String>, msg: &Message) {
        let target = msg.param(0).to_string();
        let Target::Conversation(conv) = self.target(&target) else {
            return self.reply(id, label.as_deref(), Vec::new());
        };
        if self.session.phase != Phase::Ready {
            return self.reply(id, label.as_deref(), Vec::new());
        }
        if msg.tag_any(&["+typing", "+draft/typing"]) == Some("active") {
            self.typing(&conv);
        }
        let reply_id = msg
            .tag_any(&["+reply", "+draft/reply"])
            .map(|s| MessageId(s.to_string()));
        let react = msg
            .tag_any(&["+draft/react", "+react"])
            .map(|e| (e.to_string(), true));
        let unreact = msg
            .tag_any(&["+draft/unreact", "+unreact"])
            .map(|e| (e.to_string(), false));
        let Some(((emoji, add), msg_id)) = react.or(unreact).zip(reply_id) else {
            return self.reply(id, label.as_deref(), Vec::new());
        };
        let Some(reaction) = Reaction::from_emoji(&emoji) else {
            let f = self.fail(
                "TAGMSG",
                "INVALID_REACTION",
                &[&target],
                "Teams has no reaction for that emoji",
            );
            return self.reply(id, label.as_deref(), vec![f]);
        };
        let previous = self
            .store
            .set_reaction_optimistic(&conv, &msg_id, &reaction, add);
        if previous.is_none()
            && self
                .store
                .find(&conv, &MessageKey::Server(msg_id.clone()))
                .is_some()
        {
            // Already in that state.
            return self.reply(id, label.as_deref(), Vec::new());
        }
        self.spawn(|b| async move {
            let r = b.react(&conv, &msg_id, &reaction, add).await;
            Box::new(move |gw: &mut Gateway| match r {
                Ok(()) => {
                    let Some(me) = gw.store.me().cloned() else {
                        return;
                    };
                    gw.send_reaction(&conv, &msg_id, &reaction, &me, add, Some((id, label)));
                }
                Err(e) => {
                    if let Some(previous) = previous {
                        gw.store.revert_reactions(&conv, &msg_id, previous);
                    }
                    let f = gw.fail(
                        "TAGMSG",
                        "CANNOT_REACT",
                        &[&target],
                        &format!("Teams refused the reaction: {e}"),
                    );
                    gw.reply(id, label.as_deref(), vec![f]);
                }
            }) as OpFn
        });
    }

    /// Tells Teams we are typing, at most once per its refresh interval.
    fn typing(&mut self, conv: &ConversationId) {
        let now = Instant::now();
        if self
            .typing_sent
            .get(conv)
            .is_some_and(|t| now.duration_since(*t) < ic3_teams::TYPING_REFRESH)
        {
            return;
        }
        self.typing_sent.insert(conv.clone(), now);
        let conv = conv.clone();
        self.spawn(|b| async move {
            if let Err(e) = b.send_typing(&conv).await {
                tracing::debug!(error = %e, "typing signal failed");
            }
            Box::new(|_: &mut Gateway| {}) as OpFn
        });
    }

    // ----- Edits and deletes -----

    /// Our own stored message `msgid` in the conversation `target` names.
    fn own_message(
        &self,
        command: &str,
        target: &str,
        msgid: &str,
    ) -> Result<(ConversationId, TeamsMessage), Message> {
        let conv = self.conversation_or_fail(command, target)?;
        let sm = self
            .store
            .find(&conv, &MessageKey::Server(MessageId(msgid.to_string())))
            .ok_or_else(|| {
                self.fail(
                    command,
                    "UNKNOWN_MSGID",
                    &[target, msgid],
                    "No such message",
                )
            })?;
        if self.store.me() != Some(&sm.message.from) {
            return Err(self.fail(
                command,
                "MESSAGE_NOT_OWN",
                &[target, msgid],
                "You can only change your own messages",
            ));
        }
        if sm.message.deleted {
            return Err(self.fail(
                command,
                "UNKNOWN_MSGID",
                &[target, msgid],
                "That message was deleted",
            ));
        }
        Ok((conv, sm.message.clone()))
    }

    fn edit(&mut self, id: ClientId, label: Option<String>, msg: &Message) {
        let target = msg.param(0).to_string();
        let msgid = msg
            .tag_any(&["draft/target-msgid", "target-msgid"])
            .unwrap_or("")
            .to_string();
        let text = msg
            .tag_any(&["draft/edit-text", "edit-text"])
            .unwrap_or("")
            .to_string();
        let (conv, original) = match self.own_message("EDIT", &target, &msgid) {
            Ok(x) => x,
            Err(f) => return self.reply(id, label.as_deref(), vec![f]),
        };
        let text = ctcp_action(&text).map_or(text.clone(), |a| format!("_{a}_"));
        let body = render::from_irc(&text, &self.deliver_ctx());
        if body.is_empty() {
            let f = self.fail(
                "EDIT",
                "INVALID_TEXT",
                &[&target, &msgid],
                "Use REDACT to delete a message",
            );
            return self.reply(id, label.as_deref(), vec![f]);
        }
        self.spawn(|b| async move {
            let r = b.edit(&original, &body).await;
            Box::new(move |gw: &mut Gateway| match r {
                Ok(()) => {
                    gw.store.apply_local_edit(&conv, &original.id, body);
                    let edited = gw
                        .store
                        .find(&conv, &MessageKey::Server(original.id.clone()))
                        .map(|sm| sm.message.clone());
                    if let Some(m) = edited {
                        gw.send_edit_echo(&m, Some((id, label)));
                    }
                }
                Err(e) => {
                    let f = gw.fail(
                        "EDIT",
                        "CANNOT_EDIT",
                        &[&target, &original.id.0],
                        &format!("Teams refused the edit: {e}"),
                    );
                    gw.reply(id, label.as_deref(), vec![f]);
                }
            }) as OpFn
        });
    }

    /// [`send_edit`](Gateway::send_edit), with the client that asked
    /// getting it under its label.
    fn send_edit_echo(&mut self, m: &TeamsMessage, echo: Option<(ClientId, Option<String>)>) {
        let Some((cid, label)) = echo else {
            return self.send_edit(m);
        };
        let others: Vec<ClientId> = self.clients.keys().copied().filter(|c| *c != cid).collect();
        let Some((msg, true)) = self.edit_message(m) else {
            // Teams' edit event got there first: everyone has it already.
            return self.reply(cid, label.as_deref(), Vec::new());
        };
        for o in others {
            self.send_to(o, msg.clone());
        }
        let mine = if self.caps(cid).is_some_and(|c| c.echo()) {
            vec![msg]
        } else {
            Vec::new()
        };
        self.reply(cid, label.as_deref(), mine);
    }

    fn redact(&mut self, id: ClientId, label: Option<String>, msg: &Message) {
        let (target, msgid) = (msg.param(0).to_string(), msg.param(1).to_string());
        let (conv, original) = match self.own_message("REDACT", &target, &msgid) {
            Ok(x) => x,
            Err(f) => return self.reply(id, label.as_deref(), vec![f]),
        };
        self.spawn(|b| async move {
            let r = b.delete(&original).await;
            Box::new(move |gw: &mut Gateway| match r {
                Ok(()) => {
                    gw.store.apply_local_delete(&conv, &original.id);
                    let others: Vec<ClientId> =
                        gw.clients.keys().copied().filter(|c| *c != id).collect();
                    let m = Message::new("REDACT", [target, original.id.0.clone()])
                        .with_source(gw.self_mask())
                        .with_tag("time", format_time(now_ms()));
                    for o in others {
                        gw.send_to(o, m.clone());
                    }
                    let mine = if gw.caps(id).is_some_and(|c| c.echo()) {
                        vec![m]
                    } else {
                        Vec::new()
                    };
                    gw.reply(id, label.as_deref(), mine);
                }
                Err(e) => {
                    let f = gw.fail(
                        "REDACT",
                        "REDACT_FORBIDDEN",
                        &[&target, &original.id.0],
                        &format!("Teams refused the delete: {e}"),
                    );
                    gw.reply(id, label.as_deref(), vec![f]);
                }
            }) as OpFn
        });
    }

    // ----- Read markers -----

    fn markread(&mut self, id: ClientId, label: Option<String>, msg: &Message) {
        let target = msg.param(0).to_string();
        let conv = match self.target(&target) {
            Target::Conversation(c) => c,
            // `Server`, or a buffer left from an earlier run whose name is
            // gone: nothing to mark, and no error to show for it.
            other => {
                if other != Target::Service {
                    tracing::debug!(%target, "MARKREAD for an unknown target");
                }
                let m = Message::new("MARKREAD", [target, "*".to_string()])
                    .with_source(self.cfg.server_name.clone());
                return self.reply(id, label.as_deref(), vec![m]);
            }
        };
        let current = |gw: &Gateway| {
            Message::new("MARKREAD", [target.clone(), gw.read_marker_value(&conv)])
                .with_source(gw.cfg.server_name.clone())
        };
        let Some(ts) = msg.params.get(1) else {
            let m = current(self);
            return self.reply(id, label.as_deref(), vec![m]);
        };
        let Some(at) = ts.strip_prefix("timestamp=").and_then(parse_time) else {
            let f = self.fail(
                "MARKREAD",
                "INVALID_PARAMS",
                &[&target],
                "Invalid timestamp",
            );
            return self.reply(id, label.as_deref(), vec![f]);
        };
        let read = self
            .store
            .conversation(&conv)
            .and_then(|sc| sc.conversation.read_horizon.as_ref())
            .and_then(|h| h.message_id.as_i64());
        let Some(m) = self
            .message_at_or_before(&conv, at)
            .filter(|m| read.is_none_or(|r| m.arrived_at > r))
        else {
            let m = current(self);
            return self.reply(id, label.as_deref(), vec![m]);
        };
        let r = MessageRef {
            id: m.id.clone(),
            client_message_id: m.client_message_id.clone(),
        };
        self.store.advance_horizon_local(&conv, &r);
        let marker = current(self);
        let others: Vec<ClientId> = self.clients.keys().copied().filter(|c| *c != id).collect();
        for o in others {
            self.send_to(o, marker.clone());
        }
        self.reply(id, label.as_deref(), vec![marker]);
        self.spawn(|b| async move {
            let res = b.mark_read(&conv, &r).await;
            Box::new(move |_: &mut Gateway| {
                if let Err(e) = res {
                    tracing::warn!(conversation = %conv, error = %e, "mark read failed");
                }
            }) as OpFn
        });
    }

    // ----- History -----

    fn chathistory(&mut self, id: ClientId, label: Option<String>, msg: &Message) {
        let sub = msg.param(0).to_ascii_uppercase();
        if sub == "TARGETS" {
            return self.chathistory_targets(id, label, msg);
        }
        let target = msg.param(1).to_string();
        let limit = msg
            .params
            .last()
            .and_then(|l| l.parse::<usize>().ok())
            .unwrap_or(50)
            .clamp(1, CHATHISTORY_MAX);
        let conv = match self.target(&target) {
            Target::Conversation(c) => c,
            // `Server`, or a buffer left from an earlier run whose name is
            // gone: no history, quietly.
            other => {
                if other != Target::Service {
                    tracing::debug!(%target, "CHATHISTORY for an unknown target");
                }
                let r = self.batch_ref(id);
                let srv = self.cfg.server_name.clone();
                let out = vec![
                    Message::new("BATCH", [format!("+{r}"), "chathistory".into(), target])
                        .with_source(srv.clone()),
                    Message::new("BATCH", [format!("-{r}")]).with_source(srv),
                ];
                return self.reply(id, label.as_deref(), out);
            }
        };
        let r1 = self.resolve_ref(&conv, msg.param(2));
        let r2 = self.resolve_ref(&conv, msg.param(3));
        let query = match (sub.as_str(), r1, r2) {
            ("LATEST", Some(Ref::Star), _) => Query::Latest(None),
            ("LATEST", Some(Ref::At(t)), _) => Query::Latest(Some(t)),
            ("BEFORE", Some(Ref::At(t)), _) => Query::Before(t),
            ("AFTER", Some(Ref::At(t)), _) => Query::After(t),
            ("AROUND", Some(Ref::At(t)), _) => Query::Around(t),
            ("BETWEEN", Some(Ref::At(a)), Some(Ref::At(b))) => Query::Between(a, b),
            _ => {
                let f = self.fail(
                    "CHATHISTORY",
                    "INVALID_PARAMS",
                    &[&sub],
                    "Invalid parameters",
                );
                return self.reply(id, label.as_deref(), vec![f]);
            }
        };
        if self.session.phase != Phase::Ready {
            let f = self.fail(
                "CHATHISTORY",
                "MESSAGE_ERROR",
                &[&sub, &target],
                "Not connected to Teams yet",
            );
            return self.reply(id, label.as_deref(), vec![f]);
        }
        self.history(HistoryJob {
            client: id,
            label,
            target,
            sub,
            conv,
            query,
            limit,
        });
    }

    pub(crate) fn on_history(
        &mut self,
        id: ClientId,
        label: Option<String>,
        target: String,
        sub: String,
        r: ic3_teams::Result<Vec<TeamsMessage>>,
    ) {
        let messages = match r {
            Ok(m) => m,
            Err(e) => {
                if matches!(e, ic3_teams::Error::ReauthRequired) {
                    self.reauth_needed();
                }
                let f = self.fail(
                    "CHATHISTORY",
                    "MESSAGE_ERROR",
                    &[&sub, &target],
                    &format!("Teams refused: {e}"),
                );
                return self.reply(id, label.as_deref(), vec![f]);
            }
        };
        for m in &messages {
            self.store.apply_message(m.clone());
        }
        let r = self.batch_ref(id);
        let mut out = vec![
            Message::new(
                "BATCH",
                [format!("+{r}"), "chathistory".into(), target.clone()],
            )
            .with_source(self.cfg.server_name.clone()),
        ];
        out.extend(self.history_messages(id, &r, &messages));
        out.push(
            Message::new("BATCH", [format!("-{r}")]).with_source(self.cfg.server_name.clone()),
        );
        if !self.caps(id).is_some_and(|c| c.batch()) {
            out.retain(|m| m.command != "BATCH");
        }
        self.reply(id, label.as_deref(), out);
    }

    /// A `CHATHISTORY` reference: `*`, `timestamp=…` or `msgid=…` (Teams
    /// message ids are arrival times in ms).
    fn resolve_ref(&self, conv: &ConversationId, s: &str) -> Option<Ref> {
        if s == "*" {
            return Some(Ref::Star);
        }
        if let Some(t) = s.strip_prefix("timestamp=") {
            return parse_time(t).map(Ref::At);
        }
        let id = s.strip_prefix("msgid=")?;
        let at = self
            .store
            .find(conv, &MessageKey::Server(MessageId(id.to_string())))
            .map(|sm| sm.message.arrived_at)
            .or_else(|| id.parse::<i64>().ok())?;
        Some(Ref::At(at))
    }

    fn chathistory_targets(&mut self, id: ClientId, label: Option<String>, msg: &Message) {
        let a = msg.param(1).strip_prefix("timestamp=").and_then(parse_time);
        let b = msg.param(2).strip_prefix("timestamp=").and_then(parse_time);
        let limit = msg.param(3).parse::<usize>().unwrap_or(50).clamp(1, 1000);
        let (Some(a), Some(b)) = (a, b) else {
            let f = self.fail(
                "CHATHISTORY",
                "INVALID_PARAMS",
                &["TARGETS"],
                "Invalid timestamps",
            );
            return self.reply(id, label.as_deref(), vec![f]);
        };
        let (lo, hi) = (a.min(b), a.max(b));
        let mut found: Vec<(Timestamp, ConversationId)> = self
            .store
            .conversations()
            .map(|sc| {
                (
                    self.store.last_activity(&sc.conversation.id),
                    sc.conversation.id.clone(),
                )
            })
            .filter(|(t, _)| *t >= lo && *t <= hi)
            .collect();
        found.sort();
        let mut names = Vec::new();
        for (t, conv) in found {
            let shown = is_notes(&conv)
                || self.joined.contains(&conv)
                || self
                    .store
                    .conversation(&conv)
                    .is_some_and(|sc| sc.conversation.kind == ConversationKind::OneToOne);
            if !shown {
                continue;
            }
            if let Some(name) = self.query_or_channel(&conv) {
                names.push((name, t));
            }
            if names.len() >= limit {
                break;
            }
        }
        let r = self.batch_ref(id);
        let srv = self.cfg.server_name.clone();
        let mut out = vec![
            Message::new(
                "BATCH",
                [format!("+{r}"), "draft/chathistory-targets".into()],
            )
            .with_source(srv.clone()),
        ];
        for (name, t) in names {
            out.push(
                Message::new("CHATHISTORY", ["TARGETS".to_string(), name, format_time(t)])
                    .with_source(srv.clone())
                    .with_tag("batch", r.clone()),
            );
        }
        out.push(Message::new("BATCH", [format!("-{r}")]).with_source(srv));
        self.reply(id, label.as_deref(), out);
    }

    // ----- Channels -----

    fn join(&mut self, id: ClientId, label: Option<String>, msg: &Message) {
        let mut out = Vec::new();
        for name in msg
            .param(0)
            .split(',')
            .filter(|n| !n.is_empty() && *n != "0")
        {
            match self.dir.conversation_by_channel(name).cloned() {
                Some(c) if self.joined.contains(&c) => self.join_burst(id, &c),
                Some(c) => self.join_conversation(&c),
                // A near miss: Server joins the only match, or says what
                // might have been meant.
                None => {
                    if !self.service_join(id, name.trim_start_matches('#')) {
                        out.push(self.numeric(
                            "403",
                            [
                                name.to_string(),
                                "No such channel: Teams chats can't be created from here".into(),
                            ],
                        ));
                    }
                }
            }
        }
        self.reply(id, label.as_deref(), out);
    }

    /// Joins a named channel or chat for every client; a team channel
    /// hidden in Teams is shown there again (a hidden chat cannot be: it
    /// comes back with its next message).
    fn join_conversation(&mut self, conv: &ConversationId) {
        let channel = self
            .store
            .user_state()
            .and_then(|us| us.channel(conv))
            .map(|(_, c)| c.clone());
        if let Some(c) = channel.filter(|c| self.channel_hidden(c)) {
            self.session.channel_hidden.insert(conv.clone(), false);
            self.spawn(|b| async move {
                let r = b.unhide_channel(&c).await;
                Box::new(move |gw: &mut Gateway| {
                    if let Err(e) = r {
                        tracing::warn!(conversation = %c.id, error = %e, "showing the channel in Teams failed");
                        gw.announce(&format!("Teams refused to show {} again: {e}", c.name));
                    }
                }) as OpFn
            });
        }
        let label = self
            .label_of(conv)
            .or_else(|| {
                self.dir
                    .existing_channel(conv)
                    .map(|n| n.trim_start_matches('#').to_owned())
            })
            .unwrap_or_else(|| "chat".into());
        self.force_join(conv, &label);
    }

    /// Every channel and named chat, joined or not, with the keys a fuzzy
    /// search matches it by: its IRC name and its Teams name.
    fn join_candidates(&self) -> Vec<(ConversationId, String, Vec<String>)> {
        let mut out: Vec<(ConversationId, String, Vec<String>)> = self
            .store
            .conversations()
            .filter_map(|sc| {
                let conv = &sc.conversation.id;
                let name = self.dir.existing_channel(conv)?.to_owned();
                let mut keys = vec![name.clone()];
                keys.extend(self.label_of(conv));
                Some((conv.clone(), name, keys))
            })
            .collect();
        if let Some(us) = self.store.user_state() {
            for chat in us
                .chats
                .iter()
                .filter(|c| c.kind != ic3_teams::ChatKind::OneOnOne)
            {
                if out.iter().any(|(id, _, _)| *id == chat.id) {
                    continue;
                }
                if let Some(name) = self.dir.existing_channel(&chat.id) {
                    let mut keys = vec![name.to_owned()];
                    keys.extend(self.label_of(&chat.id));
                    out.push((chat.id.clone(), name.to_owned(), keys));
                }
            }
            for t in us.teams.iter().filter(|t| !t.archived) {
                for c in t.channels.iter().filter(|c| !c.archived) {
                    if out.iter().any(|(id, _, _)| *id == c.id) {
                        continue;
                    }
                    if let Some(name) = self.dir.existing_channel(&c.id) {
                        out.push((
                            c.id.clone(),
                            name.to_owned(),
                            vec![name.to_owned(), format!("{}/{}", t.name, c.name)],
                        ));
                    }
                }
            }
        }
        out.sort_by(|a, b| a.1.cmp(&b.1));
        out
    }

    /// `Server`'s `join <text>`: joins the one channel or chat `text`
    /// names, or lists the matches. Returns whether it found the one.
    fn service_join(&mut self, id: ClientId, query: &str) -> bool {
        let cands = self.join_candidates();
        let keys: Vec<Vec<String>> = cands.iter().map(|c| c.2.clone()).collect();
        let ranked = crate::fuzzy::rank(query, &keys);
        if let Some(i) = crate::fuzzy::pick(&ranked) {
            let (conv, name, _) = cands[i].clone();
            if self.joined.contains(&conv) {
                self.join_burst(id, &conv);
                let m = self.service_msg(&format!("You are already in {name}."));
                self.send_to(id, m);
            } else {
                self.join_conversation(&conv);
                let m = self.service_msg(&format!("Joined {name}."));
                self.send_to(id, m);
            }
            return true;
        }
        let names: Vec<String> = ranked.iter().map(|(_, i)| cands[*i].1.clone()).collect();
        self.list_matches(id, "join", query, &names);
        false
    }

    /// `Server`'s `hide <text>`: hides the one joined channel, chat or 1:1
    /// (by the person's nick or name) `text` names, or lists the matches.
    fn service_hide(&mut self, id: ClientId, query: &str) {
        let mut cands: Vec<(ConversationId, String, Vec<String>)> = self
            .joined
            .iter()
            .filter_map(|c| {
                let name = self.dir.existing_channel(c)?.to_owned();
                let mut keys = vec![name.clone()];
                keys.extend(self.label_of(c));
                Some((c.clone(), name, keys))
            })
            .collect();
        // Every 1:1, by the person's nick (handed out now if they have none
        // yet) and their Teams name.
        for (conv, peer) in self.one_to_ones() {
            let nick = self.nick_of(&peer, None);
            let mut keys = vec![nick.clone()];
            keys.extend(self.store.display_name(&peer).map(str::to_owned));
            cands.push((conv, nick, keys));
        }
        cands.sort_by(|a, b| a.1.cmp(&b.1));
        let keys: Vec<Vec<String>> = cands.iter().map(|c| c.2.clone()).collect();
        let ranked = crate::fuzzy::rank(query, &keys);
        let Some(i) = crate::fuzzy::pick(&ranked) else {
            let names: Vec<String> = ranked.iter().map(|(_, i)| cands[*i].1.clone()).collect();
            return self.list_matches(id, "hide", query, &names);
        };
        let (conv, name, _) = cands[i].clone();
        if self.joined.remove(&conv) {
            let part = Message::new("PART", [name.clone(), "Hidden".to_string()])
                .with_source(self.self_mask());
            self.broadcast(&part);
        }
        self.hide_in_teams(conv);
        let m = self.service_msg(&format!("Hid {name} in Teams."));
        self.send_to(id, m);
    }

    /// `cutoff [age]`: shows or changes how old a chat may be and still be
    /// joined.
    fn cutoff_command(&mut self, id: ClientId, arg: &str) {
        use crate::settings::{format_cutoff, parse_cutoff};
        let text = if arg.trim().is_empty() {
            format!(
                "The cutoff is {}: chats idle longer are not joined (a new message brings one back). Change it with: cutoff 30d",
                format_cutoff(self.cutoff)
            )
        } else {
            match parse_cutoff(arg) {
                Err(e) => e,
                Ok(_) if self.session.phase != Phase::Ready => "Not connected to Teams yet.".into(),
                Ok(c) => {
                    let (left, joined) = self.set_cutoff(c);
                    format!(
                        "The cutoff is now {}: left {left} chats, joined {joined}.",
                        format_cutoff(c)
                    )
                }
            }
        };
        let m = self.service_msg(&text);
        self.send_to(id, m);
    }

    /// Tells client `id` what `query` matched, when it was not exactly one.
    fn list_matches(&mut self, id: ClientId, what: &str, query: &str, names: &[String]) {
        const SHOWN: usize = 15;
        let text = match names.len() {
            0 => format!("Nothing matches \"{query}\"."),
            n => {
                let list = names
                    .iter()
                    .take(SHOWN)
                    .cloned()
                    .collect::<Vec<_>>()
                    .join("  ");
                let more = if n > SHOWN {
                    format!(" (and {} more)", n - SHOWN)
                } else {
                    String::new()
                };
                format!("\"{query}\" matches {n}: {list}{more}. Be more specific: {what} <name>")
            }
        };
        let m = self.service_msg(&text);
        self.send_to(id, m);
    }

    fn part(&mut self, id: ClientId, label: Option<String>, msg: &Message) {
        let mut out = Vec::new();
        for name in msg.param(0).split(',').filter(|n| !n.is_empty()) {
            let Some(conv) = self
                .dir
                .conversation_by_channel(name)
                .cloned()
                .filter(|c| self.joined.contains(c))
            else {
                out.push(self.numeric(
                    "442",
                    [name.to_string(), "You're not on that channel".into()],
                ));
                continue;
            };
            self.joined.remove(&conv);
            let part = Message::new("PART", [name.to_string()]).with_source(self.self_mask());
            let others: Vec<ClientId> = self.clients.keys().copied().filter(|c| *c != id).collect();
            for o in others {
                self.send_to(o, part.clone());
            }
            out.push(part);
            self.hide_in_teams(conv);
        }
        self.reply(id, label.as_deref(), out);
    }

    /// Hides a parted conversation in Teams: a group chat as Teams' chat
    /// "Hide", a team channel as its channel "Hide" (it stays hidden; Teams
    /// does not bring channels back on new posts).
    fn hide_in_teams(&mut self, conv: ConversationId) {
        // An old chat only CSA lists is not in the store: its id says what
        // it is.
        let kind = Some(self.store.conversation(&conv).map_or_else(
            || ConversationKind::classify(&conv.0, &serde_json::Map::new()),
            |sc| sc.conversation.kind,
        ));
        let channel = self
            .store
            .user_state()
            .and_then(|us| us.channel(&conv))
            .map(|(_, c)| c.clone());
        let what = match (kind, channel) {
            (_, Some(c)) => {
                self.session.channel_hidden.insert(conv.clone(), true);
                HideWhat::Channel(c)
            }
            (Some(ConversationKind::Group | ConversationKind::OneToOne), None) => {
                self.note_parted(&conv);
                HideWhat::Chat
            }
            _ => return,
        };
        self.spawn(|b| async move {
            let r = match &what {
                HideWhat::Chat => b.hide(&conv).await,
                HideWhat::Channel(c) => b.hide_channel(c).await,
            };
            Box::new(move |gw: &mut Gateway| {
                if let Err(e) = r {
                    tracing::warn!(conversation = %conv, error = %e, "hiding it in Teams failed");
                    gw.announce(&format!("Teams refused to hide it: {e}"));
                }
            }) as OpFn
        });
    }

    fn names(&mut self, id: ClientId, label: Option<String>, msg: &Message) {
        let name = msg.param(0).to_string();
        let out = match self
            .dir
            .conversation_by_channel(&name)
            .cloned()
            .filter(|c| self.joined.contains(c))
        {
            Some(conv) => self.names_reply(id, &conv, &name),
            None => vec![self.numeric(
                "366",
                [
                    if name.is_empty() { "*".into() } else { name },
                    "End of /NAMES list.".into(),
                ],
            )],
        };
        self.reply(id, label.as_deref(), out);
    }

    fn topic(&mut self, id: ClientId, label: Option<String>, msg: &Message) {
        let name = msg.param(0).to_string();
        let Some(conv) = self.dir.conversation_by_channel(&name).cloned() else {
            let m = self.numeric("403", [name, "No such channel".into()]);
            return self.reply(id, label.as_deref(), vec![m]);
        };
        if msg.params.len() > 1 {
            let m = self.numeric(
                "482",
                [name, "Chat names can't be changed from here".into()],
            );
            return self.reply(id, label.as_deref(), vec![m]);
        }
        let m = match self.topic_of(&conv) {
            Some(t) => self.numeric("332", [name, t]),
            None => self.numeric("331", [name, "No topic is set".into()]),
        };
        self.reply(id, label.as_deref(), vec![m]);
    }

    /// A `352` line for `nick`.
    fn who_line(&self, channel: &str, nick: &str, real: &str) -> Message {
        self.numeric(
            "352",
            [
                channel.to_string(),
                nick.to_string(),
                crate::names::USER_HOST.to_string(),
                self.cfg.server_name.clone(),
                nick.to_string(),
                if self.away_of_nick(nick).is_some() {
                    "G"
                } else {
                    "H"
                }
                .into(),
                format!("0 {real}"),
            ],
        )
    }

    /// The display name behind a nick (ours included).
    fn real_name(&self, nick: &str) -> String {
        if casefold(nick) == casefold(&self.nick) {
            return self.store.me_name().unwrap_or(nick).to_string();
        }
        self.dir
            .user_by_nick(nick)
            .and_then(|m| self.store.display_name(m))
            .unwrap_or(nick)
            .to_string()
    }

    fn who(&mut self, id: ClientId, label: Option<String>, msg: &Message) {
        let mask = msg.param(0).to_string();
        let mut out = Vec::new();
        if let Some(conv) = self
            .dir
            .conversation_by_channel(&mask)
            .cloned()
            .filter(|c| self.joined.contains(c))
        {
            for n in self.member_nicks(&conv) {
                let real = self.real_name(&n);
                out.push(self.who_line(&mask, &n, &real));
            }
        } else if self.dir.user_by_nick(&mask).is_some() || casefold(&mask) == casefold(&self.nick)
        {
            let real = self.real_name(&mask);
            out.push(self.who_line("*", &mask, &real));
        }
        out.push(self.numeric("315", [mask, "End of /WHO list.".into()]));
        self.reply(id, label.as_deref(), out);
    }

    fn whois(&mut self, id: ClientId, label: Option<String>, msg: &Message) {
        let nick = msg.params.last().cloned().unwrap_or_default();
        let known = self.dir.user_by_nick(&nick).is_some()
            || casefold(&nick) == casefold(&self.nick)
            || self.dir.is_reserved(&nick);
        let mut out = Vec::new();
        if known {
            let real = self.real_name(&nick);
            out.push(self.numeric(
                "311",
                [
                    nick.clone(),
                    nick.clone(),
                    crate::names::USER_HOST.into(),
                    "*".into(),
                    real,
                ],
            ));
            out.push(self.numeric(
                "312",
                [
                    nick.clone(),
                    self.cfg.server_name.clone(),
                    "Microsoft Teams".into(),
                ],
            ));
            if let Some(text) = self.away_of_nick(&nick) {
                out.push(self.numeric("301", [nick.clone(), text]));
            }
        } else {
            out.push(self.numeric("401", [nick.clone(), "No such nick/channel".into()]));
        }
        out.push(self.numeric("318", [nick, "End of /WHOIS list.".into()]));
        self.reply(id, label.as_deref(), out);
    }

    fn list(&mut self, id: ClientId, label: Option<String>) {
        let mut out = vec![self.numeric("321", ["Channel".to_string(), "Users  Name".into()])];
        let joined: Vec<ConversationId> = self.joined.iter().cloned().collect();
        for conv in joined {
            let Some(ch) = self.dir.existing_channel(&conv).map(str::to_owned) else {
                continue;
            };
            let n = self.member_nicks(&conv).len();
            let topic = self.topic_of(&conv).unwrap_or_default();
            out.push(self.numeric("322", [ch, n.to_string(), topic]));
        }
        out.push(self.numeric("323", ["End of /LIST".to_string()]));
        self.reply(id, label.as_deref(), out);
    }

    fn mode(&mut self, id: ClientId, label: Option<String>, msg: &Message) {
        let target = msg.param(0).to_string();
        let m = if target.starts_with('#') {
            if msg.param(1).trim_start_matches('+') == "b" {
                self.numeric("368", [target, "End of channel ban list".into()])
            } else {
                self.numeric("324", [target, "+nt".into()])
            }
        } else {
            self.numeric("221", ["+i".to_string()])
        };
        self.reply(id, label.as_deref(), vec![m]);
    }

    fn nick_change(&mut self, id: ClientId, label: Option<String>, msg: &Message) {
        let new = msg.param(0).to_string();
        if new.is_empty() || new.starts_with(['#', ':']) || new.contains([' ', ',', '!', '@']) {
            let m = self.numeric("432", [new, "Erroneous nickname".into()]);
            return self.reply(id, label.as_deref(), vec![m]);
        }
        if self.dir.is_reserved(&new)
            || self
                .dir
                .user_by_nick(&new)
                .is_some_and(|m| self.store.me() != Some(m))
        {
            let m = self.numeric("433", [new, "Nickname is already in use".into()]);
            return self.reply(id, label.as_deref(), vec![m]);
        }
        if let Some(me) = self.store.me().cloned() {
            self.dir.force_nick(&me, &new);
        }
        let old = std::mem::replace(&mut self.nick, new.clone());
        let m = Message::new("NICK", [new])
            .with_source(format!("{old}!{old}@{}", crate::names::USER_HOST));
        let others: Vec<ClientId> = self.clients.keys().copied().filter(|c| *c != id).collect();
        for o in others {
            self.send_to(o, m.clone());
        }
        self.reply(id, label.as_deref(), vec![m]);
    }

    // ----- Metadata (draft/metadata-2) -----

    /// The value of metadata `key` for `target`, if set.
    fn metadata_value(&self, target: &str, key: &str) -> Option<String> {
        if key != "display-name" {
            return None;
        }
        if target.starts_with('#') {
            let conv = self.dir.conversation_by_channel(target)?;
            return self.topic_of(conv);
        }
        let target = if target == "*" {
            self.nick.as_str()
        } else {
            target
        };
        if casefold(target) == casefold(SERVICE_NICK) || casefold(target) == casefold(NOTES_NICK) {
            return None;
        }
        let real = self.real_name(target);
        (real != target).then_some(real)
    }

    fn metadata(&mut self, id: ClientId, label: Option<String>, msg: &Message) {
        let target = msg.param(0).to_string();
        let sub = msg.param(1).to_ascii_uppercase();
        let keys: Vec<String> = msg
            .params
            .iter()
            .skip(2)
            .map(|k| k.to_ascii_lowercase())
            .collect();
        let shown = if target == "*" {
            self.nick.clone()
        } else {
            target.clone()
        };
        let out = match sub.as_str() {
            "SUB" => {
                if let Some(c) = self.clients.get_mut(&id) {
                    c.metadata_subs.extend(keys.iter().cloned());
                }
                vec![self.numeric("770", keys)]
            }
            "UNSUB" => {
                if let Some(c) = self.clients.get_mut(&id) {
                    for k in &keys {
                        c.metadata_subs.remove(k);
                    }
                }
                vec![self.numeric("771", keys)]
            }
            "SUBS" => {
                let subs: Vec<String> = self
                    .clients
                    .get(&id)
                    .map(|c| c.metadata_subs.iter().cloned().collect())
                    .unwrap_or_default();
                if subs.is_empty() {
                    Vec::new()
                } else {
                    vec![self.numeric("772", subs)]
                }
            }
            "GET" => keys
                .iter()
                .map(|k| match self.metadata_value(&target, k) {
                    Some(v) => self.numeric("761", [shown.clone(), k.clone(), "*".into(), v]),
                    None => self.numeric("766", [shown.clone(), k.clone(), "key not set".into()]),
                })
                .collect(),
            "LIST" => self
                .metadata_value(&target, "display-name")
                .map(|v| {
                    vec![self.numeric("761", [shown.clone(), "display-name".into(), "*".into(), v])]
                })
                .unwrap_or_default(),
            "SYNC" => self
                .metadata_value(&target, "display-name")
                .map(|v| {
                    vec![
                        Message::new(
                            "METADATA",
                            [shown.clone(), "display-name".into(), "*".into(), v],
                        )
                        .with_source(self.cfg.server_name.clone()),
                    ]
                })
                .unwrap_or_default(),
            "SET" | "CLEAR" => vec![self.fail(
                "METADATA",
                "KEY_NO_PERMISSION",
                &[&target, keys.first().map_or("*", String::as_str)],
                "Metadata is read-only here",
            )],
            _ => vec![self.fail(
                "METADATA",
                "SUBCOMMAND_INVALID",
                &[&sub],
                "Unknown subcommand",
            )],
        };
        self.reply(id, label.as_deref(), out);
    }

    // ----- The service user -----

    fn service_command(&mut self, id: ClientId, label: Option<String>, text: &str) {
        let (word, rest) = text
            .trim()
            .split_once(char::is_whitespace)
            .unwrap_or((text.trim(), ""));
        let rest = rest.trim();
        self.reply(id, label.as_deref(), Vec::new());
        let ready = self.session.phase == Phase::Ready;
        match word.to_ascii_lowercase().as_str() {
            "login" => self.begin_login(),
            "logout" => self.logout(),
            "join" | "hide" if !ready => {
                let m = self.service_msg("Not connected to Teams yet.");
                self.send_to(id, m);
            }
            "cutoff" => self.cutoff_command(id, rest),
            "join" if !rest.is_empty() => {
                self.service_join(id, rest);
            }
            "hide" if !rest.is_empty() => self.service_hide(id, rest),
            "status" => {
                let text = format!(
                    "{} {} channels; {} clients attached.",
                    self.session.status_text(),
                    self.joined.len(),
                    self.clients.len()
                );
                let m = self.service_msg(&text);
                self.send_to(id, m);
            }
            _ => {
                for line in [
                    "Commands: login, logout, status, join <name>, hide <name>, cutoff [age].",
                    "cutoff [age]: chats idle longer than this are not joined (e.g. 60d, 2w, 12h, or off); without an age it shows the current one. /cutoff works too.",
                    "join <name> joins a chat or team channel by any part of its name (a channel hidden in Teams is shown again); with several matches it lists them.",
                    "hide <name> hides a chat, team channel or your 1:1 with someone in Teams, and leaves it here; a chat comes back with its next message.",
                    "/part also hides a chat or channel in Teams.",
                ] {
                    let m = self.service_msg(line);
                    self.send_to(id, m);
                }
            }
        }
    }
}

/// The text of a CTCP ACTION (`/me`), if `text` is one.
fn ctcp_action(text: &str) -> Option<&str> {
    let inner = text.strip_prefix('\x01')?.trim_end_matches('\x01');
    inner
        .strip_prefix("ACTION ")
        .or_else(|| (inner == "ACTION").then_some(""))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ctcp_actions() {
        assert_eq!(ctcp_action("\x01ACTION waves\x01"), Some("waves"));
        assert_eq!(ctcp_action("\x01ACTION waves"), Some("waves"));
        assert_eq!(ctcp_action("\x01VERSION\x01"), None);
        assert_eq!(ctcp_action("hello"), None);
    }

    #[test]
    fn timestamp_refs_round_trip() {
        let t = 1_791_882_000_123;
        assert_eq!(
            super::super::timestamp_ref(t),
            "timestamp=2026-10-13T09:00:00.123Z"
        );
    }
}
