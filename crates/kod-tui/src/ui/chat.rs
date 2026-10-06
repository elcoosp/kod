//! Chat message display widget.
//!
//! Messages always render oldest-first (stable sort by timestamp), newest
//! pinned to the live bottom unless the user scrolled back. Assistant
//! replies — finished or still streaming — render inside a rounded border
//! so live text never restyles or jumps when the response completes.

use crate::app::{KodApp, Message};
use crate::highlight;
use crate::theme::Theme;
use kod_types::MessageRole;
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::Text;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Paragraph, Widget, Wrap};

/// Tool body rows shown before collapsing to `… +N more lines (o expands)`.
/// Errors bypass the cap entirely — see the auto-expand in
/// `complete_tool_execution_with_duration`.
pub const TOOL_DISPLAY_LINES: usize = 4;

/// Widget for displaying chat messages
pub struct ChatWidget;

impl ChatWidget {
    pub fn new() -> Self {
        Self
    }

    /// True when this message is skipped because tools are hidden
    /// (the `t` toggle). Errors are never hidden — a collapsed `t`
    /// must not bury failures. Single source for the probe pass, the
    /// hidden-count, and the final render loop.
    fn tool_row_hidden(app: &KodApp, m: &Message) -> bool {
        if app.search_query().is_some() || app.show_tools() || m.role != MessageRole::Tool {
            return false;
        }
        let body = m.content.split_once('\n').map(|x| x.1).unwrap_or("");
        !body.trim_start().starts_with("Error:")
    }

    /// True when a dim rule is drawn above this message: a rule marks
    /// where a user turn begins. Everything inside a turn (tool rows,
    /// assistant bubbles, system notes) flows unseparated.
    fn starts_new_turn(_app: &KodApp, m: &Message, is_first: bool) -> bool {
        !is_first && m.role == MessageRole::User
    }

    /// Split `s` into display rows of at most `width` cells. Wraps at
    /// word boundaries like the markdown path (`wrap_spans`); a word
    /// wider than the row (URLs, CJK runs without spaces) hard-breaks
    /// mid-word exactly like before. Tab characters wrap as spaces.
    /// Pre-wrapped rows are all ≤ width, so Ratatui's `WordWrapper`
    /// passes them through unchanged — measurement and paint agree.
    fn wrap_text(s: &str, width: usize) -> Vec<String> {
        let width = width.max(1);
        let mut rows: Vec<String> = Vec::new();
        for raw in s.split('\n') {
            let mut cur = String::new();
            let mut cur_w = 0usize;
            let mut word = String::new();
            let mut word_w = 0usize;
            let flush = |cur: &mut String,
                         cur_w: &mut usize,
                         word: &mut String,
                         word_w: &mut usize,
                         rows: &mut Vec<String>| {
                let ww = *word_w;
                if ww == 0 {
                    return;
                }
                // A word wider than the whole row hard-breaks in place.
                if ww > width {
                    for ch in word.chars() {
                        let w = Span::raw(ch.to_string()).width().max(1);
                        if *cur_w + w > width && !cur.is_empty() {
                            rows.push(std::mem::take(cur));
                            *cur_w = 0;
                        }
                        cur.push(ch);
                        *cur_w += w;
                    }
                } else {
                    if *cur_w > 0 && *cur_w + 1 + ww > width {
                        rows.push(std::mem::take(cur));
                        *cur_w = 0;
                    }
                    if *cur_w > 0 {
                        cur.push(' ');
                        *cur_w += 1;
                    }
                    cur.push_str(word);
                    *cur_w += ww;
                }
                word.clear();
                *word_w = 0;
            };
            for ch in raw.chars() {
                if ch == ' ' || ch == '\t' {
                    flush(&mut cur, &mut cur_w, &mut word, &mut word_w, &mut rows);
                } else {
                    word.push(ch);
                    word_w += Span::raw(ch.to_string()).width().max(1);
                }
            }
            flush(&mut cur, &mut cur_w, &mut word, &mut word_w, &mut rows);
            rows.push(cur);
        }
        if rows.is_empty() {
            rows.push(String::new());
        }
        rows
    }

    /// Assistant reply block: rounded border, ` ai ` title, themed frame.
    /// Falls back to a plain header when the viewport is too narrow.
    /// Plain text — no markdown parsing. Long rows reflow mid-word exactly
    /// like `Paragraph` with `Wrap { trim: false }` so scroll math stays exact.
    ///
    /// `duration` is the finished turn's friendly wall-clock (`4.2s`),
    /// rendered as a dim `took 4.2s` row *below* the bubble. `None` while
    /// the reply is still streaming (there is no final figure yet) and on
    /// any reply that never got stamped.
    fn assistant_block(
        content: &str,
        width: usize,
        _style: Style,
        app: &KodApp,
        duration: Option<String>,
    ) -> Vec<Line<'static>> {
        let theme = app.theme();
        let frame = Style::default().fg(theme.accent);
        let title_style = Style::default()
            .fg(theme.accent)
            .add_modifier(Modifier::BOLD);
        let inner = width.saturating_sub(4).max(1); // "│ " + content + " │"

        // Dim, italic, indented to the text column: a timing caption, not
        // part of the reply. Kept *outside* the bordered block so the
        // frame still reads as the reply's edge.
        let duration_row = || {
            duration.as_ref().map(|label| {
                Line::from(vec![
                    Span::raw("  "),
                    Span::styled(
                        format!("took {label}"),
                        Style::default()
                            .fg(theme.dim)
                            .add_modifier(Modifier::ITALIC),
                    ),
                ])
            })
        };

        // Markdown rendering (D6.5). Headers, code fences, lists,
        // blockquotes, inline code, bold, and italic get their own
        // styling; paragraphs are reflowed to the inner width. The
        // renderer owns the wrapping, so no per-line `reflow_line`
        // call is needed — the markdown module is the single place
        // that decides where a line breaks.
        //
        // Cached: the widget re-renders every visible message on every
        // frame, so an uncached parse here is O(messages × parse_cost)
        // per keystroke. The key is (content, inner width, theme), so a
        // theme switch or a sidebar toggle gets a fresh render.
        let cached = app.render_cache().get_or_render(content, inner, theme);
        // `assistant_block` returns `Vec<Line>`, not a slice; the
        // cache gives us a shared `Arc<Vec<Line>>` to build the framed
        // output from without copying the underlying spans.
        let body: Vec<Line<'static>> = (*cached).clone();

