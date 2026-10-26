//! Domain types: newtypes, timestamps, conversations, messages, reactions and
//! the classification rules of spec §4.5.

use std::cmp::Ordering;
use std::fmt;

use serde::{Deserialize, Serialize};

use crate::format::{ReplyQuote, RichText};

/// Milliseconds since the Unix epoch.
pub type Timestamp = i64;

/// The current time as a [`Timestamp`].
pub fn now_ms() -> Timestamp {
    chrono::Utc::now().timestamp_millis()
}

/// Parses an RFC 3339 timestamp (any number of fractional digits, as Teams
/// sends seven) into milliseconds. `None` if it does not parse.
pub fn parse_iso_ms(s: &str) -> Option<Timestamp> {
    chrono::DateTime::parse_from_rfc3339(s)
        .ok()
        .map(|d| d.timestamp_millis())
}

/// A conversation (thread) id, e.g. `19:…@thread.v2` or `48:notes`.
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct ConversationId(pub String);

/// A server-assigned message id: the arrival time in milliseconds as a string.
///
/// Ordering is numeric when both ids parse as `i64`; a numeric id sorts before
/// a non-numeric one; otherwise (and to break numeric ties such as `"01"` vs
/// `"1"`) ids compare as strings, so the order is total and consistent with
/// [`Eq`]. Bot and system messages can carry non-numeric ids, and ordering must
/// never panic on them.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct MessageId(pub String);

impl MessageId {
    /// The id as a number, when it is one.
    pub fn as_i64(&self) -> Option<i64> {
        self.0.parse().ok()
    }
}

impl Ord for MessageId {
    fn cmp(&self, other: &Self) -> Ordering {
        match (self.as_i64(), other.as_i64()) {
            (Some(a), Some(b)) => a.cmp(&b).then_with(|| self.0.cmp(&other.0)),
            (Some(_), None) => Ordering::Less,
            (None, Some(_)) => Ordering::Greater,
            (None, None) => self.0.cmp(&other.0),
        }
    }
}

impl PartialOrd for MessageId {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

/// A user identity such as `8:orgid:{guid}`.
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct Mri(pub String);

impl Mri {
    /// Whether `name` is only this MRI (with or without its `8:` prefix),
    /// the last-resort "name" when none is known.
    pub(crate) fn is_bare_name(&self, name: &str) -> bool {
        name == self.0 || self.0.strip_prefix("8:") == Some(name)
    }

    /// Extracts the MRI from a contact URL (`…/v1/users/ME/contacts/8:orgid:x`).
    /// A bare MRI passes through; anything whose last path segment has no `:`
    /// is rejected.
    pub fn from_contact_url(s: &str) -> Option<Mri> {
        s.rsplit('/')
            .next()
            .filter(|last| last.contains(':'))
            .map(|last| Mri(last.to_string()))
    }

    /// The GUID of an `8:orgid:{guid}` MRI, or `None` for other kinds.
    pub fn orgid_guid(&self) -> Option<&str> {
        self.0.strip_prefix("8:orgid:")
    }
}

impl fmt::Display for ConversationId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl fmt::Display for MessageId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl fmt::Display for Mri {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// The one system conversation v1 surfaces (as "Notes").
const NOTES_ID: &str = "48:notes";

/// A message reference: the server id plus, when known, the client-chosen id
/// that was sent with it.
#[derive(Clone, Debug, PartialEq)]
pub struct MessageRef {
    pub id: MessageId,
    pub client_message_id: Option<String>,
}

/// How far a conversation has been read, from `properties.consumptionhorizon`
/// (`"id;ts;cmid"`).
#[derive(Clone, Debug, PartialEq)]
pub struct ReadHorizon {
    /// The last read message.
    pub message_id: MessageId,
    /// When it was read.
    pub at: Timestamp,
    /// The client message id of the last read message, when present.
    pub client_message_id: Option<String>,
}

impl ReadHorizon {
    /// Parses the wire form `"id;ts;cmid"`. The id must be non-empty and `ts`
    /// numeric; `cmid` may be missing or empty (`None`). Anything else is
    /// `None`.
    pub fn parse(s: &str) -> Option<ReadHorizon> {
        let mut parts = s.split(';');
        let id = parts.next().filter(|id| !id.is_empty())?;
        let at = parts.next()?.parse().ok()?;
        let client_message_id = parts.next().filter(|c| !c.is_empty()).map(str::to_owned);
        Some(ReadHorizon {
            message_id: MessageId(id.to_owned()),
            at,
            client_message_id,
        })
    }

