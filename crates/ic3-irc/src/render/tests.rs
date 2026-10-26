use ic3_teams::{Block, Block::*, Inline, Inline::*, Mri, ReplyQuote, RichText};
use pretty_assertions::assert_eq;

use super::{RenderCtx, from_irc, to_irc};

/// Nicks are the first word of the display name, lowercased; media links
/// keep the last path segment; `ada`, `bob` and `x[m]` are known.
struct Fake;

impl RenderCtx for Fake {
    fn mention_nick(&self, _mri: &Mri, name: &str) -> String {
        name.split_whitespace()
            .next()
            .unwrap_or_default()
            .to_lowercase()
    }

    fn media_url(&self, url: &str) -> String {
        format!(
            "https://gw.example/m/{}",
            url.rsplit('/').next().unwrap_or_default()
        )
    }

    fn resolve_nick(&self, nick: &str) -> Option<(Mri, String)> {
        let (mri, name) = match nick.to_ascii_lowercase().as_str() {
            "ada" => ("8:orgid:ada", "Ada Lovelace"),
            "bob" => ("8:orgid:bob", "Bob"),
            "x[m]" => ("8:orgid:xm", "Xm"),
            _ => return None,
        };
        Some((Mri(mri.into()), name.into()))
    }
}

fn t(s: &str) -> Inline {
    Text(s.into())
}

fn p(inl: Vec<Inline>) -> Block {
    Paragraph(inl)
}

fn link(url: &str) -> Inline {
    Link {
        url: url.into(),
        text: vec![],
    }
}

fn mention(who: &str) -> Inline {
    let (mri, name) = Fake.resolve_nick(who).expect("a known nick");
    Mention { mri, name }
}

fn item(text: &str) -> Vec<Block> {
    vec![p(vec![t(text)])]
}

fn out(blocks: Vec<Block>) -> String {
    to_irc(&RichText(blocks), &Fake)
}

fn para(inl: Vec<Inline>) -> String {
    out(vec![p(inl)])
}

fn parse(s: &str) -> Vec<Block> {
    from_irc(s, &Fake).0
}

/// Parses one line expected to give a single paragraph.
fn inl(s: &str) -> Vec<Inline> {
    match parse(s).as_slice() {
        [Paragraph(inl)] => inl.clone(),
        other => panic!("{s:?} gave {other:?}"),
    }
}

fn check(input: &str, expected: Vec<Inline>) {
    assert_eq!(inl(input), expected, "input: {input:?}");
}

// --- to_irc ---------------------------------------------------------------

#[test]
fn emphasis_becomes_irc_codes() {
    assert_eq!(para(vec![t("plain")]), "plain");
    assert_eq!(
        para(vec![
            Bold(vec![t("b")]),
            t(" "),
            Italic(vec![t("i")]),
            t(" "),
            Strike(vec![t("s")])
        ]),
        "\x02b\x02 \x1di\x1d \x1es\x1e"
    );
}

#[test]
fn nested_emphasis_closes_and_reopens() {
    assert_eq!(
        para(vec![Bold(vec![t("a "), Italic(vec![t("b")]), t(" c")])]),
        "\x02a \x1db\x1d c\x02"
    );
    // a span repeated inside itself emits nothing more
    assert_eq!(
        para(vec![Bold(vec![t("a"), Bold(vec![t("b")]), t("c")])]),
        "\x02abc\x02"
    );
    // codes are closed at the end of every line and reopened on the next
    assert_eq!(
        para(vec![Bold(vec![
            t("a"),
            LineBreak,
            Italic(vec![t("b")]),
            t("\nc")
        ])]),
        "\x02a\x02\n\x02\x1db\x1d\x02\n\x02c\x02"
    );
}