        if width < 20 {
            let mut lines = vec![Line::from(vec![Span::styled("ai ", title_style)])];
            for row in body {
                let mut spans = vec![Span::raw("  ")];
                spans.extend(row.spans);
                lines.push(Line::from(spans));
            }
            if let Some(row) = duration_row() {
                lines.push(row);
            }
            return lines;
        }
        let mut lines = Vec::new();
        // Top edge with embedded title: ╭─ ai ───…───╮
        let dashes = width.saturating_sub(7);
        lines.push(Line::from(vec![
            Span::styled("╭─", frame),
            Span::styled(" ai ", title_style),
            Span::styled("─".repeat(dashes) + "╮", frame),
        ]));
        for row in body {
            let pad = inner.saturating_sub(row.width());
            let mut spans = vec![Span::styled("│ ", frame)];
            spans.extend(row.spans);
            spans.push(Span::raw(" ".repeat(pad)));
            spans.push(Span::styled(" │", frame));
            lines.push(Line::from(spans));
        }
        lines.push(Line::from(vec![Span::styled(
            format!("╰{}╯", "─".repeat(width.saturating_sub(2))),
            frame,
        )]));
        // The timing caption closes the block: under the frame, never
        // inside it, so it belongs to the reply without being part of
        // the quoted content.
        if let Some(row) = duration_row() {
            lines.push(row);
        }
        lines
    }

    /// Reflow one styled line into rows of at most `width` cells, keeping
    /// each char's style. Breaks mid-word like the plain wrapper so mixed
    /// Markdown/code rows measure exactly.
    /// Only used by this module's tests today. Gated to `cfg(test)` so
    /// production carries no dead code; the tests keep the helper.
    /// A future production caller un-gates it by dropping the attribute.
    #[cfg(test)]
    fn reflow_line<'a>(line: Line<'a>, width: usize) -> Vec<Line<'a>> {
        let width = width.max(1);
        let mut rows: Vec<Vec<Span<'a>>> = vec![Vec::new()];
        let mut cur_w = 0;
        let push_span =
            |rows: &mut Vec<Vec<Span<'a>>>, cur_w: &mut usize, text: String, style: Style| {
                for ch in text.chars() {
                    let w = Span::raw(ch.to_string()).width().max(1);
                    if *cur_w + w > width && *cur_w > 0 {
                        rows.push(Vec::new());
                        *cur_w = 0;
                    }
                    match rows.last_mut().unwrap().last_mut() {
                        Some(last) if last.style == style => {
                            let mut s = last.content.clone().into_owned();
                            s.push(ch);
                            *last = Span::styled(s, style);
                        }
                        _ => rows
                            .last_mut()
                            .unwrap()
                            .push(Span::styled(ch.to_string(), style)),
                    }
                    *cur_w += w;
                }
            };
        for span in line.spans {
            let style = span.style;
            let text: String = span.content.into_owned();
            push_span(&mut rows, &mut cur_w, text, style);
        }
        rows.into_iter()
            .map(Line::from)
            .collect::<Vec<Line<'a>>>()
            .into_iter()
            .map(|l| {
                if l.spans.is_empty() {
                    Line::from("")
                } else {
                    l
                }
            })
            .collect()
    }

    /// Tint every case-insensitive hit of `query` in a line (search mode).
    fn highlight_line<'a>(line: Line<'a>, query: &str) -> Line<'a> {
        if query.is_empty() {
            return line;
        }
        let hit_style = Style::default()
            .fg(Color::Black)
            .bg(Color::Yellow)
            .add_modifier(Modifier::BOLD);
        let q = query.to_lowercase();
        let mut out: Vec<Span<'static>> = Vec::new();
        for span in line.spans {
            let text: String = span.content.into_owned();
            let lower = text.to_lowercase();
            let mut rest = 0;
            let mut matched = false;
            while let Some(pos) = lower[rest..].find(&q) {
                matched = true;
                let start = rest + pos;
                let end = start + q.len();
                // Map byte offsets back through the lowercase string. ASCII
                // text keeps offsets identical; anything else tints the
                // remainder once instead of slicing mid-char.
                let (before, hit) = if text.len() == lower.len() {
                    (text[rest..start].to_string(), text[start..end].to_string())
                } else {
                    (text[rest..].to_string(), String::new())
                };
                if !before.is_empty() {
                    out.push(Span::styled(before, span.style));
                }
                if !hit.is_empty() {
                    out.push(Span::styled(hit, hit_style));
                } else {
                    // Non-ASCII fallback: tint the remainder once, then stop.
                    out.push(Span::styled(text[rest..].to_string(), hit_style));
                    rest = text.len();
                    break;
                }
                rest = end;
            }
            if rest < text.len() {
                out.push(Span::styled(text[rest..].to_string(), span.style));
            } else if !matched && rest == 0 {
                out.push(Span::styled(text, span.style));
            }
        }
        Line::from(out)
    }

    fn message_lines<'a>(app: &KodApp, message: &'a Message, width: usize) -> Vec<Line<'a>> {
        let theme = app.theme();
        let assistant_style = Style::default().fg(theme.assistant);
        // Tool calls keep their own treatment: a `⚙ header` line plus dim
        // body lines. Content arrives as `[header] summary` from the
        // completion handler — brackets are decorative, strip one layer.
        // Long bodies collapse to a preview unless expanded (`o` on the
        // newest tool row, or click-free keyboard toggle).
        if let MessageRole::Tool = message.role {
            let mut parts = message.content.splitn(2, '\n');
            let first = parts.next().unwrap_or("").trim();
            let header = first
                .strip_prefix('[')
                .and_then(|s| s.strip_suffix(']'))
                .unwrap_or(first);
            let rest_raw = parts.next().unwrap_or("");
            let rest = crate::app::KodApp::trim_blank_lines(rest_raw);
            let is_error = rest.trim_start().starts_with("Error:");
            let icon = if is_error { "✗ " } else { "⚙ " };
            let tool_style = if is_error {
                Style::default()
                    .fg(theme.error)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(theme.tool).add_modifier(Modifier::BOLD)
            };
            let body_style = if is_error {
                Style::default().fg(theme.error)
            } else {
                Style::default().fg(Color::Gray)
            };
            let mut lines = vec![Line::from(vec![
                Span::styled(icon, tool_style),
                Span::styled(header.to_string(), tool_style),
            ])];
            if !rest.is_empty() || !rest_raw.is_empty() {
                // `write_file` / `patch_file` / `git_diff` bodies carry a
                // unified diff after a one-line lead (path or scope). Those
                // rows get `+`/`-`/`@@` colours and code tokens; everything
                // else wraps as plain text. Error bodies are red text.
                let inner = width.saturating_sub(4).max(1);
                let rows: Vec<Line<'static>> = if is_error {
                    Self::wrap_text(&rest, inner)
                        .into_iter()
                        .map(|r| Line::from(Span::styled(r, body_style)))
                        .collect()
                } else {
                    Self::body_rows(&rest, inner, body_style, theme)
                };
                let total = rows.len();
                let expanded = app.is_tool_expanded(&message.id);
                let shown = if expanded {
                    total
                } else {
                    total.min(TOOL_DISPLAY_LINES)
                };
                for row in rows.into_iter().take(shown) {
                    let mut spans = vec![Span::raw("    ")];
                    spans.extend(row.spans);
                    lines.push(Line::from(spans));
                }
                if total > shown {
                    lines.push(Line::from(vec![
                        Span::raw("    "),
                        Span::styled(
                            format!("… +{} more lines (o expands)", total - shown),
                            Style::default()
                                .fg(theme.warning)
                                .add_modifier(Modifier::ITALIC),
                        ),
                    ]));
                }
            }
            return Self::apply_search(app, lines);
        }

        if let MessageRole::Assistant = message.role {
            // The turn wall-clock stamped on the reply (finish / fail /
            // cancel paths); `None` for replies from sessions written
            // before the field existed, so old transcripts render plain.
            let duration = message
                .metadata
                .turn_duration_ms
                .map(|ms| KodApp::format_friendly_duration(std::time::Duration::from_millis(ms)));
            let lines =
                Self::assistant_block(&message.content, width, assistant_style, app, duration);
            return Self::apply_search(app, lines);
        }

        let (prefix, style) = match message.role {
            MessageRole::User => ("you", Style::default().fg(theme.user)),
            MessageRole::System => ("sys", Style::default().fg(theme.system)),
            MessageRole::Agent(_) => ("agent", Style::default().fg(Color::Blue)),
            MessageRole::Assistant | MessageRole::Tool => ("ai", assistant_style),
        };

        let mut lines = vec![Line::from(vec![Span::styled(
            prefix.to_string(),
            style.add_modifier(Modifier::BOLD),
        )])];
        // `/diff` output is a system message with a prose lead and a patch;
        // it gets the same diff colouring as a tool row.
        for row in Self::body_rows(
            &message.content,
            width.saturating_sub(2).max(1),
            style,
            theme,
        ) {
            let mut spans = vec![Span::raw("  ")];
            spans.extend(row.spans);
            lines.push(Line::from(spans));
        }
        Self::apply_search(app, lines)
    }

    /// Rows for a body that may hold a unified diff: the diff's rows get
    /// `+`/`-`/`@@` colours and code tokens, everything else wraps in
    /// `base`. Every row is at most `width` so the measuring `Paragraph`
    /// never re-wraps it.
    fn body_rows(body: &str, width: usize, base: Style, theme: &Theme) -> Vec<Line<'static>> {
        if let Some(d) = highlight::DiffBody::parse(body) {
            let mut rows: Vec<Line<'static>> = Vec::new();
            for l in &d.lead {
                for w in Self::wrap_text(l, width) {
                    rows.push(Line::from(Span::styled(w, base)));
                }
            }
            for spans in highlight::diff_rows(&d.lines, d.syntax, width, theme) {
                rows.push(Line::from(spans));
            }
            if !rows.is_empty() {
                return rows;
            }
        }
        Self::wrap_text(body, width)
            .into_iter()
            .map(|r| Line::from(Span::styled(r, base)))
            .collect()
    }

    /// Tint search hits across finished lines (no-op without a query).
    fn apply_search<'a>(app: &KodApp, lines: Vec<Line<'a>>) -> Vec<Line<'a>> {
        match app.search_query() {
            Some(q) if !q.is_empty() => lines
                .into_iter()
                .map(|l| Self::highlight_line(l, q))
                .collect(),
            _ => lines,
        }
    }

    /// Cached visual row count + tail-blank flag for one message.
    ///
    /// On a hit this skips both the `message_lines()` call and the
    /// ratatui wrap pass — the two costs the probe and the real pass
    /// used to pay in full on every frame.
    fn message_measurement_cached(app: &KodApp, m: &Message, width: usize) -> (usize, bool) {
        let content_hash = crate::render_cache::hash_content(&m.content);
        let width_u16 = width.min(u16::MAX as usize) as u16;
        if let Some(hit) =
            crate::render_cache::with_cache(|c| c.lookup(&m.id, content_hash, width_u16))
        {
            return hit;
        }
        let lines = Self::message_lines(app, m, width);
        let tail_blank = lines.last().map(|l| l.width() == 0).unwrap_or(false);
        #[allow(unstable_name_collisions)]
        let rows = Paragraph::new(Text::from(lines))
            .wrap(Wrap { trim: false })
            .line_count(width_u16);
        crate::render_cache::with_cache(|c| {
            c.insert(m.id.clone(), content_hash, width_u16, rows, tail_blank);
        });
        (rows, tail_blank)
    }

    pub fn render(&self, app: &KodApp, area: Rect, buf: &mut Buffer) {
        // Strict insertion order via monotonic sequence — never by
        // wall-clock timestamp (which can collide or go backwards after
        // a restore). Stable sort preserves file order for equal seq.
        let mut ordered: Vec<&Message> = app.messages().iter().collect();
        ordered.sort_by_key(|m| m.sequence);

        // Begin a fresh render-cache frame. See render_cache.rs.
        crate::render_cache::with_cache(|c| c.begin_frame());

        // First pass at the narrowed width decides whether the scrollbar
        // column is needed; the second pass wraps to the real text width
        // so the assistant border spans exactly the available space.
        // Use Paragraph::line_count (unstable-rendered-line-info) so the
        // count matches Ratatui's own WordWrapper instead of a hand-rolled
        // div_ceil that drifts on wide graphemes/word boundaries.
        let full_width = area.width.max(1) as usize;
        let height = area.height as usize;
        let narrow_width = area.width.saturating_sub(1).max(1) as usize;
        let show_bar = area.width >= 10 && {
            let mut hidden = 0;
            // Count hidden tools for the footer, same as final pass
            for m in &ordered {
                if app.search_query().is_none() && !app.show_tools() && m.role == MessageRole::Tool
                {
                    let body = m.content.split_once('\n').map(|x| x.1).unwrap_or("");
                    if !body.trim_start().starts_with("Error:") {
                        hidden += 1;
                    }
                }
            }
            // Row-count-only probe: sum cached per-message visual
            // row counts instead of building the full line vector
            // and asking `Paragraph::line_count`. On a cache hit
            // both the `message_lines()` call and the ratatui wrap
            // pass are skipped. The streaming body is not a
            // `Message` and is measured directly each frame — the
            // only uncached cost here, and it is bounded by the
            // size of the in-progress reply.
            let mut probe_rows: usize = 0;
            let mut prev_tail_blank = false;
            let mut probe_rendered = false;
            for (i, m) in ordered.iter().enumerate() {
                if Self::tool_row_hidden(app, m) {
                    continue;
                }
                if Self::starts_new_turn(app, m, i == 0) && !prev_tail_blank {
                    probe_rows += 1;
                }
                let (rows, tail_blank) = Self::message_measurement_cached(app, m, narrow_width);
                probe_rows += rows;
                prev_tail_blank = tail_blank;
                probe_rendered = true;
            }
            if hidden > 0 {
                probe_rows += 1;
            }
            let stream_probe = crate::app::KodApp::trim_blank_lines(app.current_response());
            if app.is_streaming() && !stream_probe.is_empty() {
                let block = Self::assistant_block(
                    &stream_probe,
                    narrow_width,
                    Style::default().fg(app.theme().assistant),
                    app,
                    None,
                );
                #[allow(unstable_name_collisions)]
                let rows = Paragraph::new(Text::from(block))
                    .wrap(Wrap { trim: false })
                    .line_count(narrow_width as u16);
                probe_rows += rows;
                probe_rendered = true;
            }
            if !probe_rendered && probe_rows == 0 {
                probe_rows = 1;
            }
            probe_rows > height
        };
        let text_width = if show_bar { narrow_width } else { full_width };
        let theme = app.theme();

        let mut lines: Vec<Line> = Vec::new();
        // A dim rule between turns (never after the last one) so exchanges
        // scan visually instead of piling up with one blank row.
        let mut hidden_tools = 0;
        // Maps each rendered message's id to the index of its first
        // line in `lines`. Used by the search-active scroll override
        // below to compute the target message's vertical position.
        let mut message_line_offsets: std::collections::HashMap<kod_types::MessageId, usize> =
            std::collections::HashMap::new();
        for (i, message) in ordered.iter().enumerate() {
            if Self::tool_row_hidden(app, message) {
                hidden_tools += 1;
                continue;
            }
            if Self::starts_new_turn(app, message, i == 0)
                && lines.last().map(|l| l.width()).unwrap_or(1) > 0
            {
                lines.push(Line::from(vec![Span::styled(
                    "─".repeat(text_width.min(120)),
                    Style::default().fg(theme.dim),
                )]));
            }
            message_line_offsets.insert(message.id.clone(), lines.len());
            lines.extend(Self::message_lines(app, message, text_width));
        }
        if hidden_tools > 0 {
            lines.push(Line::from(vec![Span::styled(
                format!("⋯ {hidden_tools} tool output(s) hidden — t to show"),
                Style::default()
                    .fg(theme.dim)
                    .add_modifier(Modifier::ITALIC),
            )]));
        }

        // Same rounded frame as a finished reply: no color or layout pop
        // when the response completes. Leading blank lines are trimmed, and
        // a whitespace-only stream renders nothing — otherwise every reply
        // opens as an empty bubble that fills in a frame later.
        let stream_body = crate::app::KodApp::trim_blank_lines(app.current_response());
        if app.is_streaming() && !stream_body.is_empty() {
            lines.extend(Self::assistant_block(
                &stream_body,
                text_width,
                Style::default().fg(theme.assistant),
                app,
                // Still running: no final figure to show. The row appears
                // when this body settles into a stamped message.
                None,
            ));
        }

        // No separate "running …" footer line here: `start_tool_execution`
        // already streams a live tool row in position (header + running
        // body), so a second indicator below the messages would duplicate
        // the same call.

        if lines.is_empty() {
            lines.push(Line::from(vec![Span::styled(
                "No messages yet — type below and press Enter. /help lists commands.",
                Style::default().fg(Color::DarkGray),
            )]));
        }

        // M-44: sum cached per-message row counts at the paint width
        // instead of cloning the whole line vector and asking
        // Paragraph to re-wrap it every frame. This mirrors the
        // scrollbar probe above (which the codebase already trusts
        // for the same quantity); each message's count comes from
        // `message_measurement_cached`, which is keyed by
        // (id, content hash, width). The stream block is not a
        // `Message` and is measured directly — it is bounded by the
        // size of the in-progress reply.
        let total_rows: usize = {
            let mut rows: usize = 0;
            let mut prev_tail_blank = false;
            let mut rendered = false;
            let mut hidden_count = 0usize;
            for (i, m) in ordered.iter().enumerate() {
                if Self::tool_row_hidden(app, m) {
                    hidden_count += 1;
                    continue;
                }
                if Self::starts_new_turn(app, m, i == 0) && !prev_tail_blank {
                    rows += 1;
                }
                let (r, tb) = Self::message_measurement_cached(app, m, text_width);
                rows += r;
                prev_tail_blank = tb;
                rendered = true;
            }
            if hidden_count > 0 {
                rows += 1;
            }
            if app.is_streaming() && !stream_body.is_empty() {
                let block = Self::assistant_block(
                    &stream_body,
                    text_width,
                    Style::default().fg(theme.assistant),
                    app,
                    None,
                );
                #[allow(unstable_name_collisions)]
                {
                    rows += Paragraph::new(Text::from(block))
                        .wrap(Wrap { trim: false })
                        .line_count(text_width as u16);
                }
                rendered = true;
            }
            if !rendered && rows == 0 { 1 } else { rows }
        };
        let max_offset = total_rows.saturating_sub(height);
        let offset = app.scroll_offset().min(max_offset);
        // Skip-to-row for this frame. While a search is active, the
        // targeted message is centered in the viewport — that is the
        // user's focus, not wherever they had scrolled to before
        // starting the search. The user's own scroll offset
        // (`app.scroll_lines`) is never touched, so clearing the
        // search (Escape) or the search finding no more matches
        // restores the previous viewport immediately.
        //
        // The centering computation is a per-frame cost only while a
        // search is active: measure the visual rows of the `lines`
        // prefix before the target, then subtract half the viewport.
        // `line_count` gives the true wrapped-row count, matching the
        // scrollbar's own measurement a few lines down.
        let skip_rows = if let Some(target_id) = app.search_target_message_id() {
            if let Some(&line_idx) = message_line_offsets.get(target_id) {
                let prefix = Text::from(lines[..line_idx].to_vec());
                #[allow(unstable_name_collisions)]
                let prefix_rows = Paragraph::new(prefix)
                    .wrap(Wrap { trim: false })
                    .line_count(text_width as u16);
                let desired = prefix_rows.saturating_sub(height / 2);
                desired.min(max_offset).min(u16::MAX as usize) as u16
            } else {
                // Target message did not render in this frame (it was
                // a tool row and `show_tools` is off, for example).
                // Fall back to the user's own scroll.
                total_rows
                    .saturating_sub(height)
                    .saturating_sub(offset)
                    .min(u16::MAX as usize) as u16
            }
        } else {
            total_rows
                .saturating_sub(height)
                .saturating_sub(offset)
                .min(u16::MAX as usize) as u16
        };
        let text_area = if show_bar {
            Rect {
                width: area.width - 1,
                ..area
            }
        } else {
            area
        };
        let paragraph = Paragraph::new(Text::from(lines))
            .wrap(Wrap { trim: false })
            .scroll((skip_rows, 0));
        paragraph.render(text_area, buf);
        crate::render_cache::with_cache(|c| c.end_frame());

        if show_bar {
            // Thumb position tracks the viewport: offset 0 (live bottom)
            // sits at the bottom, max offset at the top.
            let h = height;
            let total = total_rows;
            let thumb_h = ((h * h) / total.max(1)).max(1).min(h);
            let thumb_start = if max_offset == 0 {
                0
            } else {
                (max_offset - offset)
                    .checked_mul(h - thumb_h)
                    .and_then(|v| v.checked_div(max_offset))
                    .unwrap_or(0)
            };
            let x = area.right() - 1;
            let bar_style = Style::default().fg(Color::DarkGray);
            // Slim track (│) + half-block thumb (▌). The extremity cells
            // are arrows: ▲ (yellow) when older content sits above, ▼
            // (yellow) when newer content sits below — the wheel's direction
            // affordance. Arrows win over the thumb at the end cells.
            let thumb_style = Style::default().fg(Color::Cyan);
            let more_style = Style::default().fg(Color::Yellow);
            let more_above = offset < max_offset;
            let more_below = offset > 0;
            for i in 0..h {
                let y = area.y + i as u16;
                let mut symbol = "│";
                let mut style = bar_style;
                if i >= thumb_start && i < thumb_start + thumb_h {
                    symbol = "▌";
                    style = thumb_style;
                }
                if i == 0 && more_above && !(i + 1 == h && more_below) {
                    symbol = "▲";
                    style = more_style;
                } else if i + 1 == h && more_below {
                    symbol = "▼";
                    style = more_style;
                }
                buf[(x, y)].set_symbol(symbol).set_style(style);
            }
        }
    }
}

