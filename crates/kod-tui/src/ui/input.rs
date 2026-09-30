//! Bottom input box — grows with content, scrolls to the cursor.
//!
//! Enter sends; Ctrl+J / Shift+Enter inserts a newline. The box height
//! comes from the same [`KodApp::input_view`] builder the layout uses,
//! so what fits is what shows. When content overflows the box (or a
//! single line overflows its width), the view scrolls to keep the
//! cursor visible — typing never disappears under the fold. Large
//! pastes collapse to a `[Pasted N lines]` chip (full text still
//! submits).

use crate::app::{InputMode, KodApp};
use crate::theme::Theme;
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph, Widget};

/// Widget for user input, always docked at the bottom of the screen
pub struct InputWidget;

impl InputWidget {
    pub fn new() -> Self {
        Self
    }

    pub fn render(&self, app: &KodApp, area: Rect, buf: &mut Buffer) {
        let theme = app.theme();
        let (title, border) = match app.input_mode() {
            InputMode::Normal => (" Normal · i to type ", Color::Blue),
            InputMode::Insert if app.is_multiline_input() => {
                (" Input · Enter sends · Ctrl+J newline ", theme.accent)
            }
            InputMode::Insert => (" Input ", Color::Green),
        };

        let block = Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(border))
            .title(Span::styled(
                title,
                Style::default().fg(border).add_modifier(Modifier::BOLD),
            ));
        let inner = block.inner(area);
        block.render(area, buf);
        if inner.width == 0 || inner.height == 0 {
            return;
        }
        let inner_w = inner.width as usize;

        let show_cursor = *app.input_mode() == InputMode::Insert;
        let mut lines: Vec<Line> = Vec::new();
        if app.input().is_empty() {
            let hint = if show_cursor {
                "Type a message, / for commands… (Ctrl+J newline)"
            } else {
                "Press i to type · / for commands · ? help · q to quit"
            };
            let mut spans = vec![Span::styled(
                "❯ ",
                Style::default()
                    .fg(theme.accent)
                    .add_modifier(Modifier::BOLD),
            )];
            if show_cursor {
                spans.push(Span::styled(
                    "▌",
                    Style::default()
                        .fg(Color::White)
                        .add_modifier(Modifier::BOLD),
                ));
            }
            spans.push(Span::styled(hint, Style::default().fg(theme.dim)));
            lines.push(Line::from(spans));
        } else {
            let view = app.input_view(inner_w);
            let total = view.rows.len();
            let vis_h = inner.height as usize;
            // Cursor-anchored scroll: typing at the end pins the tail;
            // an edited earlier row stays visible instead.
            let start = if total <= vis_h {
                0
            } else {
                let max_start = total - vis_h;
                view.cursor_row
                    .saturating_sub(vis_h.saturating_sub(1))
                    .min(max_start)
            };
            for row in view.rows.iter().skip(start).take(vis_h.max(1)) {
                let prompt = if row.logical == 0 && row.first {
                    "❯ "
                } else if row.first {
                    "… "
                } else {
                    ""
                };
                let mut spans = vec![Span::styled(
                    prompt,
                    Style::default()
                        .fg(theme.accent)
                        .add_modifier(Modifier::BOLD),
                )];
                // Horizontal scroll, cursor-anchored: the cursor sits
                // at the right edge rather than running off it. Only
                // the cursor row scrolls; other rows render from col 0
                // (ratatui clips right overflow).
                let cursor_here = row.cursor_cell.filter(|_| show_cursor);
                let (body, cursor_at, chips) = match cursor_here {
                    Some(cc) => {
                        let off = cc.saturating_sub(inner_w.saturating_sub(1));
                        let (sliced, consumed) = slice_cells(&row.text, off, inner_w);
                        // Chips yield to the cursor on its row.
                        (sliced, Some(cc.saturating_sub(consumed)), Vec::new())
                    }
                    None => (row.text.clone(), None, row.chips.clone()),
                };
                spans.extend(styled_row(&body, cursor_at, &chips, theme));
                lines.push(Line::from(spans));
            }
        }