#[test]
fn formatting_never_dangles_at_line_ends() {
    // trailing whitespace goes, codes after the last visible character close
    assert_eq!(para(vec![Bold(vec![t("x ")]), t("y")]), "\x02x \x02y");
    assert_eq!(para(vec![t("a"), Bold(vec![t("  ")])]), "a");
    assert_eq!(para(vec![Italic(vec![t("x  ")])]), "\x1dx\x1d");
    // a line with nothing visible is dropped
    assert_eq!(
        para(vec![
            t("a"),
            LineBreak,
            Bold(vec![t(" ")]),
            LineBreak,
            t("b")
        ]),
        "a\nb"
    );
}

#[test]
fn code_spans_use_backtick_fences() {
    assert_eq!(para(vec![Code("x *y*".into())]), "`x *y*`");
    assert_eq!(para(vec![Code("a`b".into())]), "``a`b``");
    assert_eq!(para(vec![Code("a``b".into())]), "````a``b````");
    assert_eq!(para(vec![Code("`x".into())]), "`` `x ``");
    assert_eq!(para(vec![Code(" x ".into())]), "`  x  `");
    assert_eq!(para(vec![Code("a\nb".into())]), "`a b`");
    assert_eq!(
        para(vec![Bold(vec![t("see "), Code("x".into())])]),
        "\x02see `x`\x02"
    );
}

#[test]
fn code_blocks_are_fenced_and_verbatim() {
    assert_eq!(
        out(vec![
            p(vec![t("look:")]),
            CodeBlock("fn f() {\n    *x* \n\n}".into()),
            p(vec![t("ok")]),
        ]),
        "look:\n```\nfn f() {\n    *x*\n}\n```\nok"
    );
    assert_eq!(out(vec![CodeBlock("\n\n".into())]), "");
}

#[test]
fn quotes_prefix_every_line() {
    assert_eq!(
        out(vec![
            Quote(vec![
                p(vec![t("a"), LineBreak, t("b")]),
                Quote(vec![p(vec![t("c")])]),
            ]),
            p(vec![t("d")]),
        ]),
        "> a\n> b\n> > c\nd"
    );
}

#[test]
fn lists_number_and_indent() {
    assert_eq!(
        out(vec![List {
            ordered: false,
            items: vec![item("a"), item("b")]
        }]),
        "- a\n- b"
    );
    assert_eq!(
        out(vec![List {
            ordered: true,
            items: vec![
                item("one"),
                vec![p(vec![t("two"), LineBreak, t("more")])],
                vec![
                    p(vec![t("three")]),
                    List {
                        ordered: false,
                        items: vec![
                            item("x"),
                            vec![List {
                                ordered: false,
                                items: vec![item("deep")]
                            }]
                        ]
                    },
                ],
            ]
        }]),
        "1. one\n2. two\n  more\n3. three\n  - x\n  - - deep"
    );
    // empty items are skipped without leaving a gap in the numbering
    assert_eq!(
        out(vec![List {
            ordered: true,
            items: vec![item("a"), vec![p(vec![t(" ")])], item("b")]
        }]),
        "1. a\n2. b"
    );
}

#[test]
fn blocks_are_separated_by_single_newlines() {
    assert_eq!(
        out(vec![
            p(vec![t("one")]),
            p(vec![LineBreak, t("two"), LineBreak, LineBreak, t("three")]),
            p(vec![]),
        ]),
        "one\ntwo\nthree"
    );
    assert_eq!(out(vec![]), "");
}

