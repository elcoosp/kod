//! Approval dialog overlay: the write-approval prompt.
//!
//! Rendered as a centered modal on top of the chat when the engine has
//! paused one or more `write_file` / `patch_file` / `execute_command`
//! calls awaiting a yes/no. The items of a round are shown one at a
//! time with `n of N` in the title, and a compact list at the top so
//! the user knows how many decisions are left.
//!
//! A batch of one item renders exactly like the pre-batch single-item
//! modal: same layout, same diff preview, same key legend. The batch
//! machinery is only visible when there is more than one decision to
//! make.

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

/// Items listed in the header of a multi-item batch before the list is
/// truncated. A batch of more than a handful of writes is rare, and
/// the current item is always shown in full below.
const MAX_LISTED_ITEMS: usize = 6;

pub struct ApprovalWidget;

impl ApprovalWidget {
    pub fn new() -> Self {
        Self
    }

    pub fn render(&self, app: &KodApp, area: Rect, buf: &mut Buffer) {
        let Some(batch) = app.pending_batch() else {
            return;
        };
        let Some(current) = batch.current_item() else {
            // The batch is exhausted but not yet cleared (a race
            // between decision dispatch and state cleanup). Render
            // nothing rather than a stale dialog.
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
        let dim_style = Style::default().fg(theme.dim);

        let mut lines: Vec<Line> = Vec::new();

        // When the batch has more than one item, list them all at the
        // top so the user knows what is coming. The current item is
        // marked with `▸`.
        let total = batch.items.len();
        if total > 1 {
            for (i, item) in batch.items.iter().take(MAX_LISTED_ITEMS).enumerate() {
                let marker = if i == batch.current { "▸" } else { " " };
                let style = if i == batch.current {
                    Style::default()
                        .fg(theme.accent)
                        .add_modifier(Modifier::BOLD)
                } else {
                    dim_style
                };
                lines.push(Line::from(vec![
                    Span::styled(format!(" {marker} {:>2}. ", i + 1), style),
                    Span::styled(item.tool_name.clone(), style),
                    Span::styled(format!("  ({})", truncate(&item.summary, 40)), dim_style),
                ]));
            }
            if total > MAX_LISTED_ITEMS {
                lines.push(Line::from(Span::styled(
                    format!("    … and {} more", total - MAX_LISTED_ITEMS),
                    dim_style,
                )));
            }
            lines.push(Line::from(""));
        }

        lines.push(Line::from(vec![
            Span::styled("tool:    ", label_style),
            Span::styled(current.tool_name.clone(), Style::default()),
        ]));
        lines.push(Line::from(vec![
            Span::styled("summary: ", label_style),
            Span::styled(current.summary.clone(), Style::default()),
        ]));
        lines.push(Line::from(""));

        if let Some(diff) = &current.diff {
            let total_lines = diff.lines().count();
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
            if total_lines > shown {
                lines.push(Line::from(vec![Span::styled(
                    format!("… and {} more line(s)", total_lines - shown),
                    Style::default()
                        .fg(theme.dim)
                        .add_modifier(Modifier::ITALIC),
                )]));
            }
        } else {
            lines.push(Line::from(vec![Span::styled(
                "(no diff — the file does not exist yet, or the change is not text)",
                dim_style,
            )]));
        }

        lines.push(Line::from(""));
        if total > 1 {
            lines.push(Line::from(vec![
                Span::styled("y", label_style.add_modifier(Modifier::BOLD)),
                Span::styled(" approve  ", Style::default()),
                Span::styled("n", label_style.add_modifier(Modifier::BOLD)),
                Span::styled(" deny  ", Style::default()),
                Span::styled("l", label_style.add_modifier(Modifier::BOLD)),
                Span::styled(" always  ", Style::default()),
                Span::styled("a", label_style.add_modifier(Modifier::BOLD)),
                Span::styled(" never  ", Style::default()),
                Span::styled("l", label_style.add_modifier(Modifier::BOLD)),
                Span::styled(" learn  ", Style::default()),
                Span::styled("e", label_style.add_modifier(Modifier::BOLD)),
                Span::styled(" edit  ", Style::default()),
                Span::styled("↑/↓", label_style.add_modifier(Modifier::BOLD)),
                Span::styled(" navigate  ", Style::default()),
                Span::styled("Esc", label_style.add_modifier(Modifier::BOLD)),
                Span::styled(" deny all remaining", Style::default()),
            ]));
        } else {
            lines.push(Line::from(vec![
                Span::styled("y", label_style.add_modifier(Modifier::BOLD)),
                Span::styled(" approve   ", Style::default()),
                Span::styled("n / Esc", label_style.add_modifier(Modifier::BOLD)),
                Span::styled(" deny   ", Style::default()),
                Span::styled("l", label_style.add_modifier(Modifier::BOLD)),
                Span::styled(" always   ", Style::default()),
                Span::styled("a", label_style.add_modifier(Modifier::BOLD)),
                Span::styled(" never (session)   ", Style::default()),
                Span::styled("l", label_style.add_modifier(Modifier::BOLD)),
                Span::styled(" learn   ", Style::default()),
                Span::styled("e", label_style.add_modifier(Modifier::BOLD)),
                Span::styled(" edit args", Style::default()),
            ]));
        }

        let title_text = if total > 1 {
            format!(" approval required ({} of {}) ", batch.current + 1, total)
        } else {
            " approval required ".to_string()
        };

        let body_h = (lines.len() as u16 + 2).min(area.height);
        let body_w = 84u16.min(area.width);
        let x = area.x + area.width.saturating_sub(body_w) / 2;
        let y = area.y + area.height.saturating_sub(body_h) / 2;
        let popup = Rect::new(x, y, body_w, body_h);

        let block = Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(theme.warning))
            .title(Span::styled(title_text, title_style));

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

/// Truncate to `max` chars, appending `…` when cut.
fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let mut out: String = s.chars().take(max.saturating_sub(1)).collect();
    out.push('…');
    out
}
