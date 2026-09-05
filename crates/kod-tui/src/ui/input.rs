//! Bottom input box — multiline aware.
//!
//! Enter sends; Ctrl+J / Shift+Enter inserts a newline. The box grows with
//! the content (up to the layout clamp) and draws the cursor marker at the
//! real (line, col) position.

use crate::app::{InputMode, KodApp};
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{Block, Borders, Paragraph, Widget, Wrap};

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

        let mut display: Vec<Line> = Vec::new();
        if app.input().is_empty() {
            let hint = if *app.input_mode() == InputMode::Insert {
                "Type a message, / for commands… (Ctrl+J newline)"
            } else {
                "Press i to type · / for commands · ? help · q to quit"
            };
            display.push(Line::from(vec![
                Span::styled(
                    "❯ ",
                    Style::default()
                        .fg(theme.accent)
                        .add_modifier(Modifier::BOLD),
                ),
                Span::styled(hint, Style::default().fg(theme.dim)),
            ]));
        } else {
            // Render each input line with the ❯ prompt on the first row and
            // a cursor block at the real cursor position.
            let (cur_line, cur_col) = app.cursor_line_col();
            let show_cursor = *app.input_mode() == InputMode::Insert;
            // Split keeping a trailing empty line so "a\n" shows two rows.
            let mut rows: Vec<&str> = app.input().split('\n').collect();
            if app.input().ends_with('\n') {
                rows.push("");
            }
            for (i, row) in rows.iter().enumerate() {
                let prompt = if i == 0 { "❯ " } else { "… " };
                let mut spans = vec![Span::styled(
                    prompt,
                    Style::default()
                        .fg(theme.accent)
                        .add_modifier(Modifier::BOLD),
                )];
                if show_cursor && i == cur_line {
                    let chars: Vec<char> = row.chars().collect();
                    let col = cur_col.min(chars.len());
                    let before: String = chars[..col].iter().collect();
                    spans.push(Span::raw(before));
                    spans.push(Span::styled(
                        "▌",
                        Style::default()
                            .fg(Color::White)
                            .add_modifier(Modifier::BOLD),
                    ));
                    let after: String = chars[col..].iter().collect();
                    spans.push(Span::raw(after));
                } else {
                    spans.push(Span::raw(row.to_string()));
                }
                display.push(Line::from(spans));
            }
        }

        let block = Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(border))
            .title(Span::styled(
                title,
                Style::default().fg(border).add_modifier(Modifier::BOLD),
            ));
        let inner = block.inner(area);
        let paragraph = Paragraph::new(Text::from(display)).wrap(Wrap { trim: false });
        block.render(area, buf);
        paragraph.render(inner, buf);
    }
}

impl Default for InputWidget {
    fn default() -> Self {
        Self::new()
    }
}
