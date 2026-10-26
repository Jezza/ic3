//! The gateway against a fake Teams backend, driven through [`Input`]s like
//! a real connection would.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use futures::StreamExt;
use ic3_teams::{
    Conversation, ConversationId, ConversationKind, DeviceCodePrompt, Event, Me, Member,
    Message as TeamsMessage, MessageId, MessageRef, Mri, Outgoing, Page, PageCursor, PageMeta,
    Reaction, ReactionSummary, RichText, SendReceipt, ThreadInfo, Timestamp, UserState,
};
use tokio::sync::mpsc;

use super::{GatewayConfig, Input, run};
use crate::backend::{Backend, EventStreamBox};
use crate::caps::Caps;
use crate::irc::Message;
use crate::media::MediaTable;

/// Every call the gateway made that changes something in Teams.
#[derive(Clone, Debug, PartialEq)]
enum Call {
    Send {
        conv: String,
        text: String,
        reply_quote: Option<String>,
        thread_root: Option<String>,
        cmid: String,
        files: Vec<String>,
    },
    MarkRead {
        conv: String,
        id: String,
    },
    React {
        conv: String,
        id: String,
        reaction: Reaction,
        add: bool,
    },
    Edit {
        id: String,
        text: String,
    },
    Delete {
        id: String,
    },
    Typing {
        conv: String,
    },
    Hidden {
        conv: String,
    },
    HiddenChannel {
        conv: String,
    },
    ShownChannel {
        conv: String,
    },
    Upload {
        name: String,
        image: bool,
    },
}

#[derive(Default)]
struct FakeState {
    conversations: Vec<Conversation>,
    user_state: UserState,
    messages: HashMap<String, Vec<TeamsMessage>>,
    names: HashMap<Mri, String>,
    calls: Vec<Call>,
    events: Option<mpsc::UnboundedReceiver<Event>>,
    needs_login: bool,
    /// How many CSA calls fail before one succeeds.
    user_state_failures: u32,
    /// How many message pages were fetched.
    message_fetches: usize,
    presence: HashMap<Mri, ic3_teams::Presence>,
    subscribed: std::collections::HashSet<Mri>,
}

#[derive(Clone, Default)]
struct Fake(Arc<Mutex<FakeState>>);

impl Fake {
    fn calls(&self) -> Vec<Call> {
        self.0.lock().unwrap().calls.clone()
    }

    fn call(&self, c: Call) {
        self.0.lock().unwrap().calls.push(c);
    }
}

#[async_trait::async_trait]
impl Backend for Fake {
    async fn needs_login(&self) -> ic3_teams::Result<bool> {
        Ok(self.0.lock().unwrap().needs_login)
    }
    async fn start_login(&self) -> ic3_teams::Result<DeviceCodePrompt> {
        Ok(DeviceCodePrompt {
            user_code: "ABCD-1234".into(),
            verification_url: "https://microsoft.com/devicelogin".into(),
            message: String::new(),
            expires_at: ic3_teams::now_ms() + 900_000,
        })
    }
    async fn wait_login(&self) -> ic3_teams::Result<()> {
        std::future::pending().await
    }
    async fn logout(&self) -> ic3_teams::Result<()> {
        Ok(())
    }
    async fn me(&self) -> ic3_teams::Result<Me> {
        Ok(Me {
            mri: me(),
            display_name: "Ada Lovelace".into(),
            upn: None,
        })
    }
    async fn user_state(&self) -> ic3_teams::Result<UserState> {
        let mut st = self.0.lock().unwrap();
        if st.user_state_failures > 0 {
            st.user_state_failures -= 1;
            return Err(ic3_teams::Error::Network("operation timed out".into()));
        }
        Ok(st.user_state.clone())
    }
    async fn conversations(
        &self,
        since: Option<Timestamp>,
        _: Option<PageCursor>,
    ) -> ic3_teams::Result<Page<Conversation>> {
        let items = if since.is_some() {
            Vec::new()
        } else {
            self.0.lock().unwrap().conversations.clone()
        };
        Ok(Page {
            items,
            next: None,
            meta: PageMeta::default(),
            note: None,
        })
    }
    async fn messages(
        &self,
        conv: &ConversationId,
        _: Option<PageCursor>,
        n: usize,
    ) -> ic3_teams::Result<Page<TeamsMessage>> {
        self.0.lock().unwrap().message_fetches += 1;
        let mut items = self
            .0
            .lock()
            .unwrap()
            .messages
            .get(&conv.0)
            .cloned()
            .unwrap_or_default();
        items.sort_by_key(|m| std::cmp::Reverse(m.arrived_at));
        items.truncate(n);
        Ok(Page {
            items,
            next: None,
            meta: PageMeta::default(),
            note: None,
        })
    }
    async fn messages_since(
        &self,
        conv: &ConversationId,
        since: Timestamp,
    ) -> ic3_teams::Result<Vec<TeamsMessage>> {
        let items = self
            .0
            .lock()
            .unwrap()
            .messages
            .get(&conv.0)
            .cloned()
            .unwrap_or_default();
        Ok(items
            .into_iter()
            .filter(|m| m.arrived_at >= since)
            .collect())
    }
    async fn send(&self, conv: &ConversationId, out: &Outgoing) -> ic3_teams::Result<SendReceipt> {
        self.call(Call::Send {
            conv: conv.0.clone(),
            text: out.body.plain_text(),
            reply_quote: out.reply_to.as_ref().map(|r| r.message.id.0.clone()),
            thread_root: out.thread_root.as_ref().map(|r| r.0.clone()),
            cmid: out.client_message_id.clone(),
            files: out.files.iter().map(|f| f.name.clone()).collect(),
        });
        Ok(SendReceipt {
            client_message_id: out.client_message_id.clone(),
            arrival_time: None,
        })
    }
    async fn mark_read(&self, conv: &ConversationId, msg: &MessageRef) -> ic3_teams::Result<()> {
        self.call(Call::MarkRead {
            conv: conv.0.clone(),
            id: msg.id.0.clone(),
        });
        Ok(())
    }
    async fn react(
        &self,
        conv: &ConversationId,
        msg: &MessageId,
        reaction: &Reaction,
        add: bool,
    ) -> ic3_teams::Result<()> {
        self.call(Call::React {
            conv: conv.0.clone(),
            id: msg.0.clone(),
            reaction: reaction.clone(),
            add,
        });
        Ok(())
    }
    async fn edit(&self, original: &TeamsMessage, body: &RichText) -> ic3_teams::Result<()> {
        self.call(Call::Edit {
            id: original.id.0.clone(),
            text: body.plain_text(),
        });
        Ok(())
    }
    async fn delete(&self, original: &TeamsMessage) -> ic3_teams::Result<()> {
        self.call(Call::Delete {
            id: original.id.0.clone(),
        });
        Ok(())
    }
    async fn send_typing(&self, conv: &ConversationId) -> ic3_teams::Result<()> {
        self.call(Call::Typing {
            conv: conv.0.clone(),
        });
        Ok(())
    }
    async fn unhide_channel(&self, channel: &ic3_teams::Channel) -> ic3_teams::Result<()> {
        self.call(Call::ShownChannel {
            conv: channel.id.0.clone(),
        });
        Ok(())
    }
    async fn hide_channel(&self, channel: &ic3_teams::Channel) -> ic3_teams::Result<()> {
        self.call(Call::HiddenChannel {
            conv: channel.id.0.clone(),
        });
        Ok(())
    }
    async fn presence(&self, mris: &[Mri]) -> ic3_teams::Result<Vec<ic3_teams::Presence>> {
        let st = self.0.lock().unwrap();
        Ok(mris
            .iter()
            .filter_map(|m| st.presence.get(m).cloned())
            .collect())
    }
    async fn subscribe_presence(
        &self,
        add: &[Mri],
        _remove: &[Mri],
        purge: bool,
    ) -> ic3_teams::Result<()> {
        let mut st = self.0.lock().unwrap();
        if purge {
            st.subscribed.clear();
        }
        st.subscribed.extend(add.iter().cloned());
        Ok(())
    }
    async fn hide(&self, conv: &ConversationId) -> ic3_teams::Result<()> {
        self.call(Call::Hidden {
            conv: conv.0.clone(),
        });
        Ok(())
    }
    async fn thread(&self, conv: &ConversationId) -> ic3_teams::Result<ThreadInfo> {
        Ok(ThreadInfo {
            id: conv.clone(),
            topic: None,
            members: Vec::new(),
        })
    }
    async fn user_names(&self, mris: &[Mri]) -> ic3_teams::Result<HashMap<Mri, String>> {
        let names = &self.0.lock().unwrap().names;
        Ok(mris
            .iter()
            .filter_map(|m| names.get(m).map(|n| (m.clone(), n.clone())))
            .collect())
    }
    async fn fetch_media(&self, url: &str) -> ic3_teams::Result<ic3_teams::Media> {
        Ok(ic3_teams::Media {
            content_type: Some("image/png".into()),
            bytes: url.as_bytes().to_vec(),
            file_name: None,
        })
    }
    async fn upload_image(
        &self,
        _: &ConversationId,
        file_name: &str,
        _: &str,
        _: Vec<u8>,
    ) -> ic3_teams::Result<ic3_teams::Inline> {
        self.call(Call::Upload {
            name: file_name.into(),
            image: true,
        });
        Ok(ic3_teams::Inline::Image {
            url: format!("https://ams.test/v1/objects/{file_name}/views/imgo"),
        })
    }
    async fn upload_file(
        &self,
        _: &ConversationId,
        file_name: &str,
        _: Vec<u8>,
    ) -> ic3_teams::Result<ic3_teams::UploadedFile> {
        self.call(Call::Upload {
            name: file_name.into(),
            image: false,
        });
        Ok(ic3_teams::UploadedFile {
            name: file_name.into(),
            property: serde_json::json!({ "fileName": file_name }),
        })
    }
    async fn connect(&self) -> ic3_teams::Result<EventStreamBox> {
        let rx = self.0.lock().unwrap().events.take();
        match rx {
            Some(rx) => Ok(tokio_stream_from(rx).boxed()),
            None => Ok(futures::stream::pending().boxed()),
        }
    }
    async fn shutdown(&self) {}
}

