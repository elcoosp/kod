//! Full-screen help overlay (`?` or `/help`) — every key and command.
//!
//! Rendered as a centered popup so the chat stays visible around it.
//! The content is static text; the key list mirrors `keybindings.rs`
//! defaults and the command list mirrors `SLASH_COMMANDS`.

use crate::app::KodApp;
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Paragraph, Widget, Wrap};

/// Centered help overlay listing keys and slash commands
pub struct HelpWidget;

impl HelpWidget {
    pub fn new() -> Self {
        Self
    }

    /// Centered rect taking `w`×`h` of the screen.
    pub fn centered(area: Rect, w: u16, h: u16) -> Rect {
        let w = w.min(area.width);
        let h = h.min(area.height);
        let x = area.x + area.width.saturating_sub(w) / 2;
        let y = area.y + area.height.saturating_sub(h) / 2;
        Rect::new(x, y, w, h)
    }

    pub fn render(&self, app: &KodApp, area: Rect, buf: &mut Buffer) {
        let theme = app.theme();
        let title = Style::default()
            .fg(theme.accent)
            .add_modifier(Modifier::BOLD);
        let key = Style::default()
            .fg(theme.accent)
            .add_modifier(Modifier::BOLD);
        let dim = Style::default().fg(theme.dim);
        let normal = Style::default().fg(theme.foreground);

        let lines = vec![
            Line::from(vec![Span::styled("Modes", title)]),
            Self::row(&key, &normal, "i / Esc", "enter insert / back to normal"),
            Self::row(&key, &normal, "?", "this help (also /help, F1)"),
            Line::from(""),
            Line::from(vec![Span::styled("Typing", title)]),
            Self::row(
                &key,
                &normal,
                "Enter",
                "send · Ctrl+J / Shift+Enter newline",
            ),
            Self::row(&key, &normal, "Up/Down, Tab", "history · cycle completions"),
            Self::row(&key, &normal, "Shift+Up/Down", "scroll chat while typing"),
            Self::row(
                &key,
                &normal,
                "Ctrl+K/U/W",
                "clear line · cut to end · cut word",
            ),
            Self::row(&key, &normal, "Ctrl+Left/Right", "jump by word"),
            Line::from(""),
            Line::from(vec![Span::styled("Chat", title)]),
            Self::row(&key, &normal, "wheel · j/k · PgUp/PgDn", "scroll the chat"),
            Self::row(&key, &normal, "m · y", "select-mode+drag · copy reply"),
            Self::row(
                &key,
                &normal,
                "t / o",
                "toggle tool outputs · expand newest",
            ),
            Self::row(&key, &normal, "/ then n/N", "search next/prev match"),
            Self::row(&key, &normal, "u", "undo a /clear"),
            Line::from(vec![Span::styled("Session", title)]),
            Self::row(&key, &normal, "/retry · /theme", "reconnect · switch theme"),
            Self::row(&key, &normal, "/model · /quit", "switch model · quit"),
            Self::row(&key, &normal, "Esc", "cancel generation · close this help"),
            Line::from(vec![Span::styled(
                "Commands: type / + Tab · `m` select-mode · `y` copy reply",
                dim,
            )]),
        ];
        let block = Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(theme.accent))
            .title(Span::styled(" help — Esc closes ", title));
        let inner_h = u16::try_from(lines.len())
            .unwrap_or(u16::MAX)
            .saturating_add(2)
            .min(area.height);
        let popup = Self::centered(area, 64.min(area.width), inner_h);
        Clear.render(popup, buf);
        Paragraph::new(lines)
            .block(block)
            .wrap(Wrap { trim: false })
            .render(popup, buf);
    }

    fn row(key: &Style, normal: &Style, keys: &str, desc: &str) -> Line<'static> {
        Line::from(vec![
            Span::styled(format!("  {keys:<24}"), *key),
            Span::styled(desc.to_string(), *normal),
        ])
    }
}

impl Default for HelpWidget {
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
