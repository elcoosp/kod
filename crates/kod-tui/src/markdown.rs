//! Minimal Markdown → styled `ratatui::Line` renderer for the chat.
//!
//! # Why a hand-rolled parser
//!
//! The chat renders in a fixed-width terminal, not a browser, so
//! most of CommonMark is either irrelevant (links, tables, reference
//! definitions) or actively hostile to the layout (raw HTML). The
//! subset a coding agent actually emits — and a user actually reads —
//! is small and stable:
//!
//! - fenced code blocks (```)
//! - headers (# ## ###)
//! - bullet and numbered lists
//! - blockquotes (>)
//! - inline code (`…`)
//! - bold (**…**) and italic (*…* or _…_)
//! - blank lines as paragraph separators
//!
//! Every one of those is unambiguous at the start of a line or
//! between paired delimiters, so a single-pass scanner handles them
//! without a grammar. A full parser (pulldown-cmark and friends)
//! would add a dependency, a lifetime, and a configuration surface
//! for zero behaviour the user can see.
//!
//! # What it does not do
//!
//! - No syntax highlighting inside code blocks. The `code` theme
//!   token colours the whole block; per-token highlighting needs
//!   syntect's language grammars, which is a follow-up if the block
//!   colouring is not enough.
//! - No links. A URL in the model's answer renders as plain text;
//!   the terminal usually auto-links it anyway, and rendering a
//!   clickable anchor in a raw terminal is a per-emulator problem.
//! - No nested lists. A two-level list flattens to one level; the
//!   indent of the source is not preserved.
//! - No tables. The pipe-delimited source is rendered verbatim as
//!   paragraph text; a table is legible as-is and turning it into
//!   real columns needs column-width negotiation that this module
//!   deliberately does not own.
//!
//! # Output
//!
//! [`render`] returns `Vec<Line<'static>>` — every string is owned,
//! so the caller can hold onto the result without lifetime
//! gymnastics. The caller is expected to place each line inside
//! whatever frame it uses (the chat widget wraps the whole thing in
//! a rounded border); the width passed to `render` is the *inner*
//! width, after any border or indent the caller adds.

use crate::theme::Theme;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};

/// Parse the markdown source and render it as styled lines, each
/// fitting within `width` display columns.
///
/// `width == 0` is treated as `1` so the renderer never loops forever
/// on a degenerate caller. Long words that do not fit on one line are
/// hard-broken (mid-word) rather than allowed to overflow — the
/// alternative, letting a 200-character identifier spill past the
/// terminal's right edge, is worse than a soft wrap inside the word.
pub fn render(markdown: &str, width: usize, theme: &Theme) -> Vec<Line<'static>> {
    let width = width.max(1);
    let blocks = parse(markdown);
    let mut out: Vec<Line<'static>> = Vec::new();
    for block in &blocks {
        render_block(block, width, theme, &mut out);
    }
    // Drop trailing blank lines: the chat widget adds its own spacing
    // between messages, so a rendered message ending in a blank line
    // would leave two rows of padding where one belongs.
    while out
        .last()
        .map(|l| l.spans.iter().all(|s| s.content.trim().is_empty()))
        .unwrap_or(false)
    {
        out.pop();
    }
    out
}

// ---------------------------------------------------------------------------
// Render cache
// ---------------------------------------------------------------------------

use std::collections::HashMap;
use std::sync::Arc;

/// A bounded, keyed cache for `render`.
///
/// The chat widget re-renders every visible message every frame. Without
/// a cache that means re-parsing the markdown and allocating fresh
/// `Line` values on every keystroke — a cost that grows with the length
/// of the transcript, which is exactly the kind of overhead that turns
/// an otherwise interactive TUI into a laggy one on a long session.
///
/// The cache key includes the theme name so a theme switch invalidates
/// every entry at once (a stale palette is worse than a re-render). The
/// width is included so a chat with a sidebar (narrower assistant
/// column) and a chat without one can share the cache.
///
/// Bounded by insertion order: the oldest entry beyond `capacity` is
/// dropped on insert. A true LRU is overkill here — the working set is
/// "what is on screen", which is naturally small and shifts with the
/// scroll position, so FIFO eviction is effectively LRU for this
/// access pattern.
pub struct RenderCache {
    entries: std::sync::Mutex<HashMap<CacheKey, Arc<Vec<Line<'static>>>>>,
    /// Insertion order of live keys. Front = oldest.
    order: std::sync::Mutex<std::collections::VecDeque<CacheKey>>,
    capacity: usize,
}

