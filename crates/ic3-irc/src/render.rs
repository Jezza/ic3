//! Message text between Teams' [`RichText`] and IRC.
//!
//! Outgoing to IRC, inline emphasis uses IRC formatting codes (every client
//! shows them) and block structure uses Markdown (`> ` quotes, `- ` / `1. `
//! lists, ``` fences), which Markdown-aware clients render and others show as
//! plain, readable text. Incoming from IRC, both IRC formatting codes and
//! Markdown are understood.

use ic3_teams::{Mri, RichText};

mod emit;
mod parse;
#[cfg(test)]
mod tests;

/// Style bits shared by both directions: what an IRC formatting code or a
/// Markdown span turns on.
const BOLD: u8 = 1;
const ITALIC: u8 = 2;
const STRIKE: u8 = 4;

/// The IRC formatting codes for [`BOLD`], [`ITALIC`] and [`STRIKE`], in the
/// order a span nests them (outermost first).
const CODES: [(u8, char); 3] = [(BOLD, '\x02'), (ITALIC, '\x1d'), (STRIKE, '\x1e')];

/// What rendering needs from the gateway: nicks and media links.
pub trait RenderCtx {
    /// The IRC nick to show for a Teams mention. `mri` may be an MRI or,
    /// for mentions parsed from Teams HTML, an index into the message's
    /// mention list; `name` is the text Teams showed.
    fn mention_nick(&self, mri: &Mri, name: &str) -> String;
    /// The link to show for a Teams image or file URL (rewritten to the
    /// gateway's media endpoint).
    fn media_url(&self, url: &str) -> String;
    /// The Teams user a typed nick refers to: their MRI and display name.
    fn resolve_nick(&self, nick: &str) -> Option<(Mri, String)>;
}

/// Renders a message body as IRC text; lines are separated by `\n`.
///
/// Emphasis becomes IRC formatting codes, closed at the end of every line so
/// none leaks into the next; inline code, code blocks, quotes and lists
/// become Markdown. Blocks are separated by a single `\n` and no line is
/// empty, since IRC cannot carry an empty line. URLs (links and images) are
/// emitted bare, outside any formatting, so clients linkify them; mentions
/// become the bare nick, which is what clients highlight on.
pub fn to_irc(body: &RichText, ctx: &dyn RenderCtx) -> String {
    emit::render(body, ctx)
}

/// Parses IRC text typed by a user (lines separated by `\n`) into a body.
///
/// Both IRC formatting codes (bold, italic, strikethrough, monospace; colours,
/// underline and reverse are dropped) and Markdown are
/// understood: `**bold**`, `*italic*`, `_italic_`, `~~strike~~`, code spans,
/// ```` ``` ```` fences, `> ` quotes and `- ` / `1. ` lists (nested by
/// indentation). Bare `http(s)` URLs become links, and `@nick` or a leading
/// `nick: ` becomes a mention when [`RenderCtx::resolve_nick`] knows the
/// nick. The result is normalised.
pub fn from_irc(text: &str, ctx: &dyn RenderCtx) -> RichText {
    parse::parse(text, ctx)
}

/// A one-line plain-text excerpt of `body`, at most `max` characters, for
/// reply quotes.
pub fn excerpt(body: &RichText, max: usize) -> String {
    let flat = body
        .plain_text()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    if flat.chars().count() <= max {
        return flat;
    }
    let mut s: String = flat.chars().take(max).collect();
    s.push('…');
    s
}
