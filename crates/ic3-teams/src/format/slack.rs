//! Slack-style markup → [`RichText`] (spec §4.9.2).
//!
//! [`parse_slack`] splits a message into blocks line by line and hands each
//! paragraph-like line to [`parse_inline`], which handles the markers, code
//! spans, escapes and bare URLs within it.

use super::{Block, Inline, RichText};

/// Characters that may precede a candidate opener (spec §4.9.2 rule 3).
const OPEN_PUNCT: &[char] = &['(', '[', '{', '"', '\''];
/// Characters that may follow a candidate closer (rule 4).
const CLOSE_PUNCT: &[char] = &['.', ',', ';', ':', '!', '?', ')', ']', '}', '"', '\''];
/// Characters a backslash escapes (rule 2).
const ESCAPABLE: &[char] = &['*', '_', '~', '`', '\\'];
/// Punctuation stripped from the end of a bare URL.
const URL_TRAILING_PUNCT: &[char] = &['.', ',', ';', ':', '!', '?'];
/// Schemes that start a bare URL.
const URL_SCHEMES: &[&str] = &["http://", "https://"];

/// The span a marker character produces.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Style {
    Bold,
    Italic,
    Strike,
}

impl Style {
    fn of(c: char) -> Option<Style> {
        match c {
            '*' => Some(Style::Bold),
            '_' => Some(Style::Italic),
            '~' => Some(Style::Strike),
            _ => None,
        }
    }

    fn wrap(self, children: Vec<Inline>) -> Inline {
        match self {
            Style::Bold => Inline::Bold(children),
            Style::Italic => Inline::Italic(children),
            Style::Strike => Inline::Strike(children),
        }
    }
}

/// A marker character as rule 2 defines them: `*`, `_`, `~` or `` ` ``.
fn is_marker(c: char) -> bool {
    c == '`' || Style::of(c).is_some()
}

/// Parses a whole Slack-style message into a normalised tree (spec §4.9.2
/// "Block level").
///
/// Line by line (`\r\n` accepted): a ```` ``` ```` fence, optionally followed
/// by a language tag, opens a verbatim [`Block::CodeBlock`] closed by the next
/// line that is exactly ```` ``` ```` (an unclosed fence is literal text);
/// consecutive `> ` lines form one [`Block::Quote`]; consecutive `- `, `* ` or
/// `{digits}. ` lines form one [`Block::List`], and a change between the
/// bulleted and numbered families starts a new list; every other line joins
/// the current paragraph with a [`Inline::LineBreak`], and a blank line ends
/// it. Quote, list-item and paragraph lines go through [`parse_inline`].
pub fn parse_slack(s: &str) -> RichText {
    let lines: Vec<&str> = s
        .split('\n')
        .map(|l| l.strip_suffix('\r').unwrap_or(l))
        .collect();
    let mut blocks = Vec::new();
    let mut para: Vec<Inline> = Vec::new();
    let mut i = 0;
    while i < lines.len() {
        let line = lines[i];
        if is_fence_open(line) {
            if let Some(end) = (i + 1..lines.len()).find(|&j| lines[j] == "```") {
                flush_paragraph(&mut para, &mut blocks);
                blocks.push(Block::CodeBlock(lines[i + 1..end].join("\n")));
                i = end + 1;
                continue;
            }
            // An unclosed fence is literal text: fall through to the paragraph.
        } else if quote_line(line).is_some() {
            flush_paragraph(&mut para, &mut blocks);
            let mut inl = Vec::new();
            while let Some(rest) = lines.get(i).copied().and_then(quote_line) {
                if !inl.is_empty() {
                    inl.push(Inline::LineBreak);
                }
                inl.extend(parse_inline(rest));
                i += 1;
            }
            blocks.push(Block::Quote(vec![Block::Paragraph(inl)]));
            continue;
        } else if let Some((ordered, _)) = list_item(line) {
            flush_paragraph(&mut para, &mut blocks);
            let mut items = Vec::new();
            while let Some((o, rest)) = lines.get(i).copied().and_then(list_item) {
                if o != ordered {
                    break;
                }
                items.push(vec![Block::Paragraph(parse_inline(rest))]);
                i += 1;
            }
            blocks.push(Block::List { ordered, items });
            continue;
        }
        if line.trim().is_empty() {
            flush_paragraph(&mut para, &mut blocks);
        } else {
            if !para.is_empty() {
                para.push(Inline::LineBreak);
            }
            para.extend(parse_inline(line));
        }
        i += 1;
    }
    flush_paragraph(&mut para, &mut blocks);
    RichText(blocks).normalize()
}

