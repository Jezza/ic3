//! Rich-text tree shared by every formatter in the crate.
//!
//! Slack-style input, Teams HTML and the TUI all speak [`RichText`]. Every
//! formatter produces a normalised tree (see [`RichText::normalize`]), and the
//! round-trip invariants in the design spec are stated over normalised trees.

use crate::types::{MessageId, Mri};

pub mod html_in;
pub mod html_out;
pub mod slack;

pub use html_in::{body_from_wire, from_teams_html};
pub use html_out::to_teams_html;
pub use slack::{parse_inline, parse_slack};

/// A message body: a sequence of blocks.
#[derive(Clone, Debug, PartialEq, Eq, Default)]
pub struct RichText(pub Vec<Block>);

/// A block-level node.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Block {
    /// A run of inline content.
    Paragraph(Vec<Inline>),
    /// Preformatted code; the string is verbatim.
    CodeBlock(String),
    /// A quoted sequence of blocks.
    Quote(Vec<Block>),
    /// A bulleted or numbered list; each item is itself a block sequence.
    List {
        /// `true` for a numbered list.
        ordered: bool,
        /// The items, in order.
        items: Vec<Vec<Block>>,
    },
    /// The quoted message a reply refers to.
    Reply(ReplyQuote),
}

/// An inline-level node.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Inline {
    /// Literal text.
    Text(String),
    /// Bold span.
    Bold(Vec<Inline>),
    /// Italic span.
    Italic(Vec<Inline>),
    /// Strikethrough span.
    Strike(Vec<Inline>),
    /// Inline code; the string is verbatim.
    Code(String),
    /// A hyperlink with optional display text (empty text shows the URL).
    Link {
        /// Target URL.
        url: String,
        /// Display text.
        text: Vec<Inline>,
    },
    /// An @-mention of a user.
    ///
    /// In Teams HTML a mention is `<span itemtype="…/Mention" itemid="N">`,
    /// where `N` indexes the message's `properties.mentions` list (§9.23).
    /// [`from_teams_html`] alone keeps the `itemid` as received; a received
    /// message (`Message::body`) has it resolved to the real MRI through
    /// `properties.mentions`, keeping the raw `itemid` only when the list has
    /// no entry for it. Sending (`Api::send_message`, `Api::edit`) numbers
    /// the mentions and writes `properties.mentions` itself, so a body built
    /// for sending carries real MRIs.
    Mention {
        /// Who is mentioned: an MRI, or the raw `itemid` when it could not be
        /// resolved (see above).
        mri: Mri,
        /// The display name shown in the message.
        name: String,
    },
    /// An emoji, carried as its text alternative.
    Emoji(String),
    /// An inline image.
    Image {
        /// Source URL.
        url: String,
    },
    /// A hard line break inside a paragraph.
    LineBreak,
}

/// The message a reply quotes, as parsed from a received message.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReplyQuote {
    /// Id of the quoted message.
    pub message_id: MessageId,
    /// Author of the quoted message, when known.
    pub author: Option<Mri>,
    /// Display name of the author, when known.
    pub author_name: Option<String>,
    /// The quoted excerpt.
    pub excerpt: RichText,
}

impl RichText {
    /// Canonicalises the tree: merges adjacent [`Inline::Text`], removes empty
    /// text/code and empty containers (recursively), and strips leading and
    /// trailing [`Inline::LineBreak`]s from each paragraph.
    pub fn normalize(self) -> RichText {
        RichText(normalize_blocks(self.0))
    }

    /// `true` when there are no blocks at all.
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// A single paragraph of literal text, normalised.
    pub fn text(s: &str) -> RichText {
        RichText(vec![Block::Paragraph(vec![Inline::Text(s.to_string())])]).normalize()
    }