        // Rows arrive pre-wrapped to the inner width: no Paragraph
        // wrap, so wrapping can never misalign the cursor math.
        Paragraph::new(lines).render(inner, buf);
    }
}

impl Default for InputWidget {
    fn default() -> Self {
        Self::new()
    }
}

/// Split a row's text into styled spans: dim chip ranges, raw text
/// elsewhere, and the cursor block at its cell offset. `cursor_at`
/// and chip ranges are coords within `text`.
fn styled_row(
    text: &str,
    cursor_at: Option<usize>,
    chips: &[(usize, usize)],
    theme: &Theme,
) -> Vec<Span<'static>> {
    let chars: Vec<char> = text.chars().collect();
    // Cell offset of every char boundary; the cursor always sits on
    // one (it maps from a char index upstream).
    let mut boundary: Vec<usize> = Vec::with_capacity(chars.len() + 1);
    let mut acc = 0;
    boundary.push(0);
    for &c in &chars {
        acc += char_cells(c);
        boundary.push(acc);
    }
    let cursor_at = cursor_at.map(|c| c.min(acc));

    let mut out: Vec<Span<'static>> = Vec::new();
    let mut buf = String::new();
    let mut buf_dim = false;
    let flush = |out: &mut Vec<Span<'static>>, buf: &mut String, dim: bool| {
        if buf.is_empty() {
            return;
        }
        let s = std::mem::take(buf);
        out.push(if dim {
            Span::styled(s, Style::default().fg(theme.dim))
        } else {
            Span::raw(s)
        });
    };
    let mut i = 0;
    while i <= chars.len() {
        if cursor_at == Some(boundary[i]) {
            flush(&mut out, &mut buf, buf_dim);
            out.push(Span::styled(
                "▌",
                Style::default()
                    .fg(Color::White)
                    .add_modifier(Modifier::BOLD),
            ));
            // A cursor exactly on a chip edge belongs to the cursor.
            buf_dim = false;
            if i == chars.len() {
                break;
            }
        } else if i == chars.len() {
            break;
        }
        let want_dim = chips.iter().any(|&(s, e)| i >= s && i < e);
        if want_dim != buf_dim {
            flush(&mut out, &mut buf, buf_dim);
            buf_dim = want_dim;
        }
        buf.push(chars[i]);
        i += 1;
    }
    flush(&mut out, &mut buf, buf_dim);
    out
}

/// Take up to `take` cells of `s` starting at cell `skip`, dropping a
/// leading char straddled by the skip boundary. Returns the slice and
/// the cells consumed before it (for re-basing a cursor offset).
fn slice_cells(s: &str, skip: usize, take: usize) -> (String, usize) {
    let mut out = String::new();
    let mut cell = 0;
    let mut consumed = 0;
    let mut started = skip == 0;
    for c in s.chars() {
        let w = char_cells(c);
        if !started {
            if cell + w <= skip {
                cell += w;
                continue;
            }
            if cell < skip {
                // Straddles the boundary: drop it, keep counting from
                // the true origin so later cells stay aligned.
                cell += w;
                consumed = cell;
                started = true;
                continue;
            }
            consumed = cell;
            started = true;
        }
        if cell - consumed + w > take {
            break;
        }
        out.push(c);
        cell += w;
    }
    if !started {
        consumed = cell;
    }
    (out, consumed)
}

fn char_cells(c: char) -> usize {
    let u = c as u32;
    if u < 0x20 || (0x7F..0xA0).contains(&u) {
        0
    } else if matches!(u,
        0x1100..=0x115F
        | 0x2E80..=0x303E
        | 0x3041..=0x33FF
        | 0x3400..=0x4DBF
        | 0x4E00..=0x9FFF
        | 0xA000..=0xA4CF
        | 0xAC00..=0xD7AF
        | 0xF900..=0xFAFF
        | 0xFE30..=0xFE4F
        | 0xFF00..=0xFF60
        | 0xFFE0..=0xFFE6
        | 0x1F300..=0x1F64F
        | 0x1F680..=0x1FAFF
        | 0x20000..=0x2FFFD
        | 0x30000..=0x3FFFD
    ) {
        2
    } else {
        1
    }
}