fn tokio_stream_from(mut rx: mpsc::UnboundedReceiver<Event>) -> impl futures::Stream<Item = Event> {
    futures::stream::poll_fn(move |cx| rx.poll_recv(cx))
}

// ----- Fixtures -----

const GROUP: &str = "19:group@thread.v2";
const DM: &str = "19:dm@thread.v2";
const CHANNEL: &str = "19:general@thread.tacv2";

fn me() -> Mri {
    Mri("8:orgid:me".into())
}

fn bob() -> Mri {
    Mri("8:orgid:bob".into())
}

fn member(m: &Mri, name: &str) -> Member {
    Member {
        mri: m.clone(),
        role: None,
        friendly_name: Some(name.into()),
    }
}

fn conversation(
    id: &str,
    kind: ConversationKind,
    topic: Option<&str>,
    members: Vec<Member>,
) -> Conversation {
    Conversation {
        id: ConversationId(id.into()),
        kind,
        topic: topic.map(str::to_owned),
        members: Some(members),
        last_message: None,
        read_horizon: None,
        version: ic3_teams::now_ms(),
        hidden: false,
        extra: serde_json::Map::new(),
    }
}

fn message(conv: &str, id: i64, from: &Mri, text: &str) -> TeamsMessage {
    TeamsMessage {
        id: MessageId(id.to_string()),
        client_message_id: None,
        conversation: ConversationId(conv.into()),
        thread_root: None,
        from: from.clone(),
        from_name: None,
        arrived_at: id,
        version: id,
        edited: false,
        deleted: false,
        body: RichText::from_plain_text(text),
        reply_to: None,
        reactions: Vec::new(),
        attachments: Vec::new(),
        message_type: "RichText/Html".into(),
        extra: serde_json::Map::new(),
    }
}

fn fake() -> Fake {
    let f = Fake::default();
    {
        let mut s = f.0.lock().unwrap();
        s.conversations = vec![
            conversation(
                GROUP,
                ConversationKind::Group,
                Some("Launch plans"),
                vec![member(&me(), "Ada Lovelace"), member(&bob(), "Bob Builder")],
            ),
            conversation(
                DM,
                ConversationKind::OneToOne,
                None,
                vec![member(&me(), "Ada Lovelace"), member(&bob(), "Bob Builder")],
            ),
            conversation(CHANNEL, ConversationKind::Channel, None, Vec::new()),
        ];
        s.user_state = UserState {
            teams: vec![ic3_teams::Team {
                id: ConversationId("19:team@thread.tacv2".into()),
                name: "Engineering".into(),
                favorite: false,
                collapsed: false,
                archived: false,
                channels: vec![ic3_teams::Channel {
                    id: ConversationId(CHANNEL.into()),
                    name: "General".into(),
                    channel_type: None,
                    general: true,
                    favorite: true,
                    pinned: false,
                    muted: false,
                    archived: false,
                    interest: None,
                    member_role: None,
                }],
            }],
            ..UserState::default()
        };
    }
    f
}

// ----- Harness -----

struct Harness {
    fake: Fake,
    inputs: mpsc::Sender<Input>,
    events: mpsc::UnboundedSender<Event>,
    out: mpsc::Receiver<String>,
}

const ALL_CAPS: &[&str] = &[
    "batch",
    "draft/chathistory",
    "draft/event-playback",
    "draft/message-edit",
    "draft/message-redaction",
    "draft/metadata-2",
    "draft/multiline",
    "draft/read-marker",
    "echo-message",
    "labeled-response",
    "message-tags",
    "server-time",
];

async fn start(fake: Fake) -> Harness {
    start_with(fake, ALL_CAPS).await
}

