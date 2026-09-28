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
                Style::default()
                    .fg(theme.error)
                    .add_modifier(Modifier::BOLD),
            ));
        }

        // Context meter: one figure, colored only when it matters. The
        // old label carried a trailing percentage duplicating the k/k
        // fraction, and a separate "ctx nearly full" badge said in
        // words what the red color already says.
        let usage = app.context_usage();
        let ctx_style = if usage > 0.85 {
            Style::default()
                .fg(theme.error)
                .add_modifier(Modifier::BOLD)
        } else if usage > 0.70 {
            Style::default().fg(theme.warning)
        } else {
            dim
        };
        spans.push(Span::styled(
            format!(" ctx ≈{}/{} ", app.context_tokens_k(), app.context_limit_k()),
            ctx_style,
        ));

        // USD cost, when the endpoint carries a `[pricing]` block. Not
        // shown at all when pricing is not configured — a fake
        // `$0.0000` teaches the user that the figure is meaningless.
        if app.cost_known() {
            spans.push(Span::styled(
                format!(" {} ", format_cost(app.session_cost_usd())),
                dim,
            ));
        }

        // The active goal identifies the work; it renders before the
        // state badges so narrow terminals clip the badges first.
        if let Some(goal) = app.goal() {
            const MAX_GOAL_DISPLAY_CHARS: usize = 40;
            let shown = if goal.chars().count() > MAX_GOAL_DISPLAY_CHARS {
                let truncated: String = goal.chars().take(MAX_GOAL_DISPLAY_CHARS).collect();
                format!("{truncated}…")
            } else {
                goal.to_string()
            };
            spans.push(Span::styled(
                format!(" ◉ {shown}"),
                Style::default().fg(Color::Magenta),
            ));
        }

        // Network indicator: visible whenever the effective network
        // access is enabled. Default is off, so this marks a wider
        // blast radius — it stays.
        if app.network_access_enabled() {
            spans.push(Span::styled(
                " net:on ",
                Style::default()
                    .fg(theme.warning)
                    .add_modifier(Modifier::BOLD),
            ));
        }

        // Sandbox badge: `off` is rendered as nothing (an absence of a
        // sandbox is the quiet default); `require-missing` and an
        // active backend (bwrap/landlock/...) stay visible.
        let sandbox = app.sandbox_label();
        if !sandbox.is_empty() && sandbox != "off" {
            let (style, label) = if sandbox == "require-missing" {
                (
                    Style::default()
                        .fg(theme.error)
                        .add_modifier(Modifier::BOLD),
                    " sandbox:require-missing ".to_string(),
                )
            } else {
                (
                    Style::default().fg(theme.user).add_modifier(Modifier::BOLD),
                    format!(" sandbox:{sandbox} "),
                )
            };
            spans.push(Span::styled(label, style));
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

/// Format a USD amount with adaptive precision.
///
/// A local model configured with a very cheap price (a tenth of a
/// cent per million tokens) would round to `$0.00` under two
/// decimals; the same figure shown to four decimals is honest about
/// the fact that a session's cost is, so far, negligibly small. The
/// inverse — printing `$0.0000000` for a real fifty-cent session —
/// is equally bad. Three tiers:
///
/// - `< $0.01` → 4 decimals (`$0.0025`)
/// - `< $1.00` → 3 decimals (`$0.245`)
/// - `>= $1.00` → 2 decimals (`$3.14`)
fn format_cost(usd: f64) -> String {
    if usd < 0.01 {
        format!("${usd:.4}")
    } else if usd < 1.0 {
        format!("${usd:.3}")
    } else {
        format!("${usd:.2}")
    }
}

#[cfg(test)]
mod tests {
    use super::format_cost;

    #[test]
    fn format_cost_picks_precision_by_magnitude() {
        assert_eq!(format_cost(0.0), "$0.0000");
        assert_eq!(format_cost(0.0025), "$0.0025");
        assert_eq!(format_cost(0.0099), "$0.0099");
        assert_eq!(format_cost(0.01), "$0.010");
        assert_eq!(format_cost(0.245), "$0.245");
        assert_eq!(format_cost(0.999), "$0.999");
        assert_eq!(format_cost(1.0), "$1.00");
        assert_eq!(format_cost(2.5), "$2.50");
        assert_eq!(format_cost(1234.5), "$1234.50");
    }
}
