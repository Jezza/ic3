//! Nicks for Teams users and channel names for Teams conversations.
//!
//! A nick is made from the person's display name (`Jane Smith` →
//! `JaneSmith`), transliterated to ASCII, with a number added on a clash
//! (`JaneSmith2`). Until a name is known, a provisional nick is made from
//! the user's id; it is replaced once the name arrives. Channel names are
//! `#` plus a slug of the chat topic (or member names), and `#Team/Channel`
//! for team channels. Names stay fixed for the life of the process.

use std::collections::HashMap;

use ic3_teams::{ConversationId, Mri};

use crate::irc::casefold;

/// The service user that takes commands and reports sign-in.
pub const SERVICE_NICK: &str = "Server";
/// The pseudo-user the Notes conversation is a query with.
pub const NOTES_NICK: &str = "Notes";
/// The host part of every Teams user's mask.
pub const USER_HOST: &str = "teams";

/// Longest nick handed out.
const NICK_MAX: usize = 30;
/// Longest channel name handed out (including `#`).
const CHANNEL_MAX: usize = 64;

#[derive(Clone, Debug)]
struct UserEntry {
    nick: String,
    /// Made from the id because no name was known.
    provisional: bool,
}

/// Who and what each nick and channel name stands for.
#[derive(Debug, Default)]
pub struct Directory {
    users: HashMap<Mri, UserEntry>,
    nicks: HashMap<String, Mri>,
    channels: HashMap<ConversationId, String>,
    channel_names: HashMap<String, ConversationId>,
    /// Folded nicks nobody may take (the service users, our own nick).
    reserved: HashMap<String, String>,
}

/// A nick change to announce: `old` → `new`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Rename {
    pub mri: Mri,
    pub old: String,
    pub new: String,
}

impl Directory {
    pub fn new() -> Directory {
        let mut d = Directory::default();
        for n in [SERVICE_NICK, NOTES_NICK] {
            d.reserved.insert(casefold(n), n.to_string());
        }
        d
    }

    /// The nick of `mri`, allocating one from `name` (or the id) on first
    /// sight.
    pub fn nick(&mut self, mri: &Mri, name: Option<&str>) -> String {
        if let Some(e) = self.users.get(mri) {
            return e.nick.clone();
        }
        let (base, provisional) = match name.map(nick_base).filter(|b| !b.is_empty()) {
            Some(b) => (b, false),
            None => (provisional_base(mri), true),
        };
        let nick = self.free_nick(&base);
        self.nicks.insert(casefold(&nick), mri.clone());
        self.users.insert(
            mri.clone(),
            UserEntry {
                nick: nick.clone(),
                provisional,
            },
        );
        nick
    }

    /// The nick of `mri` if one was handed out.
    pub fn existing_nick(&self, mri: &Mri) -> Option<&str> {
        self.users.get(mri).map(|e| e.nick.as_str())
    }

    /// A real name arrived for `mri`: replaces a provisional nick.
    pub fn learn_name(&mut self, mri: &Mri, name: &str) -> Option<Rename> {
        let e = self.users.get(mri)?;
        if !e.provisional {
            return None;
        }
        let base = nick_base(name);
        if base.is_empty() {
            return None;
        }
        let old = e.nick.clone();
        self.nicks.remove(&casefold(&old));
        let new = self.free_nick(&base);
        self.nicks.insert(casefold(&new), mri.clone());
        self.users.insert(
            mri.clone(),
            UserEntry {
                nick: new.clone(),
                provisional: false,
            },
        );
        (old != new).then(|| Rename {
            mri: mri.clone(),
            old,
            new,
        })
    }

