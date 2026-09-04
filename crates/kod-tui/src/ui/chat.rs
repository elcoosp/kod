//! Chat display widget.

use crate::app::{InputMode, KodApp};
use ratatui::prelude::*;
use ratatui::widgets::{Block, Borders, Paragraph, Wrap};

/// Widget for displaying chat messages
pub struct ChatWidget {
    input_mode: InputMode,
}

impl ChatWidget {
    pub fn new() -> Self {
        Self {
            input_mode: InputMode::Normal,
        }
    }

    pub fn with_input_mode(input_mode: InputMode) -> Self {
        Self {
            input_mode,
        }
    }

    pub fn render(&self, app: &KodApp, buf: &mut Buffer) {
        let area = buf.area;

        let messages: Vec<ratatui::text::Line> = app
            .messages()
            .iter()
            .flat_map(|m| {
                let style = match m.role {
                    kod_types::MessageRole::User => Style::default().fg(Color::Yellow),
                    kod_types::MessageRole::Assistant => Style::default().fg(Color::Cyan),
                    kod_types::MessageRole::Tool => Style::default().fg(Color::Green),
                    kod_types::MessageRole::System => Style::default().fg(Color::Gray),
                    kod_types::MessageRole::Agent(_) => Style::default().fg(Color::Magenta),
                };

                vec![
                    ratatui::text::Line::from(format!("{}: {}", m.role_label(), m.content))
                        .style(style),
                    ratatui::text::Line::from(""),
                ]
            })
            .collect();

        let input_text = if self.input_mode == InputMode::Insert {
            app.current_input()
        } else {
            ""
        };

        let input_line = ratatui::text::Line::from(format!(">{}", input_text));

        let mut all_lines = messages;
        all_lines.push(input_line);

        let text = ratatui::text::Text::from(all_lines);
        let paragraph = Paragraph::new(text)
            .block(Block::default().borders(Borders::ALL).title("Chat"))
            .wrap(Wrap { trim: false });

        Widget::render(paragraph, area, buf);
    }
}

impl Default for ChatWidget {
    fn default() -> Self {
        Self::new()
    }
}
