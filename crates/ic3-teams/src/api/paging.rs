//! Pages and the cursor that requests the next one (spec §4.4.3).
//!
//! How more than one page is fetched is unverified (§9.1, §9.2, §9.11); every
//! assumption about it lives in `next_cursor`, selectable at run time by
//! [`PagingMode`] (spec §9 runtime switches).

use serde_json::Value;

/// How the next page is requested (spec §9, option `paging`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum PagingMode {
    /// Follow `_metadata.backwardLink` when present; otherwise page messages by
    /// time window and fetch a single page of conversations.
    #[default]
    Auto,
    /// Always page messages by time window; never follow `backwardLink`.
    Window,
    /// Only follow `backwardLink`; its absence ends paging with a note.
    BackwardLink,
}

impl PagingMode {
    /// Parses `auto`, `window` or `backward_link`.
    pub fn parse(s: &str) -> Option<PagingMode> {
        match s {
            "auto" => Some(PagingMode::Auto),
            "window" => Some(PagingMode::Window),
            "backward_link" => Some(PagingMode::BackwardLink),
            _ => None,
        }
    }

    /// The option string [`PagingMode::parse`] accepts.
    pub fn as_str(self) -> &'static str {
        match self {
            PagingMode::Auto => "auto",
            PagingMode::Window => "window",
            PagingMode::BackwardLink => "backward_link",
        }
    }
}

/// Where the next page starts. Opaque: obtain one from [`Page::next`] and hand
/// it back to the endpoint that produced it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PageCursor(pub(crate) CursorInner);

impl PageCursor {
    /// A time-window cursor (messages with ids up to `ms`). For consumers'
    /// test fakes only.
    #[doc(hidden)]
    pub fn end_time(ms: i64) -> PageCursor {
        PageCursor(CursorInner::EndTime(ms))
    }

    /// A `backwardLink` cursor, as if produced by a first page. For consumers'
    /// test fakes only.
    #[doc(hidden)]
    pub fn link(url: &str) -> PageCursor {
        PageCursor(CursorInner::Link {
            url: url.to_owned(),
            pages: 1,
            prev_ids: Vec::new(),
            seen: 0,
        })
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum CursorInner {
    /// Request `startTime=1&endTime={ms}`: messages with ids up to `ms`.
    EndTime(i64),
    /// Request `url` verbatim. `pages` counts the pages fetched so far and
    /// `prev_ids` holds the ids of the last one, for the loop guards; `seen`
    /// counts the items of every page so far, against `totalCount`.
    Link {
        url: String,
        pages: u32,
        prev_ids: Vec<String>,
        seen: usize,
    },
}

/// A response's `_metadata`, reported so §9.1 can be answered by capture.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct PageMeta {
    pub backward_link: Option<String>,
    pub sync_state: Option<String>,
    pub total_count: Option<i64>,
    /// The `_metadata` object as received.
    pub raw: Option<Value>,
}

/// One page of results, newest first.
#[derive(Clone, Debug, PartialEq)]
pub struct Page<T> {
    pub items: Vec<T>,
    /// The cursor for the next (older) page; `None` when paging is over.
    pub next: Option<PageCursor>,
    pub meta: PageMeta,
    /// Why paging stopped early (a loop guard fired), if it did.
    pub note: Option<String>,
}

/// The endpoint a page came from; their paging defaults differ.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Endpoint {
    Conversations,
    Messages,
}

/// Most `backwardLink` pages followed before stopping with a note.
const MAX_LINK_PAGES: u32 = 50;

/// Reads a response's `_metadata`. Empty strings count as absent; `totalCount`
/// may be a number or a numeric string.
pub(crate) fn parse_page_meta(v: Option<&Value>) -> PageMeta {
    let Some(v) = v else {
        return PageMeta::default();
    };
    let string = |k: &str| {
        v.get(k)
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .map(str::to_owned)
    };
    PageMeta {
        backward_link: string("backwardLink"),
        sync_state: string("syncState"),
        total_count: v.get("totalCount").and_then(super::wire::lenient_i64),
        raw: Some(v.clone()),
    }
}

