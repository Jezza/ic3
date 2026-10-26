//! Push events: the `Event` stream's item type, the mapping from trouter
//! `EventMessage` payloads to events, and duplicate suppression (spec §4.7).
//!
//! Mapping never panics: a payload that cannot be parsed, a non-displayable
//! message type or an unknown `resourceType` is logged at debug and dropped.

use std::collections::VecDeque;
use std::sync::Arc;
use std::time::Duration;

use serde_json::{Map, Value};

use crate::api::wire::{is_set, lenient_i64, parse_conversation, parse_message, parse_thread};
use crate::redact::redact_text;
use crate::types::{
    Conversation, ConversationId, ConversationKind, Message, MessageId, Mri, ReactionSummary,
    ReadHorizon, conversation_id_from_link, is_displayable, split_thread_id,
};

/// One item of the event stream.
#[derive(Clone, Debug, PartialEq)]
pub enum Event {
    /// A new displayable message (including the echo of our own sends; match
    /// those by `client_message_id`).
    NewMessage(Message),
    /// A message was edited; `Message::id` is the original message's id.
    MessageEdited(Message),
    /// A message was deleted. `version` is the message's version as of the
    /// delete (0 if unknown): an older server copy must not resurrect it.
    MessageDeleted {
        conversation: ConversationId,
        message: MessageId,
        version: i64,
    },
    /// The full current reaction set of a message. `version` is the
    /// message's version it came with (0 if unknown): an older server copy
    /// must not roll it back, nor this event an newer copy's set.
    ReactionsChanged {
        conversation: ConversationId,
        message: MessageId,
        reactions: Vec<ReactionSummary>,
        version: i64,
    },
    /// The user's read horizon in a conversation moved.
    ReadStateChanged {
        conversation: ConversationId,
        horizon: ReadHorizon,
    },
    /// Conversation metadata changed. `version == 0` with no topic is a
    /// "please refetch" marker (from `ThreadActivity/*` messages): consumers
    /// fill only missing fields from it and re-run `thread()`.
    ConversationUpdated(Conversation),
    /// Someone is typing in `conversation` (a `Control/Typing` message).
    /// Emitted for everyone, the signed-in user included (the event mapping
    /// does not know who that is): [`Store`](crate::Store) ignores the
    /// user's own, and consumers that skip the store must too. Momentary:
    /// dropped when the event channel is full, never caught up.
    Typing {
        conversation: ConversationId,
        from: Mri,
        from_name: Option<String>,
    },
    /// Presence changed for these people (pushed for those subscribed with
    /// [`Api::subscribe_presence`](crate::Api::subscribe_presence)).
    Presence(Vec<crate::types::Presence>),
    /// Registered (again) with trouter: catch up now.
    Connected,
    /// Events may have been lost: catch up now.
    NeedsCatchUp { reason: CatchUpReason },
    /// The push connection dropped; a reconnect is attempted after `retry_in`.
    Disconnected {
        reason: DisconnectReason,
        retry_in: Duration,
    },
    /// The stored session is no longer usable; sign in again.
    ReauthRequired,
}

/// Why [`Event::NeedsCatchUp`] was emitted.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CatchUpReason {
    /// trouter reported `trouter.message_loss`.
    MessageLoss,
    /// The event channel was full and events were dropped.
    Overflow,
}

/// Why [`Event::Disconnected`] was emitted.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DisconnectReason {
    /// The server sent a disconnect frame.
    ServerDisconnect,
    /// The socket closed or errored, or the connection could not be set up.
    SocketError,
    /// No inbound traffic within the silence timeout.
    Silence,
    /// Registration with the registrar failed.
    RegistrationFailed,
    /// Deliberate reconnect after a Skype-token renewal.
    TokenRenewal,
}

/// A trouter `/messaging` payload with `type == "EventMessage"`.
#[derive(Clone, Debug)]
pub struct EventMessage {
    pub id: Option<i64>,
    pub time: Option<String>,
    pub resource_type: String,
    pub resource: Value,
    /// The whole payload, as received.
    pub raw: Value,
}

