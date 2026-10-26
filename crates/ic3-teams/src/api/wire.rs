//! Wire structs for chatsvc payloads and their conversion into domain types
//! (spec §4.5).
//!
//! Every struct keeps the fields it does not type in `extra` (spec §4.1).
//! Parsing never panics and never fails a whole page: each `parse_*` takes one
//! JSON value and returns `None` when it lacks what the domain type needs
//! (Review Focus #1). Fields Teams sends as either strings or numbers (ids,
//! versions) are read leniently.

use std::collections::HashMap;

use serde::{Deserialize, Deserializer};
use serde_json::{Map, Value};

use crate::format::{RichText, body_from_wire, merge_split_mentions, visit_mentions};
use crate::types::{
    Attachment, AttachmentKind, Conversation, ConversationId, ConversationKind, Me, Member,
    Message, MessageId, MessageSummary, Mri, Reaction, ReactionSummary, ReadHorizon, ThreadInfo,
    parse_iso_ms, split_thread_id, thread_from_link,
};

/// Unknown fields of a wire struct.
type Extra = Map<String, Value>;

/// `GET /v1/users/ME/conversations`. Items stay raw so one malformed entry
/// cannot fail the page.
#[derive(Debug, Default, Deserialize)]
pub(crate) struct ConversationsResponse {
    #[serde(default)]
    pub conversations: Vec<Value>,
    #[serde(default, rename = "_metadata")]
    pub metadata: Option<Value>,
    #[serde(flatten)]
    #[allow(dead_code, reason = "unknown fields are kept (spec §4.1)")]
    pub extra: Extra,
}

/// `GET /v1/users/ME/conversations/{id}/messages`. Items stay raw so one
/// malformed entry cannot fail the page.
#[derive(Debug, Default, Deserialize)]
pub(crate) struct MessagesResponse {
    #[serde(default)]
    pub messages: Vec<Value>,
    #[serde(default, rename = "_metadata")]
    pub metadata: Option<Value>,
    #[serde(flatten)]
    #[allow(dead_code, reason = "unknown fields are kept (spec §4.1)")]
    pub extra: Extra,
}

/// A conversation listing entry (also a `ConversationUpdate` resource).
#[derive(Debug, Default, Deserialize)]
pub(crate) struct ConversationWire {
    #[serde(default, deserialize_with = "lenient_string")]
    pub id: Option<String>,
    #[serde(default)]
    pub version: Option<Value>,
    #[serde(default)]
    pub properties: Option<Extra>,
    #[serde(default, rename = "threadProperties")]
    pub thread_properties: Option<Extra>,
    #[serde(default, rename = "lastMessage")]
    pub last_message: Option<Value>,
    #[serde(default)]
    pub members: Option<Value>,
    #[serde(flatten)]
    pub extra: Extra,
}

/// A message (REST page item, `lastMessage`, or event resource).
#[derive(Debug, Default, Deserialize)]
pub(crate) struct MessageWire {
    #[serde(default, deserialize_with = "lenient_string")]
    pub id: Option<String>,
    #[serde(default, deserialize_with = "lenient_string")]
    pub clientmessageid: Option<String>,
    #[serde(default, rename = "conversationLink")]
    pub conversation_link: Option<String>,
    #[serde(default)]
    pub from: Option<String>,
    #[serde(default)]
    pub imdisplayname: Option<String>,
    #[serde(default)]
    pub messagetype: Option<String>,
    #[serde(default)]
    pub content: Option<String>,
    #[serde(default)]
    pub composetime: Option<String>,
    #[serde(default)]
    pub originalarrivaltime: Option<String>,
    #[serde(default)]
    pub version: Option<Value>,
    #[serde(default, deserialize_with = "lenient_string")]
    pub skypeeditedid: Option<String>,
    #[serde(default)]
    pub properties: Option<MessagePropertiesWire>,
    #[serde(flatten)]
    pub extra: Extra,
}

/// A message's `properties`. `emotions`, `files` and `mentions` are arrays,
/// or JSON strings holding one.
#[derive(Debug, Default, Deserialize)]
pub(crate) struct MessagePropertiesWire {
    #[serde(default)]
    pub edittime: Option<Value>,
    #[serde(default)]
    pub deletetime: Option<Value>,
    #[serde(default)]
    pub emotions: Option<Value>,
    #[serde(default)]
    pub files: Option<Value>,
    #[serde(default)]
    pub mentions: Option<Value>,
    #[serde(flatten)]
    #[allow(dead_code, reason = "unknown fields are kept (spec §4.1)")]
    pub extra: Extra,
}

/// One entry of `properties.emotions`.
#[derive(Debug, Default, Deserialize)]
pub(crate) struct EmotionWire {
    #[serde(default)]
    pub key: String,
    #[serde(default)]
    pub users: Vec<EmotionUserWire>,
    #[serde(flatten)]
    #[allow(dead_code, reason = "unknown fields are kept (spec §4.1)")]
    pub extra: Extra,
}

/// One user of an [`EmotionWire`] (`time` and `value` stay in `extra`).
#[derive(Debug, Default, Deserialize)]
pub(crate) struct EmotionUserWire {
    #[serde(default)]
    pub mri: Option<String>,
    #[serde(flatten)]
    #[allow(dead_code, reason = "unknown fields are kept (spec §4.1)")]
    pub extra: Extra,
}

/// One entry of `properties.files`.
#[derive(Debug, Default, Deserialize)]
pub(crate) struct FileWire {
    #[serde(default, rename = "fileName")]
    pub file_name: Option<String>,
    #[serde(default)]
    pub title: Option<String>,
    #[serde(default, rename = "objectUrl")]
    pub object_url: Option<String>,
    #[serde(default, rename = "fileUrl")]
    pub file_url: Option<String>,
    #[serde(default)]
    pub url: Option<String>,
    #[serde(default, rename = "fileType")]
    pub file_type: Option<String>,
    #[serde(flatten)]
    #[allow(dead_code, reason = "unknown fields are kept (spec §4.1)")]
    pub extra: Extra,
}

