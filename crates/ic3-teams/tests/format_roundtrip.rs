//! Round-trip property tests for `ic3_teams::format` (spec §4.9.4 invariant, §10).
//!
//! The generator deliberately produces only what `parse_slack` can produce:
//! no empty text, no `Reply`/`Mention`/`Emoji`/`Image`. A nested same-marker
//! span (e.g. `Bold[Bold]`) may still be generated; normalisation keeps it and
//! so does the HTML round trip.

use ic3_teams::format::{Block, Inline, RichText, from_teams_html, parse_slack, to_teams_html};
use proptest::prelude::*;

fn text() -> impl Strategy<Value = String> {
    "[a-zA-Z0-9 .,!?<>&\"']{1,12}".prop_filter("no edge spaces", |s| {
        !s.starts_with(' ') && !s.ends_with(' ')
    })
}

fn url() -> impl Strategy<Value = String> {
    "https://[a-z]{2,6}\\.[a-z]{2,3}/[a-z0-9_]{0,8}"
}

fn leaf() -> impl Strategy<Value = Inline> {
    prop_oneof![
        text().prop_map(Inline::Text),
        "[a-z0-9 ]{1,8}".prop_map(Inline::Code),
        url().prop_map(|u| Inline::Link {
            url: u.clone(),
            text: vec![Inline::Text(u)]
        }),
    ]
}

fn inline() -> impl Strategy<Value = Inline> {
    leaf().prop_recursive(2, 6, 3, |inner| {
        prop_oneof![
            prop::collection::vec(inner.clone(), 1..3).prop_map(Inline::Bold),
            prop::collection::vec(inner.clone(), 1..3).prop_map(Inline::Italic),
            prop::collection::vec(inner, 1..3).prop_map(Inline::Strike),
        ]
    })
}

fn paragraph() -> impl Strategy<Value = Block> {
    prop::collection::vec(prop_oneof![inline(), Just(Inline::LineBreak)], 1..5)
        .prop_map(Block::Paragraph)
}

fn block() -> impl Strategy<Value = Block> {
    prop_oneof![
        paragraph(),
        "[a-z0-9 \n*_]{1,20}".prop_map(Block::CodeBlock),
        paragraph().prop_map(|p| Block::Quote(vec![p])),
        prop::collection::vec(paragraph().prop_map(|p| vec![p]), 1..3).prop_map(|items| {
            Block::List {
                ordered: false,
                items,
            }
        }),
        prop::collection::vec(paragraph().prop_map(|p| vec![p]), 1..3).prop_map(|items| {
            Block::List {
                ordered: true,
                items,
            }
        }),
    ]
}

proptest! {
    #[test]
    fn html_round_trip(blocks in prop::collection::vec(block(), 1..4)) {
        let t = RichText(blocks).normalize();
        prop_assert_eq!(from_teams_html(&to_teams_html(&t)).normalize(), t);
    }

    #[test]
    fn slack_round_trip(s in "[a-z *_~`\n>-]{0,40}") {
        let t = parse_slack(&s);
        prop_assert_eq!(from_teams_html(&to_teams_html(&t)).normalize(), t);
    }
}