    /// The wire form `"id;ts;cmid"` (`cmid` empty when unknown).
    pub fn to_wire(&self) -> String {
        format!(
            "{};{};{}",
            self.message_id,
            self.at,
            self.client_message_id.as_deref().unwrap_or("")
        )
    }
}

/// What kind of conversation an id (and its `threadProperties`) denotes.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum ConversationKind {
    /// Ids starting `48:` (`48:notes`, `48:calllogs`, …). Only `48:notes` is
    /// surfaced in v1.
    System,
    /// A team channel: ids ending `@thread.tacv2`. Its top-level posts and
    /// their replies share the channel's id (see [`Message::thread_root`]).
    Channel,
    /// A 1:1 chat.
    OneToOne,
    /// Everything else, including meeting chats (`19:meeting_…@thread.v2`).
    Group,
}

impl ConversationKind {
    /// Classifies a conversation per spec §4.5, in this order: `48:` → System;
    /// `@thread.tacv2` → Channel; `uniquerosterthread == "true"`,
    /// `productThreadType == "OneToOneChat"`, `@unq.gbl.spaces` or `19:uni01_` →
    /// OneToOne; otherwise Group.
    pub fn classify(
        id: &str,
        thread_properties: &serde_json::Map<String, serde_json::Value>,
    ) -> Self {
        let prop = |k: &str| {
            thread_properties
                .get(k)
                .and_then(|v| v.as_str())
                .unwrap_or("")
        };
        if id.starts_with("48:") {
            Self::System
        } else if id.ends_with("@thread.tacv2") {
            Self::Channel
        } else if prop("uniquerosterthread") == "true"
            || prop("productThreadType") == "OneToOneChat"
            || id.ends_with("@unq.gbl.spaces")
            || id.starts_with("19:uni01_")
        {
            Self::OneToOne
        } else {
            Self::Group
        }
    }

