//! Color themes for the TUI.
//!
//! The default (`dark`) keeps the exact palette the TUI has always used so
//! existing users see no change; `light` is a higher-contrast variant for
//! bright terminals. A `theme.toml` next to the kod config (`theme = "light"`
//! or a `[theme.colors]` table) overrides the default.

use ratatui::style::Color;

/// Full palette consumed by the widgets. Every color has a fixed default so
/// a partial `theme.toml` still renders sensibly.
#[derive(Debug, Clone)]
pub struct Theme {
    pub name: String,
    pub background: Color,
    pub foreground: Color,
    pub assistant: Color,
    pub user: Color,
    pub system: Color,
    pub tool: Color,
    pub accent: Color,
    pub warning: Color,
    pub error: Color,
    pub dim: Color,
    pub code: Color,
    pub keyword: Color,
}

impl Theme {
    pub fn dark() -> Self {
        Self {
            name: "dark".to_string(),
            background: Color::Reset,
            foreground: Color::White,
            assistant: Color::Cyan,
            user: Color::Green,
            system: Color::Yellow,
            tool: Color::Magenta,
            accent: Color::Cyan,
            warning: Color::Yellow,
            error: Color::Red,
            dim: Color::DarkGray,
            code: Color::Yellow,
            keyword: Color::Magenta,
        }
    }

    pub fn light() -> Self {
        Self {
            name: "light".to_string(),
            background: Color::White,
            foreground: Color::Black,
            assistant: Color::Blue,
            user: Color::Green,
            system: Color::Yellow,
            tool: Color::Magenta,
            accent: Color::Blue,
            warning: Color::Red,
            error: Color::Red,
            dim: Color::Gray,
            code: Color::DarkGray,
            keyword: Color::Blue,
        }
    }

    pub fn from_name(name: &str) -> Self {
        match name.trim().to_ascii_lowercase().as_str() {
            "light" => Self::light(),
            _ => Self::dark(),
        }
    }

    /// Best-effort load: `theme = "light"` (or a full `[theme]` table) in the
    /// kod config dir, or a `.kod-theme.toml` in the current project.
    /// Anything unreadable falls back to `dark` — theming must never break
    /// startup.
    pub fn load() -> Self {
        let paths: [std::path::PathBuf; 2] = [
            std::path::PathBuf::from(".kod-theme.toml"),
            dirs::config_dir().map(|d| d.join("kod").join("theme.toml")).unwrap_or_default(),
        ];
        for path in &paths {
            if path.as_os_str().is_empty() {
                continue;
            }
            if let Ok(raw) = std::fs::read_to_string(path) {
                if let Ok(parsed) = toml::from_str::<ThemeFile>(&raw) {
                    return parsed.into_theme();
                }
                // Also accept a bare `theme = "light"` line.
                if let Ok(simple) = toml::from_str::<SimpleTheme>(&raw) {
                    return Self::from_name(&simple.theme);
                }
            }
        }
        Self::dark()
    }
}

#[derive(Debug, serde::Deserialize)]
struct SimpleTheme {
    #[serde(default)]
    theme: String,
}

#[derive(Debug, serde::Deserialize)]
struct ThemeFile {
    #[serde(default)]
    theme: Option<String>,
    #[serde(default)]
    colors: Option<ThemeColors>,
}

#[derive(Debug, Default, serde::Deserialize)]
struct ThemeColors {
    background: Option<String>,
    foreground: Option<String>,
    assistant: Option<String>,
    user: Option<String>,
    system: Option<String>,
    tool: Option<String>,
    accent: Option<String>,
    warning: Option<String>,
    error: Option<String>,
    dim: Option<String>,
    code: Option<String>,
    keyword: Option<String>,
}

impl ThemeFile {
    fn into_theme(self) -> Theme {
        let mut theme = Theme::from_name(self.theme.as_deref().unwrap_or("dark"));
        if let Some(c) = self.colors {
            let parse = |s: &Option<String>, cur: Color| {
                s.as_deref().and_then(parse_color).unwrap_or(cur)
            };
            theme.background = parse(&c.background, theme.background);
            theme.foreground = parse(&c.foreground, theme.foreground);
            theme.assistant = parse(&c.assistant, theme.assistant);
            theme.user = parse(&c.user, theme.user);
            theme.system = parse(&c.system, theme.system);
            theme.tool = parse(&c.tool, theme.tool);
            theme.accent = parse(&c.accent, theme.accent);
            theme.warning = parse(&c.warning, theme.warning);
            theme.error = parse(&c.error, theme.error);
            theme.dim = parse(&c.dim, theme.dim);
            theme.code = parse(&c.code, theme.code);
            theme.keyword = parse(&c.keyword, theme.keyword);
        }
        theme
    }
}

fn parse_color(s: &str) -> Option<Color> {
    match s.trim().to_ascii_lowercase().as_str() {
        "black" => Some(Color::Black),
        "red" => Some(Color::Red),
        "green" => Some(Color::Green),
        "yellow" => Some(Color::Yellow),
        "blue" => Some(Color::Blue),
        "magenta" => Some(Color::Magenta),
        "cyan" => Some(Color::Cyan),
        "white" => Some(Color::White),
        "gray" | "grey" => Some(Color::Gray),
        "darkgray" | "dark-gray" | "dark_grey" => Some(Color::DarkGray),
        s if s.starts_with('#') && s.len() == 7 => {
            let r = u8::from_str_radix(&s[1..3], 16).ok()?;
            let g = u8::from_str_radix(&s[3..5], 16).ok()?;
            let b = u8::from_str_radix(&s[5..7], 16).ok()?;
            Some(Color::Rgb(r, g, b))
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dark_is_default_palette() {
        let t = Theme::dark();
        assert_eq!(t.assistant, Color::Cyan);
        assert_eq!(t.name, "dark");
    }

    #[test]
    fn unknown_name_falls_back_to_dark() {
        assert_eq!(Theme::from_name("neon").name, "dark");
        assert_eq!(Theme::from_name("light").name, "light");
    }

    #[test]
    fn parses_named_and_hex_colors() {
        assert_eq!(parse_color("cyan"), Some(Color::Cyan));
        assert_eq!(parse_color("#ff0000"), Some(Color::Rgb(255, 0, 0)));
        assert_eq!(parse_color("nope"), None);
    }
}