    /// Builds a tree from plain text: one paragraph per blank-line-separated
    /// chunk, with `\n` inside a chunk becoming a [`Inline::LineBreak`]. A
    /// line is blank when it is empty or whitespace-only; content lines are
    /// kept verbatim.
    pub fn from_plain_text(s: &str) -> RichText {
        let mut blocks = Vec::new();
        let mut para: Vec<Inline> = Vec::new();
        for line in s.lines() {
            if line.trim().is_empty() {
                if !para.is_empty() {
                    blocks.push(Block::Paragraph(std::mem::take(&mut para)));
                }
                continue;
            }
            if !para.is_empty() {
                para.push(Inline::LineBreak);
            }
            para.push(Inline::Text(line.to_string()));
        }
        if !para.is_empty() {
            blocks.push(Block::Paragraph(para));
        }
        RichText(blocks).normalize()
    }

    /// Flattens the tree to plain text, one line per block (lists and quotes
    /// may span several). Formatting is dropped; mentions become `@name`,
    /// images `[image]`, links without text their URL.
    pub fn plain_text(&self) -> String {
        blocks_text(&self.0)
    }
}

fn normalize_blocks(blocks: Vec<Block>) -> Vec<Block> {
    blocks
        .into_iter()
        .filter_map(|b| match b {
            Block::Paragraph(inl) => {
                let inl = trim_breaks(normalize_inlines(inl));
                (!inl.is_empty()).then_some(Block::Paragraph(inl))
            }
            Block::CodeBlock(s) => (!s.is_empty()).then_some(Block::CodeBlock(s)),
            Block::Quote(bs) => {
                let bs = normalize_blocks(bs);
                (!bs.is_empty()).then_some(Block::Quote(bs))
            }
            Block::List { ordered, items } => {
                let items: Vec<Vec<Block>> = items
                    .into_iter()
                    .map(normalize_blocks)
                    .filter(|i| !i.is_empty())
                    .collect();
                (!items.is_empty()).then_some(Block::List { ordered, items })
            }
            Block::Reply(q) => Some(Block::Reply(ReplyQuote {
                excerpt: q.excerpt.normalize(),
                ..q
            })),
        })
        .collect()
}

fn normalize_inlines(inl: Vec<Inline>) -> Vec<Inline> {
    let mut out: Vec<Inline> = Vec::new();
    for i in inl {
        let i = match i {
            Inline::Text(s) if s.is_empty() => continue,
            Inline::Code(s) if s.is_empty() => continue,
            Inline::Bold(c) => {
                let c = normalize_inlines(c);
                if c.is_empty() {
                    continue;
                }
                Inline::Bold(c)
            }
            Inline::Italic(c) => {
                let c = normalize_inlines(c);
                if c.is_empty() {
                    continue;
                }
                Inline::Italic(c)
            }
            Inline::Strike(c) => {
                let c = normalize_inlines(c);
                if c.is_empty() {
                    continue;
                }
                Inline::Strike(c)
            }
            Inline::Link { url, text } => Inline::Link {
                url,
                text: normalize_inlines(text),
            },
            other => other,
        };
        if let (Some(Inline::Text(a)), Inline::Text(b)) = (out.last_mut(), &i) {
            a.push_str(b);
            continue;
        }
        out.push(i);
    }
    out
}

fn trim_breaks(mut v: Vec<Inline>) -> Vec<Inline> {
    while matches!(v.last(), Some(Inline::LineBreak)) {
        v.pop();
    }
    let lead = v
        .iter()
        .position(|i| !matches!(i, Inline::LineBreak))
        .unwrap_or(v.len());
    v.drain(..lead);
    v
}

fn blocks_text(blocks: &[Block]) -> String {
    blocks.iter().map(block_text).collect::<Vec<_>>().join("\n")
}

