//! Top bar of the TUI — connects the engine state to visible feedback.
//!
//! Shows model + provider, live generation phase with elapsed time,
//! connection state (offline retries included), context usage with a
//! near-limit warning, and the active theme name.

use crate::app::KodApp;
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Widget;

/// Widget for the application header, displayed at the top of the terminal
pub struct HeaderWidget;

impl HeaderWidget {
    pub fn new() -> Self {
        Self
    }

    pub fn render(&self, app: &KodApp, area: Rect, buf: &mut Buffer) {
        let theme = app.theme();
        let title_style = Style::default()
            .fg(theme.accent)
            .add_modifier(Modifier::BOLD);
        let dim = Style::default().fg(theme.dim);

        let mut spans = vec![
            Span::styled(" kod ", title_style),
            Span::styled(format!("{} ", app.model_label()), dim),
        ];

        if app.is_offline() {
            spans.push(Span::styled(
                format!(" offline ×{} ", app.consecutive_failures()),
                Style::default().fg(theme.error).add_modifier(Modifier::BOLD),
            ));
        }

        // Context meter with a near-limit warning (rough estimate).
        let usage = app.context_usage();
        let ctx_style = if usage > 0.85 {
            Style::default().fg(theme.error).add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(theme.warning)
        };
        spans.push(Span::styled(format!(" {} ", app.context_label()), ctx_style));
        if usage > 0.85 {
            spans.push(Span::styled("ctx nearly full ", ctx_style));
        }

        spans.push(Span::styled(
            format!("[{}]", app.theme_name()),
            Style::default().fg(theme.dim),
        ));

        let goal = match app.goal_progress() {
            Some((done, total)) => format!(" ◉ {done}/{total}"),
            None => String::new(),
        };
        if !goal.is_empty() {
            spans.push(Span::styled(goal, Style::default().fg(Color::Magenta)));
        }

        let line = Line::from(spans);
        Widget::render(line, area, buf);
    }
}

impl Default for HeaderWidget {
    fn default() -> Self {
        Self::new()
    }
}