    /// Gives `mri` the nick `nick` (our own, chosen by the client before
    /// sign-in): always succeeds, taking the nick from whoever had it.
    pub fn force_nick(&mut self, mri: &Mri, nick: &str) -> Option<Rename> {
        let folded = casefold(nick);
        let mut displaced = None;
        if let Some(holder) = self.nicks.get(&folded).cloned()
            && holder != *mri
        {
            self.nicks.remove(&folded);
            let base = format!("{nick}_");
            let new = self.free_nick(&base);
            self.nicks.insert(casefold(&new), holder.clone());
            if let Some(e) = self.users.get_mut(&holder) {
                e.nick = new.clone();
            }
            displaced = Some(Rename {
                mri: holder,
                old: nick.to_string(),
                new,
            });
        }
        if let Some(e) = self.users.get(mri) {
            self.nicks.remove(&casefold(&e.nick));
        }
        self.nicks.insert(folded, mri.clone());
        self.users.insert(
            mri.clone(),
            UserEntry {
                nick: nick.to_string(),
                provisional: false,
            },
        );
        displaced
    }

    /// The user a nick stands for.
    pub fn user_by_nick(&self, nick: &str) -> Option<&Mri> {
        self.nicks.get(&casefold(nick))
    }

    /// Whether `nick` is a service user.
    pub fn is_reserved(&self, nick: &str) -> bool {
        self.reserved.contains_key(&casefold(nick))
    }

    /// The channel name of `conv`, allocating `#` + slug of `label` on first
    /// sight (a team channel passes `Team/Channel`).
    pub fn channel(&mut self, conv: &ConversationId, label: &str) -> String {
        if let Some(c) = self.channels.get(conv) {
            return c.clone();
        }
        let mut slug = channel_slug(label);
        if slug.is_empty() {
            slug = "chat".into();
        }
        let base = format!("#{slug}");
        let mut name = base.clone();
        let mut n = 2;
        while self.channel_names.contains_key(&casefold(&name)) {
            name = format!("{base}-{n}");
            n += 1;
        }
        self.channel_names.insert(casefold(&name), conv.clone());
        self.channels.insert(conv.clone(), name.clone());
        name
    }

    /// The channel name of `conv` if one was handed out.
    pub fn existing_channel(&self, conv: &ConversationId) -> Option<&str> {
        self.channels.get(conv).map(String::as_str)
    }

    /// The conversation a channel name stands for.
    pub fn conversation_by_channel(&self, name: &str) -> Option<&ConversationId> {
        self.channel_names.get(&casefold(name))
    }

    fn free_nick(&self, base: &str) -> String {
        let taken = |n: &str| {
            self.nicks.contains_key(&casefold(n)) || self.reserved.contains_key(&casefold(n))
        };
        if !taken(base) {
            return base.to_string();
        }
        (2..)
            .map(|i| {
                let suffix = i.to_string();
                let keep = NICK_MAX.saturating_sub(suffix.len());
                format!("{}{suffix}", truncate(base, keep))
            })
            .find(|n| !taken(n))
            .unwrap_or_else(|| base.to_string())
    }
}

/// Characters allowed in a nick besides ASCII letters and digits.
const NICK_SPECIAL: &[char] = &['_', '-', '[', ']', '\\', '^', '{', '}', '|', '`'];

/// The nick for a display name: transliterated to ASCII, keeping letters,
/// digits and the IRC specials, never starting with a digit or `-`, at most
/// 30 characters. Empty when nothing usable is left.
pub fn nick_base(name: &str) -> String {
    let ascii = deunicode::deunicode(name);
    let mut out: String = ascii
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || NICK_SPECIAL.contains(c))
        .collect();
    while out.starts_with(|c: char| c.is_ascii_digit() || c == '-') {
        out.remove(0);
    }
    truncate(&out, NICK_MAX).to_string()
}

/// A provisional nick: `user-` and the first 6 characters of the id's GUID.
fn provisional_base(mri: &Mri) -> String {
    let id = mri.orgid_guid().unwrap_or(&mri.0);
    let short: String = id
        .chars()
        .filter(char::is_ascii_alphanumeric)
        .take(6)
        .collect();
    format!("user-{short}")
}