fn block_text(block: &Block) -> String {
    match block {
        Block::Paragraph(inl) => {
            let mut s = String::new();
            inlines_text(inl, &mut s);
            s
        }
        Block::CodeBlock(s) => s.clone(),
        Block::Quote(bs) => blocks_text(bs)
            .lines()
            .map(|l| format!("> {l}"))
            .collect::<Vec<_>>()
            .join("\n"),
        Block::List { ordered, items } => items
            .iter()
            .enumerate()
            .map(|(n, item)| {
                let marker = if *ordered {
                    format!("{}. ", n + 1)
                } else {
                    "• ".to_string()
                };
                format!("{marker}{}", blocks_text(item))
            })
            .collect::<Vec<_>>()
            .join("\n"),
        Block::Reply(q) => format!(
            "{}: {}",
            q.author_name.as_deref().unwrap_or("?"),
            q.excerpt.plain_text()
        ),
    }
}

/// Joins the pieces of one mention: Teams writes a mention of several words
/// as one span per word (`Example`, `Team`), each its own `itemid` but for
/// the same MRI, with only spaces between. Run once the MRIs are resolved;
/// adjacent mentions of different people stay apart.
pub(crate) fn merge_split_mentions(rt: &mut RichText) {
    fn blocks(bs: &mut [Block]) {
        for b in bs {
            match b {
                Block::Paragraph(inl) => inlines(inl),
                Block::Quote(bs) => blocks(bs),
                Block::List { items, .. } => {
                    for item in items {
                        blocks(item);
                    }
                }
                Block::CodeBlock(_) | Block::Reply(_) => {}
            }
        }
    }
    fn inlines(inl: &mut Vec<Inline>) {
        let mut out: Vec<Inline> = Vec::with_capacity(inl.len());
        for mut i in inl.drain(..) {
            match &mut i {
                Inline::Bold(c) | Inline::Italic(c) | Inline::Strike(c) => inlines(c),
                Inline::Link { text, .. } => inlines(text),
                Inline::Mention { mri, name } => {
                    let gap = match out.last() {
                        Some(Inline::Text(t)) if t.chars().all(char::is_whitespace) => 1,
                        _ => 0,
                    };
                    if let Some(Inline::Mention {
                        mri: prev,
                        name: prev_name,
                    }) = out.len().checked_sub(gap + 1).map(|k| &mut out[k])
                        && prev == mri
                    {
                        prev_name.push(' ');
                        prev_name.push_str(name.trim());
                        out.truncate(out.len() - gap);
                        continue;
                    }
                }
                _ => {}
            }
            out.push(i);
        }
        *inl = out;
    }
    blocks(&mut rt.0);
}

/// Calls `f` with each [`Inline::Mention`]'s `mri` and `name` in document
/// order, skipping [`Block::Reply`] quotes (their excerpt is sent and shown as
/// plain text, and their mentions are another message's).
pub(crate) fn visit_mentions(rt: &mut RichText, f: &mut dyn FnMut(&mut Mri, &str)) {
    fn blocks(bs: &mut [Block], f: &mut dyn FnMut(&mut Mri, &str)) {
        for b in bs {
            match b {
                Block::Paragraph(inl) => inlines(inl, f),
                Block::Quote(bs) => blocks(bs, f),
                Block::List { items, .. } => {
                    for item in items {
                        blocks(item, f);
                    }
                }
                Block::CodeBlock(_) | Block::Reply(_) => {}
            }
        }
    }
    fn inlines(inl: &mut [Inline], f: &mut dyn FnMut(&mut Mri, &str)) {
        for i in inl {
            match i {
                Inline::Bold(c) | Inline::Italic(c) | Inline::Strike(c) => inlines(c, f),
                Inline::Link { text, .. } => inlines(text, f),
                Inline::Mention { mri, name } => f(mri, name),
                Inline::Text(_)
                | Inline::Code(_)
                | Inline::Emoji(_)
                | Inline::Image { .. }
                | Inline::LineBreak => {}
            }
        }
    }
    blocks(&mut rt.0, f);
}

