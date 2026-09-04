//! Agent panel widget.

use crate::app::KodApp;
use ratatui::prelude::*;
use ratatui::widgets::{Block, Borders, List, ListItem};

/// Widget for displaying agent information
pub struct AgentPanelWidget;

impl AgentPanelWidget {
    pub fn new() -> Self {
        Self
    }

    pub fn render(&self, app: &KodApp, buf: &mut Buffer) {
        let area = buf.area;

        let items: Vec<ListItem> = app
            .agents_map()
            .iter()
            .map(|(name, info)| {
                ListItem::new(format!(
                    "{} ({}) - {}",
                    name,
                    info.status,
                    info.current_task.clone().unwrap_or_default()
                ))
            })
            .collect();

        let list = List::new(items).block(Block::default().borders(Borders::ALL).title("Agents"));

        Widget::render(list, area, buf);
    }
}

impl Default for AgentPanelWidget {
    fn default() -> Self {
        Self::new()
    }
}
