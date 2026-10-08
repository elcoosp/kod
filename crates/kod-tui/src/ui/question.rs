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

        let body_h = u16::try_from(lines.len())
            .unwrap_or(u16::MAX)
            .saturating_add(2)
            .min(area.height);
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

#[cfg(test)]
mod popup_height_tests {
    /// A popup body with more lines than `u16::MAX` must clamp to
    /// `area.height` (the `.min()` intent), not wrap to a tiny value.
    /// Pre-fix `lines.len() as u16` truncated: `200_000 as u16` is
    /// 34464, so `+ 2` then `.min(area.height)` returned 34466, not
    /// `area.height`. On any terminal smaller than 34466 the popup
    /// rendered off-screen.
    #[test]
    fn popup_height_saturates_instead_of_wrapping() {
        let area_height = 40u16;
        // Small bodies: h = n + 2, not clamped by area_height.
        assert_eq!(
            u16::try_from(5)
                .unwrap_or(u16::MAX)
                .saturating_add(2)
                .min(area_height),
            7
        );
        // Bodies larger than area_height: h must equal area_height.
        // The pre-fix `n as u16` truncated in the 65534..=131070
        // window, so the clamp never fired.
        for n in [41usize, 65533, 65534, 65535, 65536, 100_000, usize::MAX] {
            let h = u16::try_from(n)
                .unwrap_or(u16::MAX)
                .saturating_add(2)
                .min(area_height);
            assert_eq!(h, area_height, "n={n} produced h={h}");
        }
    }
}
