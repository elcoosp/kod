//! Input widget for text entry.

use crate::app::KodApp;
use ratatui::prelude::*;
use ratatui::widgets::{Block, Borders, Paragraph};

/// Widget for the input line
pub struct InputWidget;

impl InputWidget {
    pub fn new() -> Self {
        Self
    }

    pub fn render(&self, app: &KodApp, buf: &mut Buffer) {
        let area = buf.area;

        let input = app.current_input();
        let prompt = match app.input_mode() {
            crate::app::InputMode::Insert => "> ",
            crate::app::InputMode::Normal => "# ",
        };

        let text = ratatui::text::Text::from(format!("{prompt}{input}"));

        let paragraph = Paragraph::new(text).block(Block::default().borders(Borders::ALL).title("Input"));

        Widget::render(paragraph, area, buf);
    }
}

impl Default for InputWidget {
    fn default() -> Self {
        Self::new()
    }
}
