//! IRC text → [`RichText`] (see [`super::from_irc`]).
//!
//! IRC formatting codes are decoded first into a style per character (state
//! carries across lines, as clients render a multi-line message). Blocks are
//! then found line by line on the plain text: fences, `> ` quotes and list
//! items, whose content is parsed recursively, and paragraphs. Within a line,
//! [`Parser::tokenize`] finds code spans, URLs, mentions and emphasis markers,
//! [`resolve`] pairs the markers into spans, and [`nest`] folds the IRC
//! styles and the spans into one tree.

use ic3_teams::{Block, Inline, Mri, RichText};

use super::{BOLD, ITALIC, RenderCtx, STRIKE};

/// `\x11` monospace, which becomes inline code.
const MONO: u8 = 8;
/// The styles that become emphasis spans.
const EMPHASIS: u8 = BOLD | ITALIC | STRIKE;
/// Characters that may precede an opening marker.
const OPEN_PUNCT: &[char] = &['(', '[', '{', '"', '\''];
/// Characters that may follow a closing marker.
const CLOSE_PUNCT: &[char] = &['.', ',', ';', ':', '!', '?', ')', ']', '}', '"', '\''];
/// Schemes that start a bare URL.
const URL_SCHEMES: &[&str] = &["http://", "https://"];
/// Characters that end a bare URL (besides whitespace and controls).
const URL_STOP: &[char] = &['<', '>', '"', '\'', '`'];
/// Punctuation stripped from the end of a bare URL.
const URL_TRAILING: &[char] = &['.', ',', ';', ':', '!', '?', ']', '}', '\'', '"', '*'];

/// A character of the message and the IRC styles it was sent with.
#[derive(Clone, Copy, Debug)]
struct SChar {
    c: char,
    style: u8,
}

/// One line of the message.
#[derive(Clone, Debug)]
struct Line {
    chars: Vec<SChar>,
    /// Whether this is the first non-blank line of the message, where
    /// `nick: ` addresses someone.
    lead: bool,
}

impl Line {
    fn text(&self) -> String {
        self.chars.iter().map(|s| s.c).collect()
    }

    fn indent(&self) -> usize {
        self.chars.iter().take_while(|s| s.c == ' ').count()
    }

    fn tail(&self, from: usize) -> Line {
        Line {
            chars: self.chars[from.min(self.chars.len())..].to_vec(),
            lead: false,
        }
    }

    fn is_blank(&self) -> bool {
        self.chars.iter().all(|s| s.c.is_whitespace())
    }
}

pub(super) fn parse(text: &str, ctx: &dyn RenderCtx) -> RichText {
    let chars = decode(text);
    let mut lines: Vec<Line> = chars
        .split(|s| s.c == '\n')
        .map(|cs| Line {
            chars: cs.to_vec(),
            lead: false,
        })
        .collect();
    if let Some(first) = lines.iter_mut().find(|l| !l.is_blank()) {
        first.lead = true;
    }
    RichText(Parser { ctx }.blocks(&lines)).normalize()
}

/// Strips IRC formatting codes, recording the styles they set on each
/// character. Colours, underline and reverse are dropped; other control
/// characters (`\r`, NUL, …) are removed.
fn decode(text: &str) -> Vec<SChar> {
    let cs: Vec<char> = text.chars().collect();
    let mut out = Vec::with_capacity(cs.len());
    let mut style = 0;
    let mut i = 0;
    while i < cs.len() {
        let c = cs[i];
        i += 1;
        match c {
            '\x02' => style ^= BOLD,
            '\x1d' => style ^= ITALIC,
            '\x1e' => style ^= STRIKE,
            '\x11' => style ^= MONO,
            '\x0f' => style = 0,
            // `\x03` then an optional `fg[,bg]` of one or two digits each.
            '\x03' => {
                let fg = count(&cs, i, 2, |c| c.is_ascii_digit());
                if fg > 0 {
                    i += fg;
                    if cs.get(i) == Some(&',') {
                        let bg = count(&cs, i + 1, 2, |c| c.is_ascii_digit());
                        if bg > 0 {
                            i += 1 + bg;
                        }
                    }
                }
            }
            // `\x04` then an optional `RRGGBB[,RRGGBB]`.
            '\x04' => {
                if count(&cs, i, 6, |c| c.is_ascii_hexdigit()) == 6 {
                    i += 6;
                    if cs.get(i) == Some(&',')
                        && count(&cs, i + 1, 6, |c| c.is_ascii_hexdigit()) == 6
                    {
                        i += 7;
                    }
                }
            }
            '\n' | '\t' => out.push(SChar { c, style }),
            c if c.is_control() => {}
            c => out.push(SChar { c, style }),
        }
    }
    out
}