impl Default for ChatWidget {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod coverage_chat_widget {
    //! Coverage for the chat widget's helpers and render branches.
    //! Uses `Buffer::empty` + the widget's public `render` (or the
    //! private helpers, which a same-file test module can reach).
    //! No terminal, no I/O.
    use super::*;
    use crate::app::Message;
    use chrono::Utc;
    use kod_types::{MessageId, MessageRole};

    fn buffer_text(buf: &Buffer) -> String {
        buf.content()
            .iter()
            .map(|c| c.symbol().to_string())
            .collect()
    }

    fn push_message(app: &mut KodApp, role: MessageRole, content: &str) {
        app.add_message(Message {
            id: MessageId::new(),
            role,
            content: content.to_string(),
            timestamp: Utc::now(),
            metadata: Default::default(),
            sequence: 0,
        });
    }

    fn render(app: &KodApp, w: u16, h: u16) -> String {
        let area = ratatui::layout::Rect::new(0, 0, w, h);
        let mut buf = Buffer::empty(area);
        ChatWidget::new().render(app, area, &mut buf);
        buffer_text(&buf)
    }

    // ---- wrap_text -----------------------------------------------------

    #[test]
    fn wrap_text_splits_on_newlines_verbatim() {
        let rows = ChatWidget::wrap_text("line one\nline two", 80);
        assert_eq!(rows, vec!["line one", "line two"]);
    }