    /// Whether v1 consumers show this conversation: 1:1 and group chats, plus
    /// the `48:notes` system conversation.
    ///
    /// This was the original terminal client's rule: channels are not
    /// v1-visible. Other
    /// consumers (e.g. a gateway that shows channels) decide visibility
    /// themselves from [`ConversationKind`] and [`Conversation::hidden`].
    pub fn is_v1_visible(self, id: &str) -> bool {
        matches!(self, Self::OneToOne | Self::Group) || (self == Self::System && id == NOTES_ID)
    }
}

/// A conversation member.
#[derive(Clone, Debug, PartialEq)]
pub struct Member {
    pub mri: Mri,
    pub role: Option<String>,
    pub friendly_name: Option<String>,
}

/// The `lastMessage` of a conversation listing.
#[derive(Clone, Debug, PartialEq)]
pub struct MessageSummary {
    pub id: MessageId,
    pub from: Mri,
    pub from_name: Option<String>,
    pub message_type: String,
}

/// A conversation as listed by the server.
#[derive(Clone, Debug, PartialEq)]
pub struct Conversation {
    pub id: ConversationId,
    pub kind: ConversationKind,
    pub topic: Option<String>,
    pub members: Option<Vec<Member>>,
    pub last_message: Option<MessageSummary>,
    pub read_horizon: Option<ReadHorizon>,
    pub version: i64,
    /// Hidden by the user (Teams' "Hide": the chat leaves the chat list
    /// until a new message arrives): `properties.historyHiddenTime` at or
    /// after the last message, or the older `properties.hidden`; `false`
    /// when the payload does not say (see [`Conversation::hidden_in_payload`]).
    pub hidden: bool,
    /// Payload fields the typed ones do not cover.
    pub extra: serde_json::Map<String, serde_json::Value>,
}

impl Conversation {
    /// Whether the payload this came from carried `properties.hidden` or
    /// `properties.historyHiddenTime`, so [`Conversation::hidden`] is the
    /// server's word rather than the default. A `ConversationUpdate`
    /// without either says nothing about hiding.
    pub fn hidden_in_payload(&self) -> bool {
        let props = self.extra.get("properties");
        ["hidden", "historyHiddenTime"]
            .iter()
            .any(|k| props.and_then(|p| p.get(*k)).is_some_and(|h| !h.is_null()))
    }
}

/// A chat message.
#[derive(Clone, Debug, PartialEq)]
pub struct Message {
    pub id: MessageId,
    pub client_message_id: Option<String>,
    /// The conversation, always the bare id: a team channel's replies carry
    /// the channel's id here and their post in `thread_root`.
    pub conversation: ConversationId,
    /// In a team channel, the id of the post this message replies under;
    /// `None` for a post itself and for every chat message.
    pub thread_root: Option<MessageId>,
    pub from: Mri,
    pub from_name: Option<String>,
    /// `originalarrivaltime`, the server clock — never `composetime`.
    pub arrived_at: Timestamp,
    pub version: i64,
    pub edited: bool,
    pub deleted: bool,
    /// Canonical form: a leading reply quote is lifted into `reply_to` and
    /// excluded from the body.
    pub body: RichText,
    pub reply_to: Option<ReplyQuote>,
    pub reactions: Vec<ReactionSummary>,
    pub attachments: Vec<Attachment>,
    pub message_type: String,
    /// Payload fields the typed ones do not cover.
    pub extra: serde_json::Map<String, serde_json::Value>,
}

/// A reaction, keyed as in `properties.emotions[].key`.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum Reaction {
    Like,
    Heart,
    Laugh,
    Surprised,
    Sad,
    Angry,
    /// Any other key, kept verbatim.
    Other(String),
}

impl Reaction {
    /// The reactions offered by the picker, in display order.
    pub const PICKER: [Reaction; 6] = [
        Reaction::Like,
        Reaction::Heart,
        Reaction::Laugh,
        Reaction::Surprised,
        Reaction::Sad,
        Reaction::Angry,
    ];

    /// The wire key (`like`, `heart`, …; `Other` is its own key).
    pub fn key(&self) -> &str {
        match self {
            Reaction::Like => "like",
            Reaction::Heart => "heart",
            Reaction::Laugh => "laugh",
            Reaction::Surprised => "surprised",
            Reaction::Sad => "sad",
            Reaction::Angry => "angry",
            Reaction::Other(k) => k,
        }
    }

    /// The reaction for a wire key; unknown keys become `Other`.
    pub fn from_key(k: &str) -> Reaction {
        match k {
            "like" => Reaction::Like,
            "heart" => Reaction::Heart,
            "laugh" => Reaction::Laugh,
            "surprised" => Reaction::Surprised,
            "sad" => Reaction::Sad,
            "angry" => Reaction::Angry,
            other => Reaction::Other(other.to_owned()),
        }
    }

    /// The emoji to render: Teams' own for the key (see [`crate::emoji`]);
    /// an `Other` key Teams' table does not know renders as itself.
    pub fn glyph(&self) -> &str {
        match crate::emoji::emoji_for_key(self.key()) {
            Some(e) => e,
            None => self.key(),
        }
    }

    /// The reaction for an emoji, by Teams' table: one of the classic six
    /// when it is theirs, else `Other` with Teams' key; `None` when Teams
    /// has no reaction for it.
    pub fn from_emoji(emoji: &str) -> Option<Reaction> {
        crate::emoji::key_for_emoji(emoji).map(Reaction::from_key)
    }
}