async fn start_with(fake: Fake, caps: &[&str]) -> Harness {
    let (ev_tx, ev_rx) = mpsc::unbounded_channel();
    fake.0.lock().unwrap().events = Some(ev_rx);
    let (in_tx, in_rx) = mpsc::channel(64);
    let media = Arc::new(MediaTable::new(Some("https://gw.test".into())));
    let cfg = GatewayConfig {
        max_chat_age: None,
        ..GatewayConfig::default()
    };
    tokio::spawn(run(Arc::new(fake.clone()), cfg, media, in_rx));
    let (out_tx, out_rx) = mpsc::channel(4096);
    let mut c = Caps::default();
    for cap in caps {
        c.insert(cap);
    }
    in_tx
        .send(Input::Attach {
            id: 1,
            nick: "ada".into(),
            caps: c,
            out: out_tx,
            close: tokio_util::sync::CancellationToken::new(),
        })
        .await
        .unwrap();
    Harness {
        fake,
        inputs: in_tx,
        events: ev_tx,
        out: out_rx,
    }
}

impl Harness {
    /// Reads lines until one matches `pred`; returns everything read.
    async fn until(&mut self, pred: impl Fn(&Message) -> bool) -> Vec<Message> {
        let mut seen = Vec::new();
        loop {
            let line = tokio::time::timeout(Duration::from_secs(5), self.out.recv())
                .await
                .unwrap_or_else(|_| panic!("timed out; saw:\n{}", lines(&seen)))
                .expect("gateway closed the client");
            let m = Message::parse(&line).unwrap();
            let done = pred(&m);
            seen.push(m);
            if done {
                return seen;
            }
        }
    }

    /// Waits until the gateway has joined the channels (the "Connected"
    /// announcement).
    async fn ready(&mut self) -> Vec<Message> {
        self.until(|m| m.command == "PRIVMSG" && m.param(1).starts_with("Connected to Teams"))
            .await
    }

    async fn send(&self, line: &str) {
        self.inputs
            .send(Input::Line {
                id: 1,
                msg: Message::parse(line).unwrap(),
            })
            .await
            .unwrap();
    }