#[derive(Clone, Debug, Hash, PartialEq, Eq)]
struct CacheKey {
    content_hash: u64,
    /// The exact byte length. Included alongside the hash so a collision
    /// on the 64-bit hash of two different strings of different length
    /// is *also* a length collision — astronomically unlikely on top of
    /// an already 1-in-2^64 hash collision.
    content_len: usize,
    width: usize,
    theme: String,
}

impl RenderCache {
    /// Default capacity. 128 entries covers a session's visible
    /// transcript several times over while keeping memory bounded at a
    /// few hundred KB (a 40-line rendered message is a few KB, so 128
    /// entries is ~0.5 MB at the outside).
    pub const DEFAULT_CAPACITY: usize = 128;

    pub fn new() -> Self {
        Self::with_capacity(Self::DEFAULT_CAPACITY)
    }

    pub fn with_capacity(capacity: usize) -> Self {
        Self {
            entries: std::sync::Mutex::new(HashMap::new()),
            order: std::sync::Mutex::new(std::collections::VecDeque::new()),
            capacity: capacity.max(1),
        }
    }

    /// Return the cached render for `content` at `width` under `theme`,
    /// parsing only on a miss.
    ///
    /// The returned `Arc` is shared with the cache; the caller can clone
    /// it cheaply to build a `Text` value without copying the lines.
    pub fn get_or_render(
        &self,
        content: &str,
        width: usize,
        theme: &Theme,
    ) -> Arc<Vec<Line<'static>>> {
        let key = CacheKey {
            content_hash: fnv1a(content.as_bytes()),
            content_len: content.len(),
            width,
            theme: theme.name.clone(),
        };

        // Fast path: cache hit.
        if let Ok(entries) = self.entries.lock()
            && let Some(hit) = entries.get(&key)
        {
            return Arc::clone(hit);
        }