/// Who reacted with what, from `properties.emotions`.
#[derive(Clone, Debug, PartialEq)]
pub struct ReactionSummary {
    pub reaction: Reaction,
    pub users: Vec<Mri>,
}

/// A file or image attached to a message.
#[derive(Clone, Debug, PartialEq)]
pub struct Attachment {
    pub name: String,
    pub url: Option<String>,
    pub kind: AttachmentKind,
}

/// The broad kind of an [`Attachment`].
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum AttachmentKind {
    File,
    Image,
    Other,
}

/// What a sender supplies to reply to a message; `send` builds the outgoing
/// [`ReplyQuote`] from it.
#[derive(Clone, Debug, PartialEq)]
pub struct ReplyTarget {
    pub message: MessageRef,
    pub author: Mri,
    pub author_name: String,
    pub excerpt: String,
    pub sent_at: Timestamp,
}

/// A file uploaded for sending (see `Api::upload_file`): its entry for the
/// message's `properties.files`.
#[derive(Clone, Debug, PartialEq)]
pub struct UploadedFile {
    pub name: String,
    /// The `properties.files` entry, as Teams sends it.
    pub property: serde_json::Value,
}

/// A message to send with [`Api::send_message`](crate::Api::send_message).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Outgoing {
    pub body: RichText,
    pub reply_to: Option<ReplyTarget>,
    /// In a team channel, the post whose thread this message replies in;
    /// `None` starts a new post (or is any chat message).
    pub thread_root: Option<MessageId>,
    /// Files already uploaded, sent as attachments.
    pub files: Vec<UploadedFile>,
    /// The client message id; empty means a fresh one.
    pub client_message_id: String,
}

/// Someone's presence in Teams.
#[derive(Clone, Debug, PartialEq)]
pub struct Presence {
    pub mri: Mri,
    /// `Available`, `AvailableIdle`, `Away`, `BeRightBack`, `Busy`,
    /// `BusyIdle`, `DoNotDisturb`, `Offline`, `PresenceUnknown`, …
    pub availability: String,
    /// What they are doing: `Available`, `InAMeeting`, `InACall`,
    /// `Presenting`, `Inactive`, `OffWork`, …
    pub activity: String,
    /// `lastActiveTime`, when given.
    pub last_active: Option<Timestamp>,
    /// `calendarData.isOutOfOffice`.
    pub out_of_office: bool,
}

impl Presence {
    /// Whether they are around: `Available` or `AvailableIdle`.
    pub fn is_available(&self) -> bool {
        matches!(self.availability.as_str(), "Available" | "AvailableIdle")
    }
}

/// The signed-in user.
#[derive(Clone, Debug, PartialEq)]
pub struct Me {
    pub mri: Mri,
    pub display_name: String,
    pub upn: Option<String>,
}

impl Me {
    /// Whether `name` is only the user's bare `orgid:{guid}` (or full MRI),
    /// the last-resort fallback when no real name was found.
    pub(crate) fn is_placeholder(&self, name: &str) -> bool {
        self.mri.is_bare_name(name)
    }

    /// The display name to send as `imdisplayname`: `None` while it is
    /// only the placeholder (the service stamps `fromDisplayNameInToken`
    /// anyway, and other clients would show the guid).
    pub(crate) fn sendable_name(&self) -> Option<&str> {
        Some(self.display_name.as_str()).filter(|n| !self.is_placeholder(n))
    }
}

/// A thread's topic and roster, from the thread endpoint.
#[derive(Clone, Debug, PartialEq)]
pub struct ThreadInfo {
    pub id: ConversationId,
    pub topic: Option<String>,
    pub members: Vec<Member>,
}

/// What the server returns for a sent message.
#[derive(Clone, Debug, PartialEq)]
pub struct SendReceipt {
    pub client_message_id: String,
    pub arrival_time: Option<Timestamp>,
}

