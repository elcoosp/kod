//! Header widget for the TUI.

use crate::app::KodApp;
use ratatui::prelude::*;
use ratatui::widgets::{Block, Borders, Paragraph};

/// Widget for the header bar
pub struct HeaderWidget;

impl HeaderWidget {
    pub fn new() -> Self {
        Self
    }

    pub fn render(&self, app: &KodApp, buf: &mut Buffer) {
        let area = buf.area;
        let text = ratatui::text::Text::from(format!(
            "KOD - Mode: {:?} | Agents: {}",
            app.mode(),
            app.agents().len()
        ));

        let paragraph = Paragraph::new(text).block(Block::default().borders(Borders::ALL).title("KOD"));

        Widget::render(paragraph, area, buf);
    }
}

impl Default for HeaderWidget {
    fn default() -> Self {
        Self::new()
    }
}