impl EventMessage {
    /// `None` unless `body.type == "EventMessage"`.
    pub fn parse(body: &Value) -> Option<EventMessage> {
        (body.get("type")?.as_str()? == "EventMessage").then(|| EventMessage {
            id: body.get("id").and_then(Value::as_i64),
            time: body.get("time").and_then(Value::as_str).map(String::from),
            resource_type: body
                .get("resourceType")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string(),
            resource: body.get("resource").cloned().unwrap_or(Value::Null),
            raw: body.clone(),
        })
    }
}

/// Maps one `EventMessage` to zero or more events.
///
/// Message resources are discriminated in this order: deleted
/// (`properties.deletetime`) → edited now (`properties.edittime` equal to
/// `version`, or `skypeeditedid` without `edittime`) → reactions
/// (`MessageUpdate` with `properties.emotions`) → edited earlier
/// (`properties.edittime` still present) → new displayable message, or
/// `ThreadActivity/*` as a refetch marker; anything else is dropped.
///
/// Teams keeps `properties.edittime` on every update after an edit and bumps
/// `version` for reaction changes too (live 2026-10-14), so only the edit
/// itself has `edittime == version`.
// UNVERIFIED(§9.4): resourceType → Event discrimination order (delete → edit (edittime == version) → reactions → edit (edittime) → new/thread-activity).
pub fn map_event_message(em: &EventMessage) -> Vec<Event> {
    let r = &em.resource;
    match em.resource_type.as_str() {
        "NewMessage" | "MessageUpdate" => {
            // Before parse_message: typing messages need not carry an id.
            if em.resource_type == "NewMessage"
                && r.get("messagetype").and_then(Value::as_str) == Some("Control/Typing")
            {
                return typing_event(em).into_iter().collect();
            }
            let Some(m) = parse_message(r, None) else {
                tracing::debug!(resource_type = %em.resource_type, "unparseable message resource dropped");
                return vec![];
            };
            let has_prop = |k: &str| {
                r.get("properties")
                    .and_then(|p| p.get(k))
                    .is_some_and(|v| !v.is_null())
            };
            if m.deleted {
                return vec![Event::MessageDeleted {
                    conversation: m.conversation,
                    message: m.id,
                    version: m.version,
                }];
            }
            let edittime = r.get("properties").and_then(|p| p.get("edittime"));
            let edited_now = edittime
                .and_then(lenient_i64)
                .is_some_and(|t| r.get("version").and_then(lenient_i64) == Some(t));
            let skype_edit = r
                .get("skypeeditedid")
                .and_then(Value::as_str)
                .is_some_and(|s| !s.is_empty());
            if edited_now || (skype_edit && !is_set(edittime)) {
                return vec![Event::MessageEdited(m)];
            }
            if em.resource_type == "MessageUpdate" && has_prop("emotions") {
                return vec![Event::ReactionsChanged {
                    conversation: m.conversation,
                    message: m.id,
                    reactions: m.reactions,
                    version: m.version,
                }];
            }
            if m.edited {
                return vec![Event::MessageEdited(m)];
            }
            if is_displayable(&m.message_type) {
                return vec![Event::NewMessage(m)];
            }
            if m.message_type.starts_with("ThreadActivity/") {
                return vec![Event::ConversationUpdated(stub_conversation(
                    &m.conversation,
                ))];
            }
            tracing::debug!(message_type = %redact_text(&m.message_type), "non-displayable message event dropped");
            vec![]
        }
        "ConversationUpdate" => {
            let Some(c) = parse_conversation(r) else {
                tracing::debug!("unparseable conversation resource dropped");
                return vec![];
            };
            let mut out = Vec::with_capacity(2);
            if let Some(horizon) = c.read_horizon.clone() {
                out.push(Event::ReadStateChanged {
                    conversation: c.id.clone(),
                    horizon,
                });
            }
            out.push(Event::ConversationUpdated(c));
            out
        }
        "ThreadUpdate" => {
            let Some(t) = parse_thread(r) else {
                tracing::debug!("unparseable thread resource dropped");
                return vec![];
            };
            // UNVERIFIED(§9.4): the thread resource is assumed to carry the kind keys
            // (`productThreadType`, `uniquerosterthread`) under `properties`; conversations carry
            // them under `threadProperties`.
            let props = r
                .get("properties")
                .and_then(Value::as_object)
                .cloned()
                .unwrap_or_default();
            vec![Event::ConversationUpdated(Conversation {
                kind: ConversationKind::classify(&t.id.0, &props),
                id: t.id,
                topic: t.topic,
                members: Some(t.members),
                last_message: None,
                read_horizon: None,
                version: r.get("version").and_then(lenient_i64).unwrap_or(0),
                hidden: false,
                extra: Map::new(),
            })]
        }
        other => {
            tracing::debug!(resource_type = %redact_text(other), "unknown event resourceType dropped");
            vec![]
        }
    }
}

