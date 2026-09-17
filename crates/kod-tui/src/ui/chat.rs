//! Chat message display widget.
//!
//! Messages always render oldest-first (stable sort by timestamp), newest
//! pinned to the live bottom unless the user scrolled back. Assistant
//! replies — finished or still streaming — render inside a rounded border
//! so live text never restyles or jumps when the response completes.

use crate::app::{KodApp, Message};
use kod_types::MessageRole;
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::Text;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Paragraph, Widget, Wrap};

/// Tool body rows shown before collapsing to `… and N more`.
pub const TOOL_DISPLAY_LINES: usize = 12;

/// Widget for displaying chat messages
pub struct ChatWidget;

impl ChatWidget {
    pub fn new() -> Self {
        Self
    }

    /// Split `s` into display rows of at most `width` cells, breaking long
    /// rows mid-word exactly like `Paragraph` with `Wrap { trim: false }`.
    fn wrap_text(s: &str, width: usize) -> Vec<String> {
        let width = width.max(1);
        let mut rows = Vec::new();
        for raw in s.split('\n') {
            let mut cur = String::new();
            let mut cur_w = 0;
            for ch in raw.chars() {
                let w = Span::raw(ch.to_string()).width().max(1);
                if cur_w + w > width && !cur.is_empty() {
                    rows.push(std::mem::take(&mut cur));
                    cur_w = 0;
                }
                cur.push(ch);
                cur_w += w;
            }
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
    fn assistant_block(
        content: &str,
        width: usize,
        _style: Style,
        app: &KodApp,
    ) -> Vec<Line<'static>> {
        let theme = app.theme();
        let frame = Style::default().fg(theme.accent);
        let title_style = Style::default()
            .fg(theme.accent)
            .add_modifier(Modifier::BOLD);
        let inner = width.saturating_sub(4).max(1); // "│ " + content + " │"

        // Markdown rendering (D6.5). Headers, code fences, lists,
        // blockquotes, inline code, bold, and italic get their own
        // styling; paragraphs are reflowed to the inner width. The
        // renderer owns the wrapping, so no per-line `reflow_line`
        // call is needed — the markdown module is the single place
        // that decides where a line breaks.
        let body: Vec<Line<'static>> = crate::markdown::render(content, inner, theme);

        if width < 20 {
            let mut lines = vec![Line::from(vec![Span::styled("ai ", title_style)])];
            for row in body {
                let mut spans = vec![Span::raw("  ")];
                spans.extend(row.spans);
                lines.push(Line::from(spans));
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
        lines
    }

    /// Reflow one styled line into rows of at most `width` cells, keeping
    /// each char's style. Breaks mid-word like the plain wrapper so mixed
    /// Markdown/code rows measure exactly.
    #[allow(dead_code)]
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
                let rows: Vec<String> = Self::wrap_text(&rest, width.saturating_sub(4).max(1));
                let expanded = app.is_tool_expanded(&message.id);
                let shown = if expanded {
                    rows.len()
                } else {
                    rows.len().min(TOOL_DISPLAY_LINES)
                };
                for row in rows.iter().take(shown) {
                    lines.push(Line::from(vec![
                        Span::raw("    "),
                        Span::styled(row.clone(), body_style),
                    ]));
                }
                if rows.len() > shown {
                    lines.push(Line::from(vec![
                        Span::raw("    "),
                        Span::styled(
                            format!(
                                "… and {} more lines — o expands, t hides tools",
                                rows.len() - shown
                            ),
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
            let lines = Self::assistant_block(&message.content, width, assistant_style, app);
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
        for row in Self::wrap_text(&message.content, width.saturating_sub(2).max(1)) {
            lines.push(Line::from(vec![Span::raw("  "), Span::styled(row, style)]));
        }
        Self::apply_search(app, lines)
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

    pub fn render(&self, app: &KodApp, area: Rect, buf: &mut Buffer) {
        // Strict insertion order via monotonic sequence — never by
        // wall-clock timestamp (which can collide or go backwards after
        // a restore). Stable sort preserves file order for equal seq.
        let mut ordered: Vec<&Message> = app.messages().iter().collect();
        ordered.sort_by_key(|m| m.sequence);

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
            let mut est_lines: Vec<Line> = Vec::new();
            for (i, m) in ordered.iter().enumerate() {
                if app.search_query().is_none() && !app.show_tools() && m.role == MessageRole::Tool
                {
                    let body = m.content.split_once('\n').map(|x| x.1).unwrap_or("");
                    if !body.trim_start().starts_with("Error:") {
                        continue;
                    }
                }
                if i > 0 && est_lines.last().map(|l: &Line| l.width()).unwrap_or(1) > 0 {
                    est_lines.push(Line::from(vec![Span::styled(
                        "─".repeat(narrow_width.min(120)),
                        Style::default().fg(app.theme().dim),
                    )]));
                }
                est_lines.extend(Self::message_lines(app, m, narrow_width));
            }
            if hidden > 0 {
                est_lines.push(Line::from(vec![Span::styled(
                    format!("⋯ {hidden} tool output(s) hidden — t to show"),
                    Style::default().fg(app.theme().dim),
                )]));
            }
            // stream body must outlive est_lines lines (assistant_block borrows it)
            let stream_probe = crate::app::KodApp::trim_blank_lines(app.current_response());
            if app.is_streaming() && !stream_probe.is_empty() {
                est_lines.extend(Self::assistant_block(
                    &stream_probe,
                    narrow_width,
                    Style::default().fg(app.theme().assistant),
                    app,
                ));
            }
            if est_lines.is_empty() {
                est_lines.push(Line::from(""));
            }
            let text = Text::from(est_lines);
            #[allow(unstable_name_collisions)]
            let visual = Paragraph::new(text)
                .wrap(Wrap { trim: false })
                .line_count(narrow_width as u16);
            visual > height
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
        let mut message_line_offsets: std::collections::HashMap<
            kod_types::MessageId,
            usize,
        > = std::collections::HashMap::new();
        for (i, message) in ordered.iter().enumerate() {
            if app.search_query().is_none()
                && !app.show_tools()
                && message.role == MessageRole::Tool
            {
                // Errors are never hidden — a collapsed `t` must not bury failures.
                let body = message.content.split_once('\n').map(|x| x.1).unwrap_or("");
                let is_error = body.trim_start().starts_with("Error:");
                if !is_error {
                    hidden_tools += 1;
                    continue;
                }
            }
            if i > 0 && lines.last().map(|l| l.width()).unwrap_or(1) > 0 {
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

        // Visual row count must match Ratatui's own wrapper, not a
        // hand-rolled div_ceil. A single logical line can expand to N
        // visual rows when Wrap is on, and our old wrap_text drifted from
        // WordWrapper on wide graphemes/word boundaries. Ask Paragraph
        // for the true count.
        #[allow(unstable_name_collisions)]
        let total_rows = {
            let text = Text::from(lines.clone());
            Paragraph::new(text)
                .wrap(Wrap { trim: false })
                .line_count(text_width as u16)
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
                    .line_count(text_width as u16) as usize;
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