#[test]
fn links_are_bare_urls() {
    let url = "https://ex.am/ple";
    assert_eq!(para(vec![link(url)]), url);
    assert_eq!(
        para(vec![Link {
            url: url.into(),
            text: vec![t(url)]
        }]),
        url
    );
    assert_eq!(
        para(vec![Link {
            url: url.into(),
            text: vec![Bold(vec![t("the docs")])]
        }]),
        "\x02the docs\x02 (https://ex.am/ple)"
    );
    // a URL inside emphasis is written outside it
    assert_eq!(
        para(vec![Bold(vec![t("see "), link(url), t(" now")])]),
        "\x02see \x02https://ex.am/ple\x02 now\x02"
    );
    // kept apart from words, so clients link exactly the URL
    assert_eq!(
        para(vec![t("see"), link(url), t("now")]),
        "see https://ex.am/ple now"
    );
    assert_eq!(para(vec![link(url), t(".")]), "https://ex.am/ple.");
    assert_eq!(
        para(vec![link(url), link(url)]),
        "https://ex.am/ple https://ex.am/ple"
    );
    assert_eq!(para(vec![link("https://x.y/a b")]), "https://x.y/a%20b");
}

#[test]
fn images_emoji_and_mentions() {
    assert_eq!(
        para(vec![
            t("pic:"),
            Image {
                url: "https://teams/objects/abc".into()
            }
        ]),
        "pic:https://gw.example/m/abc"
    );
    assert_eq!(
        para(vec![t("hi "), Emoji("(smile)".into()), Emoji("😀".into())]),
        "hi (smile)😀"
    );
    assert_eq!(para(vec![mention("ada"), t(": look")]), "ada: look");
    assert_eq!(para(vec![Bold(vec![mention("bob")])]), "\x02bob\x02");
}

#[test]
fn replies_render_nothing() {
    let reply = Reply(ReplyQuote {
        message_id: ic3_teams::MessageId("1".into()),
        author: None,
        author_name: Some("Ada".into()),
        excerpt: RichText::text("earlier"),
    });
    assert_eq!(out(vec![reply.clone(), p(vec![t("answer")])]), "answer");
    assert_eq!(out(vec![reply]), "");
}

#[test]
fn control_characters_are_removed() {
    assert_eq!(para(vec![t("a\r\0\x02\x03b\tc")]), "ab\tc");
    assert_eq!(para(vec![t("a\r\nb")]), "a\nb");
    assert_eq!(out(vec![CodeBlock("x\r\ny\0".into())]), "```\nx\ny\n```");
}

// --- from_irc -------------------------------------------------------------

#[test]
fn empty_input_is_empty() {
    assert!(from_irc("", &Fake).is_empty());
    assert!(from_irc("  \n\t\n ", &Fake).is_empty());
    assert!(from_irc("\x02\x02 \x0f", &Fake).is_empty());
}

#[test]
fn irc_codes() {
    check("\x02b\x02 x", vec![Bold(vec![t("b")]), t(" x")]);
    check("\x1di\x1d", vec![Italic(vec![t("i")])]);
    check("\x1es\x1e", vec![Strike(vec![t("s")])]);
    check("a \x11*x*\x11", vec![t("a "), Code("*x*".into())]);
    check("\x1funder\x1f", vec![t("under")]);
    check("\x16rev", vec![t("rev")]);
    check(
        "\x0304,12red\x03 \x033green \x03,x",
        vec![t("red green ,x")],
    );
    check("\x0312345", vec![t("345")]);
    check("\x04ff00AAhex\x04,", vec![t("hex,")]);
    check("\x04ff00AA,00ff00hex \x04ff", vec![t("hex ff")]);
    check(
        "\x02\x1dab\x0fc",
        vec![Bold(vec![Italic(vec![t("ab")])]), t("c")],
    );
    // unclosed codes run to the end
    check("x \x02bold", vec![t("x "), Bold(vec![t("bold")])]);
}

#[test]
fn irc_codes_nest() {
    check(
        "\x02a \x1db\x1d c\x02",
        vec![Bold(vec![t("a "), Italic(vec![t("b")]), t(" c")])],
    );
    check(
        "\x1da \x02b\x02 c\x1d",
        vec![Italic(vec![t("a "), Bold(vec![t("b")]), t(" c")])],
    );
    // overlapping spans split where they cross
    check(
        "\x02a\x1db\x02c\x1d",
        vec![
            Bold(vec![t("a"), Italic(vec![t("b")])]),
            Italic(vec![t("c")]),
        ],
    );
    check("\x02\x11x\x11\x02", vec![Bold(vec![Code("x".into())])]);
}