/// How many of the (at most `max`) characters from `cs[i]` satisfy `f`.
fn count(cs: &[char], i: usize, max: usize, f: impl Fn(&char) -> bool) -> usize {
    cs.iter().skip(i).take(max).take_while(|c| f(c)).count()
}

/// The length of the run of `c` starting at `cs[i]`.
fn run_len(cs: &[char], i: usize, c: char) -> usize {
    cs[i..].iter().take_while(|&&x| x == c).count()
}

/// A character that may appear in an IRC nick.
fn is_nick_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || "_-[]\\^{}|`".contains(c)
}

struct Parser<'a> {
    ctx: &'a dyn RenderCtx,
}

impl Parser<'_> {
    /// Splits lines into blocks: fences, quotes, lists and paragraphs (runs
    /// of other lines, ended by a blank line or a block).
    fn blocks(&self, lines: &[Line]) -> Vec<Block> {
        let mut out = Vec::new();
        let mut para: Vec<Inline> = Vec::new();
        let mut i = 0;
        while i < lines.len() {
            let line = &lines[i];
            let text = line.text();
            if line.is_blank() {
                flush(&mut para, &mut out);
                i += 1;
                continue;
            }
            if let Some((code, next)) = fence(lines, i) {
                flush(&mut para, &mut out);
                out.push(Block::CodeBlock(code));
                i = next;
                continue;
            }
            if is_quote(&text) {
                flush(&mut para, &mut out);
                let mut inner = Vec::new();
                while let Some(l) = lines.get(i).filter(|l| is_quote(&l.text())) {
                    inner.push(l.tail(2));
                    i += 1;
                }
                out.push(Block::Quote(self.blocks(&inner)));
                continue;
            }
            if list_item(&text).is_some() {
                flush(&mut para, &mut out);
                let (lists, next) = self.lists(lines, i);
                out.extend(lists);
                i = next;
                continue;
            }
            if !para.is_empty() {
                para.push(Inline::LineBreak);
            }
            para.extend(self.inline(line));
            i += 1;
        }
        flush(&mut para, &mut out);
        out
    }

    /// The lists starting at `lines[i]`, and the index of the first line
    /// after them. An item runs on over the following non-blank lines
    /// indented further than its marker; those lines, dedented, are parsed
    /// as the item's own blocks, so nested lists, quotes and code work.
    /// A change between numbered and bulleted items starts a new list.
    fn lists(&self, lines: &[Line], i: usize) -> (Vec<Block>, usize) {
        let mut out = Vec::new();
        let mut cur: Option<(bool, Vec<Vec<Block>>)> = None;
        let mut j = i;
        while let Some((indent, ordered, start)) = lines.get(j).and_then(|l| list_item(&l.text())) {
            let mut k = j + 1;
            while lines
                .get(k)
                .is_some_and(|l| !l.is_blank() && l.indent() > indent)
            {
                k += 1;
            }
            let more = &lines[j + 1..k];
            let dedent = more.iter().map(Line::indent).min().unwrap_or(0);
            let mut body = vec![lines[j].tail(start)];
            body.extend(more.iter().map(|l| l.tail(dedent)));
            let item = self.blocks(&body);
            match &mut cur {
                Some((o, items)) if *o == ordered => items.push(item),
                _ => {
                    if let Some((ordered, items)) = cur.take() {
                        out.push(Block::List { ordered, items });
                    }
                    cur = Some((ordered, vec![item]));
                }
            }
            j = k;
        }
        if let Some((ordered, items)) = cur {
            out.push(Block::List { ordered, items });
        }
        (out, j)
    }

    /// Parses one line's inline content.
    fn inline(&self, line: &Line) -> Vec<Inline> {
        let mut out = Vec::new();
        nest(&resolve(self.tokenize(line)), 0, &mut out);
        out
    }

    /// First phase: IRC monospace and code spans (verbatim), bare URLs,
    /// mentions, and emphasis marker candidates.
    fn tokenize(&self, line: &Line) -> Vec<Tok> {
        let sc = &line.chars;
        let cs: Vec<char> = sc.iter().map(|s| s.c).collect();
        let mut toks = Vec::new();
        let mut i = 0;
        if line.lead
            && let Some((len, mri, name)) = self.leading_address(&cs)
        {
            toks.push(Tok::Atom(Atom::Mention(mri, name), sc[0].style & EMPHASIS));
            i = len;
        }
        while i < cs.len() {
            let c = cs[i];
            let style = sc[i].style;

            // `\x11` monospace is code, whatever is in it.
            if style & MONO != 0 {
                let end = (i..cs.len())
                    .find(|&j| sc[j].style & MONO == 0)
                    .unwrap_or(cs.len());
                toks.push(Tok::Atom(
                    Atom::Code(cs[i..end].iter().collect()),
                    style & EMPHASIS,
                ));
                i = end;
                continue;
            }

            // A run of backticks opens code up to the next run of the same
            // length; an unmatched run is literal.
            if c == '`' {
                let run = run_len(&cs, i, '`');
                match closing_ticks(&cs, i + run, run) {
                    Some(end) => {
                        toks.push(Tok::Atom(
                            Atom::Code(code_content(&cs[i + run..end])),
                            style & EMPHASIS,
                        ));
                        i = end + run;
                    }
                    None => {
                        toks.extend((0..run).map(|_| Tok::Char('`', style)));
                        i += run;
                    }
                }
                continue;
            }

            let after_opener = matches!(toks.last(), Some(Tok::Marker { open: true, .. }));

            if let Some(len) = url_at(&cs, i, after_opener) {
                toks.push(Tok::Atom(
                    Atom::Url(cs[i..i + len].iter().collect()),
                    style & EMPHASIS,
                ));
                i += len;
                continue;
            }

            // `@` starts a mention after a non-nick character or an opener.
            if c == '@'
                && (i == 0 || !is_nick_char(cs[i - 1]) || after_opener)
                && let Some((len, mri, name)) = self.mention_at(&cs, i + 1)
            {
                toks.push(Tok::Atom(Atom::Mention(mri, name), style & EMPHASIS));
                i += 1 + len;
                continue;
            }

            if matches!(c, '*' | '_' | '~') {
                let run = run_len(&cs, i, c);
                // `*`, `**`, `***`, a lone `_` and `~~`; other runs (`__init__`,
                // `****`, `~`) are literal.
                let kind = match (c, run) {
                    ('*', 1..=3) => Some(Kind::Star(run)),
                    ('_', 1) => Some(Kind::Under),
                    ('~', 2) => Some(Kind::Tilde),
                    _ => None,
                };
                if let Some(kind) = kind {
                    let prev = i.checked_sub(1).map(|p| cs[p]);
                    let next = cs.get(i + run).copied();
                    let open = (prev.is_none_or(|p| p.is_whitespace() || OPEN_PUNCT.contains(&p))
                        || after_opener)
                        && next.is_some_and(|n| !n.is_whitespace());
                    let close = prev.is_some_and(|p| !p.is_whitespace())
                        && next.is_none_or(|n| {
                            n.is_whitespace()
                                || CLOSE_PUNCT.contains(&n)
                                || matches!(n, '*' | '_' | '~' | '`')
                        });
                    if open || close {
                        toks.push(Tok::Marker {
                            kind,
                            style,
                            open,
                            close,
                        });
                        i += run;
                        continue;
                    }
                }
                toks.extend((0..run).map(|_| Tok::Char(c, style)));
                i += run;
                continue;
            }

            toks.push(Tok::Char(c, style));
            i += 1;
        }
        toks
    }

    /// A known nick starting at `cs[from]`: its length, MRI and display name.
    /// Trailing nick characters that are not alphanumeric (`bob_`, `bob-`)
    /// are given up one at a time until the nick resolves.
    fn mention_at(&self, cs: &[char], from: usize) -> Option<(usize, Mri, String)> {
        let mut len = cs[from..].iter().take_while(|&&c| is_nick_char(c)).count();
        while len > 0 {
            let nick: String = cs[from..from + len].iter().collect();
            if let Some((mri, name)) = self.ctx.resolve_nick(&nick) {
                return Some((len, mri, name));
            }
            if cs[from + len - 1].is_ascii_alphanumeric() {
                break;
            }
            len -= 1;
        }
        None
    }

    /// `nick: ` or `nick, ` at the start of the message, for a known nick:
    /// the length of the nick and its `:`/`,`, and who it names.
    fn leading_address(&self, cs: &[char]) -> Option<(usize, Mri, String)> {
        let len = cs.iter().take_while(|&&c| is_nick_char(c)).count();
        if len == 0
            || !matches!(cs.get(len), Some(':' | ','))
            || cs.get(len + 1).is_some_and(|&c| c != ' ')
        {
            return None;
        }
        let nick: String = cs[..len].iter().collect();
        let (mri, name) = self.ctx.resolve_nick(&nick)?;
        Some((len + 1, mri, name))
    }
}

