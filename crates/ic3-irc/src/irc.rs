//! IRC lines: parsing, serialising and the IRCv3 tag escapes
//! (<https://ircv3.net/specs/extensions/message-tags>).
//!
//! Tags keep their order (a `Vec`, not a map) so output is deterministic and
//! tests can compare whole lines.

use std::fmt;

/// Longest line read from a client: 8191 bytes of tags plus 512 of message.
pub const MAX_LINE: usize = 8191 + 512;

/// One IRC message.
#[derive(Clone, Debug, PartialEq, Eq, Default)]
pub struct Message {
    /// Tags in order; a value of `None` is a tag without `=`.
    pub tags: Vec<(String, Option<String>)>,
    /// `nick!user@host` or a server name, without the leading `:`.
    pub source: Option<String>,
    /// Upper-cased command or three-digit numeric.
    pub command: String,
    pub params: Vec<String>,
}

/// Why a line could not be parsed.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum ParseError {
    #[error("empty line")]
    Empty,
    #[error("no command")]
    NoCommand,
}

impl Message {
    /// A message with a command and parameters, no tags or source.
    pub fn new<I, S>(command: &str, params: I) -> Message
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        Message {
            tags: Vec::new(),
            source: None,
            command: command.to_string(),
            params: params.into_iter().map(Into::into).collect(),
        }
    }

    /// Sets the source.
    pub fn with_source(mut self, source: impl Into<String>) -> Message {
        self.source = Some(source.into());
        self
    }

    /// Sets (or replaces) a tag with a value.
    pub fn with_tag(mut self, key: &str, value: impl Into<String>) -> Message {
        self.set_tag(key, Some(value.into()));
        self
    }

    /// Sets, replaces or (with `None` value but present key) flags a tag.
    pub fn set_tag(&mut self, key: &str, value: Option<String>) {
        match self.tags.iter_mut().find(|(k, _)| k == key) {
            Some(slot) => slot.1 = value,
            None => self.tags.push((key.to_string(), value)),
        }
    }

    /// Removes a tag, returning its value if it was present.
    pub fn remove_tag(&mut self, key: &str) -> Option<Option<String>> {
        let i = self.tags.iter().position(|(k, _)| k == key)?;
        Some(self.tags.remove(i).1)
    }

    /// The value of tag `key`: `None` when absent, `Some("")` when present
    /// without a value.
    pub fn tag(&self, key: &str) -> Option<&str> {
        self.tags
            .iter()
            .find(|(k, _)| k == key)
            .map(|(_, v)| v.as_deref().unwrap_or(""))
    }

    /// The first present tag among `keys` (aliases such as `+reply` and
    /// `+draft/reply`).
    pub fn tag_any(&self, keys: &[&str]) -> Option<&str> {
        keys.iter().find_map(|k| self.tag(k))
    }

    /// Parameter `i`, or `""`.
    pub fn param(&self, i: usize) -> &str {
        self.params.get(i).map_or("", String::as_str)
    }

    /// The nick part of the source.
    pub fn source_nick(&self) -> Option<&str> {
        let s = self.source.as_deref()?;
        Some(s.split(['!', '@']).next().unwrap_or(s))
    }

    /// Parses one line (without its `\r\n`).
    pub fn parse(line: &str) -> Result<Message, ParseError> {
        let mut rest = line.trim_end_matches(['\r', '\n']).trim_start_matches(' ');
        if rest.is_empty() {
            return Err(ParseError::Empty);
        }
        let mut msg = Message::default();
        if let Some(r) = rest.strip_prefix('@') {
            let (tags, after) = r.split_once(' ').unwrap_or((r, ""));
            for t in tags.split(';').filter(|t| !t.is_empty()) {
                let (k, v) = match t.split_once('=') {
                    Some((k, v)) => (k, Some(unescape_tag(v))),
                    None => (t, None),
                };
                msg.set_tag(k, v.filter(|v| !v.is_empty()));
            }
            rest = after.trim_start_matches(' ');
        }
        if let Some(r) = rest.strip_prefix(':') {
            let (src, after) = r.split_once(' ').unwrap_or((r, ""));
            msg.source = Some(src.to_string());
            rest = after.trim_start_matches(' ');
        }
        let (cmd, mut rest) = rest.split_once(' ').unwrap_or((rest, ""));
        if cmd.is_empty() {
            return Err(ParseError::NoCommand);
        }
        msg.command = cmd.to_ascii_uppercase();
        loop {
            rest = rest.trim_start_matches(' ');
            if rest.is_empty() {
                break;
            }
            if let Some(trailing) = rest.strip_prefix(':') {
                msg.params.push(trailing.to_string());
                break;
            }
            let (p, after) = rest.split_once(' ').unwrap_or((rest, ""));
            msg.params.push(p.to_string());
            rest = after;
        }
        Ok(msg)
    }
}

