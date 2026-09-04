//! Input widget.

use crate::app::{InputMode, KodApp};
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Paragraph, Widget};

/// Widget for user input
pub struct InputWidget;

impl InputWidget {
    pub fn new() -> Self {
        Self
    }

    pub fn render(&self, app: &KodApp, area: &mut Buffer) {
        let rect = Rect {
            x: area.area.x,
            y: area.area.y,
            width: area.area.width,
            height: area.area.height,
        };

        let (title, style) = match app.input_mode() {
            InputMode::Normal => (" Normal ", Style::default().fg(Color::Blue)),
            InputMode::Insert => (" Input ", Style::default().fg(Color::Green)),
        };

        let _title_span = Span::styled(title, style.add_modifier(Modifier::BOLD));

        let mut spans = vec![Span::styled("❯ ", Style::default().fg(Color::Cyan))];

        if app.input().is_empty() {
            if *app.input_mode() == InputMode::Insert {
                spans.push(Span::styled(
                    "Type your message...",
                    Style::default().fg(Color::DarkGray),
                ));
            } else {
                spans.push(Span::styled(
                    "Press 'i' to enter input mode",
                    Style::default().fg(Color::DarkGray),
                ));
            }
        } else {
            spans.push(Span::raw(app.input().to_string()));

            if *app.input_mode() == InputMode::Insert {
                spans.push(Span::styled(
                    "▌",
                    Style::default()
                        .fg(Color::White)
                        .add_modifier(Modifier::BOLD),
                ));
            }
        }

        let line = Line::from(spans);
        let text = ratatui::text::Text::from(vec![line]);

        let paragraph = Paragraph::new(text);

        paragraph.render(rect, area);
    }
}

impl Default for InputWidget {
    fn default() -> Self {
        Self::new()
    }
}