fn flush(para: &mut Vec<Inline>, out: &mut Vec<Block>) {
    if !para.is_empty() {
        out.push(Block::Paragraph(std::mem::take(para)));
    }
}

/// A fenced code block starting at `lines[i]`: its content, and the index of
/// the line after the closing fence.
///
/// The opening line is exactly three backticks (longer runs are code spans)
/// with an optional language tag, or
/// ```` ``` ```` followed by code (closed on the same line, as in
/// ```` ```x``` ````, or a later one). The block closes at the next line
/// ending in ```` ``` ````. An unclosed fence is not a block.
fn fence(lines: &[Line], i: usize) -> Option<(String, usize)> {
    let first = lines[i].text();
    let rest = first.strip_prefix("```").filter(|r| !r.starts_with('`'))?;
    let tag = rest.trim_end();
    let mut body = Vec::new();
    if !is_lang_tag(tag) {
        if let Some(code) = tag.strip_suffix("```") {
            return Some((code.to_string(), i + 1));
        }
        if rest.contains("```") {
            // Leave ```a```b to the code-span scan.
            return None;
        }
        body.push(rest.to_string());
    }
    for (j, line) in lines.iter().enumerate().skip(i + 1) {
        let text = line.text();
        if let Some(before) = text.trim_end().strip_suffix("```") {
            if !before.is_empty() {
                body.push(before.to_string());
            }
            // A lone word on the opening line is the code itself (as
            // Markdown-aware clients read it), not a language tag.
            if body.is_empty() && !tag.is_empty() {
                body.push(tag.to_string());
            }
            return Some((body.join("\n"), j + 1));
        }
        body.push(text);
    }
    None
}