fn flush_paragraph(para: &mut Vec<Inline>, blocks: &mut Vec<Block>) {
    if !para.is_empty() {
        blocks.push(Block::Paragraph(std::mem::take(para)));
    }
}

/// ```` ``` ```` optionally followed by a language tag (`rust`, `c++`, `objective-c`, …).
fn is_fence_open(line: &str) -> bool {
    line.strip_prefix("```").is_some_and(|tag| {
        tag.chars()
            .all(|c| c.is_alphanumeric() || c == '_' || c == '+' || c == '-')
    })
}

/// The content of a quote line: `> ` stripped, or empty for a bare `>`.
fn quote_line(line: &str) -> Option<&str> {
    if line == ">" {
        Some("")
    } else {
        line.strip_prefix("> ")
    }
}

/// A list item's `(ordered, content)`: `- ` and `* ` are bulleted,
/// `{digits}. ` numbered.
fn list_item(line: &str) -> Option<(bool, &str)> {
    if let Some(rest) = line.strip_prefix("- ").or_else(|| line.strip_prefix("* ")) {
        return Some((false, rest));
    }
    let digits = line.chars().take_while(char::is_ascii_digit).count();
    if digits == 0 {
        return None;
    }
    line[digits..].strip_prefix(". ").map(|rest| (true, rest))
}

/// Output of the first phase: literal text, verbatim code, bare URLs and
/// marker candidates (rules 3–4), with the marker's source text kept so an
/// unmatched one can be emitted literally (rule 6).
#[derive(Debug)]
enum Tok {
    Text(String),
    Code(String),
    Url(String),
    Marker {
        style: Style,
        raw: String,
        open: bool,
        close: bool,
    },
}

/// Parses one line of Slack-style markup into inline nodes (spec §4.9.2
/// "Inline level" rules 1–7 and "Bare URLs").
///
/// Code spans are found first and are verbatim; `\` escapes a marker character
/// or itself; `**` and `~~` alias `*` and `~` (but `__` stays literal, so
/// `__init__` is as typed); bare `http://` and `https://` URLs become links
/// before marker scanning; unmatched markers are literal.
///
/// The result is **not** normalised: adjacent `Text` nodes may appear (an
/// unmatched opener is emitted as its own `Text`, for example — `*x*y` is
/// `[Text("*"), Text("x*y")]`). Callers pass the finished tree through
/// [`RichText::normalize`](super::RichText::normalize).
pub fn parse_inline(line: &str) -> Vec<Inline> {
    // Rule 5: a stack of open spans; the bottom frame is the line itself.
    let mut stack = vec![Frame::root()];
    for (i, tok) in tokenize(line).into_iter().enumerate() {
        match tok {
            Tok::Text(s) => top(&mut stack).children.push(Inline::Text(s)),
            Tok::Code(s) => top(&mut stack).children.push(Inline::Code(s)),
            Tok::Url(url) => top(&mut stack).children.push(Inline::Link {
                text: vec![Inline::Text(url.clone())],
                url,
            }),
            Tok::Marker {
                style,
                raw,
                open,
                close,
            } => {
                let top_frame = stack.last().expect("stack holds the root frame");
                let top_same = top_frame.style == Some(style);
                // Rule 2b: a closer immediately after its opener is not a closer.
                let adjacent = top_same && top_frame.start + 1 == i;
                let already_open = stack.iter().any(|f| f.style == Some(style));
                // Rule 5, in this order: closer if its marker is on top; else
                // opener unless the marker is already open; else literal.
                if close && top_same && !adjacent {
                    let frame = stack.pop().expect("stack holds the root frame");
                    top(&mut stack).children.push(style.wrap(frame.children));
                } else if open && !already_open {
                    stack.push(Frame {
                        style: Some(style),
                        raw,
                        start: i,
                        children: Vec::new(),
                    });
                } else {
                    top(&mut stack).children.push(Inline::Text(raw));
                }
            }
        }
    }
    // Rule 6: at end of line every still-open opener is literal text.
    while stack.len() > 1 {
        let frame = stack.pop().expect("stack holds the root frame");
        let parent = top(&mut stack);
        parent.children.push(Inline::Text(frame.raw));
        parent.children.extend(frame.children);
    }
    stack.pop().expect("stack holds the root frame").children
}