/// Presence entries from a `getpresence` answer (a list) or a pushed
/// `unifiedPresenceService` body (`{"presence": [...]}`): each
/// `{mri, presence: {availability, activity, lastActiveTime,
/// calendarData: {isOutOfOffice}}}` (observed 2026-10-22). Entries without
/// an MRI or availability are skipped.
pub fn parse_presence(v: &serde_json::Value) -> Vec<Presence> {
    let list = v
        .get("presence")
        .and_then(serde_json::Value::as_array)
        .or_else(|| v.as_array());
    let Some(list) = list else {
        return Vec::new();
    };
    list.iter()
        .filter_map(|e| {
            let mri = e.get("mri")?.as_str()?;
            let p = e.get("presence")?;
            let s = |k: &str| p.get(k).and_then(serde_json::Value::as_str);
            Some(Presence {
                mri: Mri(mri.to_owned()),
                availability: s("availability")?.to_owned(),
                activity: s("activity").unwrap_or("").to_owned(),
                last_active: s("lastActiveTime").and_then(parse_iso_ms),
                out_of_office: p
                    .get("calendarData")
                    .and_then(|c| c.get("isOutOfOffice"))
                    .and_then(serde_json::Value::as_bool)
                    .unwrap_or(false),
            })
        })
        .collect()
}

/// Whether a `messagetype` is shown (and counted as unread) as a chat message:
/// `Text`, `RichText`, `RichText/Html` and `RichText/Media_*`.
pub fn is_displayable(message_type: &str) -> bool {
    matches!(message_type, "Text" | "RichText" | "RichText/Html")
        || message_type.starts_with("RichText/Media_")
}

/// Whether this is the `48:notes` conversation.
pub fn is_notes(id: &ConversationId) -> bool {
    id.0 == NOTES_ID
}

/// Extracts the conversation id from a link of the form
/// `…/conversations/{id}[/…]` (a `conversationLink` or a trouter resource
/// URL), percent-decoded and without a channel thread's `;messageid=…`
/// suffix (see [`thread_from_link`]). `None` when the link has no such
/// segment.
pub fn conversation_id_from_link(link: &str) -> Option<ConversationId> {
    thread_from_link(link).map(|(conv, _)| conv)
}

/// [`conversation_id_from_link`], plus the thread root its id names, if any
/// (see [`split_thread_id`]).
pub fn thread_from_link(link: &str) -> Option<(ConversationId, Option<MessageId>)> {
    let rest = link.split_once("/conversations/")?.1;
    let id = rest.split(['/', '?']).next()?;
    if id.is_empty() {
        return None;
    }
    Some(split_thread_id(&percent_decode(id)))
}

/// Splits a conversation id that may address a team-channel thread
/// (`19:…@thread.tacv2;messageid={root}`) into the bare conversation id and
/// the root post's id. An id without the suffix (or with an empty root) is
/// the bare id and `None`.
// UNVERIFIED(§9.20): a channel reply's conversation is `{channel id};messageid={root id}` (in `conversationLink`, `conversationid`, `to` and trouter resource URLs), the suffix spelled exactly so; a root post's suffix (if any) names itself.
pub fn split_thread_id(id: &str) -> (ConversationId, Option<MessageId>) {
    match id.split_once(";messageid=") {
        Some((bare, root)) if !root.is_empty() => (
            ConversationId(bare.to_owned()),
            Some(MessageId(root.to_owned())),
        ),
        Some((bare, _)) => (ConversationId(bare.to_owned()), None),
        None => (ConversationId(id.to_owned()), None),
    }
}

