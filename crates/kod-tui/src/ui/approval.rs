//! Approval dialog overlay: the write-approval prompt.
//!
//! Rendered as a centered modal on top of the chat when the engine has
//! paused a `write_file` / `patch_file` call awaiting a yes/no. The
//! diff is shown bounded (a huge rewrite cannot push the prompt off
//! the visible terminal); the y/n/Esc legend is always at the bottom.

use crate::app::KodApp;
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Paragraph, Widget, Wrap};

/// Lines of the diff shown in the dialog. Beyond this, a
/// `… and N more lines` summary is printed — the user can approve,
/// deny, or inspect the full file after the fact via the tool row.
const MAX_DIFF_LINES: usize = 40;

pub struct ApprovalWidget;

impl ApprovalWidget {
    pub fn new() -> Self {
        Self
    }

    pub fn render(&self, app: &KodApp, area: Rect, buf: &mut Buffer) {
        let Some(approval) = app.pending_approval() else {
            return;
        };
        let theme = app.theme();
        let title_style = Style::default()
            .fg(theme.warning)
            .add_modifier(Modifier::BOLD);
        let label_style = Style::default().fg(theme.accent);
        let diff_add_style = Style::default().fg(theme.user);
        let diff_del_style = Style::default().fg(theme.error);
        let diff_ctx_style = Style::default().fg(theme.foreground);

        let mut lines: Vec<Line> = Vec::new();
        lines.push(Line::from(vec![
            Span::styled("tool:    ", label_style),
            Span::styled(approval.tool_name.clone(), Style::default()),
        ]));
        lines.push(Line::from(vec![
            Span::styled("summary: ", label_style),
            Span::styled(approval.summary.clone(), Style::default()),
        ]));
        lines.push(Line::from(""));

        if let Some(diff) = &approval.diff {
            let total = diff.lines().count();
            let mut shown = 0usize;
            for l in diff.lines() {
                if shown >= MAX_DIFF_LINES {
                    break;
                }
                let style = if l.starts_with('+') {
                    diff_add_style
                } else if l.starts_with('-') {
                    diff_del_style
                } else {
                    diff_ctx_style
                };
                lines.push(Line::from(vec![Span::styled(l.to_string(), style)]));
                shown += 1;
            }
            if total > shown {
                lines.push(Line::from(vec![Span::styled(
                    format!("… and {} more line(s)", total - shown),
                    Style::default()
                        .fg(theme.dim)
                        .add_modifier(Modifier::ITALIC),
                )]));
            }
        } else {
            lines.push(Line::from(vec![Span::styled(
                "(no diff — the file does not exist yet, or the change is not text)",
                Style::default().fg(theme.dim),
            )]));
        }

        lines.push(Line::from(""));
        lines.push(Line::from(vec![
            Span::styled("y", label_style.add_modifier(Modifier::BOLD)),
            Span::styled(" approve   ", Style::default()),
            Span::styled("n / Esc", label_style.add_modifier(Modifier::BOLD)),
            Span::styled(" deny   ", Style::default()),
            Span::styled("a", label_style.add_modifier(Modifier::BOLD)),
            Span::styled(" never (session)", Style::default()),
        ]));

        let body_h = (lines.len() as u16 + 2).min(area.height);
        let body_w = 80u16.min(area.width);
        let x = area.x + area.width.saturating_sub(body_w) / 2;
        let y = area.y + area.height.saturating_sub(body_h) / 2;
        let popup = Rect::new(x, y, body_w, body_h);

        let block = Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(theme.warning))
            .title(Span::styled(" approval required ", title_style));

        Clear.render(popup, buf);
        Paragraph::new(lines)
            .block(block)
            .wrap(Wrap { trim: false })
            .render(popup, buf);
    }
}

impl Default for ApprovalWidget {
    fn default() -> Self {
        Self::new()
    }
}