/// A member of a thread or (§9.10) a conversation listing.
#[derive(Debug, Default, Deserialize)]
pub(crate) struct MemberWire {
    #[serde(default)]
    pub id: Option<String>,
    #[serde(default, rename = "userLink")]
    pub user_link: Option<String>,
    #[serde(default, rename = "linkedMri")]
    pub linked_mri: Option<String>,
    #[serde(default)]
    pub role: Option<String>,
    #[serde(default, rename = "friendlyName")]
    pub friendly_name: Option<String>,
    /// Some payloads spell it in lower case.
    #[serde(default, rename = "friendlyname")]
    pub friendlyname: Option<String>,
    #[serde(flatten)]
    #[allow(dead_code, reason = "unknown fields are kept (spec §4.1)")]
    pub extra: Extra,
}

/// `GET /v1/threads/{id}` (also a `ThreadUpdate` resource).
#[derive(Debug, Default, Deserialize)]
pub(crate) struct ThreadWire {
    #[serde(default, deserialize_with = "lenient_string")]
    pub id: Option<String>,
    #[serde(default)]
    pub topic: Option<String>,
    #[serde(default)]
    pub properties: Option<Extra>,
    #[serde(default)]
    pub members: Option<Value>,
    #[serde(flatten)]
    #[allow(dead_code, reason = "unknown fields are kept (spec §4.1)")]
    pub extra: Extra,
}

/// `GET /v1/users/ME/properties`. `userDetails` is a JSON string (an object
/// is accepted too).
#[derive(Debug, Default, Deserialize)]
pub(crate) struct MeWire {
    #[serde(default, rename = "skypeName")]
    pub skype_name: Option<String>,
    #[serde(default, rename = "primaryMemberName")]
    pub primary_member_name: Option<String>,
    #[serde(default, rename = "userDetails")]
    pub user_details: Option<Value>,
    #[serde(flatten)]
    #[allow(dead_code, reason = "unknown fields are kept (spec §4.1)")]
    pub extra: Extra,
}

/// A number, or a string holding one.
pub(crate) fn lenient_i64(v: &Value) -> Option<i64> {
    match v {
        Value::Number(n) => n.as_i64(),
        Value::String(s) => s.parse().ok(),
        _ => None,
    }
}

/// Deserializes a string or a number as a string; anything else is `None`.
fn lenient_string<'de, D: Deserializer<'de>>(d: D) -> Result<Option<String>, D::Error> {
    Ok(match Value::deserialize(d)? {
        Value::String(s) => Some(s),
        Value::Number(n) => Some(n.to_string()),
        _ => None,
    })
}

/// `Some` non-empty string.
fn non_empty(s: Option<&str>) -> Option<String> {
    s.filter(|s| !s.is_empty()).map(str::to_owned)
}

/// A property that is set: present, not null and not an empty string.
pub(crate) fn is_set(v: Option<&Value>) -> bool {
    match v {
        None | Some(Value::Null) => false,
        Some(Value::String(s)) => !s.is_empty(),
        Some(_) => true,
    }
}

/// The items of an array, or of a JSON string holding one; otherwise none.
fn json_list(v: Option<&Value>) -> Vec<Value> {
    match v {
        Some(Value::Array(a)) => a.clone(),
        Some(Value::String(s)) => serde_json::from_str(s).unwrap_or_default(),
        _ => Vec::new(),
    }
}

/// The message's id: always `id`. An edited or deleted message keeps its
/// original `id`; `skypeeditedid` (only an edit marker) holds the message's
/// `clientmessageid` (live 2026-10-14: every edited or deleted message seen,
/// in listings and in `MessageUpdate` events). `None` without an id.
fn edited_message_id(w: &MessageWire) -> Option<String> {
    non_empty(w.id.as_deref())
}

/// Converts a message payload. `None` without an id, a sender MRI, or a
/// conversation (from `conversationLink`, else `conversation_hint`). The
/// conversation is always the bare id; a channel reply's thread root comes
/// from the first `;messageid=` suffix among `conversationLink`,
/// `conversationid`, `to` and the hint, and is `None` when it names the
/// message itself (a root post). Mentions in the body are resolved to MRIs
/// through `properties.mentions`.
pub(crate) fn parse_message(
    v: &Value,
    conversation_hint: Option<&ConversationId>,
) -> Option<Message> {
    let w: MessageWire = serde_json::from_value(v.clone()).ok()?;
    non_empty(w.id.as_deref())?;
    let id = edited_message_id(&w)?;
    let from_link = w.conversation_link.as_deref().and_then(thread_from_link);
    let hint = conversation_hint.map(|c| split_thread_id(&c.0));
    let conversation = from_link
        .as_ref()
        .or(hint.as_ref())
        .map(|(c, _)| c.clone())?;
    let raw_id = |k: &str| {
        v.get(k)
            .and_then(Value::as_str)
            .and_then(|s| split_thread_id(s).1)
    };
    let thread_root = from_link
        .and_then(|(_, r)| r)
        .or_else(|| raw_id("conversationid"))
        .or_else(|| raw_id("to"))
        .or_else(|| hint.and_then(|(_, r)| r))
        .filter(|r| r.0 != id);
    let from = Mri::from_contact_url(w.from.as_deref()?)?;
    // The server clock, never the sender's: `originalarrivaltime`, else the id
    // (which is the arrival time), with `composetime` only as a last resort.
    let arrived_at = w
        .originalarrivaltime
        .as_deref()
        .and_then(parse_iso_ms)
        .or_else(|| id.parse().ok())
        .or_else(|| w.composetime.as_deref().and_then(parse_iso_ms))
        .unwrap_or(0);
    let version = w
        .version
        .as_ref()
        .and_then(lenient_i64)
        .unwrap_or(arrived_at);
    let props = w.properties.as_ref();
    let edited = non_empty(w.skypeeditedid.as_deref()).is_some()
        || props.is_some_and(|p| is_set(p.edittime.as_ref()));
    // UNVERIFIED(§9.4): a delete is recognised by `properties.deletetime` only; one that just blanks the
    // content (spec §4.7 rule 1, "empty content with a delete marker") would read as an empty message.
    let deleted = props.is_some_and(|p| is_set(p.deletetime.as_ref()));
    let message_type = w.messagetype.clone().unwrap_or_default();
    let (mut body, reply_to) = if deleted {
        (RichText::default(), None)
    } else {
        body_from_wire(&message_type, w.content.as_deref().unwrap_or(""))
    };
    let mentioned = mention_mris(props);
    if !mentioned.is_empty() {
        visit_mentions(&mut body, &mut |mri, _| {
            if let Some(real) = mentioned.get(&mri.0) {
                *mri = real.clone();
            }
        });
        merge_split_mentions(&mut body);
    }
    let mut extra = w.extra;
    // Kept raw so an edit can echo it (spec C.1).
    if let Some(ct) = &w.composetime {
        extra.insert("composetime".into(), Value::String(ct.clone()));
    }
    // `properties` is only partly typed; keep the rest reachable.
    if let Some(p) = v.get("properties") {
        extra.insert("properties".into(), p.clone());
    }
    Some(Message {
        id: MessageId(id),
        client_message_id: non_empty(w.clientmessageid.as_deref()),
        conversation,
        thread_root,
        from,
        from_name: non_empty(w.imdisplayname.as_deref()),
        arrived_at,
        version,
        edited,
        deleted,
        body,
        reply_to,
        reactions: props.map(reactions).unwrap_or_default(),
        attachments: attachments(&message_type, props),
        message_type,
        extra,
    })
}