    #[test]
    fn wrap_text_breaks_long_rows_at_the_width() {
        let rows = ChatWidget::wrap_text("abcdefghij", 4);
        assert_eq!(rows, vec!["abcd", "efgh", "ij"]);
    }

    #[test]
    fn wrap_text_handles_empty_input() {
        assert_eq!(ChatWidget::wrap_text("", 10), vec![""]);
    }

    #[test]
    fn wrap_text_wraps_at_word_boundaries() {
        let rows = ChatWidget::wrap_text("hello brave world", 11);
        assert_eq!(rows, vec!["hello brave", "world"]);
    }

    #[test]
    fn wrap_text_keeps_multibyte_width_exact_with_spaces() {
        // CJK runs without spaces hard-break; with spaces the wrap
        // respects the space.
        let rows = ChatWidget::wrap_text("ab cd ef", 5);
        assert_eq!(rows, vec!["ab cd", "ef"]);
    }

    #[test]
    fn wrap_text_zero_width_is_clamped_to_one() {
        // `width.max(1)` at the top of `wrap_text` prevents an
        // infinite loop. Each char occupies its own row.
        let rows = ChatWidget::wrap_text("abc", 0);
        assert_eq!(rows, vec!["a", "b", "c"]);
    }

    #[test]
    fn wrap_text_multibyte_counts_display_cells_not_bytes() {
        // CJK characters occupy two display cells each (per
        // `unicode-width`). At width 4 exactly two fit per row —
        // not four (which would be a byte-count bug) and not one
        // (which would be a char-count bug). Three bytes per char,
        // so a byte counter would put only one char per row; a raw
        // char counter would put four.
        let rows = ChatWidget::wrap_text("日本語です", 4);
        assert_eq!(rows, vec!["日本", "語で", "す"]);
    }

