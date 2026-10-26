//! Fuzzy matching of what a user types (`join agile topics`) against
//! channel names, chat names and people.
//!
//! Text is compared transliterated, lowercased, with everything but letters
//! and digits as word breaks. A candidate matches when every word of the
//! query appears in one of its keys (its IRC name, its Teams name); failing
//! that, when the query's letters appear in order. Better matches rank
//! first: exact, then every word at a word start, then anywhere, then the
//! in-order letters.

/// How well a candidate matched; higher is better.
pub type Score = u32;

/// A score at or above this is an exact match of a whole key.
pub const EXACT: Score = 1000;
/// A score at or above this matched the query's words, not just its
/// letters in order.
const WORDS: Score = 200;

/// `s` as comparable words.
fn words(s: &str) -> Vec<String> {
    deunicode::deunicode(s)
        .to_lowercase()
        .split(|c: char| !c.is_ascii_alphanumeric())
        .filter(|w| !w.is_empty())
        .map(str::to_owned)
        .collect()
}

/// How well `query` matches one key, if at all.
fn score_key(query: &[String], key: &str) -> Option<Score> {
    let kw = words(key);
    if kw.is_empty() || query.is_empty() {
        return None;
    }
    if kw == query {
        return Some(EXACT);
    }
    let joined = kw.join(" ");
    if query
        .iter()
        .all(|q| kw.iter().any(|w| w.starts_with(q.as_str())))
    {
        return Some(500 - (kw.len().min(100) as Score));
    }
    if query.iter().all(|q| joined.contains(q.as_str())) {
        return Some(300 - (kw.len().min(100) as Score));
    }
    let flat: String = kw.concat();
    let mut rest = flat.chars();
    let in_order = query
        .concat()
        .chars()
        .all(|c| rest.by_ref().any(|k| k == c));
    in_order.then(|| 100 - (flat.len().min(99) as Score))
}

/// The candidates that match `query`, best first, as indexes into
/// `candidates`; each candidate is matched by its best key.
pub fn rank<K: AsRef<str>>(query: &str, candidates: &[Vec<K>]) -> Vec<(Score, usize)> {
    let q = words(query);
    let mut out: Vec<(Score, usize)> = candidates
        .iter()
        .enumerate()
        .filter_map(|(i, keys)| {
            keys.iter()
                .filter_map(|k| score_key(&q, k.as_ref()))
                .max()
                .map(|s| (s, i))
        })
        .collect();
    // Letters in order are only a fallback: they match far too much when
    // some name has the words themselves.
    if out.iter().any(|(s, _)| *s >= WORDS) {
        out.retain(|(s, _)| *s >= WORDS);
    }
    out.sort_by(|a, b| b.0.cmp(&a.0).then(a.1.cmp(&b.1)));
    out
}

/// The one candidate a query picks: an exact match of a whole key, or the
/// only match. `None` when there is no match or the choice is ambiguous.
pub fn pick(ranked: &[(Score, usize)]) -> Option<usize> {
    match ranked {
        [(_, i)] => Some(*i),
        [(a, i), (b, _), ..] if *a >= EXACT && *b < EXACT => Some(*i),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cands() -> Vec<Vec<&'static str>> {
        vec![
            vec![
                "#WorkSpace-AgileTech/00-General",
                "WorkSpace AgileTech/00 General",
            ],
            vec![
                "#WorkSpace-AgileTech/10-Topcis",
                "WorkSpace AgileTech/10 Topcis",
            ],
            vec![
                "#Development-Team/02-Topics-all-Devs",
                "Development Team/02 Topics, all Devs",
            ],
            vec!["#Launch-plans", "Launch plans"],
            vec!["JoséÁlvarez", "José Álvarez"],
        ]
    }

    #[test]
    fn words_match_at_word_starts_first() {
        let r = rank("agile gen", &cands());
        assert_eq!(pick(&r), Some(0));
        let r = rank("topics", &cands());
        assert_eq!(r.len(), 1, "{r:?}"); // "Topcis" is a different word
        assert_eq!(pick(&r), Some(2));
        let r = rank("agile", &cands());
        assert_eq!(r.len(), 2);
        assert_eq!(pick(&r), None);
        // "Guidelines" has a-g-i-l-e in order, but words win.
        let c = vec![vec!["#Team/04-Guidelines"], vec!["#AgileTech/General"]];
        assert_eq!(rank("agile", &c), vec![(500 - 2, 1)]);
    }

    #[test]
    fn exact_names_win_over_other_matches() {
        let c = vec![vec!["#General"], vec!["#General-chat"]];
        let r = rank("#general", &c);
        assert_eq!(pick(&r), Some(0));
    }

    #[test]
    fn accents_and_in_order_letters() {
        let r = rank("jose", &cands());
        assert_eq!(pick(&r), Some(4));
        let r = rank("lnchpln", &cands());
        assert_eq!(pick(&r), Some(3));
        assert!(rank("zzz", &cands()).is_empty());
        assert!(rank("", &cands()).is_empty());
    }
}
