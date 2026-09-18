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
        let mut message_line_offsets: std::collections::HashMap<kod_types::MessageId, usize> =
            std::collections::HashMap::new();
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
        buf.content().iter().map(|c| c.symbol().to_string()).collect()
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
        push_message(&mut app, MessageRole::Agent(kod_types::AgentId::new()), "agent reply");
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
        push_message(
            &mut app,
            MessageRole::Tool,
            "[run] a tool\nbody text",
        );
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
}