    // ---- render: empty state -------------------------------------------

    #[test]
    fn render_empty_chat_shows_the_placeholder() {
        let app = KodApp::new();
        let text = render(&app, 80, 10);
        assert!(
            text.contains("No messages yet"),
            "placeholder missing: {text}",
        );
        assert!(
            text.contains("/help"),
            "placeholder must point at /help: {text}",
        );
    }

    #[test]
    fn render_with_one_user_message_shows_the_content() {
        let mut app = KodApp::new();
        push_message(&mut app, MessageRole::User, "hello world");
        let text = render(&app, 80, 10);
        assert!(text.contains("hello world"), "message body: {text}");
        assert!(text.contains("you"), "user prefix: {text}");
    }

    #[test]
    fn render_with_a_system_message_shows_the_sys_prefix() {
        let mut app = KodApp::new();
        push_message(&mut app, MessageRole::System, "a system note");
        let text = render(&app, 80, 10);
        assert!(text.contains("a system note"));
        assert!(text.contains("sys"));
    }

    #[test]
    fn render_with_an_agent_message_shows_the_agent_prefix() {
        let mut app = KodApp::new();
        push_message(
            &mut app,
            MessageRole::Agent(kod_types::AgentId::new()),
            "agent reply",
        );
        let text = render(&app, 80, 10);
        assert!(text.contains("agent reply"));
        assert!(text.contains("agent"));
    }