/// An open span: the marker that opened it (`None` for the line itself), its
/// source text, the index of its opening token, and the content so far.
#[derive(Debug)]
struct Frame {
    style: Option<Style>,
    raw: String,
    start: usize,
    children: Vec<Inline>,
}

impl Frame {
    fn root() -> Frame {
        Frame {
            style: None,
            raw: String::new(),
            start: 0,
            children: Vec::new(),
        }
    }
}

fn top(stack: &mut [Frame]) -> &mut Frame {
    stack.last_mut().expect("stack holds the root frame")
}

/// First phase: escapes, code spans, bare URLs and marker classification.
fn tokenize(line: &str) -> Vec<Tok> {
    let chars: Vec<char> = line.chars().collect();
    let mut toks = Vec::new();
    let mut buf = String::new();
    // Index of the character most recently emitted by an escape; it is plain
    // text and so never counts as a neighbouring marker.
    let mut escaped_at = None;
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];

        // Rule 2: escapes, honoured before the code-first scan.
        if c == '\\' && chars.get(i + 1).is_some_and(|n| ESCAPABLE.contains(n)) {
            buf.push(chars[i + 1]);
            escaped_at = Some(i + 1);
            i += 2;
            continue;
        }

        // Rule 1: code spans first; content verbatim. An unmatched backtick
        // is literal, and `` is not special (rule 2a).
        if c == '`' {
            match (i + 1..chars.len()).find(|&j| chars[j] == '`') {
                Some(end) if end == i + 1 => {
                    buf.push_str("``");
                    i += 2;
                }
                Some(end) => {
                    flush(&mut buf, &mut toks);
                    toks.push(Tok::Code(chars[i + 1..end].iter().collect()));
                    i = end + 1;
                }
                None => {
                    buf.push(c);
                    i += 1;
                }
            }
            continue;
        }

        // Bare URLs are recognised before marker scanning.
        if let Some(url) = bare_url_at(&chars, i) {
            let consumed = url.chars().count();
            flush(&mut buf, &mut toks);
            toks.push(Tok::Url(url));
            i += consumed;
            continue;
        }

        let Some(style) = Style::of(c) else {
            buf.push(c);
            i += 1;
            continue;
        };

        // Rule 2a (decision #26): `_` adjacent to another `_` is literal, so
        // `__init__` stays as typed. `__` is not an alias.
        let underscore_before = i > 0 && chars[i - 1] == '_' && escaped_at != Some(i - 1);
        if c == '_' && (underscore_before || chars.get(i + 1) == Some(&'_')) {
            buf.push(c);
            i += 1;
            continue;
        }

        // Rule 2a: `**` and `~~` are single markers.
        let doubled = c != '_' && chars.get(i + 1) == Some(&c);
        let width = if doubled { 2 } else { 1 };
        let prev = i.checked_sub(1).map(|p| chars[p]);
        let next = chars.get(i + width).copied();
        let after_opener =
            buf.is_empty() && matches!(toks.last(), Some(Tok::Marker { open: true, .. }));
        // Rule 3.
        let open = (prev.is_none_or(|p| p.is_whitespace() || OPEN_PUNCT.contains(&p))
            || after_opener)
            && next.is_some_and(|n| !n.is_whitespace());
        // Rule 4.
        let close = prev.is_some_and(|p| !p.is_whitespace())
            && next.is_none_or(|n| n.is_whitespace() || CLOSE_PUNCT.contains(&n) || is_marker(n));
        let raw: String = chars[i..i + width].iter().collect();
        if open || close {
            flush(&mut buf, &mut toks);
            toks.push(Tok::Marker {
                style,
                raw,
                open,
                close,
            });
        } else {
            buf.push_str(&raw);
        }
        i += width;
    }
    flush(&mut buf, &mut toks);
    toks
}

