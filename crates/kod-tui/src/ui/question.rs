//! Question dialog overlay: the ask_user prompt.
//!
//! Centered modal on top of the chat when the agent has called
//! `ask_user` and is waiting for a text answer. Enter submits,
//! Backspace edits, Esc cancels.

use crate::app::KodApp;
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Paragraph, Widget, Wrap};

pub struct QuestionWidget;

impl QuestionWidget {
    pub fn new() -> Self {
        Self
    }

    pub fn render(&self, app: &KodApp, area: Rect, buf: &mut Buffer) {
        let Some(q) = app.pending_question() else {
            return;
        };
        let theme = app.theme();
        let title = Style::default()
            .fg(theme.accent)
            .add_modifier(Modifier::BOLD);
        let dim = Style::default().fg(theme.dim);

        let mut lines = vec![Line::from(vec![Span::styled(
            q.question.clone(),
            Style::default(),
        )])];
        if let Some(hint) = &q.placeholder {
            lines.push(Line::from(vec![Span::styled(format!("hint: {hint}"), dim)]));
        }
        lines.push(Line::from(""));
        lines.push(Line::from(vec![
            Span::styled("> ", Style::default().fg(theme.accent)),
            Span::styled(app.question_input().to_string(), Style::default()),
            Span::styled(
                "▌",
                Style::default()
                    .fg(theme.accent)
                    .add_modifier(Modifier::BOLD),
            ),
        ]));
        lines.push(Line::from(""));
        lines.push(Line::from(vec![Span::styled(
            "Enter submits · Esc cancels",
            dim,
        )]));

        let body_h = (lines.len() as u16 + 2).min(area.height);
        let body_w = 70u16.min(area.width);
        let x = area.x + area.width.saturating_sub(body_w) / 2;
        let y = area.y + area.height.saturating_sub(body_h) / 2;
        let popup = Rect::new(x, y, body_w, body_h);

        let block = Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(theme.accent))
            .title(Span::styled(" question ", title));

        Clear.render(popup, buf);
        Paragraph::new(lines)
            .block(block)
            .wrap(Wrap { trim: false })
            .render(popup, buf);
    }
}

impl Default for QuestionWidget {
    fn default() -> Self {
        Self::new()
    }
}