/// Decides how the next page is requested, from the page just received
/// (`meta`, the ids of its items, the cursor that requested it) and the page
/// size asked for. Returns the next cursor, or `None` with an optional note
/// saying why a loop guard stopped paging.
///
/// UNVERIFIED(§9.1): conversations beyond the first 100 (assumed: `_metadata.backwardLink` when present and the page was full and short of `totalCount`, else none; a refused followed page ends paging with a note, in `Api::conversations`; live 2026-10-14: a 96-chat account got all 96 on page 1 plus a `backwardLink` == `syncState`).
/// UNVERIFIED(§9.2): older messages via `startTime=1&endTime={oldest_id-1}` (purple-teams' window) unless a backwardLink exists.
/// UNVERIFIED(§9.11): the window assumes `endTime` filters on the arrival `id`, not `composetime`.
pub(crate) fn next_cursor(
    mode: PagingMode,
    endpoint: Endpoint,
    meta: &PageMeta,
    prev: Option<&PageCursor>,
    page_ids: &[String],
    page_size: usize,
) -> (Option<PageCursor>, Option<String>) {
    let link = meta
        .backward_link
        .as_ref()
        .filter(|_| matches!(mode, PagingMode::Auto | PagingMode::BackwardLink));
    if let Some(link) = link {
        let (pages, prev_link, prev_ids, prev_seen): (u32, Option<&str>, &[String], usize) =
            match prev {
                Some(PageCursor(CursorInner::Link {
                    url,
                    pages,
                    prev_ids,
                    seen,
                })) => (*pages, Some(url), prev_ids, *seen),
                _ => (0, None, &[], 0),
            };
        if page_ids.is_empty() {
            return (None, None);
        }
        let seen = prev_seen + page_ids.len();
        // A short page, or every conversation `totalCount` announced, is the
        // whole listing: the live server sends a `backwardLink` (== `syncState`,
        // a delta-sync link) even then.
        if endpoint == Endpoint::Conversations {
            let seen_i = i64::try_from(seen).unwrap_or(i64::MAX);
            match meta.total_count {
                Some(t) if seen_i >= t => return (None, None),
                // Stopped, but the server said there is more: report it.
                Some(t) if page_ids.len() < page_size => {
                    return (
                        None,
                        Some(format!(
                            "short page but totalCount {t} > {seen} seen; stopped"
                        )),
                    );
                }
                _ if page_ids.len() < page_size => return (None, None),
                _ => {}
            }
        }
        if prev_link == Some(link.as_str()) {
            return (
                None,
                Some("backwardLink repeated the previous link; stopped".into()),
            );
        }
        if !prev_ids.is_empty() && page_ids.iter().all(|id| prev_ids.contains(id)) {
            return (
                None,
                Some("backwardLink page had no new items; stopped".into()),
            );
        }
        if pages + 1 >= MAX_LINK_PAGES {
            return (
                None,
                Some(format!(
                    "backwardLink {MAX_LINK_PAGES}-page cap reached; stopped"
                )),
            );
        }
        return (
            Some(PageCursor(CursorInner::Link {
                url: link.clone(),
                pages: pages + 1,
                prev_ids: page_ids.to_vec(),
                seen,
            })),
            None,
        );
    }
    match (mode, endpoint) {
        (PagingMode::BackwardLink, _) => (
            None,
            Some("paging=backward_link but the response had no _metadata.backwardLink".into()),
        ),
        (_, Endpoint::Conversations) => (None, None),
        (_, Endpoint::Messages) => {
            if page_ids.len() < page_size {
                return (None, None);
            }
            let Some(oldest) = page_ids.iter().filter_map(|s| s.parse::<i64>().ok()).min() else {
                return (
                    None,
                    Some("no numeric message ids; cannot page by time window".into()),
                );
            };
            if let Some(PageCursor(CursorInner::EndTime(end))) = prev
                && oldest > *end
            {
                return (
                    None,
                    Some("time window did not move (server ignored endTime?); stopped".into()),
                );
            }
            (
                Some(PageCursor(CursorInner::EndTime(oldest.saturating_sub(1)))),
                None,
            )
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ids(n: usize, start: i64) -> Vec<String> {
        (0..n)
            .map(|i| (start - i as i64 * 60_000).to_string())
            .collect()
    }

    #[test]
    fn conversations_default_is_single_page_without_link() {
        let (c, note) = next_cursor(
            PagingMode::Auto,
            Endpoint::Conversations,
            &PageMeta::default(),
            None,
            &ids(100, 1_760_346_000_000),
            100,
        );
        assert_eq!(c, None);
        assert_eq!(note, None);
    }

    #[test]
    fn backward_link_is_followed_in_auto_and_link_modes_but_not_window() {
        let meta = PageMeta {
            backward_link: Some("https://x/next".into()),
            ..Default::default()
        };
        for mode in [PagingMode::Auto, PagingMode::BackwardLink] {
            let (c, _) = next_cursor(mode, Endpoint::Conversations, &meta, None, &ids(3, 1), 3);
            assert_eq!(
                c,
                Some(PageCursor(CursorInner::Link {
                    url: "https://x/next".into(),
                    pages: 1,
                    prev_ids: ids(3, 1),
                    seen: 3,
                }))
            );
        }
        let (c, _) = next_cursor(
            PagingMode::Window,
            Endpoint::Messages,
            &meta,
            None,
            &ids(5, 1_760_346_000_000),
            5,
        );
        assert_eq!(
            c,
            Some(PageCursor(CursorInner::EndTime(1_760_345_760_000 - 1)))
        );
    }

    /// Live run 2026-10-14: page 1 held all 96 conversations (`totalCount`
    /// 96, `pageSize` 100) and still carried a `backwardLink` (== `syncState`).
    /// A short page, or reaching `totalCount`, ends conversation paging.
    #[test]
    fn conversations_stop_on_short_page_or_total_count() {
        let meta = |total: Option<i64>| PageMeta {
            backward_link: Some("https://x/next".into()),
            sync_state: Some("https://x/next".into()),
            total_count: total,
            raw: None,
        };
        for mode in [PagingMode::Auto, PagingMode::BackwardLink] {
            // short page: 96 < 100
            assert_eq!(
                next_cursor(
                    mode,
                    Endpoint::Conversations,
                    &meta(Some(96)),
                    None,
                    &ids(96, 1),
                    100
                ),
                (None, None)
            );
            // short page, no totalCount
            assert_eq!(
                next_cursor(
                    mode,
                    Endpoint::Conversations,
                    &meta(None),
                    None,
                    &ids(5, 1),
                    100
                ),
                (None, None)
            );
            // a full page that reaches totalCount
            assert_eq!(
                next_cursor(
                    mode,
                    Endpoint::Conversations,
                    &meta(Some(3)),
                    None,
                    &ids(3, 1),
                    3
                ),
                (None, None)
            );
        }
        // full pages below totalCount are followed, counting what was seen
        let (c, note) = next_cursor(
            PagingMode::Auto,
            Endpoint::Conversations,
            &meta(Some(7)),
            None,
            &ids(3, 1),
            3,
        );
        assert_eq!(note, None);
        let Some(PageCursor(CursorInner::Link { seen, .. })) = &c else {
            panic!("{c:?}")
        };
        assert_eq!(*seen, 3);
        let meta2 = PageMeta {
            backward_link: Some("https://x/next2".into()),
            ..meta(Some(7))
        };
        let (c2, _) = next_cursor(
            PagingMode::Auto,
            Endpoint::Conversations,
            &meta2,
            c.as_ref(),
            &ids(3, 100),
            3,
        );
        let Some(PageCursor(CursorInner::Link { seen, pages, .. })) = c2 else {
            panic!("6 of 7 seen: follow")
        };
        assert_eq!((seen, pages), (6, 2));
        // 3 + 3 = 6 of totalCount 6: done
        assert_eq!(
            next_cursor(
                PagingMode::Auto,
                Endpoint::Conversations,
                &PageMeta {
                    total_count: Some(6),
                    ..meta2.clone()
                },
                c.as_ref(),
                &ids(3, 100),
                3
            ),
            (None, None)
        );
        // a short page while totalCount says more exist: stop, but say so
        let (c, note) = next_cursor(
            PagingMode::Auto,
            Endpoint::Conversations,
            &meta(Some(250)),
            None,
            &ids(96, 1),
            100,
        );
        assert_eq!(c, None);
        assert_eq!(
            note.as_deref(),
            Some("short page but totalCount 250 > 96 seen; stopped")
        );
        // messages keep following a link after a short page (§9.2 verified that way)
        assert!(
            next_cursor(
                PagingMode::Auto,
                Endpoint::Messages,
                &meta(Some(1)),
                None,
                &ids(2, 1),
                5
            )
            .0
            .is_some()
        );
    }

    #[test]
    fn backward_link_guards() {
        let meta = PageMeta {
            backward_link: Some("https://x/next".into()),
            ..Default::default()
        };
        let prev = PageCursor(CursorInner::Link {
            url: "https://x/next".into(),
            pages: 1,
            prev_ids: ids(3, 1),
            seen: 3,
        });
        let note = |r: (Option<PageCursor>, Option<String>)| {
            assert_eq!(r.0, None, "a guard stop yields no cursor");
            r.1.unwrap()
        };
        assert!(
            note(next_cursor(
                PagingMode::Auto,
                Endpoint::Messages,
                &meta,
                Some(&prev),
                &ids(3, 7),
                3
            ))
            .contains("repeated")
        );
        let meta2 = PageMeta {
            backward_link: Some("https://x/next2".into()),
            ..Default::default()
        };
        assert!(
            note(next_cursor(
                PagingMode::Auto,
                Endpoint::Messages,
                &meta2,
                Some(&prev),
                &ids(3, 1),
                3
            ))
            .contains("no new items")
        );
        let prev49 = PageCursor(CursorInner::Link {
            url: "https://x/p49".into(),
            pages: 49,
            prev_ids: vec![],
            seen: 0,
        });
        assert!(
            note(next_cursor(
                PagingMode::Auto,
                Endpoint::Messages,
                &meta2,
                Some(&prev49),
                &ids(3, 1),
                3
            ))
            .contains("50")
        );
        assert_eq!(
            next_cursor(PagingMode::Auto, Endpoint::Messages, &meta2, None, &[], 3),
            (None, None)
        );
        assert!(
            note(next_cursor(
                PagingMode::BackwardLink,
                Endpoint::Messages,
                &PageMeta::default(),
                None,
                &ids(5, 1),
                5
            ))
            .contains("backwardLink")
        );
    }

    #[test]
    fn message_window_stops_on_short_page_or_no_progress() {
        assert_eq!(
            next_cursor(
                PagingMode::Auto,
                Endpoint::Messages,
                &PageMeta::default(),
                None,
                &ids(2, 1_760_346_000_000),
                5
            ),
            (None, None)
        );
        let prev = PageCursor(CursorInner::EndTime(1_760_345_759_999));
        let (c, note) = next_cursor(
            PagingMode::Auto,
            Endpoint::Messages,
            &PageMeta::default(),
            Some(&prev),
            &ids(5, 1_760_346_000_000),
            5,
        );
        assert_eq!(c, None);
        assert!(note.unwrap().contains("window"));
        let (c, _) = next_cursor(
            PagingMode::Auto,
            Endpoint::Messages,
            &PageMeta::default(),
            Some(&prev),
            &ids(5, 1_760_345_700_000),
            5,
        );
        assert_eq!(
            c,
            Some(PageCursor(CursorInner::EndTime(1_760_345_460_000 - 1)))
        );
    }

    /// Review Focus #1: a full page of non-numeric ids cannot be windowed; it
    /// stops with a note instead of panicking or looping.
    #[test]
    fn message_window_without_numeric_ids_stops_with_note() {
        let page: Vec<String> = (0..5).map(|i| format!("bot-{i}")).collect();
        let (c, note) = next_cursor(
            PagingMode::Window,
            Endpoint::Messages,
            &PageMeta::default(),
            None,
            &page,
            5,
        );
        assert_eq!(c, None);
        assert!(note.unwrap().contains("numeric"));
    }

    #[test]
    fn page_meta_parsing_and_mode_strings() {
        let v = serde_json::json!({"backwardLink": "l", "syncState": "s", "totalCount": 7});
        let m = parse_page_meta(Some(&v));
        assert_eq!(
            (
                m.backward_link.as_deref(),
                m.sync_state.as_deref(),
                m.total_count
            ),
            (Some("l"), Some("s"), Some(7))
        );
        assert_eq!(m.raw, Some(v));
        assert_eq!(parse_page_meta(None), PageMeta::default());
        // an empty link is no link; a numeric-string count still counts
        let m = parse_page_meta(Some(
            &serde_json::json!({"backwardLink": "", "totalCount": "9"}),
        ));
        assert_eq!((m.backward_link, m.total_count), (None, Some(9)));
        assert_eq!(
            PagingMode::parse("backward_link"),
            Some(PagingMode::BackwardLink)
        );
        assert_eq!(PagingMode::parse("x"), None);
        for mode in [
            PagingMode::Auto,
            PagingMode::Window,
            PagingMode::BackwardLink,
        ] {
            assert_eq!(PagingMode::parse(mode.as_str()), Some(mode));
        }
        assert_eq!(PagingMode::default(), PagingMode::Auto);
        assert_eq!(PageCursor::end_time(5), PageCursor(CursorInner::EndTime(5)));
        assert_eq!(
            PageCursor::link("u"),
            PageCursor(CursorInner::Link {
                url: "u".into(),
                pages: 1,
                prev_ids: vec![],
                seen: 0,
            })
        );
    }
}
