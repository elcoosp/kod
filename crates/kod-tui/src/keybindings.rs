//! Configurable normal-mode keybindings.
//!
//! Single-char keys in normal mode map to actions. Defaults match the
//! built-in help; `~/.kod/tui_keys.toml` (or `.kod-keys.toml` in the
//! project) overrides individual keys:
//!
//! ```toml
//! [keys]
//! quit = "Q"
//! scroll_down = "J"
//! ```
//!
//! A corrupt or missing file silently keeps the defaults — key config must
//! never break startup.

use serde::Deserialize;
use std::collections::HashMap;

/// Actions a single normal-mode key can trigger.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum KeyAction {
    Insert,
    Quit,
    Help,
    Panel,
    ScrollUp,
    ScrollDown,
    Top,
    Bottom,
    EditLast,
    Undo,
    ToggleTools,
    SearchPrefix,
    CopyLast,
}

/// Default single-char bindings, also rendered by the help overlay.
pub fn default_bindings() -> HashMap<char, KeyAction> {
    [
        ('i', KeyAction::Insert),
        // 'I' is a legacy alias for insert (the pre-configurable TUI
        // accepted both); keep it in the default set so a user can
        // rebind or remove it, rather than hardcoding it downstream.
        ('I', KeyAction::Insert),
        ('q', KeyAction::Quit),
        ('h', KeyAction::Help),
        ('?', KeyAction::Help),
        ('a', KeyAction::Panel),
        ('j', KeyAction::ScrollDown),
        ('k', KeyAction::ScrollUp),
        ('g', KeyAction::Top),
        ('G', KeyAction::Bottom),
        ('e', KeyAction::EditLast),
        ('u', KeyAction::Undo),
        ('t', KeyAction::ToggleTools),
        ('f', KeyAction::SearchPrefix),
        ('y', KeyAction::CopyLast),
    ]
    .into_iter()
    .collect()
}

#[derive(Debug, Default, Deserialize)]
struct KeysFile {
    #[serde(default)]
    keys: HashMap<String, String>,
}

/// Load user overrides over the defaults. Unknown action names and
/// multi-char keys are ignored (first char wins for single-char values).
pub fn load_bindings() -> HashMap<char, KeyAction> {
    let mut bindings = default_bindings();
    let paths = [
        std::path::PathBuf::from(".kod-keys.toml"),
        dirs::config_dir()
            .map(|d| d.join("kod").join("tui_keys.toml"))
            .unwrap_or_default(),
    ];
    for path in paths {
        if path.as_os_str().is_empty() {
            continue;
        }
        let Ok(raw) = std::fs::read_to_string(&path) else {
            continue;
        };
        let Ok(file) = toml::from_str::<KeysFile>(&raw) else {
            continue;
        };
        for (action_name, key) in &file.keys {
            let Some(action) = parse_action(action_name) else {
                continue;
            };
            let Some(ch) = key.chars().next() else {
                continue;
            };
            // Remove the old key holding this action so bindings stay 1:1.
            bindings.retain(|_, a| *a != action);
            bindings.insert(ch, action);
        }
    }
    bindings
}

fn parse_action(name: &str) -> Option<KeyAction> {
    match name.trim().to_ascii_lowercase().as_str() {
        "insert" => Some(KeyAction::Insert),
        "quit" => Some(KeyAction::Quit),
        "help" => Some(KeyAction::Help),
        "panel" => Some(KeyAction::Panel),
        "scroll_up" => Some(KeyAction::ScrollUp),
        "scroll_down" => Some(KeyAction::ScrollDown),
        "top" => Some(KeyAction::Top),
        "bottom" => Some(KeyAction::Bottom),
        "edit_last" => Some(KeyAction::EditLast),
        "undo" => Some(KeyAction::Undo),
        "toggle_tools" => Some(KeyAction::ToggleTools),
        "search_prefix" => Some(KeyAction::SearchPrefix),
        "copy_last" => Some(KeyAction::CopyLast),
        _ => None,
    }
}

/// One-line cheat sheet for the help overlay and `/help` text.
pub fn cheat_sheet() -> &'static [(&'static str, &'static str)] {
    &[
        ("i / Esc", "type / back to commands"),
        ("Enter", "send (Ctrl+J adds a newline)"),
        ("Ctrl+J", "newline in the input box"),
        ("←/→, Ctrl+W/U", "move cursor, delete word/line"),
        ("Up/Down", "prompt history (draft kept)"),
        ("PgUp/PgDn, j/k, wheel", "scroll chat"),
        ("g / G, Home/End", "oldest / newest"),
        ("e", "edit your last message"),
        ("u", "undo a /clear"),
        ("t", "hide/show tool outputs"),
        ("Enter on tool row*", "expand full output (*chat focus)"),
        ("/search q, n/N", "find in chat"),
        ("f", "start a search"),
        ("y", "copy last assistant reply"),
        ("Tab / a", "agent panel"),
        ("?", "this help"),
        ("q", "quit (asks when busy)"),
        ("Esc / Ctrl+C", "cancel the running prompt"),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_cover_core_actions() {
        let b = default_bindings();
        assert_eq!(b.get(&'i'), Some(&KeyAction::Insert));
        assert_eq!(b.get(&'j'), Some(&KeyAction::ScrollDown));
        assert_eq!(b.get(&'k'), Some(&KeyAction::ScrollUp));
    }

    #[test]
    fn unknown_actions_rejected() {
        assert_eq!(parse_action("nope"), None);
        assert_eq!(parse_action("quit"), Some(KeyAction::Quit));
    }
}