/// A fence's language tag (`rust`, `c++`, `c#`, `objective-c`), or nothing.
fn is_lang_tag(s: &str) -> bool {
    s.chars()
        .all(|c| c.is_alphanumeric() || matches!(c, '_' | '+' | '#' | '.' | '-'))
}

/// `> text`, or a bare `>`.
fn is_quote(line: &str) -> bool {
    line == ">" || line.starts_with("> ")
}

/// A list item's indent, kind (`true` for numbered) and the character index
/// where its content starts: `- `, `* `, `+ `, `1. ` or `1) ` after any
/// indent, then non-blank content.
fn list_item(line: &str) -> Option<(usize, bool, usize)> {
    let cs: Vec<char> = line.chars().collect();
    let indent = run_len(&cs, 0, ' ');
    let mut k = indent;
    let ordered = if matches!(cs.get(k), Some('-' | '*' | '+')) {
        k += 1;
        false
    } else {
        let digits = cs[k..].iter().take_while(|c| c.is_ascii_digit()).count();
        if !(1..=9).contains(&digits) || !matches!(cs.get(k + digits), Some('.' | ')')) {
            return None;
        }
        k += digits + 1;
        true
    };
    let spaces = run_len(&cs, k, ' ');
    (spaces > 0 && k + spaces < cs.len()).then_some((indent, ordered, k + spaces))
}

/// The start of the next run of exactly `run` backticks at or after `from`.
fn closing_ticks(cs: &[char], from: usize, run: usize) -> Option<usize> {
    let mut j = from;
    while j < cs.len() {
        if cs[j] == '`' {
            let r = run_len(cs, j, '`');
            if r == run {
                return Some(j);
            }
            j += r;
        } else {
            j += 1;
        }
    }
    None
}

/// A code span's content: verbatim, except that one space each side is
/// stripped when both are there and the content is not all spaces (as
/// CommonMark does, so `` `` `x` `` `` can hold backticks at its edges).
fn code_content(cs: &[char]) -> String {
    let s: String = cs.iter().collect();
    if s.len() >= 2 && s.starts_with(' ') && s.ends_with(' ') && !s.trim().is_empty() {
        s[1..s.len() - 1].to_string()
    } else {
        s
    }
}