/// A `Control/Typing` message → [`Event::Typing`]. Needs only the
/// conversation (`conversationLink`, else `to` — a link or a bare id — else
/// the EventMessage's `resourceLink`; a channel thread's is its channel's)
/// and the sender; `None` without either.
fn typing_event(em: &EventMessage) -> Option<Event> {
    let r = &em.resource;
    let s = |k: &str| r.get(k).and_then(Value::as_str).filter(|s| !s.is_empty());
    let conversation = s("conversationLink")
        .and_then(conversation_id_from_link)
        .or_else(|| {
            s("to").map(|t| conversation_id_from_link(t).unwrap_or_else(|| split_thread_id(t).0))
        })
        .or_else(|| {
            em.raw
                .get("resourceLink")
                .and_then(Value::as_str)
                .and_then(conversation_id_from_link)
        })?;
    let from = Mri::from_contact_url(s("from")?)?;
    let from_name = s("fromDisplayNameInToken")
        .or_else(|| s("imdisplayname"))
        .map(str::to_owned);
    Some(Event::Typing {
        conversation,
        from,
        from_name,
    })
}

/// A "please refetch" conversation: id and kind only, `version` 0.
fn stub_conversation(id: &ConversationId) -> Conversation {
    Conversation {
        id: id.clone(),
        kind: ConversationKind::classify(&id.0, &Map::new()),
        topic: None,
        members: None,
        last_message: None,
        read_horizon: None,
        version: 0,
        hidden: false,
        extra: Map::new(),
    }
}

/// The last `cap` EventMessage `(id, time)` pairs, to drop exact trouter
/// duplicates (spec §4.7).
pub(crate) struct DedupRing {
    cap: usize,
    seen: VecDeque<(Option<i64>, Option<String>)>,
}

impl DedupRing {
    pub fn new(cap: usize) -> Self {
        Self {
            cap,
            seen: VecDeque::with_capacity(cap),
        }
    }

    /// `true` if `(id, time)` was seen within the last `cap` events (a
    /// duplicate); otherwise records it and returns `false`. Events with
    /// neither an id nor a time are never treated as duplicates.
    pub fn seen_or_insert(&mut self, id: Option<i64>, time: Option<&str>) -> bool {
        if id.is_none() && time.is_none() {
            return false;
        }
        if self
            .seen
            .iter()
            .any(|(i, t)| *i == id && t.as_deref() == time)
        {
            return true;
        }
        if self.seen.len() == self.cap {
            self.seen.pop_front();
        }
        self.seen.push_back((id, time.map(String::from)));
        false
    }
}

/// One decoded `/messaging` EventMessage as seen by trouter, for the raw event
/// hook (smoke `events` prints these).
#[derive(Clone, Debug)]
pub struct RawEvent {
    pub url: String,
    pub resource_type: String,
    pub time: Option<String>,
    pub id: Option<i64>,
    /// The payload, redacted.
    pub body: Value,
    /// `Debug` names of the events it was mapped to.
    pub mapped: Vec<String>,
}

