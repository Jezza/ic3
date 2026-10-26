//! [`RichText`] → IRC text (see [`super::to_irc`]).
//!
//! Each block renders to a list of lines; quotes and lists prefix the lines
//! of their children. Inline content goes through a [`Writer`], which emits
//! formatting codes lazily (only just before visible text) and closes them
//! at the end of every line.

use ic3_teams::{Block, Inline, RichText};

use super::{BOLD, CODES, ITALIC, RenderCtx, STRIKE};

/// Characters that may follow a bare URL without a space: clients strip them
/// from the end of a URL rather than linking them.
const URL_FOLLOWERS: &[char] = &['.', ',', ';', ':', '!', '?', ')', ']', '}', '"', '\''];

pub(super) fn render(body: &RichText, ctx: &dyn RenderCtx) -> String {
    blocks(&body.0, ctx).join("\n")
}

fn blocks(bs: &[Block], ctx: &dyn RenderCtx) -> Vec<String> {
    bs.iter().flat_map(|b| block(b, ctx)).collect()
}

fn block(b: &Block, ctx: &dyn RenderCtx) -> Vec<String> {
    match b {
        Block::Paragraph(inl) => {
            let mut w = Writer::new(ctx);
            w.inlines(inl);
            w.finish()
        }
        Block::CodeBlock(code) => {
            // Content is verbatim, except that an empty line cannot be sent.
            let body: Vec<String> = code
                .split('\n')
                .map(|l| sanitize(l).trim_end().to_string())
                .filter(|l| !l.is_empty())
                .collect();
            if body.is_empty() {
                return Vec::new();
            }
            let mut out = Vec::with_capacity(body.len() + 2);
            out.push("```".to_string());
            out.extend(body);
            out.push("```".to_string());
            out
        }
        Block::Quote(inner) => blocks(inner, ctx)
            .into_iter()
            .map(|l| format!("> {l}"))
            .collect(),
        Block::List { ordered, items } => {
            let mut out = Vec::new();
            let mut n = 0;
            for item in items {
                let lines = blocks(item, ctx);
                if lines.is_empty() {
                    continue;
                }
                n += 1;
                let marker = if *ordered {
                    format!("{n}. ")
                } else {
                    "- ".to_string()
                };
                for (k, l) in lines.into_iter().enumerate() {
                    out.push(if k == 0 {
                        format!("{marker}{l}")
                    } else {
                        format!("  {l}")
                    });
                }
            }
            out
        }
        // ic3-teams lifts the quoted message into `Message::reply_to`.
        Block::Reply(_) => Vec::new(),
    }
}

/// Writes inline content as IRC lines.
struct Writer<'a> {
    ctx: &'a dyn RenderCtx,
    lines: Vec<String>,
    line: String,
    /// The styles whose codes are open on the current line.
    have: u8,
    /// The styles of the span being written.
    want: u8,
    /// The last character written to the current line, codes excluded.
    last: Option<char>,
    /// Whether the current line ends in a bare URL.
    after_url: bool,
}

impl<'a> Writer<'a> {
    fn new(ctx: &'a dyn RenderCtx) -> Self {
        Writer {
            ctx,
            lines: Vec::new(),
            line: String::new(),
            have: 0,
            want: 0,
            last: None,
            after_url: false,
        }
    }

    fn finish(mut self) -> Vec<String> {
        self.newline();
        self.lines
    }

    fn inlines(&mut self, inl: &[Inline]) {
        for i in inl {
            self.inline(i);
        }
    }

    fn inline(&mut self, i: &Inline) {
        match i {
            Inline::Text(s) => {
                for (k, part) in s.split('\n').enumerate() {
                    if k > 0 {
                        self.newline();
                    }
                    self.put(part);
                }
            }
            Inline::Bold(c) => self.span(BOLD, c),
            Inline::Italic(c) => self.span(ITALIC, c),
            Inline::Strike(c) => self.span(STRIKE, c),
            Inline::Code(s) => self.put(&code_span(s)),
            Inline::Link { url, text } => {
                let mut shown = String::new();
                plain(text, &mut shown);
                let shown = shown.trim();
                if shown.is_empty() || shown == url.trim() {
                    self.url(url);
                } else {
                    self.inlines(text);
                    // The URL and its parentheses go outside any formatting.
                    let outer = std::mem::replace(&mut self.want, 0);
                    self.put(" (");
                    self.url(url);
                    self.put(")");
                    self.want = outer;
                }
            }
            Inline::Mention { mri, name } => {
                let nick = self.ctx.mention_nick(mri, name);
                self.put(&nick);
            }
            Inline::Emoji(alt) => self.put(alt),
            Inline::Image { url } => {
                let url = self.ctx.media_url(url);
                self.url(&url);
            }
            Inline::LineBreak => self.newline(),
        }
    }

