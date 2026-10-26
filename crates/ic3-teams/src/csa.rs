//! CSA, the "chat service aggregator" behind `{teams}/api/csa/{geo}/…`
//! (spec A): the user's conversation folders, every chat with its members
//! and every team with its channels, from one
//! `GET …/api/v3/teams/users/me` ([`Api::user_state`](crate::Api::user_state)).
//!
//! Parsing is lenient but reported: [`parse_user_state`] never fails, and
//! its [`ParseReport`] says what was missing or dropped, so a partial parse
//! can be told from a clean one.

use serde_json::Value;

use crate::types::{ConversationId, Member, Mri, Timestamp, parse_iso_ms};

/// Everything one user-state read returns.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct UserState {
    /// `conversationFolders.conversationFolders`, without deleted ones.
    pub folders: Vec<ConversationFolder>,
    /// `conversationFolders.conversationFolderOrder`.
    pub folder_order: Vec<FolderId>,
    /// `chats`.
    pub chats: Vec<ChatSummary>,
    /// `teams`.
    pub teams: Vec<Team>,
}

impl UserState {
    /// The team and channel whose channel id is `id` (a chatsvc
    /// conversation id, `19:…@thread.tacv2`; a team's General channel shares
    /// the team's id), for naming a channel `Team/Channel`.
    pub fn channel(&self, id: &ConversationId) -> Option<(&Team, &Channel)> {
        self.teams
            .iter()
            .find_map(|t| t.channels.iter().find(|c| &c.id == id).map(|c| (t, c)))
    }
}

/// How clean a parse was.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ParseReport {
    /// Sections (wire keys) absent or not of the expected type.
    pub missing_sections: Vec<&'static str>,
    /// Per section (wire key): entries dropped for having no id or not being
    /// an object. Only non-zero counts appear.
    pub skipped: Vec<(&'static str, usize)>,
}

impl ParseReport {
    /// Nothing missing, nothing skipped.
    pub fn is_clean(&self) -> bool {
        self.missing_sections.is_empty() && self.skipped.is_empty()
    }

    fn skip(&mut self, section: &'static str, n: usize) {
        if n > 0 {
            self.skipped.push((section, n));
        }
    }
}

/// A conversation folder's id.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct FolderId(pub String);

/// A conversation folder (custom section, Favorites, …).
#[derive(Clone, Debug, PartialEq)]
pub struct ConversationFolder {
    pub id: FolderId,
    /// `name`; user folders only.
    pub name: Option<String>,
    /// `folderType`.
    pub kind: FolderKind,
    /// `sortType`, raw.
    pub sort: Option<String>,
    /// `isExpanded`.
    pub expanded: bool,
    /// `conversationFolderItems[].conversationId`, in order.
    pub items: Vec<ConversationId>,
}

/// A folder's `folderType`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum FolderKind {
    UserCreated,
    Favorites,
    QuickViews,
    RecentChats,
    MeetingChats,
    MutedChats,
    TeamsAndChannels,
    EngageCommunities,
    /// Any other value, verbatim (empty when absent).
    Other(String),
}

impl FolderKind {
    /// The kind for a wire `folderType`.
    pub fn from_wire(s: &str) -> FolderKind {
        match s {
            "UserCreated" => FolderKind::UserCreated,
            "Favorites" => FolderKind::Favorites,
            "QuickViews" => FolderKind::QuickViews,
            "RecentChats" => FolderKind::RecentChats,
            "MeetingChats" => FolderKind::MeetingChats,
            "MutedChats" => FolderKind::MutedChats,
            "TeamsAndChannels" => FolderKind::TeamsAndChannels,
            "EngageCommunities" => FolderKind::EngageCommunities,
            other => FolderKind::Other(other.to_owned()),
        }
    }
}

/// A chat as CSA lists it.
#[derive(Clone, Debug, PartialEq)]
pub struct ChatSummary {
    pub id: ConversationId,
    /// `title`; empty is `None`.
    pub title: Option<String>,
    /// `isOneOnOne` → `OneOnOne`; `chatType == "meeting"` → `Meeting`; else `Group`.
    pub kind: ChatKind,
    /// `hidden`.
    pub hidden: bool,
    /// `isMuted`.
    pub muted: bool,
    /// `members[]`.
    pub members: Vec<ChatMember>,
    /// `lastMessage.originalArrivalTime`, else `lastMessage.composeTime`.
    pub last_message_at: Option<Timestamp>,
}

/// What kind of chat a [`ChatSummary`] is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ChatKind {
    OneOnOne,
    Group,
    Meeting,
}