/// Called for every new (non-duplicate) EventMessage, after mapping.
pub type RawEventHook = Arc<dyn Fn(&RawEvent) + Send + Sync>;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::Reaction;
    use serde_json::json;

    fn em(name: &str) -> EventMessage {
        EventMessage::parse(&serde_json::from_str(name).unwrap()).unwrap()
    }

    #[test]
    fn discriminator_order() {
        assert!(
            matches!(map_event_message(&em(fixture!("9.04-event-payloads/new-message.json"))).as_slice(), [Event::NewMessage(m)] if m.id.0 == "1760346000000")
        );
        assert!(
            matches!(map_event_message(&em(fixture!("9.04-event-payloads/message-edit.json"))).as_slice(), [Event::MessageEdited(m)] if m.edited && m.id.0 == "1760346000000")
        );
        assert!(
            matches!(map_event_message(&em(fixture!("9.04-event-payloads/message-delete.json"))).as_slice(), [Event::MessageDeleted { message, .. }] if message.0 == "1760346000000")
        );
        assert!(
            matches!(map_event_message(&em(fixture!("9.04-event-payloads/reaction-add.json"))).as_slice(), [Event::ReactionsChanged { reactions, .. }] if reactions.len() == 1 && reactions[0].reaction == Reaction::Like)
        );
        let evs = map_event_message(&em(fixture!("9.04-event-payloads/read-horizon.json")));
        assert!(evs.iter().any(|e| matches!(e, Event::ReadStateChanged { horizon, .. } if horizon.message_id.0 == "1760345000000")));
        assert!(evs.iter().any(
            |e| matches!(e, Event::ConversationUpdated(c) if c.kind == ConversationKind::OneToOne)
        ));
        assert!(
            matches!(map_event_message(&em(fixture!("9.04-event-payloads/topic-update.json"))).as_slice(), [Event::ConversationUpdated(c)] if c.id.0 == "19:groupgroupgroup@thread.v2" && c.topic.is_none())
        );
        assert!(
            matches!(map_event_message(&em(fixture!("9.04-event-payloads/thread-update.json"))).as_slice(), [Event::ConversationUpdated(c)] if c.topic.as_deref() == Some("Platform team (renamed)"))
        );
        assert!(matches!(
            map_event_message(&em(fixture!("9.04-event-payloads/typing.json"))).as_slice(),
            [Event::Typing { .. }]
        ));
        let mut v: serde_json::Value =
            serde_json::from_str(fixture!("9.04-event-payloads/new-message.json")).unwrap();
        v["resourceType"] = "UserPresence".into();
        assert!(map_event_message(&EventMessage::parse(&v).unwrap()).is_empty());
        v["type"] = "Other".into();
        assert!(EventMessage::parse(&v).is_none());
    }

    /// Payloads matching several rules take the first one in §4.7's order.
    #[test]
    fn discriminator_precedence() {
        let base: Value =
            serde_json::from_str(fixture!("9.04-event-payloads/reaction-add.json")).unwrap();
        let map = |f: &dyn Fn(&mut Value)| {
            let mut v = base.clone();
            f(&mut v);
            map_event_message(&EventMessage::parse(&v).unwrap())
        };
        // delete wins over edit and reactions
        let evs = map(&|v| {
            v["resource"]["properties"]["deletetime"] = "1760346120000".into();
            v["resource"]["properties"]["edittime"] = "1760346060000".into();
        });
        assert!(matches!(evs.as_slice(), [Event::MessageDeleted { .. }]));
        // edit wins over reactions
        let evs = map(&|v| v["resource"]["skypeeditedid"] = "1760346000000".into());
        assert!(matches!(evs.as_slice(), [Event::MessageEdited(_)]));
        // emotions only mean ReactionsChanged on a MessageUpdate
        let evs = map(&|v| v["resourceType"] = "NewMessage".into());
        assert!(matches!(evs.as_slice(), [Event::NewMessage(m)] if m.reactions.len() == 1));
        let evs = map(&|v| v["resource"]["properties"]["emotions"] = Value::Null);
        assert!(matches!(evs.as_slice(), [Event::NewMessage(_)]));
        // an unparseable resource is dropped, not a panic
        assert!(map(&|v| v["resource"] = json!({"id": 5})).is_empty());
        assert!(map(&|v| v["resource"] = Value::Null).is_empty());
    }

    /// Live run 2026-10-14 (anonymised): one message edited, liked, unliked,
    /// then deleted. Teams keeps `properties.edittime` on every later update
    /// and bumps `version` for reactions too; only the edit itself has
    /// `edittime == version`. The delete's `skypeeditedid` is the message's
    /// `clientmessageid`, not its id.
    #[test]
    fn live_edit_reaction_delete_sequence() {
        let ev = |f: &str| map_event_message(&em(f));
        assert!(
            matches!(ev(fixture!("9.04-event-payloads/live-edit.json")).as_slice(),
                [Event::MessageEdited(m)] if m.id.0 == "1760346000000" && m.edited && m.version == 1_760_346_060_000),
        );
        let evs = ev(fixture!("9.04-event-payloads/live-reaction-on-edited.json"));
        assert!(
            matches!(evs.as_slice(),
                [Event::ReactionsChanged { message, reactions, version, .. }]
                    if message.0 == "1760346000000"
                        && *version == 1_760_346_120_000
                        && reactions.len() == 1
                        && reactions[0].reaction == Reaction::Like
                        && reactions[0].users.len() == 1),
            "{evs:?}"
        );
        let evs = ev(fixture!(
            "9.04-event-payloads/live-unreaction-on-edited.json"
        ));
        assert!(
            matches!(evs.as_slice(),
                [Event::ReactionsChanged { message, reactions, version, .. }]
                    if message.0 == "1760346000000"
                        && *version == 1_760_346_180_000
                        && reactions.iter().all(|r| r.users.is_empty())),
            "{evs:?}"
        );
        let evs = ev(fixture!("9.04-event-payloads/live-delete-after-edit.json"));
        assert!(
            matches!(evs.as_slice(),
                [Event::MessageDeleted { message, version, .. }]
                    if message.0 == "1760346000000" && *version == 1_760_346_240_000),
            "the original id, not the skypeeditedid (a client message id): {evs:?}"
        );
    }

    /// The edit test compares `edittime` with `version` as numbers, and an
    /// update with an older `edittime` and no reactions is still an edit.
    #[test]
    fn edit_versus_reaction_rules() {
        let base: Value =
            serde_json::from_str(fixture!("9.04-event-payloads/live-reaction-on-edited.json"))
                .unwrap();
        let map = |f: &dyn Fn(&mut Value)| {
            let mut v = base.clone();
            f(&mut v);
            map_event_message(&EventMessage::parse(&v).unwrap())
        };
        // edittime == version (number vs string) wins over emotions
        let evs = map(&|v| {
            v["resource"]["version"] = json!(1_760_346_060_000_i64);
        });
        assert!(
            matches!(evs.as_slice(), [Event::MessageEdited(_)]),
            "{evs:?}"
        );
        // an older edittime without emotions: still an edit
        let evs = map(&|v| {
            v["resource"]["properties"]
                .as_object_mut()
                .unwrap()
                .remove("emotions");
        });
        assert!(
            matches!(evs.as_slice(), [Event::MessageEdited(_)]),
            "{evs:?}"
        );
        // an older edittime with emotions on a NewMessage is not a reaction update
        let evs = map(&|v| v["resourceType"] = "NewMessage".into());
        assert!(
            matches!(evs.as_slice(), [Event::MessageEdited(_)]),
            "{evs:?}"
        );
        // skypeeditedid with no edittime and no deletetime: an edit
        let evs = map(&|v| {
            let r = &mut v["resource"];
            r["skypeeditedid"] = r["clientmessageid"].clone();
            r["properties"].as_object_mut().unwrap().remove("edittime");
        });
        assert!(
            matches!(evs.as_slice(), [Event::MessageEdited(m)] if m.id.0 == "1760346000000"),
            "{evs:?}"
        );
        // skypeeditedid next to an older edittime does not hide a reaction
        let evs = map(&|v| {
            v["resource"]["skypeeditedid"] = v["resource"]["clientmessageid"].clone();
        });
        assert!(
            matches!(evs.as_slice(), [Event::ReactionsChanged { .. }]),
            "{evs:?}"
        );
    }

    #[test]
    fn conversation_update_without_horizon_is_only_an_update() {
        let mut v: Value =
            serde_json::from_str(fixture!("9.04-event-payloads/read-horizon.json")).unwrap();
        v["resource"]["properties"]
            .as_object_mut()
            .unwrap()
            .remove("consumptionhorizon");
        let evs = map_event_message(&EventMessage::parse(&v).unwrap());
        assert!(
            matches!(evs.as_slice(), [Event::ConversationUpdated(c)] if c.version == 1_760_346_200_000)
        );
        v["resource"] = json!([]);
        assert!(map_event_message(&EventMessage::parse(&v).unwrap()).is_empty());
        v["resourceType"] = "ThreadUpdate".into();
        assert!(map_event_message(&EventMessage::parse(&v).unwrap()).is_empty());
    }

    #[test]
    fn event_message_parse_fields() {
        let v: Value =
            serde_json::from_str(fixture!("9.04-event-payloads/new-message.json")).unwrap();
        let em = EventMessage::parse(&v).unwrap();
        assert_eq!(em.id, Some(1001));
        assert_eq!(em.time.as_deref(), Some("2025-10-13T09:00:00.000Z"));
        assert_eq!(em.resource_type, "NewMessage");
        assert_eq!(em.resource, v["resource"]);
        assert_eq!(em.raw, v);
        let em = EventMessage::parse(&json!({"type": "EventMessage"})).unwrap();
        assert_eq!(
            (
                em.id,
                em.time.as_deref(),
                em.resource_type.as_str(),
                &em.resource
            ),
            (None, None, "", &Value::Null)
        );
        assert!(map_event_message(&em).is_empty());
        assert!(EventMessage::parse(&json!([])).is_none());
    }

    #[test]
    fn dedup_ring_keeps_64() {
        let mut r = DedupRing::new(64);
        assert!(!r.seen_or_insert(Some(1), Some("t1")));
        assert!(r.seen_or_insert(Some(1), Some("t1")));
        assert!(
            !r.seen_or_insert(Some(1), Some("t2")),
            "same id, different time is a different event"
        );
        for i in 2..70 {
            r.seen_or_insert(Some(i), Some("t"));
        }
        assert!(
            !r.seen_or_insert(Some(1), Some("t1")),
            "evicted after 64 newer entries"
        );
        assert!(
            !r.seen_or_insert(None, None),
            "events without id/time are never deduplicated"
        );
        assert!(!r.seen_or_insert(None, None));
    }

    #[test]
    fn a_channel_reply_event_belongs_to_the_channel() {
        let evs = map_event_message(&em(fixture!(
            "9.20-channel-threads/channel-reply-event.json"
        )));
        assert!(
            matches!(evs.as_slice(), [Event::NewMessage(m)]
                if m.conversation.0 == "19:team-general@thread.tacv2"
                    && m.thread_root.as_ref().is_some_and(|r| r.0 == "1760346000000")),
            "{evs:?}"
        );
        // typing in a thread is typing in the channel, whichever field names it
        let mut v: Value =
            serde_json::from_str(fixture!("9.04-event-payloads/typing.json")).unwrap();
        let r = resource(&mut v);
        r.remove("conversationLink");
        r.insert(
            "to".into(),
            "19:team-general@thread.tacv2;messageid=1760346000000".into(),
        );
        assert!(matches!(
            map_event_message(&EventMessage::parse(&v).unwrap()).as_slice(),
            [Event::Typing { conversation, .. }] if conversation.0 == "19:team-general@thread.tacv2"
        ));
    }

    #[test]
    fn conversation_updates_carry_the_hidden_flag() {
        let mut v: Value =
            serde_json::from_str(fixture!("9.04-event-payloads/read-horizon.json")).unwrap();
        let updated = |v: &Value| {
            map_event_message(&EventMessage::parse(v).unwrap())
                .into_iter()
                .find_map(|e| match e {
                    Event::ConversationUpdated(c) => Some(c),
                    _ => None,
                })
                .unwrap()
        };
        let c = updated(&v);
        assert!(!c.hidden && !c.hidden_in_payload(), "absent says nothing");
        v["resource"]["properties"]["hidden"] = "true".into();
        assert!(updated(&v).hidden);
        v["resource"]["properties"]["hidden"] = "false".into();
        let c = updated(&v);
        assert!(!c.hidden && c.hidden_in_payload());
    }

    fn resource(v: &mut Value) -> &mut Map<String, Value> {
        v["resource"].as_object_mut().unwrap()
    }

    #[test]
    fn typing_needs_only_a_conversation_and_a_sender() {
        let base: Value =
            serde_json::from_str(fixture!("9.04-event-payloads/typing.json")).unwrap();
        let map = |f: &dyn Fn(&mut Value)| {
            let mut v = base.clone();
            f(&mut v);
            map_event_message(&EventMessage::parse(&v).unwrap())
        };
        let bea = Mri("8:orgid:bbbbbbbb-2222-2222-2222-222222222222".into());
        let group = ConversationId("19:groupgroupgroup@thread.v2".into());
        let typing = |conv: &ConversationId, name: Option<&str>| Event::Typing {
            conversation: conv.clone(),
            from: bea.clone(),
            from_name: name.map(str::to_owned),
        };
        assert_eq!(map(&|_| {}), [typing(&group, Some("Bea Example"))]);
        // Review Focus #5: no id needed (parse_message would drop it).
        assert_eq!(
            map(&|v| {
                resource(v).remove("id");
            }),
            [typing(&group, Some("Bea Example"))]
        );
        // The name in the token wins over imdisplayname.
        assert_eq!(
            map(&|v| {
                resource(v).insert("fromDisplayNameInToken".into(), "Bea E.".into());
            }),
            [typing(&group, Some("Bea E."))]
        );
        assert_eq!(
            map(&|v| {
                resource(v).insert("imdisplayname".into(), "".into());
            }),
            [typing(&group, None)],
            "an empty name is None"
        );
        // Without conversationLink: `to`, else the EventMessage's resourceLink.
        let other = ConversationId("19:other@thread.v2".into());
        assert_eq!(
            map(&|v| {
                let r = resource(v);
                r.remove("conversationLink");
                r.insert("to".into(), "19:other@thread.v2".into());
            }),
            [typing(&other, Some("Bea Example"))]
        );
        assert_eq!(
            map(&|v| {
                resource(v).remove("conversationLink");
            }),
            [typing(&group, Some("Bea Example"))]
        );
        // Without a sender, or without any conversation: dropped.
        assert!(
            map(&|v| {
                resource(v).remove("from");
            })
            .is_empty()
        );
        assert!(
            map(&|v| {
                resource(v).remove("conversationLink");
                v.as_object_mut().unwrap().remove("resourceLink");
            })
            .is_empty()
        );
        // Only on NewMessage: a MessageUpdate typing payload stays dropped.
        assert!(map(&|v| v["resourceType"] = "MessageUpdate".into()).is_empty());
    }
}