    fn span(&mut self, style: u8, children: &[Inline]) {
        let outer = self.want;
        self.want |= style;
        self.inlines(children);
        self.want = outer;
    }

    /// Writes visible text (no newlines) in the current style.
    fn put(&mut self, s: &str) {
        let s = sanitize(s);
        let Some(first) = s.chars().next() else {
            return;
        };
        if self.after_url && !first.is_whitespace() && !URL_FOLLOWERS.contains(&first) {
            self.line.push(' ');
        }
        self.after_url = false;
        self.sync(self.want);
        self.line.push_str(&s);
        self.last = s.chars().last();
    }

    /// Writes a bare URL, outside any formatting and apart from the words
    /// around it, so clients see and link all of it.
    fn url(&mut self, raw: &str) {
        let mut url = String::with_capacity(raw.len());
        for c in sanitize(raw).trim().chars() {
            if c.is_whitespace() {
                url.push_str("%20");
            } else {
                url.push(c);
            }
        }
        if url.is_empty() {
            return;
        }
        if self.after_url || self.last.is_some_and(|c| c.is_alphanumeric() || c == '_') {
            self.line.push(' ');
        }
        self.sync(0);
        self.line.push_str(&url);
        self.last = url.chars().last();
        self.after_url = true;
    }

    /// Emits the codes that change the open styles to `to`: closing first,
    /// innermost first, then opening, outermost first.
    fn sync(&mut self, to: u8) {
        for (bit, code) in CODES.iter().rev() {
            if self.have & !to & bit != 0 {
                self.line.push(*code);
            }
        }
        for (bit, code) in CODES {
            if to & !self.have & bit != 0 {
                self.line.push(code);
            }
        }
        self.have = to;
    }

    fn newline(&mut self) {
        let line = std::mem::take(&mut self.line);
        self.lines.extend(finish_line(&line));
        self.have = 0;
        self.last = None;
        self.after_url = false;
    }
}

/// A finished line: cut after its last visible character (dropping trailing
/// whitespace and the codes after it), with every style still open closed.
/// `None` when nothing on it is visible.
fn finish_line(raw: &str) -> Option<String> {
    let mut state = 0;
    let mut end = None;
    for (at, c) in raw.char_indices() {
        if let Some((bit, _)) = CODES.iter().find(|(_, code)| *code == c) {
            state ^= bit;
        } else if !c.is_whitespace() {
            end = Some((at + c.len_utf8(), state));
        }
    }
    let (end, open) = end?;
    let mut line = raw[..end].to_string();
    for (bit, code) in CODES.iter().rev() {
        if open & bit != 0 {
            line.push(*code);
        }
    }
    Some(line)
}

/// `s` without control characters (tab aside): they would be read as IRC
/// formatting, or break the line.
fn sanitize(s: &str) -> String {
    s.chars()
        .filter(|&c| c == '\t' || !c.is_control())
        .collect()
}

/// Inline code as a Markdown code span: fenced by a run of backticks longer
/// than any inside (but never three), and padded with a space each side where CommonMark
/// would otherwise misread or strip the edges.
fn code_span(code: &str) -> String {
    let code = sanitize(&code.replace('\n', " "));
    if code.is_empty() {
        return code;
    }
    let mut longest = 0;
    let mut run = 0;
    for c in code.chars() {
        run = if c == '`' { run + 1 } else { 0 };
        longest = longest.max(run);
    }
    // Three backticks at the start of a line would read as a block fence.
    let fence = "`".repeat(if longest == 2 { 4 } else { longest + 1 });
    let pad = code.starts_with('`')
        || code.ends_with('`')
        || (code.starts_with(' ') && code.ends_with(' ') && !code.trim().is_empty());
    let sp = if pad { " " } else { "" };
    format!("{fence}{sp}{code}{sp}{fence}")
}

/// The plain text of a link's display text, for comparing with its URL.
fn plain(inl: &[Inline], out: &mut String) {
    for i in inl {
        match i {
            Inline::Text(s) | Inline::Code(s) | Inline::Emoji(s) => out.push_str(s),
            Inline::Bold(c) | Inline::Italic(c) | Inline::Strike(c) => plain(c, out),
            Inline::Link { url, text } => {
                if text.is_empty() {
                    out.push_str(url);
                } else {
                    plain(text, out);
                }
            }
            Inline::Mention { name, .. } => out.push_str(name),
            Inline::Image { url } => out.push_str(url),
            Inline::LineBreak => out.push(' '),
        }
    }
}