fn flush(buf: &mut String, toks: &mut Vec<Tok>) {
    if !buf.is_empty() {
        toks.push(Tok::Text(std::mem::take(buf)));
    }
}

/// The bare URL starting at `chars[i]`, if there is one: a scheme followed by
/// at least one non-whitespace character, minus trailing punctuation and an
/// unbalanced trailing `)`.
fn bare_url_at(chars: &[char], i: usize) -> Option<String> {
    let scheme = URL_SCHEMES.iter().find(|s| starts_with_at(chars, i, s))?;
    let end = (i..chars.len())
        .find(|&j| chars[j].is_whitespace())
        .unwrap_or(chars.len());
    let url = trim_url(chars[i..end].iter().collect());
    (url.chars().count() > scheme.len()).then_some(url)
}

fn starts_with_at(chars: &[char], i: usize, prefix: &str) -> bool {
    prefix
        .chars()
        .enumerate()
        .all(|(k, p)| chars.get(i + k) == Some(&p))
}

fn trim_url(mut url: String) -> String {
    loop {
        let before = url.len();
        while url.ends_with(URL_TRAILING_PUNCT) {
            url.pop();
        }
        if url.ends_with(')') && url.matches('(').count() < url.matches(')').count() {
            url.pop();
        }
        if url.len() == before {
            return url;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::parse_inline;
    use crate::format::{Block, Inline, Inline::*, RichText};

    fn t(s: &str) -> Inline {
        Text(s.into())
    }
    fn norm(v: Vec<Inline>) -> Vec<Inline> {
        match RichText(vec![Block::Paragraph(v)]).normalize().0.pop() {
            Some(Block::Paragraph(v)) => v,
            _ => vec![],
        }
    }
    fn check(input: &str, expected: Vec<Inline>) {
        assert_eq!(norm(parse_inline(input)), expected, "input: {input:?}");
    }

    #[test]
    fn spec_examples() {
        check("snake_case_name", vec![t("snake_case_name")]);
        check("2*3*4", vec![t("2*3*4")]);
        check("5 * 3", vec![t("5 * 3")]);
        check("really *important", vec![t("really *important")]);
        check("*bold* x", vec![Bold(vec![t("bold")]), t(" x")]);
        check("*a _b* c_", vec![t("*a "), Italic(vec![t("b* c")])]);
        check("*_both_*", vec![Bold(vec![Italic(vec![t("both")])])]);
    }

    #[test]
    fn markers_and_aliases() {
        check("**bold**", vec![Bold(vec![t("bold")])]);
        check("~~gone~~", vec![Strike(vec![t("gone")])]);
        check("__init__", vec![t("__init__")]);
        check("**", vec![t("**")]);
        check("__", vec![t("__")]);
        check("~~", vec![t("~~")]);
        check("* *", vec![t("* *")]);
        check(
            "_it_ and ~no~",
            vec![Italic(vec![t("it")]), t(" and "), Strike(vec![t("no")])],
        );
        check("(*paren*)", vec![t("("), Bold(vec![t("paren")]), t(")")]);
        check("\"*q*\".", vec![t("\""), Bold(vec![t("q")]), t("\".")]);
        check(
            "*a* *b*",
            vec![Bold(vec![t("a")]), t(" "), Bold(vec![t("b")])],
        );
        // rule 5: a second `*` while `*` is open is literal; the closer matches the open frame
        check(
            "*outer *inner* x*",
            vec![Bold(vec![t("outer *inner")]), t(" x*")],
        );
        // rule 4: a backtick is a marker character, so it can follow a closer
        check(
            "*bold*`code`",
            vec![Bold(vec![t("bold")]), Code("code".into())],
        );
    }

    #[test]
    fn code_first_and_escapes() {
        check("`a *b* c`", vec![Code("a *b* c".into())]);
        check("x `y", vec![t("x `y")]);
        check("\\*not bold\\*", vec![t("*not bold*")]);
        check("\\` not code", vec![t("` not code")]);
        check("a \\\\ b", vec![t("a \\ b")]);
        check("\\q", vec![t("\\q")]);
        check("`keep \\* this`", vec![Code("keep \\* this".into())]);
        check("`` x", vec![t("`` x")]);
        // inside a code span `\` is verbatim and the next backtick closes it
        check("`a\\`b`", vec![Code("a\\".into()), t("b`")]);
    }

    #[test]
    fn bare_urls() {
        check(
            "see https://ex.am/ple_a_b now",
            vec![
                t("see "),
                Link {
                    url: "https://ex.am/ple_a_b".into(),
                    text: vec![t("https://ex.am/ple_a_b")],
                },
                t(" now"),
            ],
        );
        check(
            "(https://x.y/z)",
            vec![
                t("("),
                Link {
                    url: "https://x.y/z".into(),
                    text: vec![t("https://x.y/z")],
                },
                t(")"),
            ],
        );
        check(
            "https://x.y/a_(b)",
            vec![Link {
                url: "https://x.y/a_(b)".into(),
                text: vec![t("https://x.y/a_(b)")],
            }],
        );
        check(
            "go http://x.y.",
            vec![
                t("go "),
                Link {
                    url: "http://x.y".into(),
                    text: vec![t("http://x.y")],
                },
                t("."),
            ],
        );
        check("`https://x.y`", vec![Code("https://x.y".into())]);
    }

    #[test]
    fn unmatched_openers_become_literal_at_end_of_line() {
        check("*a _b", vec![t("*a _b")]);
        check("_a *b* c", vec![t("_a "), Bold(vec![t("b")]), t(" c")]);
    }

    fn link(url: &str) -> Inline {
        Link {
            url: url.into(),
            text: vec![t(url)],
        }
    }

    // Spec behaviours the table above does not pin.
    #[test]
    fn adjacent_underscores_are_literal_but_escaped_ones_are_plain_text() {
        // rule 2a / decision #26
        check("__name__", vec![t("__name__")]);
        check("_a__b_", vec![Italic(vec![t("a__b")])]);
        // an escaped `_` is text, so it neither opens nor neutralises its neighbour
        check("_a\\__", vec![Italic(vec![t("a_")])]);
    }

    #[test]
    fn no_empty_spans_through_the_stack() {
        // rule 2b with the `**` alias: `**` opens, the lone `*` is adjacent to it
        check("***", vec![t("***")]);
    }

    #[test]
    fn bare_url_edge_cases() {
        // a scheme must be followed by non-whitespace
        check("https://", vec![t("https://")]);
        check("https:// x", vec![t("https:// x")]);
        // trailing punctuation and an unbalanced `)` are both stripped, repeatedly
        check("https://x.y/z).", vec![link("https://x.y/z"), t(").")]);
        // `_` inside a URL is not a marker (URLs are recognised first)
        check(
            "_see https://a.b/_c_ now_",
            vec![Italic(vec![t("see "), link("https://a.b/_c_"), t(" now")])],
        );
    }

    #[test]
    fn non_ascii_text_is_indexed_by_character() {
        check("*héllo* 世界", vec![Bold(vec![t("héllo")]), t(" 世界")]);
        check("https://x.y/ü ok", vec![link("https://x.y/ü"), t(" ok")]);
    }

    #[test]
    fn blocks() {
        use crate::format::{Block::*, Inline::*};
        let rt = crate::format::parse_slack(
            "para one\nline two\n\npara two\n```rust\nlet *x* = 1;\n```\n> quoted *q*\n> more\n- a\n- b\n1. one\n2. two\nend",
        );
        assert_eq!(
            rt.0,
            vec![
                Paragraph(vec![
                    Text("para one".into()),
                    LineBreak,
                    Text("line two".into())
                ]),
                Paragraph(vec![Text("para two".into())]),
                CodeBlock("let *x* = 1;".into()),
                Quote(vec![Paragraph(vec![
                    Text("quoted ".into()),
                    Bold(vec![Text("q".into())]),
                    LineBreak,
                    Text("more".into())
                ])]),
                List {
                    ordered: false,
                    items: vec![
                        vec![Paragraph(vec![Text("a".into())])],
                        vec![Paragraph(vec![Text("b".into())])]
                    ]
                },
                List {
                    ordered: true,
                    items: vec![
                        vec![Paragraph(vec![Text("one".into())])],
                        vec![Paragraph(vec![Text("two".into())])]
                    ]
                },
                Paragraph(vec![Text("end".into())]),
            ]
        );
    }

    #[test]
    fn unclosed_fence_is_literal_and_star_space_is_a_list() {
        use crate::format::{Block::*, Inline::*};
        assert_eq!(
            crate::format::parse_slack("```\nnot closed").0,
            vec![Paragraph(vec![
                Text("```".into()),
                LineBreak,
                Text("not closed".into())
            ])]
        );
        assert_eq!(
            crate::format::parse_slack("* item").0,
            vec![List {
                ordered: false,
                items: vec![vec![Paragraph(vec![Text("item".into())])]]
            }]
        );
        assert_eq!(
            crate::format::parse_slack("*bold* x").0,
            vec![Paragraph(vec![
                Bold(vec![Text("bold".into())]),
                Text(" x".into())
            ])]
        );
        assert_eq!(
            crate::format::parse_slack("- a\n1. b").0.len(),
            2,
            "marker family change starts a new list"
        );
        assert_eq!(
            crate::format::parse_slack("\r\nx\r\n").0,
            vec![Paragraph(vec![Text("x".into())])]
        );
    }

    // Spec behaviours the brief's table does not pin.
    #[test]
    fn fence_and_quote_edge_cases() {
        use crate::format::{Block::*, Inline::*, parse_slack};
        // a bare `>` is an empty quote line; the quote stays one paragraph
        assert_eq!(
            parse_slack("> a\n>\n> b").0,
            vec![Quote(vec![Paragraph(vec![
                Text("a".into()),
                LineBreak,
                LineBreak,
                Text("b".into())
            ])])]
        );
        // `\r` is stripped inside a fence too; a language tag may hold `+`/`-`
        assert_eq!(
            parse_slack("```c++\r\nx;\r\n\r\ny;\r\n```\r\n").0,
            vec![CodeBlock("x;\n\ny;".into())]
        );
        // an empty fence produces nothing; a fence with trailing space is literal
        assert!(parse_slack("```\n```").is_empty());
        assert_eq!(
            parse_slack("``` \nx\n```").0,
            vec![Paragraph(vec![
                Text("``` ".into()),
                LineBreak,
                Text("x".into()),
                LineBreak,
                Text("```".into())
            ])]
        );
    }
}
