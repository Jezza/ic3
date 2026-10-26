//! [`RichText`] → Teams HTML (spec §4.9.3).
//!
//! The tag set is exactly what `from_teams_html` parses back: `<p>`, `<br>`,
//! `<b>`, `<i>`, `<s>`, `<code>`, `<pre>`, `<blockquote>`, `<ul>`/`<ol>`/`<li>`,
//! `<a href>`, the Teams mention, emoji and (assumed) AMS image markup, and
//! the (assumed) reply markup. Text nodes escape `&`, `<` and `>`; attribute values also `"`.

use std::borrow::Cow;

use super::{Block, Inline, ReplyQuote, RichText};

/// Renders a tree as Teams HTML, the body of a `messagetype: RichText/Html`
/// message.
pub fn to_teams_html(t: &RichText) -> String {
    blocks_html(&t.0)
}

fn blocks_html(blocks: &[Block]) -> String {
    blocks.iter().map(block_html).collect()
}

fn block_html(block: &Block) -> String {
    match block {
        Block::Paragraph(inl) => format!("<p>{}</p>", inlines_html(inl)),
        // The newline is deliberate: HTML parsers drop one newline right after
        // `<pre>`, so content that starts with a newline survives the round trip.
        Block::CodeBlock(code) => format!("<pre>\n{}</pre>", text(code)),
        Block::Quote(blocks) => format!("<blockquote>{}</blockquote>", blocks_html(blocks)),
        Block::List { ordered, items } => {
            let tag = if *ordered { "ol" } else { "ul" };
            let items: String = items
                .iter()
                .map(|item| format!("<li>{}</li>", blocks_html(item)))
                .collect();
            format!("<{tag}>{items}</{tag}>")
        }
        Block::Reply(q) => reply_block_html(q),
    }
}

fn inlines_html(inlines: &[Inline]) -> String {
    inlines.iter().map(inline_html).collect()
}

fn inline_html(inline: &Inline) -> String {
    match inline {
        Inline::Text(s) => text(s).into_owned(),
        Inline::Bold(c) => format!("<b>{}</b>", inlines_html(c)),
        Inline::Italic(c) => format!("<i>{}</i>", inlines_html(c)),
        Inline::Strike(c) => format!("<s>{}</s>", inlines_html(c)),
        Inline::Code(s) => format!("<code>{}</code>", text(s)),
        Inline::Link { url, text: label } => {
            let label = if label.is_empty() {
                text(url).into_owned()
            } else {
                inlines_html(label)
            };
            format!("<a href=\"{}\">{label}</a>", attr(url))
        }
        // Teams' own markup (observed 2026-10-22).
        Inline::Mention { mri, name } => format!(
            "<readonly class=\"skipProofing\" itemtype=\"http://schema.skype.com/Mention\" spellcheck=\"false\">\
             <span itemtype=\"http://schema.skype.com/Mention\" itemscope itemid=\"{}\">{}</span></readonly>",
            attr(&mri.0),
            text(name)
        ),
        Inline::Emoji(alt) => format!(
            "<img itemtype=\"http://schema.skype.com/Emoji\" alt=\"{}\">",
            attr(alt)
        ),
        // An image already in AMS (`upload_image`) gets Teams' AMSImage
        // markup; any other stays a plain `<img src>`.
        Inline::Image { url } => crate::media::ams_image_html(url)
            .unwrap_or_else(|| format!("<img src=\"{}\">", attr(url))),
        Inline::LineBreak => "<br>".to_string(),
    }
}

/// UNVERIFIED(§9.8): reply markup assumed from the spec; the official client's layout is
/// unknown until captured. `from_teams_html` parses tolerantly (any itemtype containing "Reply").
pub(crate) fn reply_block_html(q: &ReplyQuote) -> String {
    let mut s = format!(
        "<blockquote itemscope=\"\" itemtype=\"http://schema.skype.com/Reply\" itemid=\"{}\">",
        attr(&q.message_id.0)
    );
    match (&q.author, &q.author_name) {
        (Some(m), name) => {
            s += &format!(
                "<strong itemprop=\"mri\" itemid=\"{}\">{}</strong>",
                attr(&m.0),
                text(name.as_deref().unwrap_or(""))
            );
        }
        (None, Some(name)) => s += &format!("<strong>{}</strong>", text(name)),
        (None, None) => {}
    }
    s += &format!(
        "<span itemprop=\"time\" itemid=\"{}\"></span>",
        attr(&q.message_id.0)
    );
    s += &format!(
        "<p itemprop=\"preview\">{}</p></blockquote>",
        text(&q.excerpt.plain_text())
    );
    s
}

