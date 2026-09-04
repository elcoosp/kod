//! Header widget for the TUI.

use crate::app::KodApp;
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Paragraph, Widget};

/// Widget for the header bar
pub struct HeaderWidget;

impl HeaderWidget {
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

        let spans = vec![
            Span::styled(
                " KOD ",
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::raw(format!(" Mode: {:?} ", app.mode())),
            Span::raw(format!("Input: {:?} ", app.input_mode())),
            Span::raw(format!("Messages: {} ", app.messages().len())),
        ];

        let line = Line::from(spans);
        let text = ratatui::text::Text::from(vec![line]);

        let paragraph = Paragraph::new(text).style(Style::default().bg(Color::DarkGray));

        paragraph.render(rect, area);
    }
}

impl Default for HeaderWidget {
    fn default() -> Self {
        Self::new()
    }
}
