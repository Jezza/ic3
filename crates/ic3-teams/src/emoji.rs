//! Teams' reaction keys and the emoji they stand for.
//!
//! Teams names a reaction by its own emoticon id (`like`, `steamtrain`,
//! `1f3aa_circustent`, …), not by the emoji. The table (`emoji.tsv`, made by
//! `tools/teams-emoji/gen.py` from Teams' public emoji metadata) maps each id
//! to its emoji. Observed 2026-10-22: a steam train reaction is PUT as
//! `{"emotions":{"key":"steamtrain",…}}`.

use std::collections::HashMap;
use std::sync::OnceLock;

/// `id<TAB>emoji<TAB>popularity rank`, one emoticon per line.
const TABLE: &str = include_str!("emoji.tsv");

/// The reactions Teams offers first; they win when several ids share an
/// emoji.
const CLASSIC: [&str; 6] = ["like", "heart", "laugh", "surprised", "sad", "angry"];

struct Tables {
    by_key: HashMap<&'static str, &'static str>,
    /// Emoji without variation selectors → the preferred key.
    by_emoji: HashMap<String, &'static str>,
}

fn tables() -> &'static Tables {
    static T: OnceLock<Tables> = OnceLock::new();
    T.get_or_init(|| {
        let mut by_key = HashMap::new();
        let mut best: HashMap<String, (&'static str, (bool, u32))> = HashMap::new();
        for line in TABLE.lines() {
            let mut parts = line.split('\t');
            let (Some(key), Some(emoji)) = (parts.next(), parts.next()) else {
                continue;
            };
            let rank: u32 = parts
                .next()
                .and_then(|r| r.parse().ok())
                .unwrap_or(u32::MAX);
            by_key.insert(key, emoji);
            // Classic keys first, then the most popular.
            let order = (!CLASSIC.contains(&key), rank);
            let slot = best.entry(bare(emoji)).or_insert((key, order));
            if order < slot.1 {
                *slot = (key, order);
            }
        }
        let by_emoji = best.into_iter().map(|(e, (k, _))| (e, k)).collect();
        Tables { by_key, by_emoji }
    })
}

/// `emoji` without variation selectors (U+FE0E, U+FE0F), so `❤` and `❤️`
/// match.
fn bare(emoji: &str) -> String {
    emoji
        .chars()
        .filter(|c| !matches!(c, '\u{fe0e}' | '\u{fe0f}'))
        .collect()
}

/// The emoji a reaction key stands for. A key Teams extends with a
/// suffix after `;` (e.g. a skin tone) falls back to its base.
pub fn emoji_for_key(key: &str) -> Option<&'static str> {
    let t = tables();
    t.by_key
        .get(key)
        .or_else(|| key.split_once(';').and_then(|(base, _)| t.by_key.get(base)))
        .copied()
}

/// The reaction key Teams uses for `emoji`, if it has one.
pub fn key_for_emoji(emoji: &str) -> Option<&'static str> {
    tables().by_emoji.get(&bare(emoji.trim())).copied()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classic_and_named_keys() {
        assert_eq!(emoji_for_key("like"), Some("👍"));
        assert_eq!(emoji_for_key("laugh"), Some("😆"));
        assert_eq!(emoji_for_key("steamtrain"), Some("🚂"));
        assert_eq!(emoji_for_key("1f3aa_circustent"), Some("🎪"));
        assert_eq!(emoji_for_key("like;skintone2"), Some("👍"));
        assert_eq!(emoji_for_key("no-such-key"), None);
    }

    #[test]
    fn emoji_back_to_keys() {
        assert_eq!(key_for_emoji("🚂"), Some("steamtrain"));
        assert_eq!(key_for_emoji("👍"), Some("like"));
        assert_eq!(key_for_emoji("❤"), Some("heart"));
        assert_eq!(key_for_emoji("❤️"), Some("heart"));
        assert_eq!(key_for_emoji("😆"), Some("laugh"));
        assert_eq!(key_for_emoji("not an emoji"), None);
    }

    #[test]
    fn every_key_round_trips_to_a_key_with_the_same_emoji() {
        for line in TABLE.lines() {
            let key = line.split('\t').next().unwrap();
            let emoji = emoji_for_key(key).unwrap();
            let back = key_for_emoji(emoji).unwrap();
            assert_eq!(bare(emoji_for_key(back).unwrap()), bare(emoji), "{key}");
        }
    }
}