    async fn wait_call(&self, pred: impl Fn(&Call) -> bool) -> Call {
        for _ in 0..200 {
            if let Some(c) = self.fake.calls().into_iter().find(|c| pred(c)) {
                return c;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        panic!("call never made; calls: {:?}", self.fake.calls());
    }
}

fn lines(ms: &[Message]) -> String {
    ms.iter()
        .map(|m| m.to_string())
        .collect::<Vec<_>>()
        .join("\n")
}

// ----- Tests -----

#[tokio::test]
async fn startup_joins_chats_and_channels_and_renames_us() {
    let mut h = start(fake()).await;
    let seen = h.ready().await;
    let text = lines(&seen);
    assert!(text.contains(" 001 ada "), "{text}");
    assert!(text.contains("NICK AdaLovelace"), "{text}");
    assert!(
        text.contains(":AdaLovelace!AdaLovelace@teams JOIN #Launch-plans"),
        "{text}"
    );
    assert!(
        text.contains(" 332 AdaLovelace #Launch-plans :Launch plans"),
        "{text}"
    );
    assert!(
        text.contains(" 353 AdaLovelace = #Launch-plans :AdaLovelace BobBuilder"),
        "{text}"
    );
    assert!(text.contains("JOIN #Engineering/General"), "{text}");
    assert!(text.contains("MARKREAD #Launch-plans *"), "{text}");
    // 1:1 chats are queries, not channels.
    assert!(!text.contains("JOIN #Bob"), "{text}");
}

#[tokio::test]
async fn live_messages_become_privmsgs() {
    let mut h = start(fake()).await;
    h.ready().await;
    h.events
        .send(Event::NewMessage(message(
            GROUP,
            1_000,
            &bob(),
            "hello all",
        )))
        .unwrap();
    let seen = h
        .until(|m| m.command == "PRIVMSG" && m.param(0) == "#Launch-plans")
        .await;
    let m = seen.last().unwrap();
    assert_eq!(m.source.as_deref(), Some("BobBuilder!BobBuilder@teams"));
    assert_eq!(m.param(1), "hello all");
    assert_eq!(m.tag("msgid"), Some("1000"));
    assert_eq!(m.tag("time"), Some("1970-01-01T00:00:01.000Z"));

    h.events
        .send(Event::NewMessage(message(DM, 2_000, &bob(), "psst")))
        .unwrap();
    let seen = h
        .until(|m| m.command == "PRIVMSG" && m.param(1) == "psst")
        .await;
    let m = seen.last().unwrap();
    assert_eq!(m.param(0), "AdaLovelace");
    assert_eq!(m.source_nick(), Some("BobBuilder"));
}

#[tokio::test]
async fn sending_echoes_under_the_label_when_teams_echoes() {
    let mut h = start(fake()).await;
    h.ready().await;
    h.send("@label=l1 PRIVMSG #Launch-plans :ship it").await;
    let Call::Send {
        conv, text, cmid, ..
    } = h.wait_call(|c| matches!(c, Call::Send { .. })).await
    else {
        unreachable!()
    };
    assert_eq!((conv.as_str(), text.as_str()), (GROUP, "ship it"));
    let mut echo = message(GROUP, 3_000, &me(), "ship it");
    echo.client_message_id = Some(cmid);
    h.events.send(Event::NewMessage(echo)).unwrap();
    let seen = h
        .until(|m| m.command == "PRIVMSG" && m.param(1) == "ship it")
        .await;
    let m = seen.last().unwrap();
    assert_eq!(m.tag("label"), Some("l1"));
    assert_eq!(m.tag("msgid"), Some("3000"));
    assert_eq!(m.source_nick(), Some("AdaLovelace"));
}

#[tokio::test]
async fn replies_quote_in_chats_and_thread_in_channels() {
    let f = fake();
    f.0.lock()
        .unwrap()
        .messages
        .insert(GROUP.into(), vec![message(GROUP, 500, &bob(), "question?")]);
    f.0.lock().unwrap().messages.insert(
        CHANNEL.into(),
        vec![message(CHANNEL, 600, &bob(), "a post")],
    );
    let mut h = start(f).await;
    h.ready().await;
    // Load history so the replied-to messages are known.
    h.send("CHATHISTORY LATEST #Launch-plans * 10").await;
    h.until(|m| m.command == "BATCH" && m.param(0).starts_with('-'))
        .await;
    h.send("CHATHISTORY LATEST #Engineering/General * 10").await;
    h.until(|m| m.command == "BATCH" && m.param(0).starts_with('-'))
        .await;

    h.send("@+draft/reply=500 PRIVMSG #Launch-plans :answer")
        .await;
    let c = h
        .wait_call(|c| matches!(c, Call::Send { text, .. } if text == "answer"))
        .await;
    assert!(
        matches!(c, Call::Send { reply_quote: Some(ref q), thread_root: None, .. } if q == "500"),
        "{c:?}"
    );

    h.send("@+reply=600 PRIVMSG #Engineering/General :in thread")
        .await;
    let c = h
        .wait_call(|c| matches!(c, Call::Send { text, .. } if text == "in thread"))
        .await;
    assert!(
        matches!(c, Call::Send { reply_quote: None, thread_root: Some(ref r), .. } if r == "600"),
        "{c:?}"
    );
}

#[tokio::test]
async fn chathistory_latest_returns_a_batch_with_reactions() {
    let f = fake();
    let mut m1 = message(GROUP, 1_000, &bob(), "first");
    m1.reactions = vec![ReactionSummary {
        reaction: Reaction::Like,
        users: vec![me()],
    }];
    let m2 = message(GROUP, 2_000, &me(), "line one\nline two");
    f.0.lock()
        .unwrap()
        .messages
        .insert(GROUP.into(), vec![m2, m1]);
    let mut h = start(f).await;
    h.ready().await;
    h.send("@label=h CHATHISTORY LATEST #Launch-plans * 50")
        .await;
    let seen = h
        .until(|m| m.command == "BATCH" && m.param(0).starts_with('-') && m.tag("batch").is_none())
        .await;
    let text = lines(&seen);
    let start = seen
        .iter()
        .position(|m| m.param(1) == "chathistory")
        .expect(&text);
    let body: Vec<&Message> = seen[start..].iter().collect();
    assert_eq!(body[1].param(1), "first", "{text}");
    assert_eq!(body[2].command, "TAGMSG", "{text}");
    assert_eq!(body[2].tag("+draft/react"), Some("👍"));
    assert_eq!(body[3].param(1), "draft/multiline", "{text}");
    assert_eq!(body[4].param(1), "line one");
    assert_eq!(body[5].param(1), "line two");
    assert!(text.contains("labeled-response"), "{text}");
}

#[tokio::test]
async fn reactions_both_ways() {
    let f = fake();
    f.0.lock()
        .unwrap()
        .messages
        .insert(GROUP.into(), vec![message(GROUP, 1_000, &bob(), "nice")]);
    let mut h = start(f).await;
    h.ready().await;
    h.send("CHATHISTORY LATEST #Launch-plans * 10").await;
    h.until(|m| m.command == "BATCH" && m.param(0).starts_with('-'))
        .await;

    h.send("@label=r;+draft/react=❤️;+draft/reply=1000 TAGMSG #Launch-plans")
        .await;
    let c = h.wait_call(|c| matches!(c, Call::React { .. })).await;
    assert_eq!(
        c,
        Call::React {
            conv: GROUP.into(),
            id: "1000".into(),
            reaction: Reaction::Heart,
            add: true
        }
    );
    let seen = h
        .until(|m| m.command == "TAGMSG" && m.tag("label") == Some("r"))
        .await;
    assert_eq!(seen.last().unwrap().tag("+draft/react"), Some("❤️"));

    // Bob adds a like: only the new reaction is announced.
    h.events
        .send(Event::ReactionsChanged {
            conversation: ConversationId(GROUP.into()),
            message: MessageId("1000".into()),
            reactions: vec![
                ReactionSummary {
                    reaction: Reaction::Heart,
                    users: vec![me()],
                },
                ReactionSummary {
                    reaction: Reaction::Like,
                    users: vec![bob()],
                },
            ],
            version: 0,
        })
        .unwrap();
    let seen = h
        .until(|m| m.command == "TAGMSG" && m.source_nick() == Some("BobBuilder"))
        .await;
    assert_eq!(seen.last().unwrap().tag("+draft/react"), Some("👍"));
    assert!(
        !seen
            .iter()
            .any(|m| m.tag("+draft/react") == Some("❤️") && m.source_nick() == Some("AdaLovelace"))
    );

    h.send("@label=x;+draft/react=nope;+draft/reply=1000 TAGMSG #Launch-plans")
        .await;
    let seen = h.until(|m| m.command == "FAIL").await;
    assert_eq!(seen.last().unwrap().param(1), "INVALID_REACTION");
}

#[tokio::test]
async fn part_hides_the_chat_in_teams_and_a_new_message_brings_it_back() {
    let mut h = start(fake()).await;
    h.ready().await;
    h.send("@label=p PART #Launch-plans").await;
    let seen = h.until(|m| m.command == "PART").await;
    assert_eq!(seen.last().unwrap().tag("label"), Some("p"));
    let c = h.wait_call(|c| matches!(c, Call::Hidden { .. })).await;
    assert_eq!(c, Call::Hidden { conv: GROUP.into() });
    h.events
        .send(Event::NewMessage(message(
            GROUP,
            5_000,
            &bob(),
            "you there?",
        )))
        .unwrap();
    let seen = h
        .until(|m| m.command == "PRIVMSG" && m.param(1) == "you there?")
        .await;
    let text = lines(&seen);
    assert!(text.contains("JOIN #Launch-plans"), "{text}");
}

#[tokio::test]
async fn markread_moves_the_teams_horizon() {
    let f = fake();
    f.0.lock().unwrap().messages.insert(
        GROUP.into(),
        vec![
            message(GROUP, 1_000, &bob(), "a"),
            message(GROUP, 2_000, &bob(), "b"),
        ],
    );
    let mut h = start(f).await;
    h.ready().await;
    h.send("CHATHISTORY LATEST #Launch-plans * 10").await;
    h.until(|m| m.command == "BATCH" && m.param(0).starts_with('-'))
        .await;
    h.send("@label=m MARKREAD #Launch-plans timestamp=1970-01-01T00:00:01.500Z")
        .await;
    let seen = h
        .until(|m| m.command == "MARKREAD" && m.tag("label") == Some("m"))
        .await;
    assert_eq!(
        seen.last().unwrap().param(1),
        "timestamp=1970-01-01T00:00:01.000Z"
    );
    let c = h.wait_call(|c| matches!(c, Call::MarkRead { .. })).await;
    assert_eq!(
        c,
        Call::MarkRead {
            conv: GROUP.into(),
            id: "1000".into()
        }
    );
}

#[tokio::test]
async fn edits_and_deletes_both_ways() {
    let f = fake();
    f.0.lock().unwrap().messages.insert(
        GROUP.into(),
        vec![
            message(GROUP, 1_000, &me(), "helo"),
            message(GROUP, 2_000, &bob(), "hi"),
        ],
    );
    let mut h = start(f).await;
    h.ready().await;
    h.send("CHATHISTORY LATEST #Launch-plans * 10").await;
    h.until(|m| m.command == "BATCH" && m.param(0).starts_with('-'))
        .await;

    h.send("@label=e;draft/target-msgid=1000;draft/edit-text=hello EDIT #Launch-plans")
        .await;
    let seen = h.until(|m| m.command == "EDIT").await;
    let m = seen.last().unwrap();
    assert_eq!(
        (m.tag("label"), m.tag("draft/edit-text")),
        (Some("e"), Some("hello"))
    );
    assert_eq!(
        h.wait_call(|c| matches!(c, Call::Edit { .. })).await,
        Call::Edit {
            id: "1000".into(),
            text: "hello".into()
        }
    );

    h.send("@label=f;draft/target-msgid=2000;draft/edit-text=x EDIT #Launch-plans")
        .await;
    let seen = h.until(|m| m.command == "FAIL").await;
    assert_eq!(seen.last().unwrap().param(1), "MESSAGE_NOT_OWN");

    h.send("@label=d REDACT #Launch-plans 1000").await;
    let seen = h.until(|m| m.command == "REDACT").await;
    assert_eq!(seen.last().unwrap().tag("label"), Some("d"));

    // Bob edits his message in Teams.
    let mut edited = message(GROUP, 2_000, &bob(), "hi everyone");
    edited.version = 9_000;
    edited.edited = true;
    h.events.send(Event::MessageEdited(edited)).unwrap();
    let seen = h
        .until(|m| m.command == "EDIT" && m.source_nick() == Some("BobBuilder"))
        .await;
    let m = seen.last().unwrap();
    assert_eq!(
        (m.tag("draft/target-msgid"), m.tag("draft/edit-text")),
        (Some("2000"), Some("hi everyone"))
    );
}

#[tokio::test]
async fn typing_both_ways() {
    let mut h = start(fake()).await;
    h.ready().await;
    h.send("@+typing=active TAGMSG #Launch-plans").await;
    assert_eq!(
        h.wait_call(|c| matches!(c, Call::Typing { .. })).await,
        Call::Typing { conv: GROUP.into() }
    );
    h.events
        .send(Event::Typing {
            conversation: ConversationId(GROUP.into()),
            from: bob(),
            from_name: Some("Bob Builder".into()),
        })
        .unwrap();
    let seen = h
        .until(|m| m.command == "TAGMSG" && m.tag("+typing").is_some())
        .await;
    let m = seen.last().unwrap();
    assert_eq!(
        (m.source_nick(), m.tag("+typing"), m.param(0)),
        (Some("BobBuilder"), Some("active"), "#Launch-plans")
    );
}

#[tokio::test]
async fn multiline_batches_are_sent_as_one_message() {
    let mut h = start(fake()).await;
    h.ready().await;
    h.send("@label=ml BATCH +b1 draft/multiline #Launch-plans")
        .await;
    h.send("@batch=b1 PRIVMSG #Launch-plans :first line").await;
    h.send("@batch=b1 PRIVMSG #Launch-plans :second line").await;
    h.send("BATCH -b1").await;
    let c = h.wait_call(|c| matches!(c, Call::Send { .. })).await;
    let Call::Send { text, .. } = c else {
        unreachable!()
    };
    assert_eq!(text, "first line\nsecond line");
}

#[tokio::test]
async fn clients_without_tags_get_plain_lines() {
    let mut h = start_with(fake(), &[]).await;
    h.ready().await;
    h.events
        .send(Event::NewMessage(message(GROUP, 1_000, &bob(), "one\ntwo")))
        .unwrap();
    let seen = h
        .until(|m| m.command == "PRIVMSG" && m.param(1) == "two")
        .await;
    let first = &seen[seen.len() - 2];
    assert_eq!(first.param(1), "one");
    assert!(first.tags.is_empty() && seen.last().unwrap().tags.is_empty());
}

#[tokio::test]
async fn sign_in_is_announced_by_the_service_user() {
    let f = fake();
    f.0.lock().unwrap().needs_login = true;
    let mut h = start(f).await;
    let seen = h
        .until(|m| m.command == "PRIVMSG" && m.param(1).contains("ABCD-1234"))
        .await;
    let m = seen.last().unwrap();
    assert_eq!(m.source_nick(), Some("Server"));
    assert!(m.param(1).contains("https://microsoft.com/devicelogin"));
}

#[tokio::test]
async fn metadata_gives_display_names() {
    let mut h = start(fake()).await;
    h.ready().await;
    h.send("@label=md METADATA BobBuilder GET display-name avatar")
        .await;
    let seen = h
        .until(|m| m.command == "BATCH" && m.param(0).starts_with('-'))
        .await;
    let text = lines(&seen);
    assert!(
        text.contains(" 761 AdaLovelace BobBuilder display-name * :Bob Builder"),
        "{text}"
    );
    assert!(
        text.contains(" 766 AdaLovelace BobBuilder avatar"),
        "{text}"
    );
}

#[tokio::test]
async fn hidden_chats_wait_for_a_new_message() {
    let f = fake();
    {
        let c = &mut f.0.lock().unwrap().conversations[0];
        c.hidden = true;
        c.extra
            .insert("properties".into(), serde_json::json!({ "hidden": "true" }));
    }
    let mut h = start(f).await;
    let seen = h.ready().await;
    assert!(
        !lines(&seen).contains("JOIN #Launch-plans"),
        "{}",
        lines(&seen)
    );
    h.events
        .send(Event::NewMessage(message(
            GROUP,
            7_000,
            &bob(),
            "back again",
        )))
        .unwrap();
    let seen = h
        .until(|m| m.command == "PRIVMSG" && m.param(1) == "back again")
        .await;
    assert!(
        lines(&seen).contains("JOIN #Launch-plans"),
        "{}",
        lines(&seen)
    );
}

#[tokio::test]
async fn uploaded_files_are_sent_to_teams() {
    let (ev_tx, ev_rx) = mpsc::unbounded_channel();
    let f = fake();
    f.0.lock().unwrap().events = Some(ev_rx);
    let (in_tx, in_rx) = mpsc::channel(64);
    let media = Arc::new(MediaTable::new(Some("https://gw.test".into())));
    let img = media
        .add_upload("cat.png", "image/png", vec![1, 2, 3])
        .unwrap();
    let doc = media
        .add_upload("plan.pdf", "application/pdf", vec![4])
        .unwrap();
    let cfg = GatewayConfig {
        max_chat_age: None,
        ..GatewayConfig::default()
    };
    tokio::spawn(run(Arc::new(f.clone()), cfg, media, in_rx));
    let (out_tx, out_rx) = mpsc::channel(4096);
    in_tx
        .send(Input::Attach {
            id: 1,
            nick: "ada".into(),
            caps: Caps::default(),
            out: out_tx,
            close: tokio_util::sync::CancellationToken::new(),
        })
        .await
        .unwrap();
    let mut h = Harness {
        fake: f,
        inputs: in_tx,
        events: ev_tx,
        out: out_rx,
    };
    h.ready().await;
    h.send(&format!("PRIVMSG #Launch-plans :look {img} and {doc}"))
        .await;
    let c = h.wait_call(|c| matches!(c, Call::Send { .. })).await;
    let Call::Send { text, files, .. } = c else {
        unreachable!()
    };
    assert_eq!(files, vec!["plan.pdf".to_string()]);
    assert!(text.starts_with("look and"), "{text}");
    assert!(text.contains("[image]"), "{text}");
    let uploads: Vec<Call> = h
        .fake
        .calls()
        .into_iter()
        .filter(|c| matches!(c, Call::Upload { .. }))
        .collect();
    assert_eq!(
        uploads,
        vec![
            Call::Upload {
                name: "cat.png".into(),
                image: true
            },
            Call::Upload {
                name: "plan.pdf".into(),
                image: false
            }
        ]
    );
}

#[tokio::test]
async fn long_lines_are_split_for_every_client() {
    let mut h = start(fake()).await;
    h.ready().await;
    let long = "word ".repeat(150).trim_end().to_string();
    h.events
        .send(Event::NewMessage(message(GROUP, 8_000, &bob(), &long)))
        .unwrap();
    let seen = h
        .until(|m| m.command == "BATCH" && m.param(0).starts_with('-'))
        .await;
    let parts: Vec<&Message> = seen.iter().filter(|m| m.command == "PRIVMSG").collect();
    assert_eq!(parts.len(), 2, "{}", lines(&seen));
    assert!(parts[1].tag("draft/multiline-concat").is_some());
    assert_eq!(format!("{}{}", parts[0].param(1), parts[1].param(1)), long);
}

#[tokio::test]
async fn our_edit_is_announced_once_whichever_copy_arrives_first() {
    let f = fake();
    f.0.lock()
        .unwrap()
        .messages
        .insert(GROUP.into(), vec![message(GROUP, 1_000, &me(), "helo")]);
    let mut h = start(f).await;
    h.ready().await;
    h.send("CHATHISTORY LATEST #Launch-plans * 10").await;
    h.until(|m| m.command == "BATCH" && m.param(0).starts_with('-'))
        .await;
    h.send("@label=e;draft/target-msgid=1000;draft/edit-text=hello EDIT #Launch-plans")
        .await;
    h.until(|m| m.command == "EDIT").await;
    // Teams' own edit event for the same text follows.
    let mut edited = message(GROUP, 1_000, &me(), "hello");
    edited.version = 9_000;
    edited.edited = true;
    h.events.send(Event::MessageEdited(edited)).unwrap();
    h.events
        .send(Event::NewMessage(message(GROUP, 2_000, &bob(), "marker")))
        .unwrap();
    let seen = h
        .until(|m| m.command == "PRIVMSG" && m.param(1) == "marker")
        .await;
    assert!(
        !seen.iter().any(|m| m.command == "EDIT"),
        "{}",
        lines(&seen)
    );
}

#[tokio::test]
async fn part_hides_a_team_channel_in_teams_and_hidden_channels_are_not_joined() {
    let f = fake();
    {
        let mut st = f.0.lock().unwrap();
        let mut hidden = st.user_state.teams[0].channels[0].clone();
        hidden.id = ConversationId("19:hidden@thread.tacv2".into());
        hidden.name = "Old".into();
        hidden.general = false;
        hidden.interest = Some("NotInterested".into());
        st.user_state.teams[0].channels.push(hidden);
    }
    let mut h = start(f).await;
    let seen = h.ready().await;
    let text = lines(&seen);
    assert!(text.contains("JOIN #Engineering/General"), "{text}");
    assert!(!text.contains("#Engineering/Old"), "{text}");
    h.send("PART #Engineering/General").await;
    let c = h
        .wait_call(|c| matches!(c, Call::HiddenChannel { .. }))
        .await;
    assert_eq!(
        c,
        Call::HiddenChannel {
            conv: CHANNEL.into()
        }
    );
}

/// A fake with a second, hidden channel in the Engineering team.
fn fake_with_hidden_channel() -> Fake {
    let f = fake();
    {
        let mut st = f.0.lock().unwrap();
        let mut hidden = st.user_state.teams[0].channels[0].clone();
        hidden.id = ConversationId("19:old@thread.tacv2".into());
        hidden.name = "Old Topics".into();
        hidden.general = false;
        hidden.channel_type = Some("0".into());
        hidden.interest = Some("NotInterested".into());
        st.user_state.teams[0].channels.push(hidden);
    }
    f
}

fn server_says(seen: &[Message]) -> Vec<String> {
    seen.iter()
        .filter(|m| m.command == "PRIVMSG" && m.source_nick() == Some("Server"))
        .map(|m| m.param(1).to_string())
        .collect()
}

#[tokio::test]
async fn joining_a_hidden_channel_shows_it_in_teams() {
    let mut h = start(fake_with_hidden_channel()).await;
    let seen = h.ready().await;
    assert!(!lines(&seen).contains("#Engineering/Old-Topics"));
    h.send("JOIN #Engineering/Old-Topics").await;
    h.until(|m| m.command == "JOIN" && m.param(0) == "#Engineering/Old-Topics")
        .await;
    let c = h
        .wait_call(|c| matches!(c, Call::ShownChannel { .. }))
        .await;
    assert_eq!(
        c,
        Call::ShownChannel {
            conv: "19:old@thread.tacv2".into()
        }
    );
}

#[tokio::test]
async fn server_join_picks_the_only_match_or_lists_them() {
    let mut h = start(fake_with_hidden_channel()).await;
    h.ready().await;
    h.send("PRIVMSG Server :join old top").await;
    let seen = h
        .until(|m| m.command == "JOIN" && m.param(0) == "#Engineering/Old-Topics")
        .await;
    assert!(seen.iter().all(|m| m.command != "FAIL"));
    h.send("PRIVMSG Server :join engineering").await;
    let seen = h
        .until(|m| m.source_nick() == Some("Server") && m.param(1).contains("matches"))
        .await;
    let said = server_says(&seen).join("\n");
    assert!(
        said.contains("#Engineering/General") && said.contains("#Engineering/Old-Topics"),
        "{said}"
    );
}

#[tokio::test]
async fn a_mistyped_join_gets_a_suggestion() {
    let mut h = start(fake()).await;
    h.ready().await;
    h.send("JOIN #launch").await;
    let seen = h.until(|m| m.source_nick() == Some("Server")).await;
    // The only match is already joined: Server says so, and it is no error.
    assert!(
        server_says(&seen)
            .iter()
            .any(|s| s.contains("#Launch-plans")),
        "{}",
        lines(&seen)
    );
    assert!(seen.iter().all(|m| m.command != "403"), "{}", lines(&seen));
    // Nothing matches: an error and a note from Server.
    h.send("JOIN #zzzz").await;
    let seen = h.until(|m| m.command == "403").await;
    assert!(
        server_says(&seen)
            .iter()
            .any(|s| s.contains("Nothing matches")),
        "{}",
        lines(&seen)
    );
}

#[tokio::test]
async fn server_hide_hides_a_one_to_one_by_name() {
    let mut h = start(fake()).await;
    h.ready().await;
    // Bob's nick exists once his 1:1 has been seen.
    h.events
        .send(Event::NewMessage(message(DM, 1_000, &bob(), "hi")))
        .unwrap();
    h.until(|m| m.command == "PRIVMSG" && m.param(1) == "hi")
        .await;
    h.send("PRIVMSG Server :hide bob").await;
    let c = h.wait_call(|c| matches!(c, Call::Hidden { .. })).await;
    assert_eq!(c, Call::Hidden { conv: DM.into() });
}

#[tokio::test]
async fn server_hide_finds_a_one_to_one_nobody_has_written_in_yet() {
    let mut h = start(fake()).await;
    h.ready().await;
    h.send("PRIVMSG Server :hide builder").await;
    let c = h.wait_call(|c| matches!(c, Call::Hidden { .. })).await;
    assert_eq!(c, Call::Hidden { conv: DM.into() });
}

#[tokio::test]
async fn server_hide_finds_an_old_one_to_one_only_csa_lists() {
    const OLD: &str = "19:me_carol@unq.gbl.spaces";
    let carol = Mri("8:orgid:carol".into());
    let f = fake();
    {
        let mut st = f.0.lock().unwrap();
        st.names.insert(carol.clone(), "Carol Danvers".into());
        st.user_state.chats.push(ic3_teams::ChatSummary {
            id: ConversationId(OLD.into()),
            title: None,
            kind: ic3_teams::ChatKind::OneOnOne,
            // CSA's own flag, which Teams' list does not follow.
            hidden: true,
            muted: false,
            members: vec![
                ic3_teams::ChatMember {
                    mri: me(),
                    role: None,
                    friendly_name: None,
                },
                ic3_teams::ChatMember {
                    mri: carol.clone(),
                    role: None,
                    friendly_name: None,
                },
            ],
            last_message_at: None,
        });
    }
    let mut h = start(f).await;
    h.ready().await;
    // The name arrives from the startup lookup.
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;
    h.send("PRIVMSG Server :hide carol").await;
    let c = h.wait_call(|c| matches!(c, Call::Hidden { .. })).await;
    assert_eq!(c, Call::Hidden { conv: OLD.into() });
}

#[tokio::test]
async fn the_cutoff_leaves_and_joins_chats_and_is_kept() {
    const OLD: &str = "19:old-group@thread.v2";
    let f = fake();
    {
        let mut st = f.0.lock().unwrap();
        let mut old = conversation(OLD, ConversationKind::Group, Some("Old project"), vec![]);
        old.version = ic3_teams::now_ms() - 200 * 24 * 3600 * 1000;
        st.conversations.push(old);
    }
    let dir = tempfile::tempdir().unwrap();
    let settings = dir.path().join("settings.json");
    let (ev_tx, ev_rx) = mpsc::unbounded_channel();
    f.0.lock().unwrap().events = Some(ev_rx);
    let (in_tx, in_rx) = mpsc::channel(64);
    let cfg = GatewayConfig {
        settings_path: Some(settings.clone()),
        ..GatewayConfig::default()
    };
    tokio::spawn(run(
        Arc::new(f.clone()),
        cfg,
        Arc::new(MediaTable::new(None)),
        in_rx,
    ));
    let (out_tx, out_rx) = mpsc::channel(4096);
    let mut c = Caps::default();
    c.insert("message-tags");
    in_tx
        .send(Input::Attach {
            id: 1,
            nick: "ada".into(),
            caps: c,
            out: out_tx,
            close: tokio_util::sync::CancellationToken::new(),
        })
        .await
        .unwrap();
    let mut h = Harness {
        fake: f,
        inputs: in_tx,
        events: ev_tx,
        out: out_rx,
    };
    // 60 days by default: the 200-day-old chat is not joined.
    let seen = h.ready().await;
    assert!(lines(&seen).contains("JOIN #Launch-plans"));
    assert!(!lines(&seen).contains("#Old-project"));
    h.send("CUTOFF off").await;
    let seen = h
        .until(|m| m.source_nick() == Some("Server") && m.param(1).contains("cutoff is now"))
        .await;
    assert!(
        lines(&seen).contains("JOIN #Old-project"),
        "{}",
        lines(&seen)
    );
    assert!(server_says(&seen).last().unwrap().contains("joined 1"));
    h.send("PRIVMSG Server :cutoff 30d").await;
    let seen = h
        .until(|m| m.source_nick() == Some("Server") && m.param(1).contains("cutoff is now"))
        .await;
    assert!(
        lines(&seen).contains("PART #Old-project"),
        "{}",
        lines(&seen)
    );
    assert_eq!(
        crate::settings::Settings::load(&settings).cutoff(),
        Some(Some(std::time::Duration::from_secs(30 * 24 * 3600)))
    );
    h.send("PRIVMSG Server :cutoff").await;
    let seen = h.until(|m| m.source_nick() == Some("Server")).await;
    assert!(
        server_says(&seen)[0].contains("30 days"),
        "{}",
        lines(&seen)
    );
}

#[tokio::test]
async fn team_channels_arrive_when_a_failed_csa_load_is_retried() {
    let f = fake();
    f.0.lock().unwrap().user_state_failures = 1;
    let mut h = start(f).await;
    let seen = h.ready().await;
    assert!(lines(&seen).contains("JOIN #Launch-plans"));
    assert!(!lines(&seen).contains("#Engineering/General"));
    let seen = h
        .until(|m| m.source_nick() == Some("Server") && m.param(1).contains("Team channels loaded"))
        .await;
    assert!(
        lines(&seen).contains("JOIN #Engineering/General"),
        "{}",
        lines(&seen)
    );
}

/// Asks for the latest history of #Launch-plans; returns what was read up
/// to the end of the batch.
async fn history(h: &mut Harness, label: &str) -> Vec<Message> {
    h.send(&format!(
        "@label={label} CHATHISTORY LATEST #Launch-plans * 10"
    ))
    .await;
    h.until(|m| m.command == "BATCH" && m.param(0).starts_with('-') && m.tag("batch").is_none())
        .await
}

#[tokio::test]
async fn history_inside_the_window_is_answered_from_memory() {
    let f = fake();
    f.0.lock().unwrap().messages.insert(
        GROUP.into(),
        vec![
            message(GROUP, 1_000, &bob(), "a"),
            message(GROUP, 2_000, &bob(), "b"),
        ],
    );
    let mut h = start(f).await;
    h.ready().await;
    let fetches = |h: &Harness| h.fake.0.lock().unwrap().message_fetches;
    history(&mut h, "h1").await;
    assert_eq!(fetches(&h), 1);
    // A live message keeps the window complete; it is in the next answer.
    h.events
        .send(Event::NewMessage(message(GROUP, 3_000, &bob(), "c")))
        .unwrap();
    h.until(|m| m.command == "PRIVMSG" && m.param(1) == "c")
        .await;
    let seen = history(&mut h, "h2").await;
    assert_eq!(fetches(&h), 1, "answered from memory");
    let texts: Vec<&str> = seen
        .iter()
        .filter(|m| m.command == "PRIVMSG")
        .map(|m| m.param(1))
        .collect();
    assert_eq!(texts, vec!["a", "b", "c"]);
    // A dropped push connection may have lost events: back to Teams. (A
    // marker after it shows the event was handled; events stay in order.)
    h.events
        .send(Event::Disconnected {
            reason: ic3_teams::DisconnectReason::SocketError,
            retry_in: std::time::Duration::from_secs(1),
        })
        .unwrap();
    h.events
        .send(Event::NewMessage(message(DM, 4_000, &bob(), "marker")))
        .unwrap();
    h.until(|m| m.command == "PRIVMSG" && m.param(1) == "marker")
        .await;
    history(&mut h, "h3").await;
    assert_eq!(fetches(&h), 2);
}

#[tokio::test]
async fn history_and_read_markers_for_server_or_a_stale_name_are_quiet() {
    let mut h = start(fake()).await;
    h.ready().await;
    for target in ["Server", "user-gone1"] {
        h.send(&format!("@label=m MARKREAD {target}")).await;
        let seen = h.until(|m| m.tag("label") == Some("m")).await;
        let m = seen.last().unwrap();
        assert_eq!(
            (m.command.as_str(), m.param(1)),
            ("MARKREAD", "*"),
            "{}",
            lines(&seen)
        );
        h.send(&format!("@label=c CHATHISTORY LATEST {target} * 50"))
            .await;
        let seen = h
            .until(|m| {
                m.command == "BATCH" && m.param(0).starts_with('-') && m.tag("batch").is_none()
            })
            .await;
        assert!(
            seen.iter()
                .all(|m| m.command != "401" && m.command != "FAIL"),
            "{}",
            lines(&seen)
        );
    }
}

fn presence(who: &Mri, availability: &str, activity: &str) -> ic3_teams::Presence {
    ic3_teams::Presence {
        mri: who.clone(),
        availability: availability.into(),
        activity: activity.into(),
        last_active: None,
        out_of_office: false,
    }
}

#[tokio::test]
async fn teams_presence_is_away_status_and_monitor() {
    let f = fake();
    f.0.lock()
        .unwrap()
        .presence
        .insert(bob(), presence(&bob(), "Busy", "InAMeeting"));
    let mut caps = ALL_CAPS.to_vec();
    caps.push("away-notify");
    let mut h = start_with(f, &caps).await;
    h.ready().await;
    let seen = h.until(|m| m.command == "AWAY").await;
    let m = seen.last().unwrap();
    assert_eq!(m.source.as_deref(), Some("BobBuilder!BobBuilder@teams"));
    assert_eq!(m.param(0), "In a meeting");
    assert!(h.fake.0.lock().unwrap().subscribed.contains(&bob()));

    h.send("WHO #Launch-plans").await;
    let seen = h.until(|m| m.command == "315").await;
    let flags: Vec<(&str, &str)> = seen
        .iter()
        .filter(|m| m.command == "352")
        .map(|m| (m.param(5), m.param(6)))
        .collect();
    assert!(flags.contains(&("BobBuilder", "G")), "{}", lines(&seen));
    assert!(flags.contains(&("AdaLovelace", "H")), "{}", lines(&seen));
    h.send("WHOIS BobBuilder").await;
    let seen = h.until(|m| m.command == "318").await;
    assert!(
        seen.iter()
            .any(|m| m.command == "301" && m.param(2) == "In a meeting"),
        "{}",
        lines(&seen)
    );

    h.send("MONITOR + BobBuilder,nobody").await;
    let seen = h.until(|m| m.command == "731").await;
    assert!(
        seen.iter()
            .any(|m| m.command == "730" && m.param(1) == "BobBuilder!BobBuilder@teams"),
        "{}",
        lines(&seen)
    );
    assert_eq!(seen.last().unwrap().param(1), "nobody");

    // Pushed changes: back to available, then offline.
    h.events
        .send(Event::Presence(vec![presence(
            &bob(),
            "Available",
            "Available",
        )]))
        .unwrap();
    let seen = h.until(|m| m.command == "AWAY").await;
    assert!(seen.last().unwrap().params.is_empty(), "{}", lines(&seen));
    h.events
        .send(Event::Presence(vec![presence(
            &bob(),
            "Offline",
            "Offline",
        )]))
        .unwrap();
    let seen = h.until(|m| m.command == "731").await;
    assert!(
        seen.iter()
            .any(|m| m.command == "AWAY" && m.param(0) == "Offline"),
        "{}",
        lines(&seen)
    );
    assert_eq!(seen.last().unwrap().param(1), "BobBuilder");
}

#[tokio::test]
async fn away_lines_need_away_notify() {
    let f = fake();
    f.0.lock()
        .unwrap()
        .presence
        .insert(bob(), presence(&bob(), "Away", "Away"));
    let mut h = start(f).await;
    h.ready().await;
    for _ in 0..50 {
        if h.fake.0.lock().unwrap().subscribed.contains(&bob()) {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    h.send("WHO BobBuilder").await;
    let seen = h.until(|m| m.command == "315").await;
    assert!(seen.iter().all(|m| m.command != "AWAY"), "{}", lines(&seen));
    let who = seen.iter().find(|m| m.command == "352").unwrap();
    assert_eq!(who.param(6), "G");
}

#[tokio::test]
async fn a_parted_chat_stays_parted_through_updates_and_cutoff_changes() {
    let mut h = start(fake()).await;
    h.ready().await;
    h.send("PART #Launch-plans").await;
    h.until(|m| m.command == "PART").await;
    h.wait_call(|c| matches!(c, Call::Hidden { .. })).await;
    // Teams echoes the hide as a conversation update, which need not say
    // it is hidden.
    h.events
        .send(Event::ConversationUpdated(conversation(
            GROUP,
            ConversationKind::Group,
            Some("Launch plans"),
            vec![member(&me(), "Ada Lovelace"), member(&bob(), "Bob Builder")],
        )))
        .unwrap();
    h.events
        .send(Event::NewMessage(message(DM, 6_000, &bob(), "marker")))
        .unwrap();
    let seen = h
        .until(|m| m.command == "PRIVMSG" && m.param(1) == "marker")
        .await;
    let text = lines(&seen);
    assert!(!text.contains("JOIN #Launch-plans"), "{text}");
    // Nor does a changed cutoff bring it back.
    h.send("CUTOFF off").await;
    let seen = h
        .until(|m| m.source_nick() == Some("Server") && m.param(1).contains("cutoff is now"))
        .await;
    let text = lines(&seen);
    assert!(!text.contains("JOIN #Launch-plans"), "{text}");
    assert!(
        server_says(&seen).last().unwrap().contains("joined 0"),
        "{text}"
    );
}

#[tokio::test]
async fn team_mentions_read_as_teams_shows_them() {
    let mut h = start(fake()).await;
    h.ready().await;
    let mention = |mri: &Mri, name: &str| ic3_teams::Inline::Mention {
        mri: mri.clone(),
        name: name.into(),
    };
    let mut m = message(GROUP, 7_000, &bob(), "");
    m.body = RichText(vec![ic3_teams::Block::Paragraph(vec![
        mention(&Mri("19:team@thread.tacv2".into()), "Example Team"),
        ic3_teams::Inline::Text(" and ".into()),
        mention(&me(), "Ada Lovelace"),
        ic3_teams::Inline::Text(": lunch?".into()),
    ])]);
    h.events.send(Event::NewMessage(m)).unwrap();
    let seen = h
        .until(|m| m.command == "PRIVMSG" && m.param(0) == "#Launch-plans")
        .await;
    assert_eq!(
        seen.last().unwrap().param(1),
        "@Example Team and AdaLovelace: lunch?"
    );
}
