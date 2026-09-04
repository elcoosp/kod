//! Chat message display widget.

use crate::app::{KodApp, Message};
use kod_types::MessageRole;
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Color, Style};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{Paragraph, Widget, Wrap};

/// Widget for displaying chat messages
pub struct ChatWidget;

impl ChatWidget {
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

        let mut lines: Vec<Line> = Vec::new();

        let messages: Vec<&Message> = app.messages().iter().collect();

        for message in messages {
            let (prefix, style) = match message.role {
                MessageRole::User => (
                    "[You] ",
                    Style::default().fg(Color::Green),
                ),
                MessageRole::Assistant => (
                    "[AI] ",
                    Style::default().fg(Color::Cyan),
                ),
                MessageRole::System => (
                    "[System] ",
                    Style::default().fg(Color::Yellow),
                ),
                MessageRole::Tool => (
                    "[Tool] ",
                    Style::default().fg(Color::Magenta),
                ),
                MessageRole::Agent(_) => (
                    "[Agent] ",
                    Style::default().fg(Color::Blue),
                ),
            };

            let timestamp = message.timestamp.format("%H:%M:%S");

            lines.push(Line::from(vec![
                Span::styled(
                    format!("{} ", timestamp),
                    Style::default().fg(Color::DarkGray),
                ),
                Span::styled(prefix.to_string(), style),
            ]));

            let content_lines: Vec<&str> = message.content.lines().collect();
            for (i, line) in content_lines.iter().enumerate() {
                if i == 0 {
                    lines.push(Line::from(vec![
                        Span::raw("  "),
                        Span::styled(*line, style),
                    ]));
                } else {
                    lines.push(Line::from(format!("  {}", line)));
                }
            }

            lines.push(Line::from(""));
        }

        if app.is_streaming() {
            lines.push(Line::from(vec![
                Span::styled("[AI] ", Style::default().fg(Color::Cyan)),
                Span::styled("(streaming...)", Style::default().fg(Color::DarkGray)),
            ]));

            for line in app.current_response().lines() {
                lines.push(Line::from(format!("  {}", line)));
            }
        }

        let text = Text::from(lines);
        let paragraph = Paragraph::new(text)
            .wrap(Wrap { trim: false });

        paragraph.render(rect, area);
    }
}

impl Default for ChatWidget {
    fn default() -> Self {
        Self::new()
    }
}
