//! The capabilities the server offers and what each client enabled.

use std::collections::BTreeSet;

/// Longest multiline message accepted and sent, in bytes and lines.
pub const MULTILINE_MAX_BYTES: usize = 40_000;
pub const MULTILINE_MAX_LINES: usize = 200;

/// Most messages one CHATHISTORY request returns.
pub const CHATHISTORY_MAX: usize = 100;

/// Every capability offered, with its `CAP LS 302` value.
pub fn offered() -> Vec<(&'static str, String)> {
    vec![
        ("away-notify", String::new()),
        ("batch", String::new()),
        ("cap-notify", String::new()),
        ("draft/chathistory", String::new()),
        ("draft/event-playback", String::new()),
        ("draft/message-edit", String::new()),
        ("draft/message-redaction", String::new()),
        ("draft/metadata-2", "max-subs=16,max-keys=0".into()),
        (
            "draft/multiline",
            format!("max-bytes={MULTILINE_MAX_BYTES},max-lines={MULTILINE_MAX_LINES}"),
        ),
        ("draft/read-marker", String::new()),
        ("echo-message", String::new()),
        ("labeled-response", String::new()),
        ("message-tags", String::new()),
        ("sasl", "PLAIN".into()),
        ("server-time", String::new()),
        ("standard-replies", String::new()),
        ("userhost-in-names", String::new()),
    ]
}

/// Whether `name` is offered.
pub fn is_offered(name: &str) -> bool {
    offered().iter().any(|(n, _)| *n == name)
}

/// The capabilities one client enabled.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Caps(BTreeSet<String>);

impl Caps {
    pub fn has(&self, name: &str) -> bool {
        self.0.contains(name)
    }

    pub fn insert(&mut self, name: &str) {
        self.0.insert(name.to_string());
    }

    pub fn remove(&mut self, name: &str) {
        self.0.remove(name);
    }

    pub fn names(&self) -> impl Iterator<Item = &str> {
        self.0.iter().map(String::as_str)
    }

    pub fn tags(&self) -> bool {
        self.has("message-tags")
    }

    pub fn time(&self) -> bool {
        self.has("server-time")
    }

    pub fn batch(&self) -> bool {
        self.has("batch")
    }

    pub fn echo(&self) -> bool {
        self.has("echo-message")
    }

    pub fn labels(&self) -> bool {
        self.has("labeled-response")
    }

    pub fn multiline(&self) -> bool {
        self.has("draft/multiline") && self.batch()
    }

    pub fn edit(&self) -> bool {
        self.has("draft/message-edit")
    }

    pub fn redaction(&self) -> bool {
        self.has("draft/message-redaction")
    }

    pub fn read_marker(&self) -> bool {
        self.has("draft/read-marker")
    }

    pub fn event_playback(&self) -> bool {
        self.has("draft/event-playback")
    }

    pub fn metadata(&self) -> bool {
        self.has("draft/metadata-2")
    }

    pub fn userhost_in_names(&self) -> bool {
        self.has("userhost-in-names")
    }
}

/// The `CAP LS` lines: tokens split so no line's list exceeds ~400 bytes.
/// `version >= 302` adds values.
pub fn ls_lists(version: u32) -> Vec<String> {
    let toks: Vec<String> = offered()
        .into_iter()
        .map(|(n, v)| {
            if version >= 302 && !v.is_empty() {
                format!("{n}={v}")
            } else {
                n.to_string()
            }
        })
        .collect();
    let mut lines = Vec::new();
    let mut cur = String::new();
    for t in toks {
        if !cur.is_empty() && cur.len() + t.len() + 1 > 400 {
            lines.push(std::mem::take(&mut cur));
        }
        if !cur.is_empty() {
            cur.push(' ');
        }
        cur.push_str(&t);
    }
    lines.push(cur);
    lines
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ls_has_values_only_for_302() {
        let l = ls_lists(302).join(" ");
        assert!(l.contains("sasl=PLAIN"));
        assert!(l.contains("draft/multiline=max-bytes=40000,max-lines=200"));
        let old = ls_lists(0).join(" ");
        assert!(old.contains(" sasl ") || old.ends_with(" sasl") || old.contains("sasl "));
        assert!(!old.contains('='));
    }

    #[test]
    fn multiline_needs_batch() {
        let mut c = Caps::default();
        c.insert("draft/multiline");
        assert!(!c.multiline());
        c.insert("batch");
        assert!(c.multiline());
        assert!(is_offered("draft/read-marker"));
        assert!(is_offered("away-notify"));
        assert!(!is_offered("account-notify"));
    }
}
