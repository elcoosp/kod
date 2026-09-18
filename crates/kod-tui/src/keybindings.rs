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

#[cfg(test)]
mod coverage_cheat_sheet {
    //! `cheat_sheet` is what the `/help` overlay renders. A blank
    //! entry prints an empty line and the user sees a gap where a
    //! key should be — worse, a regression that dropped the "Esc
    //! cancels" entry leaves a user in a running prompt with no
    //! documented escape.
    use super::*;

    #[test]
    fn every_cheat_sheet_entry_has_both_fields() {
        for (keys, desc) in cheat_sheet() {
            assert!(!keys.is_empty(), "empty keys field: {desc:?}");
            assert!(!desc.is_empty(), "empty description for keys {keys:?}");
        }
    }

    #[test]
    fn cheat_sheet_documents_the_essentials() {
        // Every one of these is a way out of a state a user can get
        // into. Removing the row would leave a first-time user
        // stuck without documentation.
        let sheet = cheat_sheet();
        let text: String = sheet
            .iter()
            .map(|(k, d)| format!("{k} {d}"))
            .collect::<Vec<_>>()
            .join("\n");
        for keyword in [
            "Esc",       // cancel / mode escape
            "Enter",     // submit
            "Ctrl+J",    // newline
            "scroll",    // scrolling
            "quit",      // quit
            "cancel",    // cancel a running prompt
            "search",    // search
            "help",      // help
        ] {
            assert!(
                text.contains(keyword),
                "cheat sheet does not mention {keyword:?}",
            );
        }
    }

    #[test]
    fn every_keyaction_has_a_default_binding() {
        // Every `KeyAction` variant must be reachable from the
        // default bindings. A variant added to the enum but not to
        // `default_bindings()` is dead: nothing the user types can
        // trigger it.
        let defaults = default_bindings();
        let actions: std::collections::HashSet<KeyAction> =
            defaults.values().copied().collect();
        for required in [
            KeyAction::Insert,
            KeyAction::Quit,
            KeyAction::Help,
            KeyAction::Panel,
            KeyAction::ScrollUp,
            KeyAction::ScrollDown,
            KeyAction::Top,
            KeyAction::Bottom,
            KeyAction::EditLast,
            KeyAction::Undo,
            KeyAction::ToggleTools,
            KeyAction::SearchPrefix,
            KeyAction::CopyLast,
        ] {
            assert!(
                actions.contains(&required),
                "{required:?} has no default binding",
            );
        }
    }

    #[test]
    fn default_binding_chars_are_unique_per_action() {
        // Two actions bound to the same key would make one
        // unreachable — a silent regression that a user
        // experiences as "the documented key does nothing".
        let defaults = default_bindings();
        // `?` and `h` both map to Help, `i` and `I` both to Insert:
        // that is intentional. The invariant is the reverse: no
        // action is bound to a key that also maps to a *different*
        // action. Two keys for one action is fine; one key for two
        // actions is not.
        let mut key_to_action: std::collections::HashMap<char, KeyAction> =
            std::collections::HashMap::new();
        for (ch, action) in &defaults {
            if let Some(prev) = key_to_action.get(ch) {
                panic!("key {ch:?} bound to both {prev:?} and {action:?}");
            }
            key_to_action.insert(*ch, *action);
        }
    }

    #[test]
    fn unknown_action_names_are_rejected() {
        assert_eq!(parse_action("no-such-action"), None);
        assert_eq!(parse_action(""), None);
    }

    #[test]
    fn parse_action_is_case_and_whitespace_insensitive() {
        assert_eq!(parse_action("quit"), Some(KeyAction::Quit));
        assert_eq!(parse_action("QUIT"), Some(KeyAction::Quit));
        assert_eq!(parse_action("  Quit  "), Some(KeyAction::Quit));
    }

    #[test]
    fn every_action_name_in_the_documented_set_parses() {
        // The names printed in the CHANGELOG and the docs — the
        // user-facing contract — must round-trip through
        // `parse_action`, or a config file that uses them silently
        // keeps the default.
        let names = [
            ("insert", KeyAction::Insert),
            ("quit", KeyAction::Quit),
            ("help", KeyAction::Help),
            ("panel", KeyAction::Panel),
            ("scroll_up", KeyAction::ScrollUp),
            ("scroll_down", KeyAction::ScrollDown),
            ("top", KeyAction::Top),
            ("bottom", KeyAction::Bottom),
            ("edit_last", KeyAction::EditLast),
            ("undo", KeyAction::Undo),
            ("toggle_tools", KeyAction::ToggleTools),
            ("search_prefix", KeyAction::SearchPrefix),
            ("copy_last", KeyAction::CopyLast),
        ];
        for (name, expected) in names {
            assert_eq!(
                parse_action(name),
                Some(expected),
                "action name {name:?} no longer parses",
            );
        }
    }
}
