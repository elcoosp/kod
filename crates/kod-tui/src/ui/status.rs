//! Bottom status line — the "what now?" strip.
//!
//! Priority order: confirmation prompts > search mode > error banner >
//! offline notice > generation progress > tool activity > key hints.

use crate::app::KodApp;
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Widget;

/// Widget for the status bar, displayed just above the input
pub struct StatusWidget;

impl StatusWidget {
    pub fn new() -> Self {
        Self
    }

    pub fn render(&self, app: &KodApp, area: Rect, buf: &mut Buffer) {
        let theme = app.theme();
        let dim = Style::default().fg(theme.dim);

        // 1. Confirmation prompts win — they need an answer.
        if let Some(confirm) = app.pending_confirm() {
            let label = match confirm {
                crate::app::ConfirmKind::Clear => "Clear all messages? y = yes · n/Esc = keep",
                crate::app::ConfirmKind::Quit => {
                    "Quit with a generation running? y = quit · n/Esc = stay"
                }
            };
            Widget::render(
                Line::from(vec![Span::styled(
                    format!(" {label}"),
                    Style::default()
                        .fg(theme.warning)
                        .add_modifier(Modifier::BOLD),
                )]),
                area,
                buf,
            );
            return;
        }

        // 2. Search mode shows the live query + match count. The label
        // is built in KodApp::search_status_label so the widget cannot
        // disagree with the app about what "0 matches" means. (The
        // previous widget read (0, 0) from search_position and
        // rendered "no matches" even when no search had been run.)
        if app.is_searching() {
            Widget::render(
                Line::from(vec![Span::styled(
                    format!("{} ", app.search_status_label()),
                    Style::default()
                        .fg(theme.accent)
                        .add_modifier(Modifier::BOLD),
                )]),
                area,
                buf,
            );
            return;
        }

        // 3. Errors stay visible until the next keypress.
        if let Some(error) = app.last_error() {
            Widget::render(
                Line::from(vec![Span::styled(
                    format!(" ! {error}"),
                    Style::default().fg(theme.error),
                )]),
                area,
                buf,
            );
            return;
        }

        // 4. Offline notice with a way back.
        if app.is_offline() {
            Widget::render(
                Line::from(vec![Span::styled(
                    " offline — provider unreachable · /retry to reconnect · /model to switch ",
                    Style::default()
                        .fg(theme.error)
                        .add_modifier(Modifier::BOLD),
                )]),
                area,
                buf,
            );
            return;
        }

        // 5. Live progress beats idle hints. Single source of truth:
        //    spinner + phase_label ("thinking…" / "tool: …"). No elapsed
        //    seconds — they were duplicated across header/status and noisy.
        if app.is_generating() {
            let phase = app.phase_label().unwrap_or_else(|| "thinking…".to_string());
            Widget::render(
                Line::from(vec![
                    Span::styled(
                        format!("{} ", app.spinner_frame()),
                        Style::default()
                            .fg(theme.warning)
                            .add_modifier(Modifier::BOLD),
                    ),
                    Span::styled(phase, Style::default().fg(theme.warning)),
                    Span::styled(" · Esc cancels", dim),
                ]),
                area,
                buf,
            );
            return;
        }

        if let Some((total, done, label)) = app.active_tool() {
            Widget::render(
                Line::from(vec![Span::styled(
                    format!(" ⚙ {label} {done}/{total} "),
                    Style::default().fg(theme.tool),
                )]),
                area,
                buf,
            );
            return;
        }

        Widget::render(
            Line::from(vec![Span::styled(format!(" {} ", app.hint_line()), dim)]),
            area,
            buf,
        );
    }
}

impl Default for StatusWidget {
    fn default() -> Self {
        Self::new()
    }
}