/// A chat member. `friendlyName` is rare; names otherwise come from
/// `user_names`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ChatMember {
    pub mri: Mri,
    pub role: Option<String>,
    pub friendly_name: Option<String>,
}

impl From<&ChatMember> for Member {
    fn from(m: &ChatMember) -> Member {
        Member {
            mri: m.mri.clone(),
            role: m.role.clone(),
            friendly_name: m.friendly_name.clone(),
        }
    }
}

/// A team with its channels.
#[derive(Clone, Debug, PartialEq)]
pub struct Team {
    /// The team's thread id, which is also its General channel's id.
    pub id: ConversationId,
    /// `displayName` (empty when absent).
    pub name: String,
    /// `isFavorite`.
    pub favorite: bool,
    /// `isCollapsed`.
    pub collapsed: bool,
    /// `isArchived`.
    pub archived: bool,
    pub channels: Vec<Channel>,
}

/// A team channel.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Channel {
    /// The channel's chatsvc conversation id (`19:…@thread.tacv2`), as
    /// [`Api::messages`](crate::Api::messages) and events use it.
    pub id: ConversationId,
    /// `displayName` (empty when absent).
    pub name: String,
    /// `channelType`, raw (a string, or a number as text).
    pub channel_type: Option<String>,
    /// `isGeneral`.
    pub general: bool,
    /// `isFavorite`.
    pub favorite: bool,
    /// `isPinned`.
    pub pinned: bool,
    /// `isMuted`.
    pub muted: bool,
    /// `isArchived`.
    pub archived: bool,
    /// `interest`: Teams' "Hide" sets `NotInterested` (observed
    /// 2026-10-23); `None` when absent.
    pub interest: Option<String>,
    /// `memberRole`: 1 for an ordinary member; 4 seen on a team's General
    /// channel when the user is only in the team's shared channels.
    pub member_role: Option<i64>,
}

impl Channel {
    /// Whether Teams leaves the channel out of the team's list: the user
    /// hid it (`interest` is `NotInterested`, or it is not a favorite), or
    /// `memberRole` is 4.
    // UNVERIFIED(§9.33): on a live account this matched Teams' list exactly (24 channels); `memberRole` 4 rests on one channel (a General channel of a team the user is in only through shared channels).
    pub fn hidden(&self) -> bool {
        self.interest.as_deref() == Some("NotInterested")
            || !self.favorite
            || self.member_role == Some(4)
    }
}

/// Where a user-state answer came from (smoke §9.13).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CsaSource {
    /// The Teams host the request went to, without a trailing slash.
    pub host: String,
    /// The `{geo}` path segment that answered 2xx.
    pub geo: String,
}

/// A non-empty string field.
fn str_of(v: &Value, k: &str) -> Option<String> {
    v.get(k)
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
}

/// A boolean field; missing or not a boolean is `false`.
fn flag(v: &Value, k: &str) -> bool {
    v.get(k).and_then(Value::as_bool).unwrap_or(false)
}

/// An ISO-8601 string or a number of milliseconds.
fn time_of(v: Option<&Value>) -> Option<Timestamp> {
    match v? {
        Value::String(s) => parse_iso_ms(s),
        Value::Number(n) => n.as_i64(),
        _ => None,
    }
}

/// The items of `v[key]`; records `name` as missing unless it is an array.
fn section<'a>(
    v: &'a Value,
    key: &str,
    name: &'static str,
    report: &mut ParseReport,
) -> &'a [Value] {
    match v.get(key).and_then(Value::as_array) {
        Some(a) => a.as_slice(),
        None => {
            report.missing_sections.push(name);
            &[]
        }
    }
}

/// The items of an optional array field (no report: an absent list is empty).
fn list<'a>(v: &'a Value, key: &str) -> &'a [Value] {
    v.get(key)
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or_default()
}

