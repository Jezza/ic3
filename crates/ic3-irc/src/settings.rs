//! Settings changed at run time (through `Server`), kept in a small JSON
//! file next to the session so they survive restarts.

use std::path::Path;
use std::time::Duration;

use serde::{Deserialize, Serialize};

/// What is kept. Every field is optional: unset falls back to the config.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Settings {
    /// Group chats idle longer than this many seconds are not joined;
    /// `Some(0)` joins every chat.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cutoff_secs: Option<u64>,
    /// Chats left from IRC (hidden in Teams), by id, with when (ms): they
    /// stay left until a message newer than that.
    #[serde(default, skip_serializing_if = "std::collections::BTreeMap::is_empty")]
    pub parted: std::collections::BTreeMap<String, i64>,
}

impl Settings {
    /// Reads `path`; a missing or unreadable file is no settings.
    pub fn load(path: &Path) -> Settings {
        match std::fs::read_to_string(path) {
            Ok(text) => serde_json::from_str(&text).unwrap_or_else(|e| {
                tracing::warn!(path = %path.display(), error = %e, "ignoring unreadable settings");
                Settings::default()
            }),
            Err(_) => Settings::default(),
        }
    }

    /// Writes `path` (through a temporary file, so a crash never leaves a
    /// half-written one).
    pub fn save(&self, path: &Path) -> std::io::Result<()> {
        let tmp = path.with_extension("json.tmp");
        std::fs::write(&tmp, serde_json::to_vec_pretty(self)?)?;
        std::fs::rename(tmp, path)
    }

    /// The cutoff this sets, if it sets one: `Some(None)` is "no cutoff".
    pub fn cutoff(&self) -> Option<Option<Duration>> {
        self.cutoff_secs
            .map(|s| (s > 0).then(|| Duration::from_secs(s)))
    }

    pub fn set_cutoff(&mut self, cutoff: Option<Duration>) {
        self.cutoff_secs = Some(cutoff.map_or(0, |d| d.as_secs()));
    }
}

/// Parses a cutoff: anything `humantime` takes (`60d`, `8w`, `12h`,
/// `1w 3d`), a bare number of days, or `off` / `0` / `all` for none.
pub fn parse_cutoff(s: &str) -> Result<Option<Duration>, String> {
    let s = s.trim().to_ascii_lowercase();
    if matches!(s.as_str(), "off" | "all" | "none") {
        return Ok(None);
    }
    let d = match s.parse::<u64>() {
        Ok(days) => Duration::from_secs(days.saturating_mul(24 * 3600)),
        Err(_) => humantime::parse_duration(&s)
            .map_err(|e| format!("\"{s}\" is not a duration ({e}); try 60d, 8w or 12h"))?,
    };
    Ok((!d.is_zero()).then_some(d))
}

/// A cutoff the way a person reads it: `60 days`, `2 weeks`, `12 hours`,
/// `off`.
pub fn format_cutoff(cutoff: Option<Duration>) -> String {
    let Some(d) = cutoff else {
        return "off (every chat)".into();
    };
    let s = d.as_secs();
    let plural = |n: u64, unit: &str| format!("{n} {unit}{}", if n == 1 { "" } else { "s" });
    const DAY: u64 = 24 * 3600;
    let days = s / DAY;
    if s.is_multiple_of(DAY) && days >= 14 && days.is_multiple_of(7) {
        plural(days / 7, "week")
    } else if s.is_multiple_of(DAY) {
        plural(days, "day")
    } else {
        plural(s.div_ceil(3600), "hour")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn durations() {
        let days = |n: u64| Some(Duration::from_secs(n * 24 * 3600));
        assert_eq!(parse_cutoff("60d"), Ok(days(60)));
        assert_eq!(parse_cutoff("60"), Ok(days(60)));
        assert_eq!(parse_cutoff(" 2W "), Ok(days(14)));
        assert_eq!(
            parse_cutoff("12h"),
            Ok(Some(Duration::from_secs(12 * 3600)))
        );
        assert_eq!(parse_cutoff("off"), Ok(None));
        assert_eq!(parse_cutoff("0d"), Ok(None));
        assert_eq!(parse_cutoff("1w 3d"), Ok(days(10)));
        assert!(parse_cutoff("soon").is_err());
        assert_eq!(format_cutoff(days(60)), "60 days");
        assert_eq!(format_cutoff(days(14)), "2 weeks");
        assert_eq!(format_cutoff(days(1)), "1 day");
        assert_eq!(
            format_cutoff(Some(Duration::from_secs(12 * 3600))),
            "12 hours"
        );
        assert_eq!(format_cutoff(None), "off (every chat)");
    }

    #[test]
    fn saved_and_loaded() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("settings.json");
        assert_eq!(Settings::load(&path), Settings::default());
        let mut s = Settings::default();
        s.set_cutoff(None);
        s.save(&path).unwrap();
        assert_eq!(Settings::load(&path).cutoff(), Some(None));
        s.set_cutoff(Some(Duration::from_secs(3600)));
        s.save(&path).unwrap();
        assert_eq!(
            Settings::load(&path).cutoff(),
            Some(Some(Duration::from_secs(3600)))
        );
    }
}