/// The length of the bare URL starting at `cs[i]`, if there is one: an
/// `http://` or `https://` scheme not inside a word (an opening marker just
/// before it does not count), then at least one more character, minus
/// trailing punctuation and an unbalanced trailing `)`.
fn url_at(cs: &[char], i: usize, after_opener: bool) -> Option<usize> {
    if i > 0 && !after_opener && (cs[i - 1].is_alphanumeric() || cs[i - 1] == '_') {
        return None;
    }
    let scheme = URL_SCHEMES.iter().find(|s| {
        s.chars()
            .enumerate()
            .all(|(k, p)| cs.get(i + k).is_some_and(|c| c.eq_ignore_ascii_case(&p)))
    })?;
    let mut end = i;
    while end < cs.len()
        && !cs[end].is_whitespace()
        && !cs[end].is_control()
        && !URL_STOP.contains(&cs[end])
    {
        end += 1;
    }
    loop {
        let before = end;
        while end > i && URL_TRAILING.contains(&cs[end - 1]) {
            end -= 1;
        }
        let url = &cs[i..end];
        let opens = url.iter().filter(|&&c| c == '(').count();
        let closes = url.iter().filter(|&&c| c == ')').count();
        if url.last() == Some(&')') && opens < closes {
            end -= 1;
        }
        if end == before {
            break;
        }
    }
    (end - i > scheme.len()).then_some(end - i)
}

/// An emphasis marker.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Kind {
    /// A run of one to three `*`: italic, bold, or both.
    Star(usize),
    /// `_`: italic.
    Under,
    /// `~~`: strikethrough.
    Tilde,
}

impl Kind {
    fn mask(self) -> u8 {
        match self {
            Kind::Star(1) | Kind::Under => ITALIC,
            Kind::Star(2) => BOLD,
            Kind::Star(_) => BOLD | ITALIC,
            Kind::Tilde => STRIKE,
        }
    }

    fn raw(self) -> &'static str {
        match self {
            Kind::Star(n) => &"***"[..n],
            Kind::Under => "_",
            Kind::Tilde => "~~",
        }
    }
}

/// Content that emphasis markers cannot reach into.
#[derive(Clone, Debug)]
enum Atom {
    Code(String),
    Url(String),
    Mention(Mri, String),
}

/// Output of [`Parser::tokenize`]; each carries its IRC styles.
#[derive(Debug)]
enum Tok {
    Char(char, u8),
    Atom(Atom, u8),
    Marker {
        kind: Kind,
        style: u8,
        open: bool,
        close: bool,
    },
}

/// A node of the line before IRC styles are folded in: a styled character
/// or atom, or a Markdown span with the styles it sets.
#[derive(Debug)]
enum Item {
    Char(char, u8),
    Atom(Atom, u8),
    Span(u8, Vec<Item>),
}

/// An open span: its marker (`None` for the line itself), the marker's IRC
/// style, the index of its opening token, and its content so far.
#[derive(Debug)]
struct Frame {
    kind: Option<Kind>,
    style: u8,
    start: usize,
    children: Vec<Item>,
}

fn top(stack: &mut [Frame]) -> &mut Frame {
    stack.last_mut().expect("stack holds the root frame")
}

fn literal(kind: Kind, style: u8) -> impl Iterator<Item = Item> {
    kind.raw().chars().map(move |c| Item::Char(c, style))
}

/// Second phase: pairs markers into spans with a stack. A closer closes the
/// span on top of the stack when its marker matches (and the span is not
/// empty); otherwise a marker opens a span unless one of its kind is open;
/// otherwise, and for openers still open at the end, it is literal.
fn resolve(toks: Vec<Tok>) -> Vec<Item> {
    let mut stack = vec![Frame {
        kind: None,
        style: 0,
        start: 0,
        children: Vec::new(),
    }];
    for (i, tok) in toks.into_iter().enumerate() {
        match tok {
            Tok::Char(c, s) => top(&mut stack).children.push(Item::Char(c, s)),
            Tok::Atom(a, s) => top(&mut stack).children.push(Item::Atom(a, s)),
            Tok::Marker {
                kind,
                style,
                open,
                close,
            } => {
                if close && close_span(&mut stack, kind, i) {
                    continue;
                }
                if open && !stack.iter().any(|f| f.kind == Some(kind)) {
                    stack.push(Frame {
                        kind: Some(kind),
                        style,
                        start: i,
                        children: Vec::new(),
                    });
                } else {
                    top(&mut stack).children.extend(literal(kind, style));
                }
            }
        }
    }
    while stack.len() > 1 {
        let frame = stack.pop().expect("stack holds the root frame");
        let parent = top(&mut stack);
        if let Some(kind) = frame.kind {
            parent.children.extend(literal(kind, frame.style));
        }
        parent.children.extend(frame.children);
    }
    stack.pop().expect("stack holds the root frame").children
}

