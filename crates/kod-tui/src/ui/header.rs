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

        // Context meter with a near-limit warning (rough estimate).
        let usage = app.context_usage();
        let ctx_style = if usage > 0.85 {
            Style::default()
                .fg(theme.error)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(theme.warning)
        };
        spans.push(Span::styled(
            format!(" {} ", app.context_label()),
            ctx_style,
        ));
        if usage > 0.85 {
            spans.push(Span::styled("ctx nearly full ", ctx_style));
        }

        spans.push(Span::styled(
            format!(" {} ", app.accounting_label()),
            Style::default().fg(theme.dim),
        ));

        // USD cost, when the endpoint carries a `[pricing]` block.
        // Not shown at all when pricing is not configured — a fake
        // `$0.0000` on an endpoint whose pricing we do not know is
        // worse than showing nothing, because it teaches the user
        // that the figure is meaningless.
        if app.cost_known() {
            let usd = app.session_cost_usd();
            let formatted = format_cost(usd);
            spans.push(Span::styled(
                format!(" {formatted} "),
                Style::default().fg(theme.dim),
            ));
        }

        spans.push(Span::styled(
            format!("[{}]", app.theme_name()),
            Style::default().fg(theme.dim),
        ));

        // Network indicator: visible whenever the effective network
        // access is enabled. The default is off (llm.network_access =
        // false), so this is a positive signal — a user who enabled
        // web_fetch sees the badge and is reminded of the wider blast
        // radius.
        if app.network_access_enabled() {
            spans.push(Span::styled(
                " net:on ",
                Style::default()
                    .fg(theme.warning)
                    .add_modifier(Modifier::BOLD),
            ));
        }

        // Show the active goal text, not a fake counter. The previous
        // header rendered `◉ 0/1` whenever a goal was set — the goal
        // loop runs turns inside `process_goal_streaming` and the TUI
        // has no visibility into them, so the `0/1` never changed.
        // Rendering the actual goal text is useful (a glance tells you
        // what the session is working toward) and honest.
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

        let line = Line::from(spans);
        Widget::render(line, area, buf);
    }
}

impl Default for HeaderWidget {
    fn default() -> Self {
        Self::new()
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
        assert_eq!(format_cost(3.14159), "$3.14");
        assert_eq!(format_cost(1234.5), "$1234.50");
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
