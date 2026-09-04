//! Agent status panel widget.

use crate::app::KodApp;
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Paragraph, Widget, Wrap};

/// Widget for displaying agent status
pub struct AgentPanelWidget;

impl AgentPanelWidget {
    pub fn new() -> Self {
        Self
    }

    pub fn render(&self, app: &KodApp, area: &mut Buffer) {
        let rect = Rect {
            x: area.area.x,
            y: area.area.y,
            width: area.area.width,
            height: area.area.height,
        };

        let mut lines: Vec<Line> = Vec::new();

        lines.push(Line::from(vec![Span::styled(
            "Agents",
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
        )]));

        lines.push(Line::from("─".repeat(area.area.width as usize)));

        let agents = app.agents();
        if agents.is_empty() {
            lines.push(Line::from("No active agents"));
        } else {
            for agent in agents {
                let status_color = match agent.status.as_str() {
                    "running" => Color::Green,
                    "idle" => Color::Gray,
                    "error" => Color::Red,
                    _ => Color::White,
                };

                lines.push(Line::from(vec![
                    Span::styled("● ".to_string(), Style::default().fg(status_color)),
                    Span::styled(
                        agent.name.clone(),
                        Style::default().add_modifier(Modifier::BOLD),
                    ),
                ]));

                if !agent.capabilities.is_empty() {
                    let caps = agent.capabilities.join(", ");
                    lines.push(Line::from(format!("  caps: {}", caps)));
                }

                if let Some(task) = &agent.current_task {
                    lines.push(Line::from(format!("  task: {}", task)));
                }

                lines.push(Line::from(format!("  status: {}", agent.status)));

                lines.push(Line::from(""));
            }
        }

        lines.push(Line::from(vec![Span::styled(
            "Tool Executions",
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        )]));

        if let Some(current_tool) = app.current_tool() {
            lines.push(Line::from(format!("▶ {} (running...)", current_tool)));
        }

        let text = ratatui::text::Text::from(lines);
        let paragraph = Paragraph::new(text).wrap(Wrap { trim: false });

        paragraph.render(rect, area);
    }
}

impl Default for AgentPanelWidget {
    fn default() -> Self {
        Self::new()
    }
}