#[test]
fn irc_style_carries_across_lines() {
    assert_eq!(
        parse("\x02a\nb\x02 c"),
        vec![p(vec![
            Bold(vec![t("a")]),
            LineBreak,
            Bold(vec![t("b")]),
            t(" c")
        ])]
    );
}

#[test]
fn markdown_emphasis() {
    check("**bold** x", vec![Bold(vec![t("bold")]), t(" x")]);
    check(
        "*it* _it_",
        vec![Italic(vec![t("it")]), t(" "), Italic(vec![t("it")])],
    );
    check("***both***", vec![Bold(vec![Italic(vec![t("both")])])]);
    check("~~gone~~", vec![Strike(vec![t("gone")])]);
    check(
        "**a *b* c**",
        vec![Bold(vec![t("a "), Italic(vec![t("b")]), t(" c")])],
    );
    check(
        "*a **b** c*",
        vec![Italic(vec![t("a "), Bold(vec![t("b")]), t(" c")])],
    );
    check("***a* b**", vec![Bold(vec![Italic(vec![t("a")]), t(" b")])]);
    check("***a** b*", vec![Italic(vec![Bold(vec![t("a")]), t(" b")])]);
    check("**a *b***", vec![Bold(vec![t("a "), Italic(vec![t("b")])])]);
    check(
        "(*paren*).",
        vec![t("("), Italic(vec![t("paren")]), t(").")],
    );
    check("_~~x~~_", vec![Italic(vec![Strike(vec![t("x")])])]);
    // Markdown inside IRC bold is not applied twice
    check("\x02**x** y\x02", vec![Bold(vec![t("x y")])]);
    // when both cover the same text, the IRC style goes outside
    check("**\x1dx\x1d**", vec![Italic(vec![Bold(vec![t("x")])])]);
}

#[test]
fn markers_outside_word_boundaries_are_literal() {
    for s in [
        "snake_case_name",
        "2*3*4",
        "5 * 3",
        "__init__",
        "*",
        "**",
        "***",
        "****x****",
        "**unbalanced",
        "unbalanced**",
        "a ** b",
        "~single~",
        "~~~x~~~",
        "foo*bar*",
    ] {
        check(s, vec![t(s)]);
    }
    check("*a _b", vec![t("*a _b")]);
    // a closer skips over spans opened inside its own, which become literal
    check("**a *b**", vec![Bold(vec![t("a *b")])]);
    check("*a **b* c**", vec![Italic(vec![t("a **b")]), t(" c**")]);
}

#[test]
fn code_spans() {
    check("`*not* _em_`", vec![Code("*not* _em_".into())]);
    check("a `x` b", vec![t("a "), Code("x".into()), t(" b")]);
    check("``a`b``", vec![Code("a`b".into())]);
    check("`` `x` ``", vec![Code("`x`".into())]);
    check("` x `", vec![Code("x".into())]);
    check("`  `", vec![Code("  ".into())]);
    check("x `y", vec![t("x `y")]);
    check("``x`", vec![t("``x`")]);
    check("`\x02b\x02`", vec![Code("b".into())]);
    check("**`c`**", vec![Bold(vec![Code("c".into())])]);
    check("`https://x.y @ada`", vec![Code("https://x.y @ada".into())]);
    check(
        "see ```x``` now",
        vec![t("see "), Code("x".into()), t(" now")],
    );
}