        // Miss: parse, insert, evict.
        let rendered = Arc::new(render(content, width, theme));
        if let Ok(mut entries) = self.entries.lock() {
            entries.insert(key.clone(), Arc::clone(&rendered));
        }
        if let Ok(mut order) = self.order.lock() {
            order.push_back(key.clone());
            // Evict oldest beyond capacity.
            while order.len() > self.capacity {
                if let Some(old) = order.pop_front()
                    && let Ok(mut entries) = self.entries.lock()
                {
                    entries.remove(&old);
                }
            }
        }
        rendered
    }

    /// Number of live entries. Exposed for tests.
    pub fn len(&self) -> usize {
        self.entries.lock().map(|e| e.len()).unwrap_or(0)
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

impl Default for RenderCache {
    fn default() -> Self {
        Self::new()
    }
}

/// FNV-1a 64-bit. Same hasher the checkpoint directory uses; kept local
/// so this module does not depend on a cross-crate helper for one
/// function.
fn fnv1a(bytes: &[u8]) -> u64 {
    const OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
    const PRIME: u64 = 0x0000_0100_0000_01b3;
    let mut h = OFFSET;
    for b in bytes {
        h ^= *b as u64;
        h = h.wrapping_mul(PRIME);
    }
    h
}

// ---------------------------------------------------------------------------
// Parsing
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
enum Block {
    Code { lines: Vec<String> },
    Header { level: u8, text: String },
    Bullet { text: String },
    Numbered { num: usize, text: String },
    Quote { text: String },
    Paragraph { text: String },
    Blank,
}

fn parse(markdown: &str) -> Vec<Block> {
    let mut blocks: Vec<Block> = Vec::new();
    let mut para = String::new();
    let mut in_code = false;
    let mut code_lines: Vec<String> = Vec::new();

    for raw in markdown.split('\n') {
        if in_code {
            if raw.trim_start().starts_with("```") {
                blocks.push(Block::Code {
                    lines: std::mem::take(&mut code_lines),
                });
                in_code = false;
            } else {
                code_lines.push(raw.to_string());
            }
            continue;
        }

        let trimmed = raw.trim_start();

        if trimmed.starts_with("```") {
            flush_para(&mut blocks, &mut para);
            in_code = true;
            continue;
        }

        if raw.trim().is_empty() {
            flush_para(&mut blocks, &mut para);
            blocks.push(Block::Blank);
            continue;
        }

        if let Some(rest) = trimmed.strip_prefix("### ") {
            flush_para(&mut blocks, &mut para);
            blocks.push(Block::Header {
                level: 3,
                text: rest.to_string(),
            });
            continue;
        }
        if let Some(rest) = trimmed.strip_prefix("## ") {
            flush_para(&mut blocks, &mut para);
            blocks.push(Block::Header {
                level: 2,
                text: rest.to_string(),
            });
            continue;
        }
        if let Some(rest) = trimmed.strip_prefix("# ") {
            flush_para(&mut blocks, &mut para);
            blocks.push(Block::Header {
                level: 1,
                text: rest.to_string(),
            });
            continue;
        }

        // Bullet list: `- ` or `* ` at the start of a line. The `* `
        // spelling collides with italic `*…*`, but only at position 0
        // followed by a space — italics in the model's output start
        // with `*word`, never `* word`.
        if let Some(rest) = trimmed
            .strip_prefix("- ")
            .or_else(|| trimmed.strip_prefix("* "))
        {
            flush_para(&mut blocks, &mut para);
            blocks.push(Block::Bullet {
                text: rest.to_string(),
            });
            continue;
        }

        if let Some((num, rest)) = parse_numbered(trimmed) {
            flush_para(&mut blocks, &mut para);
            blocks.push(Block::Numbered { num, text: rest });
            continue;
        }

        if let Some(rest) = trimmed.strip_prefix("> ") {
            flush_para(&mut blocks, &mut para);
            blocks.push(Block::Quote {
                text: rest.to_string(),
            });
            continue;
        }

        // Continuation of the current paragraph. The model writes
        // long paragraphs with hard newlines; joining them with a
        // space, rather than preserving the newline, is what makes
        // the paragraph reflow to the widget's width.
        if !para.is_empty() {
            para.push(' ');
        }
        para.push_str(trimmed);
    }

    if in_code {
        // Unclosed code fence: emit what we have so the content is
        // not silently dropped. This is a live-streaming case as
        // much as a malformed-input case — the renderer runs on
        // partial content while the model is still writing.
        blocks.push(Block::Code { lines: code_lines });
    }
    flush_para(&mut blocks, &mut para);
    blocks
}

fn flush_para(blocks: &mut Vec<Block>, para: &mut String) {
    let trimmed = para.trim();
    if !trimmed.is_empty() {
        blocks.push(Block::Paragraph {
            text: trimmed.to_string(),
        });
    }
    para.clear();
}

/// `1. text` or `10) text`, returned as `(num, text)`.
fn parse_numbered(s: &str) -> Option<(usize, String)> {
    let digits_end = s.find(|c: char| !c.is_ascii_digit())?;
    if digits_end == 0 {
        return None;
    }
    let digits = &s[..digits_end];
    let rest = &s[digits_end..];
    if !(rest.starts_with(". ") || rest.starts_with(") ")) {
        return None;
    }
    let num: usize = digits.parse().ok()?;
    Some((num, rest[2..].to_string()))
}

// ---------------------------------------------------------------------------
// Rendering
// ---------------------------------------------------------------------------

fn render_block(block: &Block, width: usize, theme: &Theme, out: &mut Vec<Line<'static>>) {
    match block {
        Block::Blank => out.push(Line::from("")),

        Block::Header { level, text } => {
            let style = header_style(*level, theme);
            let prefix = "#".repeat(*level as usize);
            let mut spans = vec![Span::styled(format!("{prefix} "), style)];
            for span in parse_inline(text, theme) {
                spans.push(Span::styled(
                    span.content.into_owned(),
                    style.patch(span.style),
                ));
            }
            out.extend(wrap_spans(spans, width));
        }

        Block::Bullet { text } => {
            let inline = parse_inline(text, theme);
            let mut spans = vec![Span::styled(
                "• ",
                Style::default()
                    .fg(theme.accent)
                    .add_modifier(Modifier::BOLD),
            )];
            spans.extend(inline);
            let wrapped = wrap_spans(spans, width);
            out.extend(indent_continuation(wrapped, "  "));
        }

        Block::Numbered { num, text } => {
            let marker = format!("{num}. ");
            let indent = " ".repeat(marker.chars().count());
            let inline = parse_inline(text, theme);
            let mut spans = vec![Span::styled(
                marker,
                Style::default()
                    .fg(theme.accent)
                    .add_modifier(Modifier::BOLD),
            )];
            spans.extend(inline);
            let wrapped = wrap_spans(spans, width);
            out.extend(indent_continuation(wrapped, &indent));
        }

        Block::Quote { text } => {
            let inline = parse_inline(text, theme);
            let bar_style = Style::default().fg(theme.dim);
            let text_style = Style::default()
                .fg(theme.dim)
                .add_modifier(Modifier::ITALIC);
            let mut spans = vec![Span::styled("│ ", bar_style)];
            for span in inline {
                spans.push(Span::styled(span.content.into_owned(), text_style));
            }
            let wrapped = wrap_spans(spans, width);
            out.extend(indent_continuation(wrapped, "│ "));
        }

        Block::Paragraph { text } => {
            let inline = parse_inline(text, theme);
            let base = Style::default().fg(theme.assistant);
            let spans: Vec<Span<'static>> = inline
                .into_iter()
                .map(|span| {
                    // The inline parser returns `Style::default()`
                    // for unstyled runs; the paragraph's base colour
                    // fills in for those, while bold/italic/code
                    // spans keep their modifiers and only get the
                    // foreground if they did not set one.
                    let mut style = span.style;
                    if style.fg.is_none() {
                        style.fg = base.fg;
                    }
                    Span::styled(span.content.into_owned(), style)
                })
                .collect();
            out.extend(wrap_spans(spans, width));
        }

        Block::Code { lines } => {
            let fg = theme.code;
            let bg = Style::default().bg(Color::Rgb(28, 30, 38));
            let style = Style::default().fg(fg).patch(bg);
            for line in lines {
                // Code does not wrap: an identifier broken mid-token
                // is unreadable. Truncate at the width instead, with
                // a trailing ellipsis when the line was cut.
                let truncated: String = if display_width(line) > width.saturating_sub(1) {
                    let mut take = width.saturating_sub(2);
                    let mut acc = String::new();
                    for ch in line.chars() {
                        if take == 0 {
                            break;
                        }
                        acc.push(ch);
                        let w = char_width(ch);
                        if w > take {
                            break;
                        }
                        take -= w;
                    }
                    format!("{acc}…")
                } else {
                    line.clone()
                };
                let padded = format!(" {truncated}");
                out.push(Line::from(Span::styled(padded, style)));
            }
        }
    }
}

fn header_style(level: u8, theme: &Theme) -> Style {
    let base = Style::default()
        .fg(theme.accent)
        .add_modifier(Modifier::BOLD);
    match level {
        1 => base,
        2 => base,
        _ => base.add_modifier(Modifier::ITALIC),
    }
}

// ---------------------------------------------------------------------------
// Inline parsing
// ---------------------------------------------------------------------------

/// Scan a text run for `` `code` ``, `**bold**`, `*italic*`, and
/// `_italic_` delimiters. Unterminated delimiters are emitted
/// literally — a streaming reply may end mid-bold, and dropping the
/// marker would make the partial text unreadable.
fn parse_inline(text: &str, theme: &Theme) -> Vec<Span<'static>> {
    let mut spans: Vec<Span<'static>> = Vec::new();
    let mut buf = String::new();
    let chars: Vec<char> = text.chars().collect();
    let mut i = 0;

    while i < chars.len() {
        let c = chars[i];

        if c == '`' {
            if let Some(end) = find_char(&chars, i + 1, '`') {
                let content: String = chars[i + 1..end].iter().collect();
                if !buf.is_empty() {
                    spans.push(Span::raw(std::mem::take(&mut buf)));
                }
                spans.push(Span::styled(
                    content,
                    Style::default().fg(theme.code).add_modifier(Modifier::BOLD),
                ));
                i = end + 1;
                continue;
            }
            buf.push(c);
            i += 1;
            continue;
        }

        if c == '*' && i + 1 < chars.len() && chars[i + 1] == '*' {
            if let Some(end) = find_double(&chars, i + 2, '*') {
                let content: String = chars[i + 2..end].iter().collect();
                if !buf.is_empty() {
                    spans.push(Span::raw(std::mem::take(&mut buf)));
                }
                spans.push(Span::styled(
                    content,
                    Style::default().add_modifier(Modifier::BOLD),
                ));
                i = end + 2;
                continue;
            }
            buf.push_str("**");
            i += 2;
            continue;
        }

        if c == '*' || c == '_' {
            // Single-delimiter italic. Guard against the `_snake_case_`
            // case: an underscore surrounded by alphanumerics is part
            // of an identifier, not an italic marker.
            let is_word_char = |ch: char| ch.is_alphanumeric();
            let left_is_word = i > 0 && is_word_char(chars[i - 1]);
            let right_is_word = i + 1 < chars.len() && is_word_char(chars[i + 1]);
            let escaped = c == '_' && left_is_word && right_is_word;

            if !escaped && let Some(end) = find_char(&chars, i + 1, c) {
                let content: String = chars[i + 1..end].iter().collect();
                if !buf.is_empty() {
                    spans.push(Span::raw(std::mem::take(&mut buf)));
                }
                spans.push(Span::styled(
                    content,
                    Style::default().add_modifier(Modifier::ITALIC),
                ));
                i = end + 1;
                continue;
            }
            buf.push(c);
            i += 1;
            continue;
        }

        buf.push(c);
        i += 1;
    }

    if !buf.is_empty() {
        spans.push(Span::raw(buf));
    }
    spans
}