impl fmt::Display for Message {
    /// The wire form, without `\r\n`. The last parameter is written as a
    /// trailing parameter when it needs to be (empty, has a space or starts
    /// with `:`). `\r`, `\n` and NUL in parameters become spaces.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if !self.tags.is_empty() {
            f.write_str("@")?;
            for (i, (k, v)) in self.tags.iter().enumerate() {
                if i > 0 {
                    f.write_str(";")?;
                }
                f.write_str(k)?;
                if let Some(v) = v.as_deref().filter(|v| !v.is_empty()) {
                    write!(f, "={}", escape_tag(v))?;
                }
            }
            f.write_str(" ")?;
        }
        if let Some(s) = &self.source {
            write!(f, ":{s} ")?;
        }
        f.write_str(&self.command)?;
        let n = self.params.len();
        for (i, p) in self.params.iter().enumerate() {
            let p = clean_param(p);
            let last = i + 1 == n;
            if last && (p.is_empty() || p.contains(' ') || p.starts_with(':')) {
                write!(f, " :{p}")?;
            } else {
                write!(f, " {p}")?;
            }
        }
        Ok(())
    }
}

fn clean_param(p: &str) -> std::borrow::Cow<'_, str> {
    if p.contains(['\r', '\n', '\0']) {
        p.replace(['\r', '\n', '\0'], " ").into()
    } else {
        p.into()
    }
}

/// Escapes a tag value (`;` → `\:`, space → `\s`, `\` → `\\`, CR → `\r`,
/// LF → `\n`).
pub fn escape_tag(v: &str) -> String {
    let mut out = String::with_capacity(v.len());
    for c in v.chars() {
        match c {
            ';' => out.push_str("\\:"),
            ' ' => out.push_str("\\s"),
            '\\' => out.push_str("\\\\"),
            '\r' => out.push_str("\\r"),
            '\n' => out.push_str("\\n"),
            c => out.push(c),
        }
    }
    out
}

/// Reverses [`escape_tag`]; an unknown escape drops the backslash and a
/// trailing lone backslash is dropped, as the spec says.
pub fn unescape_tag(v: &str) -> String {
    let mut out = String::with_capacity(v.len());
    let mut chars = v.chars();
    while let Some(c) = chars.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        match chars.next() {
            Some(':') => out.push(';'),
            Some('s') => out.push(' '),
            Some('\\') => out.push('\\'),
            Some('r') => out.push('\r'),
            Some('n') => out.push('\n'),
            Some(other) => out.push(other),
            None => {}
        }
    }
    out
}

/// ASCII case folding (`CASEMAPPING=ascii`).
pub fn casefold(s: &str) -> String {
    s.to_ascii_lowercase()
}

/// Formats a timestamp in milliseconds for the `time` tag
/// (`YYYY-MM-DDThh:mm:ss.sssZ`).
pub fn format_time(ms: i64) -> String {
    chrono::DateTime::from_timestamp_millis(ms)
        .unwrap_or_default()
        .format("%Y-%m-%dT%H:%M:%S%.3fZ")
        .to_string()
}

/// Parses a `time` tag value or a `timestamp=` reference value to
/// milliseconds.
pub fn parse_time(s: &str) -> Option<i64> {
    chrono::DateTime::parse_from_rfc3339(s)
        .ok()
        .map(|d| d.timestamp_millis())
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    #[test]
    fn parses_tags_source_and_trailing() {
        let m = Message::parse(
            "@label=a1;+draft/reply=123;flag :nick!u@h PRIVMSG #chan :hello there\r\n",
        )
        .unwrap();
        assert_eq!(m.tag("label"), Some("a1"));
        assert_eq!(m.tag_any(&["+reply", "+draft/reply"]), Some("123"));
        assert_eq!(m.tag("flag"), Some(""));
        assert_eq!(m.source_nick(), Some("nick"));
        assert_eq!(m.command, "PRIVMSG");
        assert_eq!(m.params, vec!["#chan", "hello there"]);
    }

    #[test]
    fn parses_without_trailing_and_lowercase_command() {
        let m = Message::parse("nick  alice").unwrap();
        assert_eq!(m.command, "NICK");
        assert_eq!(m.params, vec!["alice"]);
        assert_eq!(Message::parse(""), Err(ParseError::Empty));
        assert_eq!(Message::parse("@a=b "), Err(ParseError::NoCommand));
    }

    #[test]
    fn round_trips_escaped_tags() {
        let m = Message::new("EDIT", ["#a"])
            .with_tag("draft/edit-text", "hello world;\\ x\nnext")
            .with_source("me!me@teams");
        let line = m.to_string();
        assert_eq!(
            line,
            "@draft/edit-text=hello\\sworld\\:\\\\\\sx\\nnext :me!me@teams EDIT #a"
        );
        assert_eq!(Message::parse(&line).unwrap(), m);
    }

    #[test]
    fn trailing_only_when_needed() {
        assert_eq!(
            Message::new("PRIVMSG", ["#a", "hi"]).to_string(),
            "PRIVMSG #a hi"
        );
        assert_eq!(
            Message::new("PRIVMSG", ["#a", ":)"]).to_string(),
            "PRIVMSG #a ::)"
        );
        assert_eq!(Message::new("TOPIC", ["#a", ""]).to_string(), "TOPIC #a :");
        assert_eq!(
            Message::new("PRIVMSG", ["#a", "a\r\nb"]).to_string(),
            "PRIVMSG #a :a  b"
        );
    }

    #[test]
    fn unescape_edge_cases() {
        assert_eq!(unescape_tag("a\\b"), "ab");
        assert_eq!(unescape_tag("a\\"), "a");
    }

    #[test]
    fn time_format_round_trips() {
        let t = 1_791_882_000_123;
        assert_eq!(format_time(t), "2026-10-13T09:00:00.123Z");
        assert_eq!(parse_time(&format_time(t)), Some(t));
    }
}