#[test]
fn bare_urls() {
    check(
        "see https://ex.am/ple_a_b?q=*x*&y=1 now",
        vec![
            t("see "),
            link("https://ex.am/ple_a_b?q=*x*&y=1"),
            t(" now"),
        ],
    );
    check("**https://x.y**", vec![Bold(vec![link("https://x.y")])]);
    check(
        "_https://x.y/a_b now_",
        vec![Italic(vec![link("https://x.y/a_b"), t(" now")])],
    );
    check(
        "(https://x.y/z).",
        vec![t("("), link("https://x.y/z"), t(").")],
    );
    check("https://x.y/a_(b)", vec![link("https://x.y/a_(b)")]);
    check("go HTTP://x.y!", vec![t("go "), link("HTTP://x.y"), t("!")]);
    check("<https://x.y>", vec![t("<"), link("https://x.y"), t(">")]);
    check("xhttps://x.y https://", vec![t("xhttps://x.y https://")]);
    check("https://x.y/@ada", vec![link("https://x.y/@ada")]);
}

#[test]
fn at_mentions() {
    check("@ada hi", vec![mention("ada"), t(" hi")]);
    check("hi @ADA.", vec![t("hi "), mention("ada"), t(".")]);
    check("(@bob)!", vec![t("("), mention("bob"), t(")!")]);
    check("@bob_ x", vec![mention("bob"), t("_ x")]);
    check("@bob-, hi", vec![mention("bob"), t("-, hi")]);
    check("@x[m]: yo", vec![mention("x[m]"), t(": yo")]);
    check("**@ada**", vec![Bold(vec![mention("ada")])]);
    check("_@ada_", vec![Italic(vec![mention("ada")])]);
    // unknown nicks, nicks inside longer words, and mail addresses stay text
    check("@nobody hi", vec![t("@nobody hi")]);
    check("@adam", vec![t("@adam")]);
    check("me@ada.example", vec![t("me@ada.example")]);
    check("x_@ada", vec![t("x_@ada")]);
}

#[test]
fn leading_address() {
    check("ada: look", vec![mention("ada"), t(" look")]);
    check("Bob, hi", vec![mention("bob"), t(" hi")]);
    check("ada:", vec![mention("ada")]);
    check(
        "ada: *x*",
        vec![mention("ada"), t(" "), Italic(vec![t("x")])],
    );
    check("nobody: hi", vec![t("nobody: hi")]);
    check("ada:hi", vec![t("ada:hi")]);
    check(" ada: hi", vec![t(" ada: hi")]);
    check("hi ada: x", vec![t("hi ada: x")]);
    // only the first line of the message
    assert_eq!(
        parse("x\nada: y"),
        vec![p(vec![t("x"), LineBreak, t("ada: y")])]
    );
}

#[test]
fn paragraphs_and_line_breaks() {
    assert_eq!(
        parse("a\r\nb\n\n\nc"),
        vec![p(vec![t("a"), LineBreak, t("b")]), p(vec![t("c")])]
    );
}

#[test]
fn fenced_code_blocks() {
    assert_eq!(
        parse("look:\n```rust\nlet *x* = 1;\n\n  `y`\n```\nok"),
        vec![
            p(vec![t("look:")]),
            CodeBlock("let *x* = 1;\n\n  `y`".into()),
            p(vec![t("ok")]),
        ]
    );
    assert_eq!(parse("```x```"), vec![CodeBlock("x".into())]);
    assert_eq!(parse("```a b\nc```"), vec![CodeBlock("a b\nc".into())]);
    assert_eq!(parse("```word\n```"), vec![CodeBlock("word".into())]);
    assert_eq!(parse("```\n```"), vec![]);
    assert_eq!(
        parse("```\nnot closed"),
        vec![p(vec![t("```"), LineBreak, t("not closed")])]
    );
}