/// Calls `f` with the URL of every [`Inline::Image`] in `rt`, in document
/// order (reply quotes skipped).
pub(crate) fn visit_images(rt: &RichText, f: &mut dyn FnMut(&str)) {
    fn blocks(bs: &[Block], f: &mut dyn FnMut(&str)) {
        for b in bs {
            match b {
                Block::Paragraph(inl) => inlines(inl, f),
                Block::Quote(bs) => blocks(bs, f),
                Block::List { items, .. } => items.iter().for_each(|item| blocks(item, f)),
                Block::CodeBlock(_) | Block::Reply(_) => {}
            }
        }
    }
    fn inlines(inl: &[Inline], f: &mut dyn FnMut(&str)) {
        for i in inl {
            match i {
                Inline::Bold(c) | Inline::Italic(c) | Inline::Strike(c) => inlines(c, f),
                Inline::Link { text, .. } => inlines(text, f),
                Inline::Image { url } => f(url),
                Inline::Text(_)
                | Inline::Code(_)
                | Inline::Emoji(_)
                | Inline::Mention { .. }
                | Inline::LineBreak => {}
            }
        }
    }
    blocks(&rt.0, f);
}

fn inlines_text(inl: &[Inline], out: &mut String) {
    for i in inl {
        match i {
            Inline::Text(s) | Inline::Code(s) | Inline::Emoji(s) => out.push_str(s),
            Inline::Bold(c) | Inline::Italic(c) | Inline::Strike(c) => inlines_text(c, out),
            Inline::Link { url, text } => {
                if text.is_empty() {
                    out.push_str(url);
                } else {
                    inlines_text(text, out);
                }
            }
            Inline::Mention { name, .. } => {
                out.push('@');
                out.push_str(name);
            }
            Inline::Image { .. } => out.push_str("[image]"),
            Inline::LineBreak => out.push('\n'),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn t(s: &str) -> Inline {
        Inline::Text(s.into())
    }

    #[test]
    fn normalize_merges_adjacent_text_and_drops_empties() {
        let rt = RichText(vec![Block::Paragraph(vec![
            t("a"),
            t(""),
            t("b"),
            Inline::Bold(vec![]),
            Inline::Code(String::new()),
        ])]);
        assert_eq!(
            rt.normalize(),
            RichText(vec![Block::Paragraph(vec![t("ab")])])
        );
    }

    #[test]
    fn normalize_strips_leading_and_trailing_breaks_only() {
        let rt = RichText(vec![Block::Paragraph(vec![
            Inline::LineBreak,
            t("a"),
            Inline::LineBreak,
            t("b"),
            Inline::LineBreak,
        ])]);
        assert_eq!(
            rt.normalize(),
            RichText(vec![Block::Paragraph(vec![
                t("a"),
                Inline::LineBreak,
                t("b")
            ])])
        );
    }

    #[test]
    fn normalize_removes_empty_containers_recursively() {
        let rt = RichText(vec![
            Block::Paragraph(vec![]),
            Block::Quote(vec![Block::Paragraph(vec![t("")])]),
            Block::List {
                ordered: false,
                items: vec![vec![], vec![Block::Paragraph(vec![t("x")])]],
            },
            Block::CodeBlock(String::new()),
        ]);
        assert_eq!(
            rt.normalize(),
            RichText(vec![Block::List {
                ordered: false,
                items: vec![vec![Block::Paragraph(vec![t("x")])]]
            }])
        );
    }

    #[test]
    fn normalize_merges_text_inside_nested_spans() {
        let rt = RichText(vec![Block::Paragraph(vec![Inline::Bold(vec![
            t("a"),
            t("b"),
        ])])]);
        assert_eq!(
            rt.normalize(),
            RichText(vec![Block::Paragraph(vec![Inline::Bold(vec![t("ab")])])])
        );
    }

    #[test]
    fn from_plain_text_splits_paragraphs_and_breaks() {
        let rt = RichText::from_plain_text("a\nb\n\n\nc");
        assert_eq!(
            rt,
            RichText(vec![
                Block::Paragraph(vec![t("a"), Inline::LineBreak, t("b")]),
                Block::Paragraph(vec![t("c")]),
            ])
        );
        assert!(RichText::from_plain_text("  \n ").is_empty());
    }

    #[test]
    fn from_plain_text_treats_whitespace_only_lines_as_blank_and_keeps_content_verbatim() {
        assert_eq!(
            RichText::from_plain_text("a \r\n  \r\n b"),
            RichText(vec![
                Block::Paragraph(vec![t("a ")]),
                Block::Paragraph(vec![t(" b")]),
            ])
        );
        assert_eq!(
            RichText::from_plain_text(" hello "),
            RichText::text(" hello ")
        );
    }

    #[test]
    fn plain_text_round_trips_simple_content() {
        let rt = RichText(vec![
            Block::Paragraph(vec![
                t("hi "),
                Inline::Bold(vec![t("there")]),
                Inline::LineBreak,
                Inline::Mention {
                    mri: crate::Mri("8:orgid:x".into()),
                    name: "Ada".into(),
                },
            ]),
            Block::CodeBlock("let x;".into()),
            Block::List {
                ordered: true,
                items: vec![vec![Block::Paragraph(vec![t("one")])]],
            },
        ]);
        assert_eq!(rt.plain_text(), "hi there\n@Ada\nlet x;\n1. one");
    }

    #[test]
    fn split_mentions_are_joined() {
        let m = |who: &str, n: &str| Inline::Mention {
            mri: crate::Mri(who.into()),
            name: n.into(),
        };
        let team = "19:team@thread.tacv2";
        let mut rt = RichText(vec![Block::Paragraph(vec![
            m(team, "Example"),
            t("\u{a0}"),
            m(team, "Team"),
            t(" "),
            m("8:orgid:a", "Ada"),
            m("8:orgid:a", "Lovelace"),
            t(" and "),
            m("8:orgid:b", "Bob"),
            t(" "),
            m("8:orgid:c", "Carol"),
            Inline::Bold(vec![m(team, "Example"), t(" "), m(team, "Team")]),
        ])]);
        merge_split_mentions(&mut rt);
        assert_eq!(
            rt.0[0],
            Block::Paragraph(vec![
                m(team, "Example Team"),
                t(" "),
                m("8:orgid:a", "Ada Lovelace"),
                t(" and "),
                m("8:orgid:b", "Bob"),
                t(" "),
                m("8:orgid:c", "Carol"),
                Inline::Bold(vec![m(team, "Example Team")]),
            ])
        );
    }

    #[test]
    fn visit_mentions_goes_in_document_order_and_skips_reply_quotes() {
        let m = |n: &str| Inline::Mention {
            mri: crate::Mri(format!("8:orgid:{n}")),
            name: n.into(),
        };
        let mut rt = RichText(vec![
            Block::Reply(ReplyQuote {
                message_id: MessageId("1".into()),
                author: None,
                author_name: None,
                excerpt: RichText(vec![Block::Paragraph(vec![m("quoted")])]),
            }),
            Block::Paragraph(vec![m("a"), Inline::Bold(vec![m("b")])]),
            Block::Quote(vec![Block::Paragraph(vec![Inline::Link {
                url: "https://x".into(),
                text: vec![m("c")],
            }])]),
            Block::List {
                ordered: false,
                items: vec![vec![Block::Paragraph(vec![Inline::Italic(vec![m("d")])])]],
            },
        ]);
        let mut seen = Vec::new();
        visit_mentions(&mut rt, &mut |mri, name| {
            seen.push(name.to_owned());
            *mri = crate::Mri(seen.len().to_string());
        });
        assert_eq!(seen, ["a", "b", "c", "d"]);
        let Block::Paragraph(p) = &rt.0[1] else {
            panic!()
        };
        assert_eq!(
            p[0],
            Inline::Mention {
                mri: crate::Mri("1".into()),
                name: "a".into()
            }
        );
    }
}
