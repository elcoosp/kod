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
            Self::row(&key, &normal, "Enter", "send · Ctrl+J / Shift+Enter newline"),
            Self::row(&key, &normal, "Up/Down, Tab", "history · cycle completions"),
            Self::row(&key, &normal, "Ctrl+U/K/W", "clear line · cut to end · cut word"),
            Self::row(&key, &normal, "Ctrl+Left/Right", "jump by word"),
            Line::from(""),
            Line::from(vec![Span::styled("Chat", title)]),
            Self::row(&key, &normal, "j/k · g/G · PgUp/PgDn", "scroll (also arrows, wheel)"),
            Self::row(&key, &normal, "t / o", "toggle tool outputs · expand newest"),
            Self::row(&key, &normal, "/ then n/N", "search next/prev match"),
            Self::row(&key, &normal, "y", "copy last assistant reply"),
            Self::row(&key, &normal, "u", "undo a /clear"),
            Line::from(""),
            Line::from(vec![Span::styled("Session", title)]),
            Self::row(
                &key,
                &normal,
                "/retry · /theme · /model · /quit",
                "reconnect · switch theme · switch model · quit",
            ),
            Self::row(&key, &normal, "Esc", "cancel generation · close this help"),
            Line::from(vec![Span::styled(
                "Full command list: type / and Tab-complete · mouse wheel scrolls · Option/Shift+drag selects text",
                dim,
            )]),
        ];
        let block = Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(theme.accent))
            .title(Span::styled(" help — Esc closes ", title));
        let inner_h = (lines.len() as u16 + 2).min(area.height);
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