#[test]
fn quotes() {
    assert_eq!(
        parse("> a *b*\n>\n> c\nd"),
        vec![
            Quote(vec![
                p(vec![t("a "), Italic(vec![t("b")])]),
                p(vec![t("c")])
            ]),
            p(vec![t("d")]),
        ]
    );
    assert_eq!(
        parse("> > deep\n> - item\n> ```\n> code\n> ```"),
        vec![Quote(vec![
            Quote(vec![p(vec![t("deep")])]),
            List {
                ordered: false,
                items: vec![item("item")]
            },
            CodeBlock("code".into()),
        ])]
    );
    // not quotes
    assert_eq!(parse(">_<"), vec![p(vec![t(">_<")])]);
    assert_eq!(parse("a > b"), vec![p(vec![t("a > b")])]);
}

#[test]
fn lists() {
    assert_eq!(
        parse("- a\n* b\n+ c"),
        vec![List {
            ordered: false,
            items: vec![item("a"), item("b"), item("c")]
        }]
    );
    assert_eq!(
        parse("text\n1. one\n2) two\n- x\nafter"),
        vec![
            p(vec![t("text")]),
            List {
                ordered: true,
                items: vec![item("one"), item("two")]
            },
            List {
                ordered: false,
                items: vec![item("x")]
            },
            p(vec![t("after")]),
        ]
    );
    assert_eq!(
        parse("- **a**\n    - b\n    - c\n      more\n- d"),
        vec![List {
            ordered: false,
            items: vec![
                vec![
                    p(vec![Bold(vec![t("a")])]),
                    List {
                        ordered: false,
                        items: vec![item("b"), vec![p(vec![t("c"), LineBreak, t("more")])]]
                    }
                ],
                item("d"),
            ]
        }]
    );
    // a blank line ends the list; markers need a space and content
    assert_eq!(
        parse("- a\n\n- b"),
        vec![
            List {
                ordered: false,
                items: vec![item("a")]
            },
            List {
                ordered: false,
                items: vec![item("b")]
            },
        ]
    );
    for s in ["-1 degrees", "- ", "1.5 litres", "1234567890. x"] {
        assert_eq!(parse(s), vec![p(vec![t(s)])], "input: {s:?}");
    }
}

// --- round trips ----------------------------------------------------------

#[test]
fn round_trips() {
    let trees = vec![
        vec![p(vec![
            t("hello "),
            Bold(vec![t("bold "), Italic(vec![t("both")])]),
            t(" "),
            Strike(vec![t("gone")]),
            t(" "),
            Code("x*y_z".into()),
            LineBreak,
            t("next 2*3*4 snake_case"),
        ])],
        vec![p(vec![
            link("https://ex.am/ple_a_b?q=*x*&y=1"),
            t(" and "),
            Italic(vec![t("it "), Bold(vec![Strike(vec![t("all")])])]),
        ])],
        vec![p(vec![
            Code("a`b".into()),
            t(" "),
            Code("`x`".into()),
            t(" "),
            Bold(vec![Code("c".into())]),
        ])],
        vec![
            p(vec![t("code:")]),
            CodeBlock("fn main() {\n    println!(\"*hi*\");\n}".into()),
            p(vec![t("after")]),
        ],
        vec![
            Quote(vec![
                Quote(vec![p(vec![t("inner")])]),
                p(vec![t("quoted "), Bold(vec![t("b")]), LineBreak, t("two")]),
            ]),
            p(vec![t("reply")]),
        ],
        vec![List {
            ordered: false,
            items: vec![
                vec![p(vec![t("a"), LineBreak, t("a2")])],
                vec![
                    p(vec![Italic(vec![t("b")])]),
                    List {
                        ordered: true,
                        items: vec![item("c"), item("d")],
                    },
                ],
                vec![p(vec![t("e")]), CodeBlock("y\n  z".into())],
                vec![Quote(vec![p(vec![t("q")])])],
            ],
        }],
        vec![List {
            ordered: true,
            items: (1..=12).map(|n| item(&format!("n{n}"))).collect(),
        }],
    ];
    for tree in trees {
        let rt = RichText(tree).normalize();
        let irc = to_irc(&rt, &Fake);
        assert_eq!(from_irc(&irc, &Fake), rt, "via {irc:?}");
    }
}