/// A channel-name slug: transliterated, whitespace runs become `-`, only
/// characters every client takes kept (letters, digits, `-_./&+'`), dashes
/// collapsed and trimmed.
pub fn channel_slug(label: &str) -> String {
    let ascii = deunicode::deunicode(label);
    let mut out = String::with_capacity(ascii.len());
    for c in ascii.chars() {
        let c = if c.is_whitespace() || c == ',' {
            '-'
        } else {
            c
        };
        if !(c.is_ascii_alphanumeric() || "-_./&+'".contains(c)) {
            continue;
        }
        if c == '-' && out.ends_with('-') {
            continue;
        }
        out.push(c);
    }
    let trimmed = out.trim_matches('-');
    truncate(trimmed, CHANNEL_MAX - 1)
        .trim_end_matches('-')
        .to_string()
}

fn truncate(s: &str, max: usize) -> &str {
    match s.char_indices().nth(max) {
        Some((i, _)) => &s[..i],
        None => s,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mri(s: &str) -> Mri {
        Mri(format!("8:orgid:{s}"))
    }

    #[test]
    fn nick_from_display_name() {
        assert_eq!(nick_base("Jane Smith"), "JaneSmith");
        assert_eq!(nick_base("José Álvarez-Núñez"), "JoseAlvarez-Nunez");
        assert_eq!(nick_base("3D Printer"), "DPrinter");
        assert_eq!(nick_base("(external) O'Brien, Pat"), "externalOBrienPat");
        assert_eq!(nick_base("🙂"), "slightsmile");
        assert_eq!(nick_base("   "), "");
    }

    #[test]
    fn clashes_get_numbers_and_reserved_names_are_skipped() {
        let mut d = Directory::new();
        assert_eq!(d.nick(&mri("a"), Some("Jane Smith")), "JaneSmith");
        assert_eq!(d.nick(&mri("b"), Some("Jane Smith")), "JaneSmith2");
        assert_eq!(d.nick(&mri("a"), Some("Other")), "JaneSmith");
        assert_eq!(d.nick(&mri("c"), Some("server")), "server2");
        assert_eq!(d.user_by_nick("janesmith2"), Some(&mri("b")));
    }

    #[test]
    fn provisional_nick_is_replaced_once() {
        let mut d = Directory::new();
        let m = mri("1a2b3c4d-0000");
        assert_eq!(d.nick(&m, None), "user-1a2b3c");
        let r = d.learn_name(&m, "Ada Lovelace").unwrap();
        assert_eq!(
            (r.old.as_str(), r.new.as_str()),
            ("user-1a2b3c", "AdaLovelace")
        );
        assert_eq!(d.learn_name(&m, "Someone Else"), None);
        assert_eq!(d.user_by_nick("user-1a2b3c"), None);
    }

    #[test]
    fn force_nick_displaces_the_holder() {
        let mut d = Directory::new();
        d.nick(&mri("a"), Some("Ada"));
        let r = d.force_nick(&mri("me"), "Ada").unwrap();
        assert_eq!(r.new, "Ada_");
        assert_eq!(d.user_by_nick("ada"), Some(&mri("me")));
        assert_eq!(d.existing_nick(&mri("a")), Some("Ada_"));
    }

    #[test]
    fn channel_names() {
        let mut d = Directory::new();
        let c1 = ConversationId("19:a@thread.v2".into());
        let c2 = ConversationId("19:b@thread.v2".into());
        assert_eq!(
            d.channel(&c1, "Project Ünicorn, phase 2!"),
            "#Project-Unicorn-phase-2"
        );
        assert_eq!(
            d.channel(&c2, "project unicorn phase 2"),
            "#project-unicorn-phase-2-2"
        );
        assert_eq!(d.channel(&c1, "renamed"), "#Project-Unicorn-phase-2");
        assert_eq!(
            d.conversation_by_channel("#PROJECT-UNICORN-PHASE-2"),
            Some(&c1)
        );
        let c3 = ConversationId("19:c@thread.tacv2".into());
        assert_eq!(
            d.channel(&c3, "Engineering/General"),
            "#Engineering/General"
        );
        let c4 = ConversationId("19:d@thread.v2".into());
        assert_eq!(d.channel(&c4, "!!!"), "#chat");
    }
}
