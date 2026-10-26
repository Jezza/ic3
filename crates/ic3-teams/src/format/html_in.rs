//! Teams HTML → [`RichText`] (spec §4.9.4) and the wire-body entry point.
//!
//! The HTML is parsed by html5ever ([`scraper::Html::parse_fragment`]) and a
//! recursive walk over the `ego_tree` nodes builds the blocks. Known tags map
//! to tree nodes; everything else degrades to its content and nothing is ever
//! an error (Review Focus #2): `<div>`, `<span>` without an `itemtype`, `<u>`,
//! `<font>` … are walked through, entities decode (`&nbsp;` stays U+00A0) and
//! text is never trimmed. The one piece of whitespace that is dropped is the
//! layout kind: a whitespace-only text node between block elements inside a
//! block container (`is_layout_whitespace`). Nesting is capped at `MAX_DEPTH`
//! (html5ever caps nothing): deeper markup keeps its text only, so neither the
//! walk nor the tree it builds can overflow the stack on a peer-authored body.

use ego_tree::{NodeId, NodeRef, iter::Edge};
use scraper::{Html, Node, node::Element};

use super::{Block, Inline, ReplyQuote, RichText};
use crate::types::{MessageId, Mri};

/// Tags the walker turns into blocks. A whitespace-only text node next to one
/// of these is layout, not content.
const BLOCK_TAGS: &[&str] = &["p", "div", "pre", "blockquote", "ul", "ol", "li"];

/// Block containers whose whitespace-only text children may be layout (ruling
/// D13: never `p` or `li`, whose whitespace is content). `html` is the element
/// `parse_fragment` wraps the content in; fragment parsing never creates `body`.
const BLOCK_CONTAINERS: &[&str] = &["html", "div", "blockquote", "ul", "ol"];

/// Nesting depth (in elements, from the fragment root) past which an element
/// contributes only its text. Bounds both the recursion of the walk and the
/// depth of the tree it builds; real messages nest a dozen levels at most.
const MAX_DEPTH: usize = 64;

/// Bodies larger than this (in bytes) skip html5ever (see [`from_teams_html`]).
const MAX_HTML_BYTES: usize = 256 * 1024;

/// Bodies whose tags nest deeper than this, by the pre-scan, skip html5ever
/// (see [`needs_fallback`]). Equal to the walker's `MAX_DEPTH`: deeper markup
/// would only contribute its text anyway.
const MAX_HTML_NESTING: usize = 64;

/// Bodies with more start tags (`<` + ASCII letter) than this skip html5ever
/// (see [`needs_fallback`]).
const MAX_HTML_START_TAGS: usize = 2048;

/// Elements that never have content, so never nest.
const VOID_TAGS: &[&str] = &[
    "area", "base", "br", "col", "embed", "hr", "img", "input", "keygen", "link", "meta", "param",
    "source", "track", "wbr",
];

/// Parses a `RichText/Html` body into a normalised tree.
///
/// html5ever's tree builder is quadratic in nesting depth and can multiply
/// formatting elements, so untrusted HTML (a peer's message) could stall the
/// caller. A body over 256 KiB, with more than 2048 start tags, or whose tags
/// nest deeper than 64 levels by a linear pre-scan never reaches it: it
/// degrades to its tag-stripped, entity-decoded text instead.
pub fn from_teams_html(html: &str) -> RichText {
    if needs_fallback(html) {
        return RichText::from_plain_text(&strip_tags(html));
    }
    parse_html(html)
}

/// Whether `html` must not reach html5ever (see [`from_teams_html`]).
///
/// What the limits buy, and what they do not prove: every start-tag token
/// html5ever produces begins with `<` + letter, so it gets at most
/// [`MAX_HTML_START_TAGS`] of them. Elements are not bounded by tokens alone,
/// though: formatting elements (`b`, `i`, `a`, …) left open stay in html5ever's
/// active formatting list after their parent closes, and every later text or
/// tag re-opens (clones) the whole list, so the tree can grow as
/// list length × start tags. The scan counts such unclosed formatting tags as
/// open, so the list a passing body can build is about [`MAX_HTML_NESTING`]
/// long, and the tree at most about 64 × 2048 ≈ 131 k elements (measured: the
/// worst reconstruction body that passes builds ~127 k nodes; parsing and
/// walking it takes ~50 ms in release). That holds as far as the scan's
/// tokenizing matches html5ever's (see [`pieces`] for where it approximates,
/// always towards deeper); it is an engineering bound, not a proof over every
/// tree-construction path.
fn needs_fallback(html: &str) -> bool {
    html.len() > MAX_HTML_BYTES
        || too_many_start_tags(html, MAX_HTML_START_TAGS)
        || exceeds_nesting(html, MAX_HTML_NESTING)
}

/// The html5ever parse and tree walk behind [`from_teams_html`], unguarded.
fn parse_html(html: &str) -> RichText {
    let doc = Html::parse_fragment(html);
    let mut b = Builder::default();
    // parse_fragment wraps the content in <html>; walking from the tree root
    // reaches it through the "unknown tag" arm.
    walk(doc.tree.root(), &mut b, 0);
    RichText(b.finish()).normalize()
}

/// Parses a message body by its `messagetype`: `Text` bodies are plain text
/// (spec §4.9.4), everything else is Teams HTML ([`from_teams_html`], size-
/// and depth-guarded). A leading [`Block::Reply`] is lifted out as the
/// message's `reply_to` (spec §4.5); the body excludes it.
pub fn body_from_wire(message_type: &str, content: &str) -> (RichText, Option<ReplyQuote>) {
    let mut rt = if message_type == "Text" {
        RichText::from_plain_text(content)
    } else {
        from_teams_html(content)
    };
    let reply = matches!(rt.0.first(), Some(Block::Reply(_)))
        .then(|| rt.0.remove(0))
        .and_then(|b| match b {
            Block::Reply(q) => Some(q),
            _ => None,
        });
    (rt, reply)
}