/// `properties.mentions` → the MRI of each mention `itemid` (as text, the
/// way the body's `itemid` attribute holds it). Entries without an
/// `itemid` or an MRI are skipped.
// UNVERIFIED(§9.23): `properties.mentions` is an array, or a JSON string holding one, of `{"@type": "http://schema.skype.com/Mention", "itemid": <n>, "mri": "8:orgid:…", "mentionType": "person", "displayName": "…"}`, and the body's `itemid` is that `itemid` (a number or a string).
fn mention_mris(p: Option<&MessagePropertiesWire>) -> HashMap<String, Mri> {
    json_list(p.and_then(|p| p.mentions.as_ref()))
        .iter()
        .filter_map(|m| {
            let itemid = match m.get("itemid")? {
                Value::String(s) if !s.is_empty() => s.clone(),
                Value::Number(n) => n.to_string(),
                _ => return None,
            };
            let mri = Mri::from_contact_url(m.get("mri")?.as_str()?)?;
            Some((itemid, mri))
        })
        .collect()
}

/// `properties.emotions` → who reacted with what. User MRIs may be bare or
/// contact URLs; entries without a key are skipped.
fn reactions(p: &MessagePropertiesWire) -> Vec<ReactionSummary> {
    json_list(p.emotions.as_ref())
        .into_iter()
        .filter_map(|e| serde_json::from_value::<EmotionWire>(e).ok())
        .filter(|e| !e.key.is_empty())
        .map(|e| ReactionSummary {
            reaction: Reaction::from_key(&e.key),
            users: e
                .users
                .iter()
                .filter_map(|u| u.mri.as_deref().and_then(Mri::from_contact_url))
                .collect(),
        })
        .collect()
}

/// File types shown as images.
const IMAGE_TYPES: &[&str] = &["png", "jpg", "jpeg", "gif", "webp"];

/// `properties.files` → attachments; a `RichText/Media_*` message without
/// files gets one placeholder named after its media kind.
fn attachments(message_type: &str, p: Option<&MessagePropertiesWire>) -> Vec<Attachment> {
    let files: Vec<Attachment> = json_list(p.and_then(|p| p.files.as_ref()))
        .into_iter()
        .filter_map(|f| serde_json::from_value::<FileWire>(f).ok())
        .map(|f| {
            let name = non_empty(f.file_name.as_deref())
                .or_else(|| non_empty(f.title.as_deref()))
                .unwrap_or_else(|| "file".to_owned());
            let url = non_empty(f.object_url.as_deref())
                .or_else(|| non_empty(f.file_url.as_deref()))
                .or_else(|| non_empty(f.url.as_deref()));
            let file_type = non_empty(f.file_type.as_deref())
                .or_else(|| name.rsplit_once('.').map(|(_, ext)| ext.to_owned()));
            let is_image =
                file_type.is_some_and(|t| IMAGE_TYPES.iter().any(|i| i.eq_ignore_ascii_case(&t)));
            Attachment {
                name,
                url,
                kind: if is_image {
                    AttachmentKind::Image
                } else {
                    AttachmentKind::File
                },
            }
        })
        .collect();
    match message_type.strip_prefix("RichText/Media_") {
        Some(kind) if files.is_empty() => vec![Attachment {
            name: kind.to_owned(),
            url: None,
            kind: AttachmentKind::Other,
        }],
        _ => files,
    }
}

/// Converts a conversation's `lastMessage`. `None` without an id or sender;
/// `messagetype` defaults to `Text`.
pub(crate) fn parse_message_summary(v: &Value) -> Option<MessageSummary> {
    let w: MessageWire = serde_json::from_value(v.clone()).ok()?;
    Some(MessageSummary {
        id: MessageId(non_empty(w.id.as_deref())?),
        from: Mri::from_contact_url(w.from.as_deref()?)?,
        from_name: non_empty(w.imdisplayname.as_deref()),
        message_type: non_empty(w.messagetype.as_deref()).unwrap_or_else(|| "Text".to_owned()),
    })
}

/// Whether a conversation is hidden: Teams' "Hide" sets
/// `properties.historyHiddenTime` (ms, a string; observed 2026-10-22), and
/// the chat stays hidden until a message newer than that arrives. Older
/// payloads carry `properties.hidden` (`"true"` / `"false"` or a bool).
fn is_hidden(props: Option<&Extra>, last: Option<&MessageSummary>) -> bool {
    let get = |k: &str| props.and_then(|p| p.get(k));
    let legacy = match get("hidden") {
        Some(Value::Bool(b)) => *b,
        Some(Value::String(s)) => s.eq_ignore_ascii_case("true"),
        _ => false,
    };
    let since = get("historyHiddenTime")
        .and_then(lenient_i64)
        .filter(|t| *t > 0);
    legacy
        || since.is_some_and(|t| {
            last.and_then(|m| m.id.as_i64())
                .is_none_or(|newest| newest <= t)
        })
}