/// Decodes `%XX` escapes; malformed escapes are kept verbatim and invalid UTF-8
/// is replaced, so it never fails.
fn percent_decode(s: &str) -> String {
    let hex = |b: &u8| (*b as char).to_digit(16).map(|d| d as u8);
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%'
            && let (Some(hi), Some(lo)) = (
                bytes.get(i + 1).and_then(hex),
                bytes.get(i + 2).and_then(hex),
            )
        {
            out.push((hi << 4) | lo);
            i += 3;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mri_from_contact_url() {
        assert_eq!(
            Mri::from_contact_url("https://x/v1/users/ME/contacts/8:orgid:abc")
                .unwrap()
                .0,
            "8:orgid:abc"
        );
        assert_eq!(
            Mri::from_contact_url("8:orgid:abc").unwrap().0,
            "8:orgid:abc"
        );
        assert_eq!(Mri::from_contact_url("nonsense"), None);
        assert_eq!(Mri("8:orgid:abc".into()).orgid_guid(), Some("abc"));
    }

    #[test]
    fn iso_to_ms() {
        // `date -u -d '2026-10-13T09:00:00Z' +%s` = 1791882000 (ruling D2;
        // the plan's original value was one day off).
        assert_eq!(
            parse_iso_ms("2026-10-13T09:00:00.000Z"),
            Some(1_791_882_000_000)
        );
        assert_eq!(
            parse_iso_ms("2026-10-13T09:00:00.1234567Z"),
            Some(1_791_882_000_123)
        );
        assert_eq!(parse_iso_ms("garbage"), None);
    }

    #[test]
    fn message_id_orders_numerically_with_string_fallback() {
        let a = MessageId("9".into());
        let b = MessageId("10".into());
        let c = MessageId("x-9".into());
        let d = MessageId("x-10".into());
        assert!(a < b); // numeric, not lexicographic
        assert!(c > b && d < c); // non-numeric sorts after numeric; among themselves string order
        let mut v = vec![b.clone(), c.clone(), a.clone(), d.clone()];
        v.sort();
        assert_eq!(v, vec![a, b, d, c]);
        assert_eq!(
            MessageId("01".into()).cmp(&MessageId("1".into())),
            std::cmp::Ordering::Less
        ); // consistent with Eq
    }

    /// Review Focus #1: ids that are empty, overflow `i64`, or are not ASCII
    /// must sort without panicking and give a total order.
    #[test]
    fn message_id_ordering_never_panics_on_odd_ids() {
        let ids: Vec<MessageId> = [
            "",
            " ",
            "99999999999999999999",
            "-5",
            "0",
            "1760346000000",
            "ünïcode",
            "x",
        ]
        .into_iter()
        .map(|s| MessageId(s.into()))
        .collect();
        let mut sorted = ids.clone();
        sorted.sort();
        assert!(sorted.is_sorted());
        // numeric ids first, in numeric order
        assert_eq!(sorted[0].0, "-5");
        assert_eq!(sorted[1].0, "0");
        assert_eq!(sorted[2].0, "1760346000000");
        // antisymmetry over every pair
        for x in &ids {
            for y in &ids {
                assert_eq!(x.cmp(y), y.cmp(x).reverse(), "{x:?} vs {y:?}");
                assert_eq!(x.cmp(y) == std::cmp::Ordering::Equal, x == y);
            }
        }
    }

    #[test]
    fn read_horizon_parse_and_wire() {
        let h = ReadHorizon::parse("1760346000000;1760346200000;17603459999991234").unwrap();
        assert_eq!(h.message_id.0, "1760346000000");
        assert_eq!(h.at, 1_760_346_200_000);
        assert_eq!(h.client_message_id.as_deref(), Some("17603459999991234"));
        assert_eq!(ReadHorizon::parse("1;2;").unwrap().client_message_id, None);
        assert_eq!(ReadHorizon::parse("garbage"), None);
        assert_eq!(h.to_wire(), "1760346000000;1760346200000;17603459999991234");
    }

    #[test]
    fn read_horizon_edge_cases() {
        // two fields is enough; a missing or empty cmid is `None` and round-trips as empty
        let h = ReadHorizon::parse("1;2").unwrap();
        assert_eq!(h.client_message_id, None);
        assert_eq!(h.to_wire(), "1;2;");
        // the message id need not be numeric (Review Focus #1), but `at` must be
        assert_eq!(ReadHorizon::parse("abc;2;").unwrap().message_id.0, "abc");
        assert_eq!(ReadHorizon::parse("1;x;"), None);
        assert_eq!(ReadHorizon::parse(";2;"), None);
        assert_eq!(ReadHorizon::parse(""), None);
    }

    #[test]
    fn conversation_kind_rules() {
        use serde_json::{Map, json};
        let props = |v: serde_json::Value| v.as_object().cloned().unwrap_or_default();
        assert_eq!(
            ConversationKind::classify("48:notes", &Map::new()),
            ConversationKind::System
        );
        assert_eq!(
            ConversationKind::classify("19:abc@thread.tacv2", &Map::new()),
            ConversationKind::Channel
        );
        assert_eq!(
            ConversationKind::classify("19:a_b@unq.gbl.spaces", &Map::new()),
            ConversationKind::OneToOne
        );
        assert_eq!(
            ConversationKind::classify("19:uni01_xyz@thread.v2", &Map::new()),
            ConversationKind::OneToOne
        );
        assert_eq!(
            ConversationKind::classify(
                "19:abc@thread.v2",
                &props(json!({"uniquerosterthread": "true"}))
            ),
            ConversationKind::OneToOne
        );
        assert_eq!(
            ConversationKind::classify(
                "19:abc@thread.v2",
                &props(json!({"productThreadType": "OneToOneChat"}))
            ),
            ConversationKind::OneToOne
        );
        assert_eq!(
            ConversationKind::classify("19:meeting_x@thread.v2", &Map::new()),
            ConversationKind::Group
        );
        assert!(ConversationKind::System.is_v1_visible("48:notes"));
        assert!(!ConversationKind::System.is_v1_visible("48:calllogs"));
        assert!(!ConversationKind::Channel.is_v1_visible("19:x@thread.tacv2"));
        assert!(ConversationKind::Group.is_v1_visible("19:x@thread.v2"));
    }

    #[test]
    fn presence_from_an_answer_and_a_push() {
        let entry = serde_json::json!({"mri": "8:orgid:a", "presence": {
            "availability": "Busy", "activity": "InAMeeting",
            "lastActiveTime": "2026-10-22T08:00:00Z", "calendarData": {"isOutOfOffice": true}}});
        let answer = parse_presence(&serde_json::json!([entry, {"mri": "8:orgid:b"}]));
        assert_eq!(answer.len(), 1);
        assert_eq!(answer[0].availability, "Busy");
        assert!(answer[0].out_of_office && !answer[0].is_available());
        let pushed = parse_presence(&serde_json::json!({"presence": [entry]}));
        assert_eq!(pushed, answer);
        assert!(parse_presence(&serde_json::json!({})).is_empty());
    }

    #[test]
    fn displayable_types_and_reactions() {
        for t in [
            "Text",
            "RichText",
            "RichText/Html",
            "RichText/Media_Card",
            "RichText/Media_GenericFile",
        ] {
            assert!(is_displayable(t), "{t}");
        }
        for t in [
            "Control/Typing",
            "Event/Call",
            "ThreadActivity/AddMember",
            "RichText/UriObject",
            "",
        ] {
            assert!(!is_displayable(t), "{t}");
        }
        assert_eq!(Reaction::from_key("like"), Reaction::Like);
        assert_eq!(Reaction::from_key("zzz"), Reaction::Other("zzz".into()));
        assert_eq!(Reaction::Heart.key(), "heart");
        assert_eq!(Reaction::Laugh.glyph(), "😆");
        assert_eq!(Reaction::Other("steamtrain".into()).glyph(), "🚂");
        assert_eq!(
            Reaction::from_emoji("🚂"),
            Some(Reaction::Other("steamtrain".into()))
        );
        assert_eq!(Reaction::from_emoji("👍"), Some(Reaction::Like));
        assert_eq!(Reaction::Other("x".into()).glyph(), "x");
        assert_eq!(Reaction::PICKER.len(), 6);
        assert_eq!(
            conversation_id_from_link(
                "https://de.ng.msg.teams.microsoft.com/v1/users/ME/conversations/19:abc@thread.v2/messages/1"
            )
            .unwrap()
            .0,
            "19:abc@thread.v2"
        );
        assert_eq!(
            conversation_id_from_link("https://x/v1/users/ME/conversations/19%3Aabc%40thread.v2")
                .unwrap()
                .0,
            "19:abc@thread.v2"
        );
        assert!(is_notes(&ConversationId("48:notes".into())));
    }

    #[test]
    fn reaction_keys_round_trip_through_picker() {
        for r in &Reaction::PICKER {
            assert_eq!(&Reaction::from_key(r.key()), r);
            assert_ne!(r.glyph(), r.key());
        }
        assert_eq!(Reaction::from_key("x").key(), "x");
    }

    #[test]
    fn conversation_id_from_link_edge_cases() {
        assert_eq!(
            conversation_id_from_link("https://x/v1/users/ME/contacts/8:orgid:a"),
            None
        );
        assert_eq!(
            conversation_id_from_link("https://x/v1/users/ME/conversations/"),
            None
        );
        assert_eq!(
            conversation_id_from_link("https://x/v1/users/ME/conversations/19:a@thread.v2?view=x")
                .unwrap()
                .0,
            "19:a@thread.v2"
        );
        // malformed escapes pass through untouched rather than panicking
        assert_eq!(
            conversation_id_from_link("https://x/conversations/19%3Aa%ZZ%4")
                .unwrap()
                .0,
            "19:a%ZZ%4"
        );
        assert!(!is_notes(&ConversationId("48:calllogs".into())));
    }

    #[test]
    fn thread_suffixes_split_off_links_and_ids() {
        let chan = ConversationId("19:chan@thread.tacv2".into());
        let root = MessageId("1760340000000".into());
        assert_eq!(
            split_thread_id("19:chan@thread.tacv2;messageid=1760340000000"),
            (chan.clone(), Some(root.clone()))
        );
        assert_eq!(
            split_thread_id("19:chan@thread.tacv2"),
            (chan.clone(), None)
        );
        assert_eq!(
            split_thread_id("19:chan@thread.tacv2;messageid="),
            (chan.clone(), None)
        );
        let link = "https://x/v1/users/ME/conversations/19:chan@thread.tacv2;messageid=1760340000000/messages/1760340060000";
        assert_eq!(conversation_id_from_link(link), Some(chan.clone()));
        assert_eq!(
            thread_from_link(link),
            Some((chan.clone(), Some(root.clone())))
        );
        // percent-encoded `;` and `=` decode before the split
        assert_eq!(
            thread_from_link(
                "https://x/v1/users/ME/conversations/19%3Achan%40thread.tacv2%3Bmessageid%3D1760340000000"
            ),
            Some((chan.clone(), Some(root)))
        );
        assert_eq!(
            thread_from_link("https://x/v1/users/ME/conversations/19:chan@thread.tacv2"),
            Some((chan, None))
        );
    }

    #[test]
    fn hidden_in_payload_needs_the_property() {
        let mut c = Conversation {
            id: ConversationId("19:a@thread.v2".into()),
            kind: ConversationKind::Group,
            topic: None,
            members: None,
            last_message: None,
            read_horizon: None,
            version: 0,
            hidden: false,
            extra: serde_json::Map::new(),
        };
        assert!(!c.hidden_in_payload());
        c.extra
            .insert("properties".into(), serde_json::json!({"hidden": null}));
        assert!(!c.hidden_in_payload());
        c.extra
            .insert("properties".into(), serde_json::json!({"hidden": "false"}));
        assert!(c.hidden_in_payload());
    }
}