/// Escapes a text node: `&`, `<`, `>` only (quotes are valid in text).
fn text(s: &str) -> Cow<'_, str> {
    html_escape::encode_text(s)
}

/// Escapes a double-quoted attribute value: `&`, `<`, `>`, `"`. Not
/// `encode_safe`, which would also escape `/` and mangle URLs.
fn attr(s: &str) -> Cow<'_, str> {
    html_escape::encode_double_quoted_attribute(s)
}

#[cfg(test)]
mod tests {
    #[test]
    fn html_output() {
        use crate::format::*;
        let rt = parse_slack(
            "hi *b* _i_ ~s~ `c` https://x.y/?a=1&b=2 <tag>\n2nd\n\n```\nlet a = \"<\";\n```\n> q\n- l1\n1. o1",
        );
        assert_eq!(
            to_teams_html(&rt),
            "<p>hi <b>b</b> <i>i</i> <s>s</s> <code>c</code> <a href=\"https://x.y/?a=1&amp;b=2\">https://x.y/?a=1&amp;b=2</a> &lt;tag&gt;<br>2nd</p>\
             <pre>\nlet a = \"&lt;\";</pre><blockquote><p>q</p></blockquote><ul><li><p>l1</p></li></ul><ol><li><p>o1</p></li></ol>"
        );
    }

    #[test]
    fn reply_markup() {
        use crate::format::*;
        let q = ReplyQuote {
            message_id: crate::MessageId("123".into()),
            author: Some(crate::Mri("8:orgid:a".into())),
            author_name: Some("Ada".into()),
            excerpt: RichText::text("hello <there>"),
        };
        let html = to_teams_html(&RichText(vec![
            Block::Reply(q),
            Block::Paragraph(vec![Inline::Text("yes".into())]),
        ]));
        assert!(html.starts_with(
            "<blockquote itemscope=\"\" itemtype=\"http://schema.skype.com/Reply\" itemid=\"123\">"
        ));
        assert!(html.contains("<strong itemprop=\"mri\" itemid=\"8:orgid:a\">Ada</strong>"));
        assert!(html.contains("<span itemprop=\"time\" itemid=\"123\"></span>"));
        assert!(
            html.contains("<p itemprop=\"preview\">hello &lt;there&gt;</p></blockquote><p>yes</p>")
        );
    }

    // Markup the brief specifies but its table does not exercise; `from_teams_html` relies on it.
    #[test]
    fn mention_emoji_image_and_empty_label_link_markup() {
        use crate::format::*;
        let rt = RichText(vec![Block::Paragraph(vec![
            Inline::Mention {
                mri: crate::Mri("8:orgid:a\"b".into()),
                name: "A <B>".into(),
            },
            Inline::Emoji("smile".into()),
            Inline::Image {
                url: "https://i/x.png?a=1&b=2".into(),
            },
            Inline::Link {
                url: "https://x.y/?q=\"1\"".into(),
                text: vec![],
            },
        ])]);
        assert_eq!(
            to_teams_html(&rt),
            "<p><readonly class=\"skipProofing\" itemtype=\"http://schema.skype.com/Mention\" spellcheck=\"false\">\
             <span itemtype=\"http://schema.skype.com/Mention\" itemscope itemid=\"8:orgid:a&quot;b\">A &lt;B&gt;</span></readonly>\
             <img itemtype=\"http://schema.skype.com/Emoji\" alt=\"smile\">\
             <img src=\"https://i/x.png?a=1&amp;b=2\">\
             <a href=\"https://x.y/?q=&quot;1&quot;\">https://x.y/?q=\"1\"</a></p>"
        );
    }

    #[test]
    fn reply_without_author() {
        use crate::format::*;
        let q = |author_name: Option<&str>| {
            Block::Reply(ReplyQuote {
                message_id: crate::MessageId("7".into()),
                author: None,
                author_name: author_name.map(str::to_string),
                excerpt: RichText::text("x"),
            })
        };
        assert_eq!(
            to_teams_html(&RichText(vec![q(Some("Ada"))])),
            "<blockquote itemscope=\"\" itemtype=\"http://schema.skype.com/Reply\" itemid=\"7\">\
             <strong>Ada</strong><span itemprop=\"time\" itemid=\"7\"></span>\
             <p itemprop=\"preview\">x</p></blockquote>"
        );
        assert_eq!(
            to_teams_html(&RichText(vec![q(None)])),
            "<blockquote itemscope=\"\" itemtype=\"http://schema.skype.com/Reply\" itemid=\"7\">\
             <span itemprop=\"time\" itemid=\"7\"></span>\
             <p itemprop=\"preview\">x</p></blockquote>"
        );
    }
}