/// Converts a conversation listing entry. `None` without an id. A channel
/// thread's id (`…;messageid=…`) is taken as its channel's: a thread is not
/// a conversation of its own. `hidden`: see [`is_hidden`].
pub(crate) fn parse_conversation(v: &Value) -> Option<Conversation> {
    let w: ConversationWire = serde_json::from_value(v.clone()).ok()?;
    let (ConversationId(id), _) = split_thread_id(&non_empty(w.id.as_deref())?);
    let last_message = w.last_message.as_ref().and_then(parse_message_summary);
    let hidden = is_hidden(w.properties.as_ref(), last_message.as_ref());
    let thread_props = w.thread_properties.clone().unwrap_or_default();
    let prop =
        |m: Option<&Extra>, k: &str| non_empty(m.and_then(|m| m.get(k)).and_then(Value::as_str));
    let members = members_from_conversation(&w);
    let mut extra = w.extra;
    // properties / threadProperties are only partly typed; keep them reachable.
    for k in ["properties", "threadProperties"] {
        if let Some(p) = v.get(k) {
            extra.insert(k.into(), p.clone());
        }
    }
    Some(Conversation {
        kind: ConversationKind::classify(&id, &thread_props),
        topic: prop(Some(&thread_props), "topic"),
        members,
        last_message,
        read_horizon: prop(w.properties.as_ref(), "consumptionhorizon")
            .as_deref()
            .and_then(ReadHorizon::parse),
        version: w.version.as_ref().and_then(lenient_i64).unwrap_or(0),
        hidden,
        id: ConversationId(id),
        extra,
    })
}

/// The members included in a conversation listing, when there are any. An
/// absent or empty `members` is `None` ("not included"), so callers fall back
/// to `thread()`.
///
/// UNVERIFIED(§9.10): whether the conversations response carries `members` at
/// all; assumed to be a top-level array of thread-style members, the MRI taken
/// from `id`, else `userLink`, else `linkedMri`.
fn members_from_conversation(w: &ConversationWire) -> Option<Vec<Member>> {
    let members = members(w.members.as_ref(), |m| [&m.id, &m.user_link, &m.linked_mri]);
    (!members.is_empty()).then_some(members)
}

/// Converts the items of a `members` array, taking each MRI from the first
/// candidate field that holds one (bare or as a contact URL). Members without
/// one are skipped.
fn members(
    v: Option<&Value>,
    mri_fields: impl Fn(&MemberWire) -> [&Option<String>; 3],
) -> Vec<Member> {
    json_list(v)
        .into_iter()
        .filter_map(|m| serde_json::from_value::<MemberWire>(m).ok())
        .filter_map(|m| {
            let mri = mri_fields(&m)
                .into_iter()
                .find_map(|f| f.as_deref().and_then(Mri::from_contact_url))?;
            Some(Member {
                mri,
                role: non_empty(m.role.as_deref()),
                friendly_name: non_empty(m.friendly_name.as_deref())
                    .or_else(|| non_empty(m.friendlyname.as_deref())),
            })
        })
        .collect()
}

/// Converts a thread. `None` without an id; a channel thread's id is taken
/// as its channel's. The topic is the top-level
/// `topic`, else `properties.topic`; member MRIs come from `userLink`, else
/// `linkedMri`, else `id`.
pub(crate) fn parse_thread(v: &Value) -> Option<ThreadInfo> {
    let w: ThreadWire = serde_json::from_value(v.clone()).ok()?;
    let topic = non_empty(w.topic.as_deref()).or_else(|| {
        non_empty(
            w.properties
                .as_ref()
                .and_then(|p| p.get("topic"))
                .and_then(Value::as_str),
        )
    });
    Some(ThreadInfo {
        id: split_thread_id(&non_empty(w.id.as_deref())?).0,
        topic,
        members: members(w.members.as_ref(), |m| [&m.user_link, &m.linked_mri, &m.id]),
    })
}