/// Parses a `GET …/api/v3/teams/users/me` answer (spec A.4). Never fails:
/// a missing or wrong-typed section yields an empty list and is named in
/// [`ParseReport::missing_sections`]; an entry without an id (or not an
/// object) is dropped and counted in [`ParseReport::skipped`]; missing
/// booleans are `false`; deleted folders (`isDeleted`) are left out.
// UNVERIFIED(§9.14): every field name below (folders, chats + members, teams + channels) is assumed from the official client's traffic.
pub fn parse_user_state(v: &Value) -> (UserState, ParseReport) {
    let mut report = ParseReport::default();
    let mut state = UserState::default();
    match v.get("conversationFolders").filter(|f| f.is_object()) {
        Some(f) => {
            let (mut bad, mut bad_items) = (0, 0);
            let folders = section(
                f,
                "conversationFolders",
                "conversationFolders.conversationFolders",
                &mut report,
            );
            for raw in folders {
                if flag(raw, "isDeleted") {
                    continue;
                }
                match parse_folder(raw, &mut bad_items) {
                    Some(folder) => state.folders.push(folder),
                    None => bad += 1,
                }
            }
            report.skip("conversationFolders", bad);
            report.skip("conversationFolderItems", bad_items);
            let mut bad_order = 0;
            let order = section(
                f,
                "conversationFolderOrder",
                "conversationFolders.conversationFolderOrder",
                &mut report,
            );
            for o in order {
                match o.as_str().filter(|s| !s.is_empty()) {
                    Some(id) => state.folder_order.push(FolderId(id.to_owned())),
                    None => bad_order += 1,
                }
            }
            report.skip("conversationFolderOrder", bad_order);
        }
        None => report.missing_sections.push("conversationFolders"),
    }
    let (mut bad_chats, mut bad_members) = (0, 0);
    for raw in section(v, "chats", "chats", &mut report) {
        match parse_chat(raw, &mut bad_members) {
            Some(chat) => state.chats.push(chat),
            None => bad_chats += 1,
        }
    }
    report.skip("chats", bad_chats);
    report.skip("members", bad_members);
    let (mut bad_teams, mut bad_channels) = (0, 0);
    for raw in section(v, "teams", "teams", &mut report) {
        match parse_team(raw, &mut bad_channels) {
            Some(team) => state.teams.push(team),
            None => bad_teams += 1,
        }
    }
    report.skip("teams", bad_teams);
    report.skip("channels", bad_channels);
    (state, report)
}

fn parse_folder(raw: &Value, bad_items: &mut usize) -> Option<ConversationFolder> {
    let id = str_of(raw, "id")?;
    let items = list(raw, "conversationFolderItems")
        .iter()
        .filter_map(|i| {
            let id = str_of(i, "conversationId").map(ConversationId);
            if id.is_none() {
                *bad_items += 1;
            }
            id
        })
        .collect();
    Some(ConversationFolder {
        id: FolderId(id),
        name: str_of(raw, "name"),
        kind: FolderKind::from_wire(raw.get("folderType").and_then(Value::as_str).unwrap_or("")),
        sort: str_of(raw, "sortType"),
        expanded: flag(raw, "isExpanded"),
        items,
    })
}

fn parse_chat(raw: &Value, bad_members: &mut usize) -> Option<ChatSummary> {
    let id = str_of(raw, "id")?;
    let kind = if flag(raw, "isOneOnOne") {
        ChatKind::OneOnOne
    } else if raw.get("chatType").and_then(Value::as_str) == Some("meeting") {
        ChatKind::Meeting
    } else {
        ChatKind::Group
    };
    let members = list(raw, "members")
        .iter()
        .filter_map(|m| {
            let member = str_of(m, "mri")
                .and_then(|s| Mri::from_contact_url(&s))
                .map(|mri| ChatMember {
                    mri,
                    role: str_of(m, "role"),
                    friendly_name: str_of(m, "friendlyName"),
                });
            if member.is_none() {
                *bad_members += 1;
            }
            member
        })
        .collect();
    let last = raw.get("lastMessage");
    let last_message_at = time_of(last.and_then(|l| l.get("originalArrivalTime")))
        .or_else(|| time_of(last.and_then(|l| l.get("composeTime"))));
    Some(ChatSummary {
        id: ConversationId(id),
        title: str_of(raw, "title"),
        kind,
        hidden: flag(raw, "hidden"),
        muted: flag(raw, "isMuted"),
        members,
        last_message_at,
    })
}

fn parse_team(raw: &Value, bad_channels: &mut usize) -> Option<Team> {
    let id = str_of(raw, "id")?;
    let channels = list(raw, "channels")
        .iter()
        .filter_map(|c| {
            let channel = parse_channel(c);
            if channel.is_none() {
                *bad_channels += 1;
            }
            channel
        })
        .collect();
    Some(Team {
        id: ConversationId(id),
        name: str_of(raw, "displayName").unwrap_or_default(),
        favorite: flag(raw, "isFavorite"),
        collapsed: flag(raw, "isCollapsed"),
        archived: flag(raw, "isArchived"),
        channels,
    })
}