/// A piece of HTML as the linear pre-scan sees it.
enum Piece<'a> {
    Text(&'a str),
    /// A start tag, by name (case as written).
    Open(&'a str),
    /// An end tag, by name (case as written).
    Close(&'a str),
    /// A comment, doctype, bogus comment (`<?…>`, `</3…>`), `</>`, or a tag
    /// cut off by the end of input (html5ever drops those).
    Other,
}

/// Splits HTML into text and markup in one linear pass, ending each piece
/// where html5ever's tokenizer ends it: a tag at the first `>` outside a quoted
/// attribute value; a comment at `-->` or `--!>` (or at once for `<!-->` and
/// `<!--->`); `<!…>`, `<?…>` and `</` + non-letter (bogus comments) at the
/// first `>`. A `<` that starts none of these is text.
///
/// Approximated: inside a raw-text element (`<script>`, `<style>`,
/// `<textarea>`, `<title>`, … up to its own end tag) and a `<![CDATA[ … ]]>`
/// section, html5ever tokenizes no tags, while the scan still reports start
/// tags there but turns end tags into [`Piece::Other`]. Both deviations only
/// make [`exceeds_nesting`] count deeper, never shallower.
fn pieces(html: &str) -> impl Iterator<Item = Piece<'_>> {
    let b = html.as_bytes();
    let mut i = 0;
    // End tags starting before this offset are inside raw text or CDATA.
    let mut quiet_until = 0;
    std::iter::from_fn(move || {
        if i >= b.len() {
            return None;
        }
        let starts_markup = |j: usize| {
            b[j] == b'<'
                && b.get(j + 1)
                    .is_some_and(|c| c.is_ascii_alphabetic() || matches!(c, b'!' | b'?' | b'/'))
                && (b[j + 1] != b'/' || j + 2 < b.len())
        };
        let start = i;
        let text_end = (i..b.len()).find(|&j| starts_markup(j)).unwrap_or(b.len());
        if text_end > start {
            i = text_end;
            return Some(Piece::Text(&html[start..text_end]));
        }
        let to_gt = |from: usize| {
            b[from..]
                .iter()
                .position(|&c| c == b'>')
                .map_or(b.len(), |p| from + p + 1)
        };
        let (piece, end) = match b[i + 1] {
            b'!' if b[i..].starts_with(b"<!--") => (Piece::Other, comment_end(b, i + 4)),
            b'!' | b'?' => (Piece::Other, to_gt(i + 2)),
            b'/' if b[i + 2].is_ascii_alphabetic() => tag(html, i + 2, Piece::Close),
            b'/' if b[i + 2] == b'>' => (Piece::Other, i + 3),
            b'/' => (Piece::Other, to_gt(i + 2)),
            _ => tag(html, i + 1, Piece::Open),
        };
        i = end;
        if start >= quiet_until {
            // Each quiet region is searched once, so the scan stays linear.
            match piece {
                Piece::Open(name) if RAW_TEXT_TAGS.iter().any(|t| t.eq_ignore_ascii_case(name)) => {
                    quiet_until = find_end_tag(b, end, name);
                }
                Piece::Other if b[start..].starts_with(b"<![CDATA[") => {
                    quiet_until = b[start..]
                        .windows(3)
                        .position(|w| w == b"]]>")
                        .map_or(b.len(), |p| start + p);
                }
                _ => {}
            }
        }
        Some(match piece {
            Piece::Close(_) if start < quiet_until => Piece::Other,
            other => other,
        })
    })
}

/// Elements whose content html5ever tokenizes as text, up to their own end
/// tag (`plaintext` has none: the rest of the input).
const RAW_TEXT_TAGS: &[&str] = &[
    "iframe",
    "noembed",
    "noframes",
    "noscript",
    "plaintext",
    "script",
    "style",
    "textarea",
    "title",
    "xmp",
];

/// The offset of the first end tag `</name` (ASCII case-insensitive, followed
/// by whitespace, `/`, `>` or the end of input) at or after `from`, or the end
/// of input.
fn find_end_tag(b: &[u8], from: usize, name: &str) -> usize {
    let n = name.len() + 2;
    (from..b.len().saturating_sub(n - 1))
        .find(|&p| {
            b[p] == b'<'
                && b[p + 1] == b'/'
                && b[p + 2..p + n].eq_ignore_ascii_case(name.as_bytes())
                && b.get(p + n)
                    .is_none_or(|c| c.is_ascii_whitespace() || matches!(c, b'/' | b'>'))
        })
        .unwrap_or(b.len())
}

/// Where a comment whose body starts at `from` ends (one past its last byte).
fn comment_end(b: &[u8], from: usize) -> usize {
    if b[from..].starts_with(b">") {
        return from + 1;
    }
    if b[from..].starts_with(b"->") {
        return from + 2;
    }
    let mut p = from;
    while p + 2 < b.len() {
        if b[p] == b'-' && b[p + 1] == b'-' {
            if b[p + 2] == b'>' {
                return p + 3;
            }
            if b[p + 2] == b'!' && b.get(p + 3) == Some(&b'>') {
                return p + 4;
            }
        }
        p += 1;
    }
    b.len()
}

/// A start or end tag whose name starts at `name_at`: the piece and where it
/// ends. Attributes are skipped as html5ever tokenizes them, so a `>` inside a
/// quoted value does not end the tag. A tag cut off by the end of input is
/// [`Piece::Other`], as html5ever drops it.
fn tag<'a>(html: &'a str, name_at: usize, kind: fn(&'a str) -> Piece<'a>) -> (Piece<'a>, usize) {
    let b = html.as_bytes();
    let ws = |c: u8| c.is_ascii_whitespace();
    let skip_ws = |mut k: usize| {
        while k < b.len() && ws(b[k]) {
            k += 1;
        }
        k
    };
    let mut k = name_at;
    while k < b.len() && !ws(b[k]) && b[k] != b'/' && b[k] != b'>' {
        k += 1;
    }
    let name = &html[name_at..k];
    let end = loop {
        // before attribute name (also after `/` and after a value)
        k = skip_ws(k);
        match b.get(k) {
            None => break None,
            Some(b'>') => break Some(k + 1),
            Some(b'/') => {
                k += 1;
                continue;
            }
            Some(_) => {}
        }
        // attribute name: the first byte always belongs to it, even `=`
        k += 1;
        while k < b.len() && !ws(b[k]) && !matches!(b[k], b'/' | b'>' | b'=') {
            k += 1;
        }
        k = skip_ws(k);
        if b.get(k) != Some(&b'=') {
            continue;
        }
        k = skip_ws(k + 1);
        match b.get(k) {
            None => break None,
            Some(b'>') => break Some(k + 1),
            Some(&q @ (b'"' | b'\'')) => match b[k + 1..].iter().position(|&c| c == q) {
                Some(p) => k += p + 2,
                None => break None,
            },
            Some(_) => {
                while k < b.len() && !ws(b[k]) && b[k] != b'>' {
                    k += 1;
                }
            }
        }
    };
    match end {
        Some(end) => (kind(name), end),
        None => (Piece::Other, b.len()),
    }
}

/// Whether more than `cap` start tags (`<` + ASCII letter, wherever it
/// appears) occur in `html`. Every start-tag token html5ever produces begins
/// with such a pair, so this bounds the elements it creates. Stops counting
/// at `cap + 1`.
fn too_many_start_tags(html: &str, cap: usize) -> bool {
    html.as_bytes()
        .windows(2)
        .filter(|w| w[0] == b'<' && w[1].is_ascii_alphabetic())
        .nth(cap)
        .is_some()
}

/// Whether the tags of `html` nest deeper than `limit`, by a linear scan over
/// [`pieces`]. An end tag closes only the innermost open element of the same
/// name; any other end tag is ignored, and void elements never nest. Where the
/// scan tokenizes as html5ever does, it never reports less nesting than
/// html5ever builds (html5ever pops at least as much for a matching end tag,
/// and may keep open what the scan keeps open, e.g. `<li><ul></li>`).
/// Misnested or unclosed markup only adds up. It is not a guarantee on its
/// own (see [`pieces`] for what is not modelled); the start-tag cap is.
fn exceeds_nesting(html: &str, limit: usize) -> bool {
    let mut open: Vec<&str> = Vec::new();
    for piece in pieces(html) {
        match piece {
            Piece::Open(name) if !VOID_TAGS.iter().any(|v| v.eq_ignore_ascii_case(name)) => {
                open.push(name);
                if open.len() > limit {
                    return true;
                }
            }
            Piece::Close(name)
                if open
                    .last()
                    .is_some_and(|top| top.eq_ignore_ascii_case(name)) =>
            {
                open.pop();
            }
            _ => {}
        }
    }
    false
}

/// The text of `html` with tags removed and entities decoded, for bodies that
/// must not reach html5ever. `<br>` becomes a line break and block tags a
/// paragraph break; everything else (comments included) is dropped.
fn strip_tags(html: &str) -> String {
    let mut out = String::with_capacity(html.len());
    for piece in pieces(html) {
        match piece {
            Piece::Text(t) => out.push_str(&html_escape::decode_html_entities(t)),
            Piece::Open(n) | Piece::Close(n) if n.eq_ignore_ascii_case("br") => out.push('\n'),
            Piece::Open(n) | Piece::Close(n)
                if BLOCK_TAGS.iter().any(|b| b.eq_ignore_ascii_case(n)) =>
            {
                out.push_str("\n\n");
            }
            _ => {}
        }
    }
    out
}

/// Accumulates blocks; inline nodes collect in `para` until a block boundary
/// flushes them as a paragraph.
#[derive(Default)]
struct Builder {
    blocks: Vec<Block>,
    para: Vec<Inline>,
}

impl Builder {
    fn flush(&mut self) {
        if !self.para.is_empty() {
            self.blocks
                .push(Block::Paragraph(std::mem::take(&mut self.para)));
        }
    }

    fn push(&mut self, i: Inline) {
        self.para.push(i);
    }

    fn push_block(&mut self, b: Block) {
        self.flush();
        self.blocks.push(b);
    }

    fn finish(mut self) -> Vec<Block> {
        self.flush();
        self.blocks
    }
}

/// Visits every child of `node`, in order. `depth` is `node`'s nesting depth.
fn walk(node: NodeRef<'_, Node>, b: &mut Builder, depth: usize) {
    for child in node.children() {
        visit(child, b, depth + 1);
    }
}

/// Visits one node at nesting depth `depth`. Comments, doctypes and the like
/// contribute nothing; an element past `MAX_DEPTH` contributes its text only,
/// which ends the recursion.
fn visit(node: NodeRef<'_, Node>, b: &mut Builder, depth: usize) {
    match node.value() {
        Node::Text(t) => {
            if !is_layout_whitespace(node) {
                b.push(Inline::Text(t.to_string()));
            }
        }
        Node::Element(_) if depth > MAX_DEPTH => b.push(Inline::Text(text_content(node))),
        Node::Element(el) => visit_element(node, el, b, depth),
        _ => {}
    }
}

fn visit_element(node: NodeRef<'_, Node>, el: &Element, b: &mut Builder, depth: usize) {
    match el.name() {
        // `li` here is a stray one outside a list; inside one, `list_items` walks it.
        "p" | "div" | "li" => {
            b.flush();
            walk(node, b, depth);
            b.flush();
        }
        "br" => b.push(Inline::LineBreak),
        "pre" => b.push_block(Block::CodeBlock(text_content(node))),
        "blockquote" => match parse_reply(node, el) {
            Some(q) => b.push_block(Block::Reply(q)),
            None => b.push_block(Block::Quote(blocks_of(node, depth))),
        },
        "ul" | "ol" => b.push_block(Block::List {
            ordered: el.name() == "ol",
            items: list_items(node, depth),
        }),
        "b" | "strong" => b.push(Inline::Bold(inlines_of(node, depth))),
        "i" | "em" => b.push(Inline::Italic(inlines_of(node, depth))),
        "s" | "strike" | "del" => b.push(Inline::Strike(inlines_of(node, depth))),
        "code" => b.push(Inline::Code(text_content(node))),
        "a" => b.push(Inline::Link {
            url: attr(el, "href").unwrap_or_default().to_string(),
            text: inlines_of(node, depth),
        }),
        "span" if item_type_is(el, "Mention") => b.push(Inline::Mention {
            mri: Mri(attr(el, "itemid").unwrap_or_default().to_string()),
            name: text_content(node),
        }),
        "img" if item_type_is(el, "Emoji") => {
            b.push(Inline::Emoji(
                attr(el, "alt").unwrap_or_default().to_string(),
            ));
        }
        // An AMS image (`…/AMSImage`) keeps its full-size view, whatever
        // view `src` shows (§9.29).
        "img" if item_type_is(el, "AMSImage") => {
            let src = attr(el, "src").unwrap_or_default();
            b.push(Inline::Image {
                url: crate::media::full_size_image_url(src).unwrap_or_else(|| src.to_string()),
            });
        }
        "img" => b.push(Inline::Image {
            url: attr(el, "src").unwrap_or_default().to_string(),
        }),
        // Unknown tags degrade to their content (Review Focus #2).
        _ => walk(node, b, depth),
    }
}

/// The blocks inside a block container at nesting depth `depth`.
fn blocks_of(node: NodeRef<'_, Node>, depth: usize) -> Vec<Block> {
    let mut inner = Builder::default();
    walk(node, &mut inner, depth);
    inner.finish()
}

/// The items of a `<ul>`/`<ol>`: each `<li>` is one. Anything else directly
/// inside the list (text, a nested list, …) is kept as an item of its own
/// rather than dropped.
fn list_items(list: NodeRef<'_, Node>, depth: usize) -> Vec<Vec<Block>> {
    list.children()
        .map(|child| {
            let mut inner = Builder::default();
            if child.value().as_element().is_some_and(|e| e.name() == "li") {
                walk(child, &mut inner, depth + 1);
            } else {
                visit(child, &mut inner, depth + 1);
            }
            inner.finish()
        })
        .filter(|item| !item.is_empty())
        .collect()
}

/// The inline content of an inline element. Block tags inside it (html5ever
/// allows `<b><p>…</p></b>`) are linearised: paragraphs contribute their
/// inlines, anything else its plain text, with a line break between blocks.
fn inlines_of(node: NodeRef<'_, Node>, depth: usize) -> Vec<Inline> {
    let mut inner = Builder::default();
    walk(node, &mut inner, depth);
    if inner.blocks.is_empty() {
        return inner.para;
    }
    let mut out = Vec::new();
    for block in inner.finish() {
        if !out.is_empty() {
            out.push(Inline::LineBreak);
        }
        match block {
            Block::Paragraph(inl) => out.extend(inl),
            other => out.push(Inline::Text(RichText(vec![other]).plain_text())),
        }
    }
    out
}

/// The concatenated text of `node` and its descendants, with `<br>` as `\n`.
fn text_content(node: NodeRef<'_, Node>) -> String {
    let mut s = String::new();
    collect_text(node, &[], &mut s);
    s
}

/// Appends the text of `node` and its descendants to `out`, skipping the
/// subtrees rooted at the ids in `skip`. Iterative (an edge traversal), so
/// the text fallback for over-deep markup cannot itself overflow the stack.
fn collect_text(node: NodeRef<'_, Node>, skip: &[NodeId], out: &mut String) {
    let mut skipping: Option<NodeId> = None;
    for edge in node.traverse() {
        match edge {
            Edge::Open(_) if skipping.is_some() => {}
            Edge::Open(n) if skip.contains(&n.id()) => skipping = Some(n.id()),
            Edge::Open(n) => match n.value() {
                Node::Text(t) => out.push_str(t),
                Node::Element(el) if el.name() == "br" => out.push('\n'),
                _ => {}
            },
            Edge::Close(n) if skipping == Some(n.id()) => skipping = None,
            Edge::Close(_) => {}
        }
    }
}

/// Whitespace-only text between block elements is layout (pretty-printing),
/// not content. Ruling D13: drop it only when the parent is a block container
/// other than `p`/`li` and at least one sibling is a block element — so
/// `<p> </p>` keeps its space, and so does `<div><span>a</span> <u>b</u></div>`,
/// where the space separates inline content. Only ASCII whitespace counts;
/// `&nbsp;` (U+00A0) is content.
fn is_layout_whitespace(node: NodeRef<'_, Node>) -> bool {
    let Some(text) = node.value().as_text() else {
        return false;
    };
    if !text.chars().all(|c| c.is_ascii_whitespace()) {
        return false;
    }
    let Some(parent) = node.parent() else {
        return false;
    };
    let in_container = parent
        .value()
        .as_element()
        .is_some_and(|e| BLOCK_CONTAINERS.contains(&e.name()));
    in_container
        && parent.children().any(|s| {
            s.value()
                .as_element()
                .is_some_and(|e| BLOCK_TAGS.contains(&e.name()))
        })
}

/// UNVERIFIED(§9.8): the official client's reply markup is assumed (spec
/// §4.9.3); recognition is tolerant so a different layout degrades to a plain
/// quote instead of failing. A `<blockquote>` is a reply when its `itemtype`
/// contains `Reply`. The message id is its `itemid`, else the `itemid` of any
/// `[itemprop=time]` descendant; without one there is nothing to lift and
/// `None` keeps the plain quote. The author is any `[itemprop=mri]` descendant
/// (`itemid` → MRI, text → name), else a leading `<strong>`/`<b>` (name only).
/// The excerpt is the text of any `[itemprop=preview]` descendant, else all
/// remaining text minus the author and time elements.
fn parse_reply(node: NodeRef<'_, Node>, el: &Element) -> Option<ReplyQuote> {
    if !item_type_is(el, "Reply") {
        return None;
    }
    let find = |prop: &str| {
        node.descendants().skip(1).find(|d| {
            d.value()
                .as_element()
                .is_some_and(|e| e.attr("itemprop") == Some(prop))
        })
    };
    let time = find("time");
    let message_id = attr(el, "itemid").or_else(|| time.and_then(|t| element_attr(t, "itemid")))?;
    let author_el = find("mri").or_else(|| {
        node.children()
            .find(|c| c.value().is_element())
            .filter(|c| {
                c.value()
                    .as_element()
                    .is_some_and(|e| matches!(e.name(), "strong" | "b"))
            })
    });
    let author = author_el
        .and_then(|a| element_attr(a, "itemid"))
        .map(|m| Mri(m.to_string()));
    let author_name = author_el.map(text_content).filter(|n| !n.is_empty());
    let excerpt = match find("preview") {
        Some(preview) => text_content(preview),
        None => {
            let skip: Vec<NodeId> = author_el
                .iter()
                .chain(time.iter())
                .map(NodeRef::id)
                .collect();
            let mut s = String::new();
            collect_text(node, &skip, &mut s);
            s
        }
    };
    Some(ReplyQuote {
        message_id: MessageId(message_id.to_string()),
        author,
        author_name,
        excerpt: RichText::from_plain_text(&excerpt),
    })
}

/// An attribute's value, treating an empty value as absent.
fn attr<'a>(el: &'a Element, name: &str) -> Option<&'a str> {
    el.attr(name).filter(|v| !v.is_empty())
}