/// Converts the signed-in user's properties. `None` without `skypeName`
/// (the bare `orgid:{guid}`; the MRI is `8:` + it). The display name is
/// `userDetails.name` (unless empty or just the skypeName), else its `upn`,
/// else `primaryMemberName`, else the skypeName. The flag says whether the
/// name came from `userDetails.name` (a real name, not a fallback).
pub(crate) fn parse_me(v: &Value) -> Option<(Me, bool)> {
    let w: MeWire = serde_json::from_value(v.clone()).ok()?;
    let skype = non_empty(w.skype_name.as_deref())?;
    let details = match w.user_details {
        Some(Value::String(s)) => serde_json::from_str(&s).unwrap_or(Value::Null),
        Some(d) => d,
        None => Value::Null,
    };
    let detail = |k: &str| non_empty(details.get(k).and_then(Value::as_str));
    let upn = detail("upn");
    let name = detail("name").filter(|n| *n != skype);
    let named = name.is_some();
    let display_name = name
        .or_else(|| upn.clone())
        .or_else(|| non_empty(w.primary_member_name.as_deref()))
        .unwrap_or_else(|| skype.clone());
    let mri = if skype.starts_with("8:") {
        skype
    } else {
        format!("8:{skype}")
    };
    Some((
        Me {
            mri: Mri(mri),
            display_name,
            upn,
        },
        named,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::paging::parse_page_meta;
    use serde_json::{Value, json};

    fn new_message() -> Value {
        let v: Value =
            serde_json::from_str(fixture!("9.04-event-payloads/new-message.json")).unwrap();
        v["resource"].clone()
    }

    #[test]
    fn parse_new_message_fixture() {
        let m = parse_message(&new_message(), None).unwrap();
        assert_eq!(m.id.0, "1760346000000");
        assert_eq!(m.client_message_id.as_deref(), Some("17603459999991234"));
        assert_eq!(m.conversation.0, "19:groupgroupgroup@thread.v2");
        assert_eq!(m.from.0, "8:orgid:bbbbbbbb-2222-2222-2222-222222222222");
        assert_eq!(m.from_name.as_deref(), Some("Bea Example"));
        assert_eq!(m.arrived_at, 1_760_346_000_000);
        assert_eq!(m.version, 1_760_346_000_000);
        assert!(!m.edited && !m.deleted && m.reply_to.is_none() && m.reactions.is_empty());
        assert_eq!(m.body.plain_text(), "hello world");
        assert_eq!(m.message_type, "RichText/Html");
        assert_eq!(
            m.extra.get("type").and_then(|v| v.as_str()),
            Some("Message"),
            "unknown fields kept"
        );
    }

    #[test]
    fn parse_message_keeps_the_raw_composetime() {
        let m = parse_message(&new_message(), None).unwrap();
        assert_eq!(
            m.extra.get("composetime").and_then(Value::as_str),
            Some("2025-10-13T08:59:59.900Z")
        );
        let mut v = new_message();
        v.as_object_mut().unwrap().remove("composetime");
        assert!(
            parse_message(&v, None)
                .unwrap()
                .extra
                .get("composetime")
                .is_none()
        );
    }

    #[test]
    fn parse_message_variants() {
        let base = new_message();
        let mut v = base.clone();
        v["skypeeditedid"] = v["clientmessageid"].clone();
        v["properties"]["edittime"] = "1760346060000".into();
        let m = parse_message(&v, None).unwrap();
        assert!(m.edited);
        assert_eq!(
            m.id.0, "1760346000000",
            "an edit keeps its id; skypeeditedid is the client message id (live 2026-10-14)"
        );
        let mut v = base.clone();
        v["skypeeditedid"] = "17603459999991234".into();
        assert!(
            parse_message(&v, None).unwrap().edited,
            "skypeeditedid alone marks an edit"
        );

        let mut v = base.clone();
        v["properties"]["deletetime"] = "1760346120000".into();
        v["content"] = "".into();
        let m = parse_message(&v, None).unwrap();
        assert!(m.deleted);
        assert!(m.body.is_empty());

        let mut v = base.clone();
        v["properties"]["emotions"] = json!([
            {"key":"like","users":[{"mri":"8:orgid:aaaaaaaa-1111-1111-1111-111111111111","time":1760346180000i64,"value":"1760346180000"}]},
            {"key":"heart","users":[]}
        ]);
        let m = parse_message(&v, None).unwrap();
        assert_eq!(
            m.reactions,
            vec![
                ReactionSummary {
                    reaction: Reaction::Like,
                    users: vec![Mri("8:orgid:aaaaaaaa-1111-1111-1111-111111111111".into())]
                },
                ReactionSummary {
                    reaction: Reaction::Heart,
                    users: vec![]
                }
            ]
        );

        let mut v = base.clone();
        v["version"] = json!(1760346000000i64);
        v.as_object_mut().unwrap().remove("originalarrivaltime");
        let m = parse_message(&v, None).unwrap();
        assert_eq!(m.arrived_at, 1_760_346_000_000, "falls back to numeric id");
        assert_eq!(m.version, 1_760_346_000_000, "numeric version accepted");

        let mut v = base.clone();
        v.as_object_mut().unwrap().remove("conversationLink");
        assert_eq!(parse_message(&v, None), None);
        assert_eq!(
            parse_message(&v, Some(&ConversationId("19:x@thread.v2".into())))
                .unwrap()
                .conversation
                .0,
            "19:x@thread.v2"
        );

        let mut v = base.clone();
        v.as_object_mut().unwrap().remove("id");
        assert_eq!(
            parse_message(&v, None),
            None,
            "Review Focus #1: no panic, just None"
        );
        let mut v = base.clone();
        v["id"] = "weird-id".into();
        assert_eq!(parse_message(&v, None).unwrap().id.0, "weird-id");

        let mut v = base.clone();
        v["messagetype"] = "Text".into();
        v["content"] = "a <b>\nb".into();
        assert_eq!(
            parse_message(&v, None).unwrap().body.plain_text(),
            "a <b>\nb"
        );

        let mut v = base.clone();
        v["messagetype"] = "RichText/Media_GenericFile".into();
        v["properties"]["files"] =
            json!([{"fileName":"report.pdf","objectUrl":"https://f/1","fileType":"pdf"}]);
        let m = parse_message(&v, None).unwrap();
        assert_eq!(
            m.attachments,
            vec![Attachment {
                name: "report.pdf".into(),
                url: Some("https://f/1".into()),
                kind: AttachmentKind::File
            }]
        );
    }

    /// Fields the brief leaves to the implementer: fallbacks, lenient types and
    /// payloads that must degrade rather than fail.
    #[test]
    fn parse_message_fallbacks_and_lenient_fields() {
        let base = new_message();
        // no `from` at all, or one with no MRI in it: not a usable message
        let mut v = base.clone();
        v.as_object_mut().unwrap().remove("from");
        assert_eq!(parse_message(&v, None), None);
        let mut v = base.clone();
        v["from"] = "nonsense".into();
        assert_eq!(parse_message(&v, None), None);
        // a numeric id is accepted; non-numeric ids with no times arrive at 0
        let mut v = base.clone();
        v["id"] = json!(1760346000000i64);
        assert_eq!(parse_message(&v, None).unwrap().id.0, "1760346000000");
        let mut v = base.clone();
        v["id"] = "x".into();
        v["version"] = Value::Null;
        for k in ["originalarrivaltime", "composetime"] {
            v.as_object_mut().unwrap().remove(k);
        }
        let m = parse_message(&v, None).unwrap();
        assert_eq!((m.arrived_at, m.version), (0, 0));
        // composetime is the last resort for the arrival time
        let mut v = base.clone();
        v["id"] = "x".into();
        v.as_object_mut().unwrap().remove("originalarrivaltime");
        assert_eq!(
            parse_message(&v, None).unwrap().arrived_at,
            1_760_345_999_900
        );
        // an empty skypeeditedid is no edit; empty display name / cmid are None
        let mut v = base.clone();
        v["skypeeditedid"] = "".into();
        v["imdisplayname"] = "".into();
        v["clientmessageid"] = "".into();
        let m = parse_message(&v, None).unwrap();
        assert!(!m.edited);
        assert_eq!(m.id.0, "1760346000000");
        assert_eq!((m.from_name, m.client_message_id), (None, None));
        // reaction users given as contact URLs; files given as a JSON string;
        // image detection by type or extension; Media_* without files is a placeholder
        let mut v = base.clone();
        v["properties"]["emotions"] =
            json!([{"key":"zzz","users":[{"mri":"https://x/v1/users/ME/contacts/8:orgid:a"}]}]);
        v["properties"]["files"] = json!(
            "[{\"title\":\"pic.PNG\",\"fileUrl\":\"https://f/2\"},{\"url\":\"https://f/3\",\"fileType\":\"jpeg\"}]"
        );
        let m = parse_message(&v, None).unwrap();
        assert_eq!(
            m.reactions,
            vec![ReactionSummary {
                reaction: Reaction::Other("zzz".into()),
                users: vec![Mri("8:orgid:a".into())]
            }]
        );
        assert_eq!(
            m.attachments,
            vec![
                Attachment {
                    name: "pic.PNG".into(),
                    url: Some("https://f/2".into()),
                    kind: AttachmentKind::Image
                },
                Attachment {
                    name: "file".into(),
                    url: Some("https://f/3".into()),
                    kind: AttachmentKind::Image
                }
            ]
        );
        let mut v = base.clone();
        v["messagetype"] = "RichText/Media_Card".into();
        assert_eq!(
            parse_message(&v, None).unwrap().attachments,
            vec![Attachment {
                name: "Card".into(),
                url: None,
                kind: AttachmentKind::Other
            }]
        );
        // a deleted message never parses its (possibly stale) content
        let mut v = base.clone();
        v["properties"]["deletetime"] = json!(1760346120000i64);
        let m = parse_message(&v, None).unwrap();
        assert!(m.deleted && m.body.is_empty() && m.reply_to.is_none());
        // properties the domain type does not model stay reachable
        let mut v = base.clone();
        v["properties"]["importance"] = "high".into();
        let m = parse_message(&v, None).unwrap();
        assert_eq!(m.extra["properties"]["importance"], "high");
        // a non-object is not a message
        assert_eq!(parse_message(&json!("x"), None), None);
    }

    #[test]
    fn parse_reply_fixture() {
        let v: Value =
            serde_json::from_str(fixture!("9.08-reply-markup/reply-message.json")).unwrap();
        let m = parse_message(&v, None).unwrap();
        let reply = m.reply_to.as_ref().unwrap();
        assert_eq!(reply.message_id.0, "1760345000000");
        assert_eq!(
            reply.author.as_ref().unwrap().0,
            "8:orgid:bbbbbbbb-2222-2222-2222-222222222222"
        );
        assert_eq!(m.body.plain_text(), "yes");
    }

    #[test]
    fn parse_conversations_fixture() {
        let v: Value = serde_json::from_str(fixture!(
            "9.01-conversations-paging/conversations-page1.json"
        ))
        .unwrap();
        let convs: Vec<Conversation> = v["conversations"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(parse_conversation)
            .collect();
        assert_eq!(convs.len(), 3);
        assert_eq!(convs[0].kind, ConversationKind::OneToOne);
        assert_eq!(
            convs[0].read_horizon.as_ref().unwrap().message_id.0,
            "1760344000000"
        );
        assert_eq!(
            convs[0].last_message.as_ref().unwrap().from_name.as_deref(),
            Some("Bea Example")
        );
        assert_eq!(convs[0].version, 1_760_345_000_000);
        assert_eq!(convs[1].topic.as_deref(), Some("Platform team"));
        assert_eq!(convs[1].kind, ConversationKind::Group);
        assert_eq!(convs[1].members, None);
        assert_eq!(
            convs[1].last_message,
            Some(MessageSummary {
                id: MessageId("1760340000000".into()),
                from: Mri("8:orgid:aaaaaaaa-1111-1111-1111-111111111111".into()),
                from_name: Some("Ada Example".into()),
                message_type: "RichText/Html".into(),
            })
        );
        assert_eq!(convs[2].kind, ConversationKind::System);
        assert!(!convs[2].kind.is_v1_visible(&convs[2].id.0));
        assert_eq!(convs[2].last_message, None);
        let m = parse_page_meta(v.get("_metadata"));
        assert_eq!(m.total_count, Some(4));
        assert!(m.backward_link.is_some());
        let w: Value = serde_json::from_str(fixture!(
            "9.10-conversation-members/conversation-with-members.json"
        ))
        .unwrap();
        let c = parse_conversation(&w).unwrap();
        assert_eq!(
            c.members.unwrap(),
            vec![
                Member {
                    mri: Mri("8:orgid:aaaaaaaa-1111-1111-1111-111111111111".into()),
                    role: Some("Admin".into()),
                    friendly_name: Some("Ada Example".into())
                },
                Member {
                    mri: Mri("8:orgid:bbbbbbbb-2222-2222-2222-222222222222".into()),
                    role: Some("User".into()),
                    friendly_name: Some("Bea Example".into())
                }
            ],
            "UNVERIFIED(§9.10)"
        );
    }

    #[test]
    fn parse_conversation_edge_cases() {
        // no id: None; an empty topic is None; a string version is accepted;
        // threadProperties/properties stay reachable through extra
        assert_eq!(parse_conversation(&json!({"type": "Conversation"})), None);
        let c = parse_conversation(&json!({
            "id": "19:x@thread.v2", "version": "12",
            "threadProperties": {"topic": "", "membercount": "3"},
            "properties": {"consumptionhorizon": "garbage"},
            "lastMessage": {}
        }))
        .unwrap();
        assert_eq!((c.topic, c.version, c.read_horizon), (None, 12, None));
        assert_eq!(c.last_message, None, "a lastMessage without id is none");
        assert_eq!(c.extra["threadProperties"]["membercount"], "3");
        // summary defaults: messagetype Text; contact URL or bare MRI
        let s = parse_message_summary(&json!({"id": "1", "from": "8:orgid:a"})).unwrap();
        assert_eq!(
            (s.message_type.as_str(), s.from.0.as_str()),
            ("Text", "8:orgid:a")
        );
        assert_eq!(s.from_name, None);
    }

    #[test]
    fn parse_thread_me_profiles() {
        let t = parse_thread(&serde_json::from_str(fixture!("9.09-names/thread.json")).unwrap())
            .unwrap();
        assert_eq!(t.id.0, "19:groupgroupgroup@thread.v2");
        assert_eq!(t.topic.as_deref(), Some("Platform team"));
        assert_eq!(t.members.len(), 3);
        assert_eq!(t.members[1].friendly_name.as_deref(), Some("Bea Example"));
        assert_eq!(
            t.members[2].mri.0,
            "8:orgid:cccccccc-3333-3333-3333-333333333333"
        );
        assert_eq!(t.members[0].role.as_deref(), Some("Admin"));
        let (me, named) =
            parse_me(&serde_json::from_str(fixture!("9.09-names/me-properties.json")).unwrap())
                .unwrap();
        assert!(named, "userDetails.name is a real name");
        assert_eq!(me.mri.0, "8:orgid:aaaaaaaa-1111-1111-1111-111111111111");
        assert_eq!(me.display_name, "Ada Example");
        assert_eq!(me.upn.as_deref(), Some("ada@acme.io"));
    }

    #[test]
    fn parse_thread_and_me_fallbacks() {
        // top-level topic wins; userLink beats linkedMri beats id; members with no MRI are skipped
        let t = parse_thread(&json!({
            "id": "19:x@thread.v2", "topic": "Top", "properties": {"topic": "Inner"},
            "members": [
                {"id": "8:orgid:c", "linkedMri": "8:orgid:b", "userLink": "https://x/contacts/8:orgid:a"},
                {"id": "8:orgid:c", "linkedMri": "8:orgid:b"},
                {"role": "User"}
            ]
        }))
        .unwrap();
        assert_eq!(t.topic.as_deref(), Some("Top"));
        let mris: Vec<&str> = t.members.iter().map(|m| m.mri.0.as_str()).collect();
        assert_eq!(mris, ["8:orgid:a", "8:orgid:b"]);
        assert_eq!(parse_thread(&json!({"members": []})), None);
        // display name falls back: name → upn → primaryMemberName → skypeName
        let me = |details: Value| {
            parse_me(&json!({"skypeName": "orgid:a", "primaryMemberName": "a@x.io", "userDetails": details}))
                .unwrap()
                .0
        };
        assert_eq!(
            me(json!("{\"name\":\"orgid:a\",\"upn\":\"u@x.io\"}")).display_name,
            "u@x.io"
        );
        let upn_only = json!({"skypeName": "orgid:a", "userDetails": "{\"upn\":\"u@x.io\"}"});
        assert!(!parse_me(&upn_only).unwrap().1, "a upn is a fallback");
        let m = me(json!("not json"));
        assert_eq!((m.display_name.as_str(), m.upn), ("a@x.io", None));
        assert_eq!(
            me(json!({"name": "Obj"})).display_name,
            "Obj",
            "an object is accepted too"
        );
        let (m, named) = parse_me(&json!({"skypeName": "orgid:a"})).unwrap();
        assert!(!named);
        assert!(m.is_placeholder(&m.display_name));
        assert_eq!(m.sendable_name(), None);
        assert_eq!(
            (m.mri.0.as_str(), m.display_name.as_str()),
            ("8:orgid:a", "orgid:a")
        );
        assert_eq!(
            parse_me(&json!({"skypeName": "8:orgid:a"}))
                .unwrap()
                .0
                .mri
                .0,
            "8:orgid:a"
        );
        assert_eq!(parse_me(&json!({"primaryMemberName": "a@x.io"})), None);
    }

    #[test]
    fn ordering_uses_id_not_composetime() {
        let v: Value = serde_json::from_str(fixture!(
            "9.11-endtime-filter/messages-composetime-skew.json"
        ))
        .unwrap();
        let mut ms: Vec<Message> = v["messages"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|m| parse_message(m, None))
            .collect();
        ms.sort_by(|a, b| a.id.cmp(&b.id));
        let ids: Vec<&str> = ms.iter().map(|m| m.id.0.as_str()).collect();
        assert_eq!(ids, vec!["1760345880000", "1760345940000", "1760346000000"]);
        // arrival follows the id even though composetime runs the other way
        assert!(ms.windows(2).all(|w| w[0].arrived_at < w[1].arrived_at));
    }

    #[test]
    fn page_fixtures_parse_whole() {
        for (text, n) in [
            (fixture!("9.02-message-paging/messages-page1.json"), 5),
            (fixture!("9.02-message-paging/messages-page2.json"), 5),
            (fixture!("9.02-message-paging/messages-short.json"), 2),
            (
                fixture!("9.02-message-paging/messages-backwardlink.json"),
                5,
            ),
        ] {
            let r: MessagesResponse = serde_json::from_str(text).unwrap();
            let ms: Vec<Message> = r
                .messages
                .iter()
                .filter_map(|m| parse_message(m, None))
                .collect();
            assert_eq!(ms.len(), n);
            let meta = parse_page_meta(r.metadata.as_ref());
            assert_eq!(meta.backward_link.is_some(), text.contains("backwardLink"));
            assert!(
                ms.iter().all(|m| m.arrived_at == m.id.as_i64().unwrap()),
                "id == arrival ms (ruling D2)"
            );
        }
        for (text, n) in [
            (
                fixture!("9.01-conversations-paging/conversations-page2.json"),
                1,
            ),
            (
                fixture!("9.01-conversations-paging/conversations-nometa.json"),
                3,
            ),
        ] {
            let r: ConversationsResponse = serde_json::from_str(text).unwrap();
            assert_eq!(
                r.conversations
                    .iter()
                    .filter_map(parse_conversation)
                    .count(),
                n
            );
        }
        let r: ConversationsResponse = serde_json::from_str(fixture!(
            "9.01-conversations-paging/conversations-nometa.json"
        ))
        .unwrap();
        assert_eq!(r.metadata, None);
    }

    #[test]
    fn channel_posts_and_replies_parse_into_the_bare_channel() {
        let r: MessagesResponse =
            serde_json::from_str(fixture!("9.20-channel-threads/channel-messages.json")).unwrap();
        let ms: Vec<Message> = r
            .messages
            .iter()
            .filter_map(|m| parse_message(m, None))
            .collect();
        assert_eq!(ms.len(), 4);
        assert!(
            ms.iter()
                .all(|m| m.conversation.0 == "19:team-general@thread.tacv2"),
            "never the ;messageid= form"
        );
        let roots: Vec<(&str, Option<&str>)> = ms
            .iter()
            .map(|m| {
                (
                    m.id.0.as_str(),
                    m.thread_root.as_ref().map(|r| r.0.as_str()),
                )
            })
            .collect();
        assert_eq!(
            roots,
            [
                ("1760346120000", Some("1760346000000")), // from conversationLink
                ("1760346060000", None),                  // a post
                ("1760346030000", Some("1760346000000")), // from conversationid
                ("1760346000000", None),                  // a post naming itself
            ]
        );
        // `to` is the last field read; the hint's suffix is the fallback
        let mut v = r.messages[0].clone();
        let obj = v.as_object_mut().unwrap();
        obj.remove("conversationLink");
        obj.remove("conversationid");
        obj.insert(
            "to".into(),
            "19:team-general@thread.tacv2;messageid=1760346000000".into(),
        );
        let hint = ConversationId("19:team-general@thread.tacv2".into());
        let m = parse_message(&v, Some(&hint)).unwrap();
        assert_eq!(m.thread_root, Some(MessageId("1760346000000".into())));
        v.as_object_mut().unwrap().remove("to");
        let threaded =
            ConversationId("19:team-general@thread.tacv2;messageid=1760346000000".into());
        let m = parse_message(&v, Some(&threaded)).unwrap();
        assert_eq!(
            (m.conversation, m.thread_root),
            (hint.clone(), Some(MessageId("1760346000000".into())))
        );
        assert_eq!(parse_message(&v, Some(&hint)).unwrap().thread_root, None);
        // chat messages never have a thread root
        assert_eq!(
            parse_message(&new_message(), None).unwrap().thread_root,
            None
        );
    }

    #[test]
    fn a_mention_split_into_words_is_one_mention() {
        // Teams' shape (observed): one span per word, each its own itemid.
        let span = |n: u8, w: &str| {
            format!(
                r#"<span itemtype="http://schema.skype.com/Mention" itemscope="" itemid="{n}">{w}</span>"#
            )
        };
        let entry = |n: u8, mri: &str, kind: &str, w: &str| {
            json!({"@type": "http://schema.skype.com/Mention", "itemid": n.to_string(),
                   "mri": mri, "mentionType": kind, "displayName": w})
        };
        let team = "19:team@thread.tacv2";
        let mut v: Value =
            serde_json::from_str(fixture!("9.23-mentions/message-with-mentions.json")).unwrap();
        v["content"] = json!(format!(
            "<p>{}&nbsp;{} hi {}&nbsp;{}</p>",
            span(0, "Example"),
            span(1, "Team"),
            span(2, "Ada"),
            span(3, "Example")
        ));
        v["properties"]["mentions"] = json!(
            json!([
                entry(0, team, "team", "Example"),
                entry(1, team, "team", "Team"),
                entry(2, "8:orgid:a", "person", "Ada"),
                entry(3, "8:orgid:a", "person", "Example"),
            ])
            .to_string()
        );
        let m = parse_message(&v, None).unwrap();
        let mut got = Vec::new();
        let mut body = m.body.clone();
        crate::format::visit_mentions(&mut body, &mut |mri, name| {
            got.push((mri.0.clone(), name.to_owned()));
        });
        assert_eq!(
            got,
            [
                (team.to_owned(), "Example Team".to_owned()),
                ("8:orgid:a".to_owned(), "Ada Example".to_owned())
            ]
        );
    }

    #[test]
    fn mentions_resolve_to_mris_through_properties() {
        let v: Value =
            serde_json::from_str(fixture!("9.23-mentions/message-with-mentions.json")).unwrap();
        let mentions = |m: &Message| {
            let mut out = Vec::new();
            let mut body = m.body.clone();
            crate::format::visit_mentions(&mut body, &mut |mri, name| {
                out.push((mri.0.clone(), name.to_owned()));
            });
            out
        };
        let m = parse_message(&v, None).unwrap();
        assert_eq!(
            mentions(&m),
            [
                (
                    "8:orgid:aaaaaaaa-1111-1111-1111-111111111111".to_owned(),
                    "Ada Example".to_owned()
                ),
                (
                    "8:orgid:cccccccc-3333-3333-3333-333333333333".to_owned(),
                    "Cy Example".to_owned()
                ),
                ("7".to_owned(), "Nobody".to_owned()), // no entry: kept as received
            ]
        );
        // an array (not a string) works too; junk entries are skipped
        let mut v2 = v.clone();
        v2["properties"]["mentions"] = json!([
            {"itemid": 0, "mri": "https://x/v1/users/ME/contacts/8:orgid:z"},
            {"itemid": 1},
            {"mri": "8:orgid:y"},
            "junk"
        ]);
        let got = mentions(&parse_message(&v2, None).unwrap());
        assert_eq!(got[0].0, "8:orgid:z");
        assert_eq!((got[1].0.as_str(), got[2].0.as_str()), ("1", "7"));
        // no list at all: the raw itemids stay
        v2["properties"] = json!({});
        let got = mentions(&parse_message(&v2, None).unwrap());
        assert_eq!(got[0].0, "0");
        // the raw list stays reachable
        assert!(m.extra["properties"]["mentions"].is_string());
    }

    #[test]
    fn hidden_is_read_from_properties_as_a_string_or_a_bool() {
        let conv = |props: Value| {
            parse_conversation(&json!({"id": "19:x@thread.v2", "properties": props})).unwrap()
        };
        assert!(conv(json!({"hidden": "true"})).hidden);
        assert!(conv(json!({"hidden": true})).hidden);
        let shown = conv(json!({"hidden": "false"}));
        assert!(!shown.hidden && shown.hidden_in_payload());
        let silent = conv(json!({}));
        assert!(!silent.hidden && !silent.hidden_in_payload());
        assert!(!conv(json!({"hidden": 1})).hidden);
    }

    #[test]
    fn history_hidden_time_hides_until_a_newer_message() {
        let conv = |props: serde_json::Value, last: Option<&str>| {
            let mut v = json!({"id": "19:a@thread.v2", "properties": props});
            if let Some(id) = last {
                v["lastMessage"] = json!({"id": id, "from": "8:orgid:x", "messagetype": "Text"});
            }
            parse_conversation(&v).unwrap()
        };
        let hidden = conv(json!({"historyHiddenTime": "2000"}), Some("1500"));
        assert!(hidden.hidden && hidden.hidden_in_payload());
        assert!(conv(json!({"historyHiddenTime": "2000"}), None).hidden);
        assert!(conv(json!({"historyHiddenTime": 2000}), Some("2000")).hidden);
        let back = conv(json!({"historyHiddenTime": "2000"}), Some("2500"));
        assert!(!back.hidden && back.hidden_in_payload());
        assert!(!conv(json!({"historyHiddenTime": "0"}), Some("1")).hidden);
    }

    #[test]
    fn a_thread_id_is_its_channels_conversation() {
        let c = parse_conversation(
            &json!({"id": "19:chan@thread.tacv2;messageid=1760346000000", "version": 5}),
        )
        .unwrap();
        assert_eq!(c.id.0, "19:chan@thread.tacv2");
        assert_eq!(c.kind, ConversationKind::Channel);
        let t = parse_thread(&json!({"id": "19:chan@thread.tacv2;messageid=1"})).unwrap();
        assert_eq!(t.id.0, "19:chan@thread.tacv2");
    }

    #[test]
    fn lenient_numbers() {
        assert_eq!(lenient_i64(&json!(5)), Some(5));
        assert_eq!(lenient_i64(&json!("6")), Some(6));
        assert_eq!(lenient_i64(&json!("x")), None);
        assert_eq!(lenient_i64(&json!(1.5)), None);
        assert_eq!(lenient_i64(&Value::Null), None);
    }
}