#[test]
fn known_lossy_round_trips() {
    let rt = |inl| RichText(vec![p(inl)]);
    let via = |inl: Vec<Inline>| from_irc(&to_irc(&rt(inl), &Fake), &Fake);
    // images become links to the gateway
    assert_eq!(
        via(vec![Image {
            url: "https://teams/objects/abc".into()
        }]),
        rt(vec![link("https://gw.example/m/abc")])
    );
    // emoji become their text
    assert_eq!(via(vec![Emoji("(smile)".into())]), rt(vec![t("(smile)")]));
    // a mention becomes the bare nick, which reads as text
    assert_eq!(via(vec![t("hi "), mention("ada")]), rt(vec![t("hi ada")]));
    // a link with its own text keeps the URL beside the text
    assert_eq!(
        via(vec![Link {
            url: "https://x.y".into(),
            text: vec![t("docs")]
        }]),
        rt(vec![t("docs ("), link("https://x.y"), t(")")])
    );
    // emphasis across a line break is split per line
    assert_eq!(
        via(vec![Bold(vec![t("a"), LineBreak, t("b")])]),
        rt(vec![Bold(vec![t("a")]), LineBreak, Bold(vec![t("b")])])
    );
    // spans covering the same text nest bold, italic, strike (outermost first)
    assert_eq!(
        via(vec![Strike(vec![Italic(vec![Bold(vec![t("x")])])])]),
        rt(vec![Bold(vec![Italic(vec![Strike(vec![t("x")])])])])
    );
    // adjacent paragraphs join into one
    assert_eq!(
        from_irc(
            &to_irc(&RichText(vec![p(vec![t("a")]), p(vec![t("b")])]), &Fake),
            &Fake
        ),
        rt(vec![t("a"), LineBreak, t("b")])
    );
}

/// Random strings over an alphabet of the characters that matter.
fn noise(seed: u64, n: usize) -> Vec<String> {
    const ALPHABET: &[&str] = &[
        "a",
        "b",
        " ",
        " ",
        "\n",
        "*",
        "**",
        "_",
        "~~",
        "`",
        "```",
        "> ",
        "- ",
        "1. ",
        "  ",
        "@ada",
        "ada: ",
        "https://x.y/",
        "(",
        ")",
        ".",
        "\x02",
        "\x1d",
        "\x1e",
        "\x11",
        "\x0f",
        "\x034,5",
        "\r",
        "\0",
        "é",
        "😀",
    ];
    let mut state = seed;
    let mut next = || {
        state = state
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        (state >> 33) as usize
    };
    (0..n)
        .map(|_| {
            let len = next() % 24;
            (0..len)
                .map(|_| ALPHABET[next() % ALPHABET.len()])
                .collect()
        })
        .collect()
}

/// Not asserted: that rendering what was parsed and parsing it again is
/// stable. Text that holds literal markers (`` ` ``, `**`, a leading `- `)
/// can pair up differently once rendered next to code spans or formatting,
/// and IRC has no escapes clients understand.
#[test]
fn random_input_keeps_the_invariants() {
    for s in noise(7, 5000) {
        let rt = from_irc(&s, &Fake);
        assert_eq!(rt.clone().normalize(), rt, "not normalised: {s:?}");
        let irc = to_irc(&rt, &Fake);
        assert!(!irc.contains(['\r', '\0']), "{s:?} -> {irc:?}");
        if irc.is_empty() {
            assert!(rt.is_empty(), "{s:?} rendered as nothing");
            continue;
        }
        for line in irc.split('\n') {
            assert!(!line.trim().is_empty(), "{s:?} -> {irc:?}");
            assert_eq!(line.trim_end(), line, "{s:?} -> {irc:?}");
        }
        // parsing what was rendered keeps the invariants too
        let again = from_irc(&irc, &Fake);
        assert_eq!(again.clone().normalize(), again);
    }
}
