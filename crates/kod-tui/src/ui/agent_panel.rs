//! Agent status panel widget (D4-D5).
//!
//! Renders one row per live swarm agent: name, subtask, model,
//! worktree, tool count, status. Reads from `KodApp::swarm_agents()`
//! (fed by the Swarm* events) rather than the legacy `agents` map —
//! the latter was filled by nothing in production, so the panel
//! showed "No active agents" while a swarm was running.

use crate::app::KodApp;
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Paragraph, Widget, Wrap};

/// Widget for displaying agent status.
pub struct AgentPanelWidget;

impl AgentPanelWidget {
    pub fn new() -> Self {
        Self
    }

    pub fn render(&self, app: &KodApp, area: Rect, buf: &mut Buffer) {
        let theme = app.theme();
        let title_style = Style::default()
            .fg(theme.accent)
            .add_modifier(Modifier::BOLD);
        let dim = Style::default().fg(theme.dim);
        let label = Style::default().fg(theme.foreground);
        let error_style = Style::default()
            .fg(theme.error)
            .add_modifier(Modifier::BOLD);
        let ok_style = Style::default().fg(theme.user);
        let warn_style = Style::default().fg(theme.warning);

        let mut lines: Vec<Line> = Vec::new();
        lines.push(Line::from(vec![Span::styled("Agents", title_style)]));
        lines.push(Line::from(Span::styled(
            "─".repeat(area.width as usize),
            dim,
        )));

        // Snapshot + sort by name for stable order.
        let mut agents: Vec<(&kod_types::AgentId, &crate::app::SwarmAgentView)> =
            app.swarm_agents().iter().collect();
        agents.sort_by(|a, b| a.1.name.cmp(&b.1.name));

        if agents.is_empty() {
            lines.push(Line::from(Span::styled("No active agents.", dim)));
            lines.push(Line::from(Span::styled(
                "Run `/swarm <goal>` to start a team.",
                dim,
            )));
        } else {
            for (_id, view) in &agents {
                let (marker, marker_style) = if view.failure.is_some() {
                    ("✗ ", error_style)
                } else if view.retry_note.is_some() {
                    ("⟳ ", warn_style)
                } else if view.finished {
                    ("● ", ok_style)
                } else {
                    ("▶ ", label)
                };
                lines.push(Line::from(vec![
                    Span::styled(marker, marker_style),
                    Span::styled(
                        view.name.clone(),
                        Style::default().add_modifier(Modifier::BOLD),
                    ),
                ]));

                let sub = truncate(&view.subtask, area.width.saturating_sub(4) as usize);
                lines.push(Line::from(vec![Span::raw("  "), Span::styled(sub, dim)]));

                if let Some(m) = &view.model {
                    lines.push(Line::from(vec![
                        Span::raw("  "),
                        Span::styled("model:    ", dim),
                        Span::styled(m.clone(), label),
                    ]));
                }
                if let Some(wt) = &view.worktree {
                    let short = shorten_path(&wt.display().to_string());
                    lines.push(Line::from(vec![
                        Span::raw("  "),
                        Span::styled("worktree: ", dim),
                        Span::styled(short, label),
                    ]));
                }
                if let Some(b) = &view.branch {
                    lines.push(Line::from(vec![
                        Span::raw("  "),
                        Span::styled("branch:   ", dim),
                        Span::styled(b.clone(), label),
                    ]));
                }
                if view.tool_count > 0 {
                    lines.push(Line::from(vec![
                        Span::raw("  "),
                        Span::styled(format!("tools:    {}", view.tool_count), label),
                    ]));
                }
                if let Some(note) = &view.retry_note {
                    let n = truncate(note, area.width.saturating_sub(4) as usize);
                    lines.push(Line::from(vec![
                        Span::raw("  "),
                        Span::styled(n, warn_style),
                    ]));
                }
                if let Some(err) = &view.failure {
                    let e = truncate(err, area.width.saturating_sub(4) as usize);
                    lines.push(Line::from(vec![
                        Span::raw("  "),
                        Span::styled(e, error_style),
                    ]));
                }

                lines.push(Line::from(""));
            }
        }

        let text = ratatui::text::Text::from(lines);
        let paragraph = Paragraph::new(text).wrap(Wrap { trim: false });
        paragraph.render(area, buf);
    }
}

impl Default for AgentPanelWidget {
    fn default() -> Self {
        Self::new()
    }
}

fn truncate(s: &str, max: usize) -> String {
    if max == 0 {
        return String::new();
    }
    if s.chars().count() <= max {
        return s.to_string();
    }
    let mut out: String = s.chars().take(max.saturating_sub(1)).collect();
    out.push('…');
    out
}

/// Keep the tail of a long path: `/a/b/c/d` → `…/c/d`.
fn shorten_path(p: &str) -> String {
    const KEEP: usize = 2;
    let mut segs: Vec<&str> = p.split('/').filter(|s| !s.is_empty()).collect();
    if segs.len() <= KEEP + 1 {
        return p.to_string();
    }
    segs = segs[segs.len() - KEEP..].to_vec();
    format!("…/{}", segs.join("/"))
}
