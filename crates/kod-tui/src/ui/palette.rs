//! Command palette overlay (Ctrl+K).
//!
//! A floating list of every slash command plus a small set of
//! key-only actions. Typing filters; Up/Down move; Enter accepts
//! (inserts the command into the input box); Escape closes.

use crate::app::KodApp;
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, List, ListItem, ListState, Widget};

/// The command palette widget.
pub struct PaletteWidget;

impl PaletteWidget {
    pub fn new() -> Self {
        Self
    }

    /// Should the palette be rendered?
    pub fn should_show(app: &KodApp) -> bool {
        app.is_palette_open()
    }

    /// Popup height: one line per candidate (bounded to 12) plus
    /// two for the border and one for the query line.
    pub fn height(app: &KodApp) -> u16 {
        let n = app.palette_candidates().len().min(12) as u16;
        (n + 4).min(20)
    }

    pub fn render(&self, app: &KodApp, area: Rect, buf: &mut Buffer) {
        let theme = app.theme();
        let candidates = app.palette_candidates();
        let query = app.palette_query().unwrap_or("");
        let selected = app.palette_selected();

        let items: Vec<ListItem> = candidates
            .iter()
            .map(|e| {
                ListItem::new(Line::from(vec![
                    Span::styled(
                        format!(" {:<24} ", e.label),
                        Style::default()
                            .fg(theme.accent)
                            .add_modifier(Modifier::BOLD),
                    ),
                    Span::styled(e.hint.clone(), Style::default().fg(theme.dim)),
                ]))
            })
            .collect();

        let mut state = ListState::default();
        if !items.is_empty() {
            state.select(Some(selected % items.len()));
        }

        let title = if query.is_empty() {
            " palette (Ctrl+K) ".to_string()
        } else {
            format!(" palette: {query} ")
        };
        let block = Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(theme.accent))
            .title(Span::styled(
                title,
                Style::default()
                    .fg(theme.accent)
                    .add_modifier(Modifier::BOLD),
            ))
            .title_bottom(Span::styled(
                " type to filter · ⏎ accept · Esc close ",
                Style::default().fg(theme.dim),
            ));

        let list = List::new(items)
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

impl Default for PaletteWidget {
    fn default() -> Self {
        Self::new()
    }
}