/// Closes a span with a `kind` closer at token `i`, if one matches. Star
/// runs may split (`***a* b**`) or combine (`**a *b***`) with the span on
/// top; otherwise the nearest open span of the same kind closes, and any
/// spans opened inside it become literal. An empty span never closes.
fn close_span(stack: &mut Vec<Frame>, kind: Kind, i: usize) -> bool {
    let Some(&Frame {
        kind: open, start, ..
    }) = stack.last()
    else {
        return false;
    };
    let empty = start + 1 == i;
    match (open, kind) {
        // `***` closed by `*` or `**`: the rest stays open.
        (Some(Kind::Star(3)), Kind::Star(n)) if n < 3 && !empty => {
            let frame = stack.pop().expect("checked above");
            stack.push(Frame {
                kind: Some(Kind::Star(3 - n)),
                children: vec![Item::Span(kind.mask(), frame.children)],
                ..frame
            });
            return true;
        }
        // `***` closing a `*` inside `**` (or the reverse) closes both.
        (Some(Kind::Star(n)), Kind::Star(3))
            if n < 3
                && !empty
                && stack.len() > 2
                && stack[stack.len() - 2].kind == Some(Kind::Star(3 - n)) =>
        {
            let inner = stack.pop().expect("checked above");
            let outer = stack.pop().expect("checked above");
            let mut children = outer.children;
            children.push(Item::Span(Kind::Star(n).mask(), inner.children));
            top(stack)
                .children
                .push(Item::Span(Kind::Star(3 - n).mask(), children));
            return true;
        }
        _ => {}
    }
    let Some(f) = stack.iter().rposition(|fr| fr.kind == Some(kind)) else {
        return false;
    };
    if f + 1 == stack.len() && empty {
        return false;
    }
    while stack.len() > f + 1 {
        let frame = stack.pop().expect("checked above");
        let parent = top(stack);
        if let Some(k) = frame.kind {
            parent.children.extend(literal(k, frame.style));
        }
        parent.children.extend(frame.children);
    }
    let frame = stack.pop().expect("checked above");
    top(stack)
        .children
        .push(Item::Span(kind.mask(), frame.children));
    true
}

/// The IRC styles an item has throughout.
fn item_style(item: &Item) -> u8 {
    match item {
        Item::Char(_, s) | Item::Atom(_, s) => s & EMPHASIS,
        Item::Span(_, children) => children.iter().fold(EMPHASIS, |m, c| m & item_style(c)),
    }
}

fn wrap(style: u8, children: Vec<Inline>) -> Inline {
    match style {
        BOLD => Inline::Bold(children),
        ITALIC => Inline::Italic(children),
        _ => Inline::Strike(children),
    }
}

/// Third phase: builds inlines from items, wrapping runs of items that share
/// an IRC style in that style's span (the style with the longest run
/// outermost) and Markdown spans in theirs. `active` holds the styles
/// already applied by enclosing spans, which are not applied again.
fn nest(items: &[Item], active: u8, out: &mut Vec<Inline>) {
    let mut i = 0;
    while i < items.len() {
        let missing = item_style(&items[i]) & !active;
        if missing != 0 {
            let mut best = (0, i);
            for bit in [BOLD, ITALIC, STRIKE] {
                if missing & bit == 0 {
                    continue;
                }
                let end = (i..items.len())
                    .find(|&j| item_style(&items[j]) & bit == 0)
                    .unwrap_or(items.len());
                if end > best.1 {
                    best = (bit, end);
                }
            }
            let (bit, end) = best;
            let mut inner = Vec::new();
            nest(&items[i..end], active | bit, &mut inner);
            out.push(wrap(bit, inner));
            i = end;
            continue;
        }
        match &items[i] {
            Item::Char(c, _) => out.push(Inline::Text(c.to_string())),
            Item::Atom(atom, _) => out.push(match atom.clone() {
                Atom::Code(s) => Inline::Code(s),
                Atom::Url(url) => Inline::Link { url, text: vec![] },
                Atom::Mention(mri, name) => Inline::Mention { mri, name },
            }),
            Item::Span(style, children) => {
                let mut inner = Vec::new();
                nest(children, active | style, &mut inner);
                for bit in [STRIKE, ITALIC, BOLD] {
                    if style & !active & bit != 0 {
                        inner = vec![wrap(bit, inner)];
                    }
                }
                out.extend(inner);
            }
        }
        i += 1;
    }
}