fn find_char(chars: &[char], from: usize, target: char) -> Option<usize> {
    (from..chars.len()).find(|&j| chars[j] == target)
}

fn find_double(chars: &[char], from: usize, target: char) -> Option<usize> {
    let mut j = from;
    while j + 1 < chars.len() {
        if chars[j] == target && chars[j + 1] == target {
            return Some(j);
        }
        j += 1;
    }
    None
}

// ---------------------------------------------------------------------------
// Wrapping
// ---------------------------------------------------------------------------

/// Flatten styled spans into a sequence of (char, style) pairs,
/// then chunk them into lines of at most `width` display columns,
/// breaking at word boundaries when possible and mid-word when a
/// single word exceeds the width.
fn wrap_spans(spans: Vec<Span<'static>>, width: usize) -> Vec<Line<'static>> {
    let width = width.max(1);

    // Flatten.
    let mut chars: Vec<(char, Style)> = Vec::new();
    for span in spans {
        let style = span.style;
        for c in span.content.chars() {
            chars.push((c, style));
        }
    }
    if chars.is_empty() {
        return vec![Line::from("")];
    }

    let mut lines: Vec<Vec<(char, Style)>> = vec![Vec::new()];
    let mut cur_width = 0usize;
    let mut i = 0;

    while i < chars.len() {
        let (c, _) = chars[i];

        // Whitespace: collapse and use as a break point if the next
        // word will not fit.
        if c == ' ' || c == '\t' {
            // Peek at the next word's width.
            let mut word_end = i;
            while word_end < chars.len() && (chars[word_end].0 == ' ' || chars[word_end].0 == '\t')
            {
                word_end += 1;
            }
            let mut next_word_width = 0usize;
            let mut k = word_end;
            while k < chars.len() && chars[k].0 != ' ' && chars[k].0 != '\t' {
                next_word_width += char_width(chars[k].0);
                k += 1;
            }
            if word_end == chars.len() {
                // Trailing whitespace: drop it and end the loop.
                // The assignment to `i` the previous version had
                // was dead — the `break` immediately followed.
                break;
            }
            if cur_width + 1 + next_word_width > width {
                lines.push(Vec::new());
                cur_width = 0;
                i = word_end;
                continue;
            }
            // Keep one space.
            lines.last_mut().unwrap().push((' ', chars[i].1));
            cur_width += 1;
            i = word_end;
            continue;
        }

        // Non-space: gather the word.
        let word_start = i;
        let mut word_end = i;
        let mut word_width = 0usize;
        while word_end < chars.len() && chars[word_end].0 != ' ' && chars[word_end].0 != '\t' {
            word_width += char_width(chars[word_end].0);
            word_end += 1;
        }

        if word_width > width {
            // Hard-break a word longer than the line.
            for &(ch, style) in &chars[word_start..word_end] {
                let w = char_width(ch);
                if cur_width + w > width {
                    lines.push(Vec::new());
                    cur_width = 0;
                }
                lines.last_mut().unwrap().push((ch, style));
                cur_width += w;
            }
        } else if cur_width + word_width > width && cur_width > 0 {
            lines.push(Vec::new());
            cur_width = 0;
            for &(ch, style) in &chars[word_start..word_end] {
                lines.last_mut().unwrap().push((ch, style));
                cur_width += char_width(ch);
            }
        } else {
            for &(ch, style) in &chars[word_start..word_end] {
                lines.last_mut().unwrap().push((ch, style));
                cur_width += char_width(ch);
            }
        }
        i = word_end;
    }

    // Collapse runs of the same style into spans, per line.
    lines
        .into_iter()
        .map(|line_chars| {
            let mut spans: Vec<Span<'static>> = Vec::new();
            for (c, style) in line_chars {
                match spans.last_mut() {
                    Some(last) if last.style == style => {
                        let mut s = last.content.clone().into_owned();
                        s.push(c);
                        *last = Span::styled(s, style);
                    }
                    _ => spans.push(Span::styled(c.to_string(), style)),
                }
            }
            Line::from(spans)
        })
        .collect()
}

/// Prepend `indent` to every line after the first. Used for list
/// continuations: a bullet's wrapped text lines up under the bullet's
/// text, not under its marker.
fn indent_continuation(lines: Vec<Line<'static>>, indent: &str) -> Vec<Line<'static>> {
    if lines.len() <= 1 {
        return lines;
    }
    lines
        .into_iter()
        .enumerate()
        .map(|(i, line)| {
            if i == 0 {
                line
            } else {
                let mut spans = vec![Span::raw(indent.to_string())];
                spans.extend(line.spans);
                Line::from(spans)
            }
        })
        .collect()
}

fn char_width(c: char) -> usize {
    // One column for anything that is not a control character. The
    // workspace's chat widget uses the same approximation; a
    // codepoint-accurate width via `unicode-width` would be more
    // correct for CJK and combining marks, but the fix that matters
    // here is not overcounting — the previous `String` per char was
    // allocating once per glyph.
    if c.is_control() { 0 } else { 1 }
}

fn display_width(s: &str) -> usize {
    s.chars().map(char_width).sum()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn theme() -> Theme {
        Theme::dark()
    }

    fn rendered_text(lines: &[Line<'static>]) -> String {
        lines
            .iter()
            .map(|l| {
                l.spans
                    .iter()
                    .map(|s| s.content.as_ref())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn plain_paragraph_renders_as_text() {
        let out = render("hello world", 40, &theme());
        assert_eq!(rendered_text(&out), "hello world");
    }

    #[test]
    fn paragraph_reflows_across_source_newlines() {
        let src = "this is a long\nparagraph that should\nreflow";
        let out = render(src, 80, &theme());
        assert_eq!(
            rendered_text(&out),
            "this is a long paragraph that should reflow"
        );
    }

    #[test]
    fn paragraph_wraps_at_width() {
        let src = "one two three four five six seven eight";
        let out = render(src, 20, &theme());
        for line in &out {
            let w: usize = line.spans.iter().map(|s| s.content.chars().count()).sum();
            assert!(w <= 20, "line too wide: {:?}", line);
        }
        // All words preserved.
        let text = rendered_text(&out);
        for word in [
            "one", "two", "three", "four", "five", "six", "seven", "eight",
        ] {
            assert!(text.contains(word), "missing {word} in {text}");
        }
    }

    #[test]
    fn header_levels_are_differentiated() {
        let src = "# top\n## sub\n### sub-sub";
        let out = render(src, 40, &theme());
        let text = rendered_text(&out);
        assert!(text.contains("# top"));
        assert!(text.contains("## sub"));
        assert!(text.contains("### sub-sub"));
    }

    #[test]
    fn bullet_list_renders_with_marker() {
        let src = "- first\n- second\n- third";
        let out = render(src, 40, &theme());
        let text = rendered_text(&out);
        assert!(text.contains("• first"));
        assert!(text.contains("• second"));
        assert!(text.contains("• third"));
    }

    #[test]
    fn numbered_list_renders_with_number() {
        let src = "1. one\n2. two\n10. ten";
        let out = render(src, 40, &theme());
        let text = rendered_text(&out);
        assert!(text.contains("1. one"));
        assert!(text.contains("2. two"));
        assert!(text.contains("10. ten"));
    }

    #[test]
    fn blockquote_renders_with_bar() {
        let src = "> quoted text";
        let out = render(src, 40, &theme());
        let text = rendered_text(&out);
        assert!(text.contains("│ quoted text"));
    }

    #[test]
    fn code_block_renders_verbatim_and_does_not_reflow() {
        let src = "before\n```rust\nfn main() {\n    println!(\"hi\");\n}\n```\nafter";
        let out = render(src, 40, &theme());
        let text = rendered_text(&out);
        assert!(text.contains("fn main() {"));
        assert!(text.contains("println!"));
        assert!(text.contains("}"));
        assert!(text.contains("before"));
        assert!(text.contains("after"));
        // The backtick fence must not appear in the output.
        assert!(!text.contains("```"), "fence leaked: {text}");
    }

    #[test]
    fn code_block_truncates_long_lines() {
        let long = "x".repeat(200);
        let src = format!("```\n{long}\n```");
        let out = render(&src, 20, &theme());
        let text = rendered_text(&out);
        assert!(text.contains("…"), "expected truncation marker: {text}");
        assert!(!text.contains(&"x".repeat(200)));
    }

    #[test]
    fn inline_code_is_kept_as_text_without_backticks() {
        let src = "use `println!` here";
        let out = render(src, 40, &theme());
        let text = rendered_text(&out);
        assert!(text.contains("println!"), "got: {text}");
        assert!(!text.contains('`'), "backticks leaked: {text}");
    }

    #[test]
    fn bold_italic_render_without_markers() {
        let src = "this is **bold** and *italic* and _also italic_";
        let out = render(src, 80, &theme());
        let text = rendered_text(&out);
        assert!(text.contains("bold"));
        assert!(text.contains("italic"));
        assert!(!text.contains('*'), "asterisks leaked: {text}");
        assert!(!text.contains('_'), "underscores leaked: {text}");
    }

    #[test]
    fn underscore_inside_snake_case_is_not_italic() {
        let src = "call read_file_tool now";
        let out = render(src, 40, &theme());
        let text = rendered_text(&out);
        assert!(
            text.contains("read_file_tool"),
            "snake_case identifier broken by italic parsing: {text}"
        );
    }

    #[test]
    fn unterminated_inline_marker_is_emitted_literally() {
        let src = "this is **unterminated";
        let out = render(src, 40, &theme());
        let text = rendered_text(&out);
        assert!(
            text.contains("**unterminated"),
            "unterminated marker should be visible: {text}"
        );
    }

    #[test]
    fn unterminated_code_fence_renders_as_code() {
        // The streaming case: the model is still writing the block.
        let src = "intro\n```python\nprint(\"hi\")";
        let out = render(src, 40, &theme());
        let text = rendered_text(&out);
        assert!(text.contains("intro"));
        assert!(text.contains("print(\"hi\")"));
        assert!(!text.contains("```"), "fence leaked: {text}");
    }

    #[test]
    fn blank_lines_separate_paragraphs() {
        let src = "first paragraph\n\nsecond paragraph";
        let out = render(src, 40, &theme());
        // At least one empty line between the two paragraphs.
        let empty = out
            .iter()
            .filter(|l| l.spans.iter().all(|s| s.content.trim().is_empty()))
            .count();
        assert!(empty >= 1, "no blank separator between paragraphs");
    }

    #[test]
    fn long_word_is_hard_broken() {
        let src = "prefix supercalifragilisticexpialidocious suffix";
        let out = render(src, 15, &theme());
        for line in &out {
            let w: usize = line.spans.iter().map(|s| s.content.chars().count()).sum();
            assert!(w <= 15, "line too wide: {:?}", line);
        }
        let text = rendered_text(&out);
        // The word is broken mid-way but every character survives.
        let chars: String = text.chars().filter(|c| !c.is_whitespace()).collect();
        assert!(chars.contains("supercalifragilisticexpialidocious"));
    }

    #[test]
    fn bullet_continuation_is_indented() {
        let src = "- a very long bullet point that must wrap";
        let out = render(src, 20, &theme());
        // First line starts with the bullet; later lines start with
        // two spaces.
        assert!(rendered_text(&out).starts_with("• "));
        assert!(out.len() >= 2, "expected at least two wrapped lines");
        let second = &out[1];
        let first_span = second.spans.first().expect("span").content.clone();
        assert!(
            first_span.starts_with("  "),
            "continuation should be indented: {:?}",
            first_span
        );
    }

    #[test]
    fn zero_width_does_not_loop_forever() {
        // A degenerate caller (width 0) must not hang. The renderer
        // coerces to 1 internally.
        let out = render("hello world", 0, &theme());
        assert!(!out.is_empty());
    }

    #[test]
    fn cache_hit_returns_same_arc() {
        let cache = RenderCache::new();
        let theme = theme();
        let a = cache.get_or_render("hello **world**", 40, &theme);
        let b = cache.get_or_render("hello **world**", 40, &theme);
        assert!(
            std::sync::Arc::ptr_eq(&a, &b),
            "second lookup of identical (content, width, theme) must hit the cache",
        );
        assert_eq!(cache.len(), 1);
    }

    #[test]
    fn cache_miss_on_width_change() {
        let cache = RenderCache::new();
        let theme = theme();
        let _a = cache.get_or_render("hello", 40, &theme);
        let _b = cache.get_or_render("hello", 20, &theme);
        assert_eq!(
            cache.len(),
            2,
            "different widths must not share a cache entry",
        );
    }

    #[test]
    fn cache_miss_on_theme_change() {
        let cache = RenderCache::new();
        let _a = cache.get_or_render("hello", 40, &Theme::dark());
        let _b = cache.get_or_render("hello", 40, &Theme::light());
        assert_eq!(
            cache.len(),
            2,
            "a theme switch must invalidate (the palette changed)",
        );
    }

    #[test]
    fn cache_evicts_beyond_capacity() {
        let cache = RenderCache::with_capacity(3);
        let theme = theme();
        for i in 0..5 {
            let _ = cache.get_or_render(&format!("msg {i}"), 40, &theme);
        }
        assert!(
            cache.len() <= 3,
            "cache must respect its capacity, got {}",
            cache.len(),
        );
    }

    #[test]
    fn trailing_blank_lines_are_dropped() {
        let src = "text\n\n\n\n";
        let out = render(src, 40, &theme());
        let last = out.last().expect("at least one line");
        let last_text: String = last.spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(!last_text.trim().is_empty(), "trailing blank line survived");
    }
}

#[cfg(test)]
mod coverage_render_cache_corners {
    //! Additional `RenderCache` behaviours: what happens on a
    //! capacity of zero, on repeated eviction, and on a theme
    //! name change that is not the default. The chat widget relies
    //! on the cache hit path being cheap and the eviction keeping
    //! the working set small.
    use super::*;

    #[test]
    fn capacity_zero_is_clamped_to_one() {
        // A caller that passes zero gets a usable cache, not one
        // that panics or refuses every insert. The clamp is what
        // makes the "user configures a cache size" knob safe.
        let c = RenderCache::with_capacity(0);
        let t = Theme::dark();
        let _ = c.get_or_render("hello", 40, &t);
        assert_eq!(c.len(), 1);
    }

    #[test]
    fn repeated_evictions_keep_the_cache_bounded() {
        let c = RenderCache::with_capacity(3);
        let t = Theme::dark();
        for i in 0..50 {
            let _ = c.get_or_render(&format!("content {i}"), 40, &t);
        }
        assert!(c.len() <= 3, "cache grew past its capacity: {}", c.len(),);
    }

    #[test]
    fn distinct_contents_do_not_collide_in_the_cache() {
        // Two different strings of the same length must not map to
        // the same entry. The content hash + length is the key; a
        // regression that keyed on length alone would return the
        // wrong render.
        let c = RenderCache::with_capacity(10);
        let t = Theme::dark();
        let a = c.get_or_render("foo **bold**", 40, &t);
        let b = c.get_or_render("bar **bold**", 40, &t);
        assert!(!std::sync::Arc::ptr_eq(&a, &b));
        assert_eq!(c.len(), 2);
    }

    #[test]
    fn empty_content_is_cacheable_and_renders_to_empty() {
        let c = RenderCache::new();
        let t = Theme::dark();
        let a = c.get_or_render("", 40, &t);
        let b = c.get_or_render("", 40, &t);
        assert!(std::sync::Arc::ptr_eq(&a, &b), "empty content must cache");
        assert!(a.is_empty(), "empty content must render empty");
    }

    #[test]
    fn cache_misses_on_the_same_content_with_a_different_theme_name() {
        // The theme name is part of the cache key. Two themes that
        // happen to have the same palette but different names must
        // still miss, because a future palette change would
        // otherwise serve the stale render for one of them.
        let c = RenderCache::new();
        let mut t1 = Theme::dark();
        t1.name = "theme-a".to_string();
        let mut t2 = Theme::dark();
        t2.name = "theme-b".to_string();
        let _ = c.get_or_render("x", 40, &t1);
        let _ = c.get_or_render("x", 40, &t2);
        assert_eq!(c.len(), 2);
    }

    #[test]
    fn is_empty_matches_len_zero() {
        let c = RenderCache::new();
        assert!(c.is_empty());
        assert_eq!(c.len(), 0);
        let _ = c.get_or_render("x", 40, &Theme::dark());
        assert!(!c.is_empty());
    }

    #[test]
    fn default_has_the_documented_capacity() {
        // The default is a named constant; a regression that
        // changed it would silently shift the working-set budget
        // for every chat.
        assert_eq!(RenderCache::DEFAULT_CAPACITY, 128);
        let c = RenderCache::default();
        assert_eq!(c.len(), 0);
    }

    #[test]
    fn list_continuation_indentation_is_preserved_through_the_cache() {
        // The wrapped continuation carries its indent spans; a
        // cache that stripped styles would lose the visual
        // alignment on the second call.
        let c = RenderCache::new();
        let t = Theme::dark();
        let a = c.get_or_render("- long bullet content that wraps", 20, &t);
        let b = c.get_or_render("- long bullet content that wraps", 20, &t);
        assert!(std::sync::Arc::ptr_eq(&a, &b));
        // And the content is stable, not just the pointer.
        let text_a: String = a
            .iter()
            .map(|l| {
                l.spans
                    .iter()
                    .map(|s| s.content.as_ref())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n");
        let text_b: String = b
            .iter()
            .map(|l| {
                l.spans
                    .iter()
                    .map(|s| s.content.as_ref())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n");
        assert_eq!(text_a, text_b);
    }
}