    #[test]
    fn render_assistant_message_includes_the_ai_frame() {
        let mut app = KodApp::new();
        push_message(&mut app, MessageRole::Assistant, "the reply");
        let text = render(&app, 80, 20);
        assert!(text.contains("ai"), "ai title: {text}");
        // The rounded border chars prove the assistant block, not
        // the plain-text path, rendered.
        assert!(text.contains("╭") || text.contains("─"), "frame: {text}");
    }

    #[test]
    fn render_assistant_message_on_a_narrow_viewport_falls_back_to_plain() {
        // width < 20 skips the frame and emits "ai " as a plain
        // prefix. This is the "no room for a border" branch.
        let mut app = KodApp::new();
        push_message(&mut app, MessageRole::Assistant, "hi");
        let text = render(&app, 12, 20);
        assert!(text.contains("ai"), "plain fallback title: {text}");
    }

    // ---- render: tool rows ---------------------------------------------

    #[test]
    fn render_tool_message_with_no_expansion_shows_the_header() {
        let mut app = KodApp::new();
        push_message(
            &mut app,
            MessageRole::Tool,
            "[execute_command] cargo check\nline one\nline two",
        );
        let text = render(&app, 100, 40);
        assert!(text.contains("execute_command"), "header: {text}");
        assert!(text.contains("line one"), "body: {text}");
        assert!(text.contains("⚙"), "tool icon: {text}");
    }

    #[test]
    fn render_error_tool_uses_the_x_icon() {
        let mut app = KodApp::new();
        push_message(
            &mut app,
            MessageRole::Tool,
            "[execute_command] cargo check\nError: exit status 1",
        );
        let text = render(&app, 100, 40);
        assert!(text.contains("✗"), "error icon: {text}");
    }

