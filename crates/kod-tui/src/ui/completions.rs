//! Completion popup — floats above the input box.
//!
//! Shows slash commands, model names, or file paths depending on what the
//! cursor is completing. The selected row highlights; `active_completion_*`
//! on the app decides the exact list so Tab-cycling and the popup agree.

use crate::app::{CompletionKind, KodApp};
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, List, ListItem, ListState, Widget};

/// Popup widget showing available completions above the input
pub struct CompletionsWidget;

impl CompletionsWidget {
    pub fn new() -> Self {
        Self
    }

    pub fn should_show(app: &KodApp) -> bool {
        app.active_completion_len() > 0
    }

    pub fn height(app: &KodApp) -> u16 {
        (app.active_completion_len().min(8) + 2).min(10) as u16
    }

    pub fn render(&self, app: &KodApp, area: Rect, buf: &mut Buffer) {
        let theme = app.theme();
        let (title, items): (String, Vec<(String, String)>) = match app.active_completion_kind() {
            CompletionKind::Slash => (
                " commands ".to_string(),
                app.completion_candidates()
                    .iter()
                    .map(|c| (c.name.to_string(), c.hint.to_string()))
                    .collect(),
            ),
            CompletionKind::Model => (
                " models ".to_string(),
                app.model_candidates()
                    .iter()
                    .map(|m| (m.to_string(), "available model".to_string()))
                    .collect(),
            ),
            CompletionKind::Path => (
                " paths ".to_string(),
                app.path_candidates()
                    .iter()
                    .map(|p| (p.clone(), "path".to_string()))
                    .collect(),
            ),
            CompletionKind::None => return,
        };
        if items.is_empty() {
            return;
        }

        let list_items: Vec<ListItem> = items
            .iter()
            .map(|(name, desc)| {
                ListItem::new(Line::from(vec![
                    Span::styled(
                        format!(" {name} "),
                        Style::default()
                            .fg(theme.accent)
                            .add_modifier(Modifier::BOLD),
                    ),
                    Span::styled(desc.clone(), Style::default().fg(theme.dim)),
                ]))
            })
            .collect();

        let mut state = ListState::default();
        state.select(Some(app.completion_index() % items.len()));

        let block = Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(theme.accent))
            .title(Span::styled(
                title,
                Style::default()
                    .fg(theme.accent)
                    .add_modifier(Modifier::BOLD),
            ));
        let list = List::new(list_items)
            .block(block)
            .highlight_style(
                Style::default()
                    .bg(theme.accent)
                    .fg(theme.background)
                    .add_modifier(Modifier::BOLD),
            )
            .highlight_symbol("▶ ");

        Clear.render(area, buf);
        ratatui::widgets::StatefulWidget::render(list, area, buf, &mut state);
    }
}

impl Default for CompletionsWidget {
    fn default() -> Self {
        Self::new()
    }
}