/// [`attr`] for a node that may or may not be an element.
fn element_attr<'a>(node: NodeRef<'a, Node>, name: &str) -> Option<&'a str> {
    attr(node.value().as_element()?, name)
}

/// `true` when the element's `itemtype` names the given Teams schema kind
/// (`Mention`, `Emoji`, `Reply`), whatever prefix it carries.
fn item_type_is(el: &Element, kind: &str) -> bool {
    attr(el, "itemtype").is_some_and(|t| t.contains(kind))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::format::{Block::*, Inline::*};

    fn t(s: &str) -> Inline {
        Text(s.into())
    }

    #[test]
    fn known_tags() {
        let rt = from_teams_html(
            "<p>hi <b>b</b> <strong>b2</strong> <i>i</i> <em>i2</em> <s>s</s> <strike>s2</strike> <del>s3</del> <code>c</code> <a href=\"https://x.y\">link</a><br>2nd</p><pre>\nlet a;</pre><blockquote><p>q</p></blockquote><ul><li><p>l1</p></li><li>l2</li></ul><ol><li>o1</li></ol>",
        );
        assert_eq!(
            rt.0,
            vec![
                Paragraph(vec![
                    t("hi "),
                    Bold(vec![t("b")]),
                    t(" "),
                    Bold(vec![t("b2")]),
                    t(" "),
                    Italic(vec![t("i")]),
                    t(" "),
                    Italic(vec![t("i2")]),
                    t(" "),
                    Strike(vec![t("s")]),
                    t(" "),
                    Strike(vec![t("s2")]),
                    t(" "),
                    Strike(vec![t("s3")]),
                    t(" "),
                    Code("c".into()),
                    t(" "),
                    Link {
                        url: "https://x.y".into(),
                        text: vec![t("link")]
                    },
                    LineBreak,
                    t("2nd")
                ]),
                CodeBlock("let a;".into()),
                Quote(vec![Paragraph(vec![t("q")])]),
                List {
                    ordered: false,
                    items: vec![
                        vec![Paragraph(vec![t("l1")])],
                        vec![Paragraph(vec![t("l2")])]
                    ]
                },
                List {
                    ordered: true,
                    items: vec![vec![Paragraph(vec![t("o1")])]]
                },
            ]
        );
    }

    #[test]
    fn unknown_tags_degrade_to_text_and_entities_decode() {
        // Review Focus #2
        let rt = from_teams_html(
            "<div><span>a&nbsp;b</span> <u>u</u> <b>x<br/>y</b><font size=\"3\">f</font></div>",
        );
        assert_eq!(
            rt.0,
            vec![Paragraph(vec![
                t("a\u{a0}b u "),
                Bold(vec![t("x"), LineBreak, t("y")]),
                t("f")
            ])]
        );
        assert_eq!(from_teams_html("").0, vec![]);
        assert_eq!(
            from_teams_html("<p></p><p> </p>").0,
            vec![Paragraph(vec![t(" ")])]
        );
    }

    #[test]
    fn teams_specials() {
        let rt = from_teams_html(
            "<p><img itemtype=\"http://schema.skype.com/Emoji\" alt=\"(smile)\" src=\"x\"> <span itemtype=\"http://schema.skype.com/Mention\" itemid=\"8:orgid:abc\">Ada</span> <img src=\"https://i.mg/1.png\"></p>",
        );
        assert_eq!(
            rt.0,
            vec![Paragraph(vec![
                Emoji("(smile)".into()),
                t(" "),
                Mention {
                    mri: crate::Mri("8:orgid:abc".into()),
                    name: "Ada".into()
                },
                t(" "),
                Image {
                    url: "https://i.mg/1.png".into()
                }
            ])]
        );
    }

    #[test]
    fn reply_blockquote_is_lifted() {
        let html = "<blockquote itemscope=\"\" itemtype=\"http://schema.skype.com/Reply\" itemid=\"123\"><strong itemprop=\"mri\" itemid=\"8:orgid:a\">Ada</strong><span itemprop=\"time\" itemid=\"123\"></span><p itemprop=\"preview\">hello</p></blockquote><p>yes</p>";
        let (body, reply) = body_from_wire("RichText/Html", html);
        assert_eq!(body.0, vec![Paragraph(vec![t("yes")])]);
        assert_eq!(
            reply,
            Some(ReplyQuote {
                message_id: crate::MessageId("123".into()),
                author: Some(crate::Mri("8:orgid:a".into())),
                author_name: Some("Ada".into()),
                excerpt: RichText::text("hello")
            })
        );
        // tolerant: different layout still recognised by itemtype
        let (_, reply2) = body_from_wire(
            "RichText/Html",
            "<blockquote itemtype=\"Reply\" itemid=\"9\"><div itemprop=\"preview\">x</div></blockquote>",
        );
        assert_eq!(reply2.unwrap().message_id.0, "9");
        // plain quote without Reply itemtype stays a Quote
        let (b3, r3) = body_from_wire("RichText/Html", "<blockquote><p>q</p></blockquote>");
        assert_eq!(r3, None);
        assert!(matches!(b3.0[0], Quote(_)));
    }

    #[test]
    fn text_bodies_are_plain() {
        let (body, reply) = body_from_wire("Text", "a <b>\nb\n\nc");
        assert_eq!(reply, None);
        assert_eq!(
            body.0,
            vec![
                Paragraph(vec![t("a <b>"), LineBreak, t("b")]),
                Paragraph(vec![t("c")])
            ]
        );
    }

    // Behaviours the brief's table does not pin: the controller's notes and the
    // whitespace rule (ruling D13).
    #[test]
    fn adjacent_breaks_layout_whitespace_and_root_inline_content() {
        // adjacent <br>s survive (a bare `>` quote line produces them)
        assert_eq!(
            from_teams_html("<p>a<br><br>b</p>").0,
            vec![Paragraph(vec![t("a"), LineBreak, LineBreak, t("b")])]
        );
        // pretty-printed whitespace between block elements is layout, not content
        assert_eq!(
            from_teams_html(
                "<p>a</p>\n<blockquote>\n  <p>q</p>\n</blockquote>\n<ul>\n  <li>x</li>\n  <li>y</li>\n</ul>\n"
            )
            .0,
            vec![
                Paragraph(vec![t("a")]),
                Quote(vec![Paragraph(vec![t("q")])]),
                List {
                    ordered: false,
                    items: vec![
                        vec![Paragraph(vec![t("x")])],
                        vec![Paragraph(vec![t("y")])]
                    ]
                },
            ]
        );
        // whitespace between inline content at the root is content
        assert_eq!(
            from_teams_html("<b>x</b> <i>y</i>").0,
            vec![Paragraph(vec![
                Bold(vec![t("x")]),
                t(" "),
                Italic(vec![t("y")])
            ])]
        );
        // `&nbsp;` is never layout, and a non-<li> child of a list is kept as an item
        assert_eq!(
            from_teams_html("<ul>&nbsp;<li>x</li></ul>").0,
            vec![List {
                ordered: false,
                items: vec![
                    vec![Paragraph(vec![t("\u{a0}")])],
                    vec![Paragraph(vec![t("x")])]
                ]
            }]
        );
    }

    // Peer-authored bodies can nest arbitrarily; a stack overflow aborts the
    // process (it is not a panic), so the walk and the tree it builds must be
    // depth-bounded. Calls the unguarded `parse_html` so the walker's own bound
    // is what is tested, not the pre-parse guard. Runs on a 2 MiB stack so the bound is tested regardless
    // of RUST_MIN_STACK.
    #[test]
    fn deep_nesting_is_bounded() {
        let run = |html: String| {
            std::thread::Builder::new()
                .stack_size(2 << 20)
                .spawn(move || parse_html(&html).plain_text())
                .expect("spawn")
                .join()
                .expect("no panic")
        };
        let nested = |tag: &str, n: usize| {
            format!(
                "{}x{}",
                format!("<{tag}>").repeat(n),
                format!("</{tag}>").repeat(n)
            )
        };
        assert_eq!(run(nested("div", 10_000)), "x");
        assert_eq!(run(nested("b", 10_000)), "x");
        // Quotes and lists recurse through their own builders; their plain text
        // prefixes each level, so only the content is checked. Smaller sizes:
        // html5ever's scope scan is quadratic for these tags (~5 s at 10 000 in
        // debug), and the unbounded walk already overflowed this stack at 1 000.
        assert!(run(nested("blockquote", 2_000)).ends_with("x"));
        assert!(run(nested("ul", 2_000)).ends_with("x"));
    }

    // html5ever is quadratic in nesting depth, so a peer-authored body that is
    // too large or too deep must never reach it. Inputs sit just past (and,
    // for contrast, at) each limit; asserting on the output's shape shows which
    // path ran — the degraded one has no formatting nodes.
    #[test]
    fn oversized_body_degrades_to_plain_text() {
        let filler = "a".repeat(MAX_HTML_BYTES);
        let degraded = body_from_wire("RichText/Html", &format!("<p><b>x</b>{filler}&amp;</p>"));
        assert_eq!(
            degraded,
            (RichText::text(&format!("x{filler}&")), None),
            "tags stripped, entities decoded"
        );
        // at the limit html5ever still runs
        let html = format!("<p><b>x</b>{}</p>", &filler[..MAX_HTML_BYTES - 15]);
        assert_eq!(html.len(), MAX_HTML_BYTES);
        let (body, _) = body_from_wire("RichText/Html", &html);
        assert!(matches!(&body.0[0], Paragraph(p) if matches!(p[0], Bold(_))));
    }

    #[test]
    fn overdeep_body_degrades_to_plain_text() {
        let nested = |n: usize| format!("{}x&lt;y{}", "<b>".repeat(n), "</b>".repeat(n));
        assert_eq!(
            body_from_wire("RichText/Html", &nested(MAX_HTML_NESTING + 1)),
            (RichText::text("x<y"), None)
        );
        let (body, _) = body_from_wire("RichText/Html", &nested(MAX_HTML_NESTING));
        assert!(matches!(&body.0[0], Paragraph(p) if matches!(p[0], Bold(_))));
        // block tags separate paragraphs and <br> breaks lines in the fallback;
        // comments, doctypes and void elements contribute nothing
        let deep = format!(
            "<!-- c --><p>a<br>b<img src=\"i\"></p><p>c</p>{}d{}",
            "<div>".repeat(MAX_HTML_NESTING + 1),
            "</div>".repeat(MAX_HTML_NESTING + 1)
        );
        assert_eq!(
            body_from_wire("RichText/Html", &deep).0,
            RichText::from_plain_text("a\nb\n\nc\n\nd")
        );
    }

    /// The guard sits in the public `from_teams_html`, so library consumers
    /// calling it directly on untrusted input are covered too.
    #[test]
    fn from_teams_html_is_guarded_directly() {
        let deep = format!(
            "{}x&amp;{}",
            "<i>".repeat(MAX_HTML_NESTING + 1),
            "</i>".repeat(MAX_HTML_NESTING + 1)
        );
        assert_eq!(from_teams_html(&deep), RichText::text("x&"));
        let big = format!("<p><b>y</b>{}</p>", "z".repeat(MAX_HTML_BYTES));
        assert_eq!(
            from_teams_html(&big),
            RichText::text(&format!("y{}", "z".repeat(MAX_HTML_BYTES)))
        );
        // under both limits html5ever still runs
        assert!(matches!(
            &from_teams_html("<p><b>y</b></p>").0[0],
            Paragraph(p) if matches!(p[0], Bold(_))
        ));
    }

    /// Review fix round 1: inputs on which a naive scan fabricates end tags
    /// html5ever never tokenizes (inside a quoted attribute value, inside a
    /// comment, swallowed by a bogus `</3 …>` comment), so it under-counts the
    /// real nesting. Each must trip the guard and degrade.
    #[test]
    fn scanner_does_not_fabricate_end_tags() {
        for unit in [
            "<div><div title=\"x></div></div>\">",
            "<div><div><!-- ></div></div> -->",
            "<div></3 </div>",
        ] {
            let html = unit.repeat(2000);
            assert!(exceeds_nesting(&html, MAX_HTML_NESTING), "{unit}");
            assert!(needs_fallback(&html), "{unit}");
            assert_eq!(
                from_teams_html(&html),
                RichText::from_plain_text(&strip_tags(&html))
            );
        }
        // html5ever's comment and attribute endings, which the scan mirrors
        let opens = |html: &str| pieces(html).filter(|p| matches!(p, Piece::Open(_))).count();
        assert_eq!(opens("<a href='x>y<b>'><i x=1>"), 2);
        assert_eq!(opens("<!--><b><!---><i><!-- --!><u> -- --><s>"), 4);
        assert_eq!(opens("<!-- <b> --!> <i>"), 1);
        assert_eq!(opens("</><b></ x <i>><u>"), 2);
        // a tag cut off by the end of input is dropped, as html5ever drops it
        assert_eq!(opens("<b>x<i title=\"y"), 1);
    }

    /// Inside CDATA (foreign content) and raw-text elements html5ever
    /// tokenizes no end tags, so the scan ignores them there; otherwise a
    /// hidden `</b>` would let html5ever's formatting list outgrow the scan.
    #[test]
    fn end_tags_in_cdata_and_raw_text_are_ignored() {
        let cdata: String = (0..65)
            .map(|i| format!("<b a={i}><svg><![CDATA[x></svg></b>]]>"))
            .collect();
        assert!(exceeds_nesting(&cdata, MAX_HTML_NESTING));
        let raw: String = (0..65)
            .map(|i| format!("<b a={i}><textarea><i></i></b></TextArea>"))
            .collect();
        assert!(exceeds_nesting(&raw, MAX_HTML_NESTING));
        // the closing tag of the raw-text element itself still closes it
        assert!(!exceeds_nesting(
            &"<p><style>a</b>{}</style></p>".repeat(100),
            2
        ));
    }

    /// The sound belt: whatever the scan's view of nesting, html5ever cannot
    /// create more elements than there are start tags in the input.
    #[test]
    fn start_tag_count_caps_html5ever_work() {
        let flat = |n: usize| "<b>x</b>".repeat(n);
        assert!(!exceeds_nesting(
            &flat(MAX_HTML_START_TAGS + 1),
            MAX_HTML_NESTING
        ));
        assert!(needs_fallback(&flat(MAX_HTML_START_TAGS + 1)));
        assert!(!needs_fallback(&flat(MAX_HTML_START_TAGS)));
        // `<` + letter counts wherever it is, even inside an attribute value
        assert!(needs_fallback(&format!(
            "<p title=\"{}\">x</p>",
            "<b".repeat(MAX_HTML_START_TAGS)
        )));
    }

    /// Review fix round 2: formatting-element reconstruction. `B` distinct
    /// `<b a=i>` (distinct attributes defeat html5ever's Noah's-Ark dedup)
    /// closed by `</div>` leave `B` entries in html5ever's active formatting
    /// list; each later `<div>x</div>` re-opens (clones) all of them, so the
    /// tree grows as `B × C`. The scan sees the `b`s as never closed, so `B`
    /// counts toward its nesting and caps the multiplier.
    fn reconstruction_bomb(b: usize, c: usize) -> String {
        let opens: String = (0..b).map(|i| format!("<b a={i}>")).collect();
        format!("<div>{opens}</div>{}", "<div>x</div>".repeat(c))
    }

    #[test]
    fn formatting_reconstruction_counts_toward_nesting() {
        // div + 65 b's = 66 open > 64
        let bomb = reconstruction_bomb(65, 20);
        assert!(exceeds_nesting(&bomb, MAX_HTML_NESTING));
        assert!(needs_fallback(&bomb));
        assert_eq!(
            from_teams_html(&bomb),
            RichText::from_plain_text(&strip_tags(&bomb))
        );
        // div + 60 b's + a cycling div = 62 open: under the cap, parsed
        // normally (and the reconstruction really happens: every `x` comes
        // back bold)
        let small = reconstruction_bomb(60, 20);
        assert!(!needs_fallback(&small));
        let rt = from_teams_html(&small);
        assert_eq!(rt.0.len(), 20);
        assert!(
            rt.0.iter()
                .all(|b| matches!(b, Paragraph(p) if matches!(p[..], [Bold(_)])))
        );
    }

    /// A long but ordinary message, a few hundred tags, is parsed normally.
    #[test]
    fn realistic_message_is_not_degraded() {
        let para = "<p>Hello <b>team</b>, see <a href=\"https://x.example/a?b=1&amp;c=2\">this</a> \
                    and <span itemtype=\"http://schema.skype.com/Mention\" itemid=\"8:orgid:a\">Ada</span>\
                    <img itemtype=\"http://schema.skype.com/Emoji\" alt=\"(smile)\" src=\"e\"><br>ok</p>\
                    <ul><li><i>one</i></li><li><code>two</code></li></ul><!-- c -->";
        let html = para.repeat(40);
        assert!(html.matches('<').count() > 400);
        assert!(!needs_fallback(&html));
        let rt = from_teams_html(&html);
        assert!(matches!(&rt.0[0], Paragraph(p) if matches!(p[1], Bold(_))));
    }

    #[test]
    fn nesting_scan_counts_only_cleanly_closed_tags_as_closed() {
        // well-nested markup and void elements do not accumulate
        let flat = "<p><b>x</b><br><img src=\"a\"><BR/></p>".repeat(1000);
        assert!(!exceeds_nesting(&flat, MAX_HTML_NESTING));
        // a closer that does not match the innermost open tag is ignored, so
        // misnested or unclosed markup over-counts rather than under-counts
        assert!(exceeds_nesting(
            &"<li><ul></li>".repeat(300),
            MAX_HTML_NESTING
        ));
        assert!(exceeds_nesting(
            &"<div></span>".repeat(513),
            MAX_HTML_NESTING
        ));
        // comments and attribute values end where html5ever ends them
        assert!(exceeds_nesting(
            &"<!--><div title=\"a>b\">".repeat(513),
            MAX_HTML_NESTING
        ));
        // tag names match case-insensitively
        assert!(!exceeds_nesting(&"<DIV></div>".repeat(600), 1));
    }

    #[test]
    fn blocks_inside_inline_tags_and_comments_degrade() {
        assert_eq!(
            from_teams_html("<b>x<p>y</p></b><!-- c --><pre><code>z</code></pre>").0,
            vec![
                Paragraph(vec![Bold(vec![t("x"), LineBreak, t("y")])]),
                CodeBlock("z".into()),
            ]
        );
    }

    #[test]
    fn reply_fallbacks_degrade_rather_than_fail() {
        use crate::format::to_teams_html;
        // no message id anywhere: a plain quote, never an error
        let (b, r) = body_from_wire(
            "RichText/Html",
            "<blockquote itemtype=\"http://schema.skype.com/Reply\"><p>x</p></blockquote>",
        );
        assert_eq!(r, None);
        assert_eq!(b.0, vec![Quote(vec![Paragraph(vec![t("x")])])]);
        // id from [itemprop=time]; name from a leading <strong>; excerpt = the rest
        let (b, r) = body_from_wire(
            "RichText/Html",
            "<blockquote itemtype=\"Reply\"><strong>Ada</strong><span itemprop=\"time\" itemid=\"5\"></span>hi<br>there</blockquote>",
        );
        assert!(b.is_empty());
        assert_eq!(
            r,
            Some(ReplyQuote {
                message_id: crate::MessageId("5".into()),
                author: None,
                author_name: Some("Ada".into()),
                excerpt: RichText::from_plain_text("hi\nthere"),
            })
        );
        // our own markup parses back for every author shape
        let mri = || Some(crate::Mri("8:orgid:a".into()));
        let name = || Some("Ada".to_string());
        for (author, author_name) in [(mri(), name()), (mri(), None), (None, name()), (None, None)]
        {
            let q = ReplyQuote {
                message_id: crate::MessageId("7".into()),
                author,
                author_name,
                excerpt: RichText::text("x"),
            };
            let html = to_teams_html(&RichText(vec![Reply(q.clone()), Paragraph(vec![t("yes")])]));
            assert_eq!(
                body_from_wire("RichText/Html", &html),
                (RichText::text("yes"), Some(q))
            );
        }
        // a reply that is not the first block stays in the body
        let (b, r) = body_from_wire(
            "RichText/Html",
            "<p>a</p><blockquote itemtype=\"Reply\" itemid=\"1\"><p itemprop=\"preview\">x</p></blockquote>",
        );
        assert_eq!(r, None);
        assert!(matches!(b.0[1], Reply(_)));
    }
}