fn parse_channel(raw: &Value) -> Option<Channel> {
    let id = str_of(raw, "id")?;
    let channel_type = match raw.get("channelType") {
        Some(Value::String(s)) if !s.is_empty() => Some(s.clone()),
        Some(Value::Number(n)) => Some(n.to_string()),
        _ => None,
    };
    Some(Channel {
        id: ConversationId(id),
        name: str_of(raw, "displayName").unwrap_or_default(),
        channel_type,
        general: flag(raw, "isGeneral"),
        favorite: flag(raw, "isFavorite"),
        pinned: flag(raw, "isPinned"),
        muted: flag(raw, "isMuted"),
        archived: flag(raw, "isArchived"),
        interest: str_of(raw, "interest"),
        member_role: raw.get("memberRole").and_then(Value::as_i64),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn hidden_channels() {
        let ch = |extra: Value| {
            let mut raw = json!({"id": "19:c@thread.tacv2", "displayName": "c", "isFavorite": true, "interest": "Interested", "memberRole": 1});
            for (k, v) in extra.as_object().unwrap() {
                raw[k] = v.clone();
            }
            parse_channel(&raw).unwrap()
        };
        assert!(!ch(json!({})).hidden());
        assert!(ch(json!({"interest": "NotInterested"})).hidden());
        assert!(ch(json!({"isFavorite": false})).hidden());
        assert!(ch(json!({"memberRole": 4})).hidden());
        assert!(!ch(json!({"isMember": false})).hidden());
    }

    fn fixture_state() -> (UserState, ParseReport) {
        let v: Value = serde_json::from_str(fixture!("9.14-csa-user-state/users-me.json")).unwrap();
        parse_user_state(&v)
    }

    #[test]
    fn the_fixture_parses_cleanly() {
        let (s, report) = fixture_state();
        assert!(report.is_clean(), "{report:?}");
        assert_eq!(
            s.folder_order,
            vec![
                FolderId("tenant~user~Favorites".into()),
                FolderId("f-user-1".into()),
                FolderId("tenant~user~QuickViews".into())
            ]
        );
        let kinds: Vec<&FolderKind> = s.folders.iter().map(|f| &f.kind).collect();
        assert_eq!(
            kinds,
            [
                &FolderKind::Favorites,
                &FolderKind::UserCreated,
                &FolderKind::QuickViews,
                &FolderKind::Other("BrandNewKind".into())
            ],
            "the deleted folder is left out"
        );
        let fav = &s.folders[0];
        assert!(fav.expanded && fav.name.is_none());
        assert_eq!(fav.sort.as_deref(), Some("UserDefinedCustomOrder"));
        assert_eq!(
            fav.items,
            [ConversationId("19:groupgroupgroup@thread.v2".into())]
        );
        let projects = &s.folders[1];
        assert_eq!(projects.name.as_deref(), Some("Projects"));
        assert!(!projects.expanded);
        assert_eq!(projects.items.len(), 2);

        let one = &s.chats[0];
        assert_eq!(one.kind, ChatKind::OneOnOne);
        assert_eq!(one.title, None, "an empty title is None");
        assert_eq!(one.last_message_at, Some(1_760_346_000_000));
        assert_eq!(
            one.members[1],
            ChatMember {
                mri: Mri("8:orgid:bbbbbbbb-2222-2222-2222-222222222222".into()),
                role: Some("Admin".into()),
                friendly_name: Some("Bea Example".into()),
            }
        );
        assert_eq!(one.members[0].friendly_name, None);
        let group = &s.chats[1];
        assert_eq!(group.kind, ChatKind::Group);
        assert_eq!(group.title.as_deref(), Some("Platform team"));
        assert!(group.muted && !group.hidden);
        assert_eq!(group.members.len(), 3);
        assert_eq!(
            group.last_message_at,
            Some(1_760_342_400_000),
            "composeTime fallback"
        );
        let meeting = &s.chats[2];
        assert_eq!(meeting.kind, ChatKind::Meeting);
        assert!(meeting.hidden && meeting.members.is_empty());
        assert_eq!(meeting.last_message_at, None);

        let team = &s.teams[0];
        assert_eq!(team.name, "Engineering");
        assert!(team.favorite && !team.collapsed && !team.archived);
        assert_eq!(
            team.channels[0].channel_type.as_deref(),
            Some("0"),
            "a number as text"
        );
        assert!(team.channels[0].general && team.channels[0].favorite);
        assert_eq!(team.channels[1].channel_type.as_deref(), Some("Private"));
        assert!(team.channels[1].pinned && team.channels[1].muted && !team.channels[1].general);
        assert!(!team.channels[0].archived && team.channels[1].archived);
    }

    /// CSA's channel ids are chatsvc conversation ids: they classify as
    /// channels, and look up to their team for naming.
    #[test]
    fn channels_are_found_by_their_conversation_id() {
        let (s, _) = fixture_state();
        for c in &s.teams[0].channels {
            assert_eq!(
                crate::types::ConversationKind::classify(&c.id.0, &serde_json::Map::new()),
                crate::types::ConversationKind::Channel
            );
        }
        let (team, chan) = s
            .channel(&ConversationId("19:chan-2@thread.tacv2".into()))
            .unwrap();
        assert_eq!(
            (team.name.as_str(), chan.name.as_str()),
            ("Engineering", "Releases")
        );
        let (_, general) = s.channel(&s.teams[0].id).unwrap();
        assert!(general.general, "the General channel shares the team's id");
        assert!(
            s.channel(&ConversationId("19:nope@thread.tacv2".into()))
                .is_none()
        );
    }

    /// Review Focus #3: wrong types and junk entries are reported, never fatal.
    #[test]
    fn a_malformed_user_state_is_reported_not_fatal() {
        let v = json!({
            "chats": {"not": "an array"},
            "teams": [
                5,
                {"displayName": "no id"},
                {"id": 7, "displayName": "numeric id"},
                {"id": "19:t@thread.tacv2", "channels": [{"displayName": "no id"}, {"id": "19:c@thread.tacv2"}]}
            ]
        });
        let (s, report) = parse_user_state(&v);
        assert_eq!(report.missing_sections, ["conversationFolders", "chats"]);
        assert_eq!(report.skipped, [("teams", 3), ("channels", 1)]);
        assert!(s.folders.is_empty() && s.chats.is_empty());
        assert_eq!(s.teams.len(), 1);
        assert_eq!(s.teams[0].name, "", "a missing displayName is empty");
        assert_eq!(s.teams[0].channels.len(), 1);

        let v = json!({
            "conversationFolders": {
                "conversationFolders": [{"id": "f", "folderType": "UserCreated",
                    "conversationFolderItems": [{"conversationId": "19:a@thread.v2"}, {"x": 1}, "junk"]}, []],
                "conversationFolderOrder": "not an array"
            },
            "chats": [{"id": "19:a@thread.v2", "members": [{"role": "User"}, {"mri": "8:orgid:x"}, 3]}, {}],
            "teams": []
        });
        let (s, report) = parse_user_state(&v);
        assert_eq!(
            report.missing_sections,
            ["conversationFolders.conversationFolderOrder"]
        );
        assert_eq!(
            report.skipped,
            [
                ("conversationFolders", 1),
                ("conversationFolderItems", 2),
                ("chats", 1),
                ("members", 2)
            ]
        );
        assert_eq!(
            s.folders[0].items,
            [ConversationId("19:a@thread.v2".into())]
        );
        assert_eq!(s.chats[0].members.len(), 1);
        assert!(
            !s.chats[0].hidden && !s.chats[0].muted,
            "missing booleans are false"
        );

        let (s, report) = parse_user_state(&json!("not even an object"));
        assert_eq!(
            report.missing_sections,
            ["conversationFolders", "chats", "teams"]
        );
        assert_eq!(s, UserState::default());
    }

    #[test]
    fn folder_kinds_map_every_known_value() {
        for (wire, kind) in [
            ("UserCreated", FolderKind::UserCreated),
            ("Favorites", FolderKind::Favorites),
            ("QuickViews", FolderKind::QuickViews),
            ("RecentChats", FolderKind::RecentChats),
            ("MeetingChats", FolderKind::MeetingChats),
            ("MutedChats", FolderKind::MutedChats),
            ("TeamsAndChannels", FolderKind::TeamsAndChannels),
            ("EngageCommunities", FolderKind::EngageCommunities),
        ] {
            assert_eq!(FolderKind::from_wire(wire), kind);
        }
        assert_eq!(FolderKind::from_wire("x"), FolderKind::Other("x".into()));
    }

    #[test]
    fn a_chat_member_becomes_a_member() {
        let m = ChatMember {
            mri: Mri("8:orgid:x".into()),
            role: Some("User".into()),
            friendly_name: Some("X".into()),
        };
        assert_eq!(
            Member::from(&m),
            Member {
                mri: Mri("8:orgid:x".into()),
                role: Some("User".into()),
                friendly_name: Some("X".into()),
            }
        );
    }
}