    fn row_spans<'t>(lines: &[Line<'t>], needle: &str) -> Vec<Span<'t>> {
        lines
            .iter()
            .find(|l| {
                let t: String = l.spans.iter().map(|s| s.content.as_ref()).collect();
                t.contains(needle)
            })
            .map(|l| l.spans.clone())
            .unwrap_or_else(|| panic!("no row containing {needle:?}"))
    }

    #[test]
    fn write_file_row_colours_its_diff() {
        let mut app = KodApp::new();
        push_message(
            &mut app,
            MessageRole::Tool,
            "[write_file · 12ms]\nsrc/main.rs:\n@@ -1 +1,2 @@\n let a = 1;\n+let b = 2;\n-let c = 3;",
        );
        let t = app.theme().clone();
        let id = app.messages().last().unwrap().id.clone();
        assert!(app.toggle_tool_expanded(&id));
        let msg = app.messages().last().unwrap();
        let lines = ChatWidget::message_lines(&app, msg, 100);
        let lead = row_spans(&lines, "src/main.rs:");
        assert_eq!(lead[0].style.fg, None);
        assert_eq!(lead[1].style.fg, Some(Color::Gray));
        let added = row_spans(&lines, "+let b = 2;");
        assert_eq!(added[1].style.fg, Some(t.user));
        assert!(
            added
                .iter()
                .any(|s| matches!(s.style.fg, Some(c) if c != t.user))
        );
        let removed = row_spans(&lines, "-let c = 3;");
        assert_eq!(removed[1].style.fg, Some(t.error));
        assert_ne!(added[1].style.bg, removed[1].style.bg);
    }

    #[test]
    fn git_diff_row_is_coloured_too() {
        let mut app = KodApp::new();
        push_message(
            &mut app,
            MessageRole::Tool,
            "[git_diff] git diff (unstaged):\ndiff --git a/a.rs b/a.rs\nindex 1..2 100644\n--- a/a.rs\n+++ b/a.rs\n@@ -1 +1 @@\n-old\n+new",
        );
        let t = app.theme().clone();
        let id = app.messages().last().unwrap().id.clone();
        assert!(app.toggle_tool_expanded(&id));
        let msg = app.messages().last().unwrap();
        let lines = ChatWidget::message_lines(&app, msg, 100);
        assert_eq!(row_spans(&lines, "@@ -1 +1 @@")[1].style.fg, Some(t.accent));
        assert_eq!(row_spans(&lines, "+new")[1].style.fg, Some(t.user));
        assert_eq!(row_spans(&lines, "index 1..2")[1].style.fg, Some(t.dim));
    }

    #[test]
    fn system_message_diff_is_coloured() {
        let mut app = KodApp::new();
        push_message(
            &mut app,
            MessageRole::System,
            "Most recent file change (cp-1 · /tmp/main.rs)\n\n--- a/main.rs\n+++ b/main.rs\n@@ -1 +1 @@\n-old\n+new",
        );
        let t = app.theme().clone();
        let msg = app.messages().last().unwrap();
        let lines = ChatWidget::message_lines(&app, msg, 100);
        assert_eq!(row_spans(&lines, "Most recent")[1].style.fg, Some(t.system));
        assert_eq!(row_spans(&lines, "+new")[1].style.fg, Some(t.user));
        assert_eq!(row_spans(&lines, "-old")[1].style.fg, Some(t.error));
    }

    #[test]
    fn non_diff_tool_body_wraps_as_before() {
        let mut app = KodApp::new();
        push_message(
            &mut app,
            MessageRole::Tool,
            "[read_file] src/main.rs · 2 lines\nfn main() {}\nfn other() {}",
        );
        let msg = app.messages().last().unwrap();
        let lines = ChatWidget::message_lines(&app, msg, 100);
        let body = row_spans(&lines, "fn main() {}");
        assert_eq!(body[1].style.fg, Some(Color::Gray));
        assert_eq!(body[1].style.bg, None);
    }

    /// Push an assistant reply carrying a turn duration — what
    /// `KodApp::stamp_reply_duration` writes when a turn finishes.
    fn push_stamped_reply(app: &mut KodApp, content: &str, turn_duration_ms: u64) {
        app.add_message(Message {
            id: MessageId::new(),
            role: MessageRole::Assistant,
            content: content.to_string(),
            timestamp: Utc::now(),
            metadata: kod_types::MessageMetadata {
                turn_duration_ms: Some(turn_duration_ms),
                ..Default::default()
            },
            sequence: 0,
        });
    }

    fn line_text(line: &Line<'_>) -> String {
        line.spans.iter().map(|s| s.content.as_ref()).collect()
    }

    #[test]
    fn stamped_reply_shows_a_took_row_under_the_block() {
        let mut app = KodApp::new();
        push_stamped_reply(&mut app, "the answer", 4200);
        let msg = app.messages().last().unwrap();
        let lines = ChatWidget::message_lines(&app, msg, 60);

        let caption = lines.last().expect("a caption row");
        // Two-cell indent to the bubble's inner text column (see the
        // `duration_row` closure in `assistant_block`).
        assert_eq!(line_text(caption), "  took 4.2s");
        let style = caption.spans[1].style;
        assert_eq!(style.fg, Some(app.theme().dim));
        assert!(
            style.add_modifier.contains(Modifier::ITALIC),
            "caption is dim + italic"
        );

        // Directly under the bubble's closing border, never inside it.
        let border = line_text(&lines[lines.len() - 2]);
        assert!(
            border.starts_with('╰'),
            "caption must sit below the frame: {border}"
        );

        // And the whole thing survives the widget's render path.
        push_message(&mut app, MessageRole::User, "next");
        assert!(render(&app, 80, 30).contains("took 4.2s"));
    }

    #[test]
    fn reply_without_a_stamp_has_no_took_row() {
        let mut app = KodApp::new();
        push_message(&mut app, MessageRole::Assistant, "the answer");
        let msg = app.messages().last().unwrap();
        let lines = ChatWidget::message_lines(&app, msg, 60);
        assert!(lines.iter().all(|l| !line_text(l).contains("took ")));
        // Still a full block: the last row is the closing border.
        assert!(line_text(lines.last().unwrap()).starts_with('╰'));
    }

    #[test]
    fn caption_tiers_match_the_header_figure() {
        // The same `format_friendly_duration` the header uses, so the
        // row under a bubble and `took …` in the header never disagree.
        for (ms, want) in [(340, "340ms"), (4200, "4.2s"), (65_000, "1m05s")] {
            let mut app = KodApp::new();
            push_stamped_reply(&mut app, "x", ms);
            let msg = app.messages().last().unwrap();
            let lines = ChatWidget::message_lines(&app, msg, 60);
            assert_eq!(line_text(lines.last().unwrap()), format!("  took {want}"));
        }
    }

    #[test]
    fn render_tool_body_over_the_cap_collapses_with_a_count() {
        // TOOL_DISPLAY_LINES is the collapse point; a body with
        // more rows than that shows the summary line.
        let mut app = KodApp::new();
        let mut body = String::from("[run] a tool\n");
        for i in 0..(TOOL_DISPLAY_LINES + 5) {
            body.push_str(&format!("row {i}\n"));
        }
        push_message(&mut app, MessageRole::Tool, &body);
        let text = render(&app, 120, 80);
        assert!(
            text.contains("more lines"),
            "collapse summary missing: {text}",
        );
        assert!(text.contains("o expands"), "expand hint: {text}");
    }

    #[test]
    fn render_tool_row_with_show_tools_off_hides_non_error_rows() {
        let mut app = KodApp::new();
        push_message(&mut app, MessageRole::Tool, "[run] a tool\nbody text");
        // `toggle_show_tools` flips from true to false by default.
        app.toggle_show_tools();
        let text = render(&app, 100, 40);
        assert!(
            !text.contains("body text"),
            "hidden tool body must not render: {text}",
        );
        assert!(
            text.contains("tool output(s) hidden"),
            "hidden-tools footer missing: {text}",
        );
    }

    #[test]
    fn render_error_tool_remains_visible_when_show_tools_is_off() {
        // The contract the collapsed view documents: a `t` toggle
        // must not bury failures. Errors are never hidden.
        let mut app = KodApp::new();
        push_message(
            &mut app,
            MessageRole::Tool,
            "[run] a tool\nError: something broke",
        );
        app.toggle_show_tools();
        let text = render(&app, 100, 40);
        assert!(
            text.contains("something broke"),
            "error tool body must render even when tools are hidden: {text}",
        );
    }

    // ---- render: streaming --------------------------------------------

    #[test]
    fn render_streaming_body_appears_as_an_ai_block() {
        let mut app = KodApp::new();
        app.begin_generation();
        app.start_response_stream();
        app.add_response_chunk("partial reply");
        let text = render(&app, 80, 20);
        assert!(text.contains("partial reply"), "streaming body: {text}");
    }

    #[test]
    fn render_empty_streaming_body_renders_nothing() {
        // `start_response_stream` sets `is_streaming = true`, but
        // with no chunks the body is whitespace-only. The widget
        // must not emit an empty bubble.
        let mut app = KodApp::new();
        app.begin_generation();
        app.start_response_stream();
        let text = render(&app, 80, 10);
        assert!(
            !text.contains("ai "),
            "no empty bubble when the stream is empty: {text}",
        );
    }

    // ---- highlight_line ------------------------------------------------

    #[test]
    fn highlight_line_is_a_noop_for_an_empty_query() {
        let line = Line::from("hello world");
        let out = ChatWidget::highlight_line(line, "");
        assert_eq!(out.spans.len(), 1);
    }

    #[test]
    fn highlight_line_splits_a_matching_span() {
        let line = Line::from("hello world");
        let out = ChatWidget::highlight_line(line, "world");
        // The output is at least: "hello " + "world" (2 spans, or
        // 3 if the trailing whitespace is preserved).
        let joined: String = out.spans.iter().map(|s| s.content.as_ref()).collect();
        assert_eq!(joined, "hello world", "text preserved across the split");
        assert!(out.spans.len() >= 2, "query must split the span");
    }

    #[test]
    fn highlight_line_is_case_insensitive() {
        let line = Line::from("Hello World");
        let out = ChatWidget::highlight_line(line, "WORLD");
        let joined: String = out.spans.iter().map(|s| s.content.as_ref()).collect();
        assert_eq!(joined, "Hello World");
        assert!(out.spans.len() >= 2, "case-folded hit must split: {out:?}");
    }

    #[test]
    fn highlight_line_with_no_match_keeps_the_input() {
        let line = Line::from("hello");
        let out = ChatWidget::highlight_line(line, "absent");
        let joined: String = out.spans.iter().map(|s| s.content.as_ref()).collect();
        assert_eq!(joined, "hello");
    }

    // ---- apply_search via render ---------------------------------------

    #[test]
    fn render_highlights_a_search_hit() {
        let mut app = KodApp::new();
        push_message(&mut app, MessageRole::User, "find the needle here");
        app.begin_search();
        app.search_type('n');
        app.search_type('e');
        app.search_type('e');
        app.search_type('d');
        app.search_type('l');
        app.search_type('e');
        let text = render(&app, 100, 20);
        // The styled hit is invisible in a text-only assertion, but
        // the text must still be present. The point of this test is
        // that the search-active code path does not panic and does
        // not drop the message.
        assert!(text.contains("needle"), "search path renders: {text}");
    }

    // ---- reflow_line ---------------------------------------------------

    #[test]
    fn reflow_line_wraps_a_long_span_at_the_width() {
        let line = Line::from("abcdefghij");
        let rows = ChatWidget::reflow_line(line, 4);
        assert_eq!(rows.len(), 3, "got {} rows: {rows:?}", rows.len());
        let joined: String = rows
            .iter()
            .flat_map(|l| l.spans.iter().map(|s| s.content.as_ref()))
            .collect();
        assert_eq!(joined, "abcdefghij", "text preserved across the wrap");
    }

    #[test]
    fn reflow_line_preserves_style_per_char() {
        let line = Line::from(vec![
            Span::styled("aa", Style::default().fg(Color::Red)),
            Span::styled("bb", Style::default().fg(Color::Blue)),
        ]);
        let rows = ChatWidget::reflow_line(line, 1);
        // One char per row, styles preserved.
        assert_eq!(rows.len(), 4);
        assert_eq!(rows[0].spans[0].style.fg, Some(Color::Red));
        assert_eq!(rows[3].spans[0].style.fg, Some(Color::Blue));
    }

    // ---- §14.5 follow-up: tool preview + rule between turns --------

    #[test]
    fn render_tool_preview_shows_four_lines_by_default() {
        let mut app = KodApp::new();
        let body = (0..10)
            .map(|i| format!("row {i}"))
            .collect::<Vec<_>>()
            .join("\n");
        push_message(
            &mut app,
            MessageRole::Tool,
            &format!("[run] a tool\n{body}"),
        );
        let text = render(&app, 100, 40);
        assert!(text.contains("row 3"), "4th line visible: {text}");
        assert!(!text.contains("row 4"), "5th line collapsed: {text}");
        assert!(
            text.contains("+6 more lines (o expands)"),
            "shorter hint: {text}",
        );
    }

    #[test]
    fn render_draws_the_rule_only_before_user_turns() {
        let mut app = KodApp::new();
        push_message(&mut app, MessageRole::User, "first question");
        push_message(&mut app, MessageRole::Tool, "[read_file] path=main.rs\nok");
        push_message(&mut app, MessageRole::Assistant, "the answer");
        push_message(&mut app, MessageRole::System, "a note");
        push_message(&mut app, MessageRole::User, "second question");
        let text = render(&app, 100, 60);

        // There is no rule between the first user row and the second:
        // every row of the first turn renders unseparated. Because the
        // rule that opens the *second* turn is part of that slice, we
        // assert instead that no other rule exists: exactly one rule
        // appears in the whole render, and it precedes "you" in the
        // second-turn prefix.
        let rule_char = '\u{2500}';
        assert!(
            text.contains(rule_char),
            "a rule must separate the two turns: {text}",
        );

        // Positional check: the rule is *between* the two user turns,
        // never inside the first one. `find` both rows; the rule glyph
        // must appear after "a note" and before the second "you".
        let idx_note = text.find("a note").expect("first-turn sys row");
        let idx_second_you = text.rfind("you").expect("second user row");
        let rule_after_note = text[idx_note..idx_second_you].contains(rule_char);
        assert!(
            rule_after_note,
            "the rule must be between the turns: {text}",
        );
        let rule_before_first_you = text
            .split("first question")
            .next()
            .unwrap_or("")
            .contains(rule_char);
        assert!(
            !rule_before_first_you,
            "no rule precedes the very first user row: {text}",
        );
        // (The raw `─` count includes the assistant frame's borders;
        // positional checks above are the real assertions.)
    }
}
