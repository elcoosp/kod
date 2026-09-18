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

#[cfg(test)]
mod coverage_agent_panel {
    //! Agent panel render and its two private string helpers.
    //! Lives inside the source file (not `tests/ui.rs`) so `truncate`
    //! and `shorten_path` are reachable without widening their
    //! visibility for a test-only reason.
    use super::*;
    use crate::app::KodApp;
    use ratatui::buffer::Buffer;

    fn render(app: &KodApp, w: u16, h: u16) -> String {
        let area = ratatui::layout::Rect::new(0, 0, w, h);
        let mut buf = Buffer::empty(area);
        AgentPanelWidget::new().render(app, area, &mut buf);
        buf.content().iter().map(|c| c.symbol().to_string()).collect()
    }

    // ---- truncate ------------------------------------------------------

    #[test]
    fn truncate_returns_short_input_unchanged() {
        assert_eq!(truncate("hello", 10), "hello");
    }

    #[test]
    fn truncate_with_zero_max_is_empty() {
        assert_eq!(truncate("anything", 0), "");
    }

    #[test]
    fn truncate_appends_an_ellipsis_when_cut() {
        assert_eq!(truncate("abcdefghij", 4), "abc…");
    }

    #[test]
    fn truncate_counts_chars_not_bytes() {
        assert_eq!(truncate("日本語です", 3), "日本…");
    }

    // ---- shorten_path --------------------------------------------------

    #[test]
    fn shorten_path_keeps_a_short_path_verbatim() {
        assert_eq!(shorten_path("/a/b"), "/a/b");
        assert_eq!(shorten_path("/a/b/c"), "/a/b/c");
    }

    #[test]
    fn shorten_path_collapses_a_long_path_to_its_tail() {
        assert_eq!(shorten_path("/a/b/c/d/e"), "…/d/e");
    }

    #[test]
    fn shorten_path_tolerates_trailing_slashes() {
        // Empty segments are filtered, so a trailing slash does not
        // count as a segment.
        assert_eq!(shorten_path("/a/b/c/d/"), "…/c/d");
    }

    // ---- render --------------------------------------------------------

    #[test]
    fn panel_says_no_active_agents_on_a_fresh_app() {
        let app = KodApp::new();
        let text = render(&app, 40, 10);
        assert!(text.contains("No active agents"), "empty state, got: {text}");
        assert!(text.contains("/swarm"), "empty-state hint, got: {text}");
    }

    #[test]
    fn panel_renders_a_running_agent_with_its_model() {
        let mut app = KodApp::new();
        app.begin_swarm();
        let id = kod_types::AgentId::new();
        app.swarm_agent_started(
            id,
            "architect",
            "design the schema",
            Some("local/qwen2.5".to_string()),
        );
        let text = render(&app, 60, 20);
        assert!(text.contains("architect"), "agent name, got: {text}");
        assert!(
            text.contains("design the schema"),
            "subtask, got: {text}"
        );
        assert!(text.contains("local/qwen2.5"), "model, got: {text}");
    }

    #[test]
    fn panel_renders_a_worktree_and_branch_when_present() {
        let mut app = KodApp::new();
        app.begin_swarm();
        let id = kod_types::AgentId::new();
        app.swarm_agent_started(id.clone(), "coder", "write the handler", None);
        app.swarm_set_worktree(
            &id,
            std::path::PathBuf::from("/tmp/kod-work/coder-1"),
            "kod/coder-1".to_string(),
        );
        let text = render(&app, 80, 20);
        assert!(text.contains("worktree:"), "worktree label, got: {text}");
        assert!(text.contains("coder-1"), "worktree tail, got: {text}");
        assert!(text.contains("branch:"), "branch label, got: {text}");
        assert!(text.contains("kod/coder-1"), "branch value, got: {text}");
    }

    #[test]
    fn panel_marks_a_failed_agent_with_an_x() {
        let mut app = KodApp::new();
        app.begin_swarm();
        let id = kod_types::AgentId::new();
        app.swarm_agent_started(id.clone(), "tester", "run the suite", None);
        app.swarm_agent_failed(&id, "compile error in test.rs");
        let text = render(&app, 80, 20);
        assert!(text.contains("✗"), "failure marker, got: {text}");
        assert!(
            text.contains("compile error"),
            "failure text, got: {text}"
        );
    }

    #[test]
    fn panel_marks_a_finished_agent_with_a_filled_circle() {
        let mut app = KodApp::new();
        app.begin_swarm();
        let id = kod_types::AgentId::new();
        app.swarm_agent_started(id.clone(), "writer", "draft the docs", None);
        app.swarm_agent_finished(&id, "done");
        let text = render(&app, 60, 20);
        assert!(text.contains("●"), "finished marker, got: {text}");
    }
}
