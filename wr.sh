#!/usr/bin/env bash
set -uo pipefail

run_with_timeout() {
    local secs="$1"; shift
    if command -v timeout >/dev/null 2>&1; then
        timeout "$secs" "$@"; return $?
    fi
    if command -v gtimeout >/dev/null 2>&1; then
        gtimeout "$secs" "$@"; return $?
    fi
    "$@" &
    local pid=$!
    ( sleep "$secs"
      if kill -0 "$pid" 2>/dev/null; then
          kill -TERM "$pid" 2>/dev/null
          sleep 2
          kill -KILL "$pid" 2>/dev/null
      fi ) &
    local watchdog=$!
    wait "$pid"; local rc=$?
    kill "$watchdog" 2>/dev/null; wait "$watchdog" 2>/dev/null
    [ "$rc" -ge 128 ] && return 124
    return "$rc"
}

COMPILE_OK=true
INCOMPLETE=false
KEYS=crates/kod-tui/src/keybindings.rs
LOOP=crates/kod-tui/src/main_loop.rs

for f in "$KEYS" "$LOOP"; do
    if [ ! -f "$f" ]; then
        echo "ERROR: missing $f — run from the kod workspace root"
        exit 1
    fi
done

echo "Wiring configurable keybindings into handle_normal_mode_key"

python3 - "$KEYS" "$LOOP" << 'PYEOF'
import os
import sys

keys_path, loop_path = sys.argv[1], sys.argv[2]

def patch(path, old, new, label, expect=1):
    with open(path, "r") as f:
        content = f.read()
    n = content.count(old)
    if n == 0:
        print(f"ERROR: old snippet not found in {path}: {label}")
        sys.exit(2)
    if expect and n != expect:
        print(f"ERROR: expected {expect} occurrence(s) of {label} in {path}, found {n}")
        sys.exit(2)
    patched = content.replace(old, new, expect if expect else n)
    tmp = path + ".tmp"
    with open(tmp, "w") as f:
        f.write(patched)
    os.replace(tmp, path)
    print(f"Patched {path}: {label}")

# ========================================================================
# 1. keybindings.rs: add 'I' as an alias for Insert in defaults
# ========================================================================
patch(
    keys_path,
    '''    [
        ('i', KeyAction::Insert),
        ('q', KeyAction::Quit),''',
    '''    [
        ('i', KeyAction::Insert),
        // 'I' is a legacy alias for insert (the pre-configurable TUI
        // accepted both); keep it in the default set so a user can
        // rebind or remove it, rather than hardcoding it downstream.
        ('I', KeyAction::Insert),
        ('q', KeyAction::Quit),''',
    "'I' alias in default bindings",
)

# ========================================================================
# 2. main_loop.rs: imports
# ========================================================================
patch(
    loop_path,
    '''use crate::{
    app::{AppMode, ConfirmKind, InputMode, KodApp},
    event::{Event, EventHandler, KeyCode},
    theme::Theme,''',
    '''use crate::{
    app::{AppMode, ConfirmKind, InputMode, KodApp},
    event::{Event, EventHandler, KeyCode},
    keybindings::{KeyAction, load_bindings},
    theme::Theme,''',
    "import KeyAction + load_bindings",
)

# ========================================================================
# 3. main_loop.rs: TuiLoop struct gains a bindings field
# ========================================================================
patch(
    loop_path,
    '''pub struct TuiLoop {
    app: KodApp,
    event_handler: EventHandler,
    terminal: Option<Terminal<CrosstermBackend<Stdout>>>,
    engine: Option<Arc<KodEngine>>,
    llm_config: Option<LlmConfig>,
    /// Background generation task. Kept so Esc / Ctrl+C / `/cancel` can
    /// abort it; cleared when the response (or error) lands.
    gen_task: Option<tokio::task::JoinHandle<()>>,
}''',
    '''pub struct TuiLoop {
    app: KodApp,
    event_handler: EventHandler,
    terminal: Option<Terminal<CrosstermBackend<Stdout>>>,
    engine: Option<Arc<KodEngine>>,
    llm_config: Option<LlmConfig>,
    /// Background generation task. Kept so Esc / Ctrl+C / `/cancel` can
    /// abort it; cleared when the response (or error) lands.
    gen_task: Option<tokio::task::JoinHandle<()>>,
    /// Char → action map for normal-mode single-key commands. Loaded at
    /// construction from `~/.config/kod/tui_keys.toml` and a project-
    /// local `.kod-keys.toml` (see [`crate::keybindings`]); a missing or
    /// corrupt file falls back to the built-in defaults. Tests override
    /// it via [`TuiLoop::set_keybindings`].
    keybindings: std::collections::HashMap<char, KeyAction>,
}''',
    "TuiLoop bindings field",
)

# ========================================================================
# 4. main_loop.rs: TuiLoop::new loads bindings
# ========================================================================
patch(
    loop_path,
    '''    pub fn new() -> Self {
        Self {
            app: KodApp::new(),
            event_handler: EventHandler::new(Duration::from_millis(100)),
            terminal: None,
            engine: None,
            llm_config: None,
            gen_task: None,
        }
    }''',
    '''    pub fn new() -> Self {
        Self {
            app: KodApp::new(),
            event_handler: EventHandler::new(Duration::from_millis(100)),
            terminal: None,
            engine: None,
            llm_config: None,
            gen_task: None,
            keybindings: load_bindings(),
        }
    }

    /// Replace the active keybinding map. Used by tests; production
    /// callers load the map once in [`TuiLoop::new`].
    pub fn set_keybindings(&mut self, bindings: std::collections::HashMap<char, KeyAction>) {
        self.keybindings = bindings;
    }''',
    "load_bindings + set_keybindings",
)

# ========================================================================
# 5. main_loop.rs: rewrite handle_normal_mode_key
# ========================================================================
old_handler = '''    /// Handle keys in normal mode
    async fn handle_normal_mode_key(&mut self, key: KeyCode) -> Result<()> {
        // Touch keybindings module so it stays wired (configurable bindings)
        let _ = crate::keybindings::default_bindings();
        match key {
            KeyCode::Char('i') | KeyCode::Char('I') => {
                self.app.set_input_mode(InputMode::Insert);
            }
            KeyCode::Char('q') => {
                if self.app.is_generating() {
                    self.app.request_confirm(ConfirmKind::Quit);
                } else {
                    self.app.quit();
                }
            }
            KeyCode::Char('j') => self.app.scroll_down(1),
            KeyCode::Char('k') => self.app.scroll_up(1),
            KeyCode::Char('g') => self.app.scroll_to_top(),
            KeyCode::Char('G') => self.app.scroll_to_bottom(),
            KeyCode::Char('e') => {
                self.app.edit_last_message();
                self.app.set_input_mode(InputMode::Insert);
            }
            KeyCode::Char('u') => {
                if self.app.undo_clear() {
                    self.app
                        .push_system_message("Restored last cleared messages.");
                } else {
                    self.app.push_system_message("Nothing to undo.");
                }
            }
            KeyCode::Char('t') => {
                let on = self.app.toggle_show_tools();
                self.app.push_system_message(if on {
                    "Tool outputs shown."
                } else {
                    "Tool outputs hidden."
                });
            }
            KeyCode::Char('f') => {
                self.app.set_input("/search ".to_string());
                self.app.set_input_mode(InputMode::Insert);
            }
            KeyCode::Char('y') => {
                if self.app.copy_last_to_clipboard() {
                    self.app
                        .push_system_message("Copied last assistant reply to clipboard.");
                } else {
                    self.app
                        .push_system_message("Nothing to copy — no assistant reply yet.");
                }
            }
            KeyCode::Char('r') => {
                self.retry_generation().await?;
            }
            KeyCode::Char('o') => {
                if self.app.expand_newest_tool() {
                    self.app.push_system_message("Expanded newest tool output.");
                }
            }
            KeyCode::Char('n') => {
                if self.app.is_searching()
                    && let Some((pos, total)) = self.app.search_next()
                {
                    self.app
                        .push_system_message(&format!("Search {pos}/{total}"));
                }
            }
            KeyCode::Char('N') => {
                if self.app.is_searching()
                    && let Some((pos, total)) = self.app.search_prev()
                {
                    self.app
                        .push_system_message(&format!("Search {pos}/{total}"));
                }
            }
            KeyCode::Escape => {
                // Do NOT quit unconditionally — only back out of live states.
                if self.app.is_generating() {
                    self.cancel_generation();
                } else if self.app.show_help() {
                    self.app.toggle_help();
                }
                // is_searching already handled in handle_key above
            }
            KeyCode::CtrlC => {
                self.cancel_generation();
            }
            KeyCode::Tab => match self.app.mode() {
                AppMode::Normal => self.app.set_mode(AppMode::AgentPanel),
                AppMode::AgentPanel => self.app.set_mode(AppMode::Normal),
                _ => self.app.set_mode(AppMode::Normal),
            },
            KeyCode::Char('h') | KeyCode::Char('?') => {
                self.app.toggle_help();
            }
            KeyCode::F(1) => {
                self.app.toggle_help();
            }
            KeyCode::Char('a') => {
                self.app.set_mode(AppMode::AgentPanel);
            }
            KeyCode::Up => {
                self.app.scroll_up(1);
            }
            KeyCode::Down => {
                self.app.scroll_down(1);
            }
            KeyCode::PageUp => {
                self.app.scroll_up(10);
            }
            KeyCode::PageDown => {
                self.app.scroll_down(10);
            }
            KeyCode::Home => {
                self.app.scroll_to_top();
            }
            KeyCode::End => {
                self.app.scroll_to_bottom();
            }
            _ => {}
        }

        Ok(())
    }'''

new_handler = '''    /// Handle keys in normal mode.
    ///
    /// Single-character keys are looked up in the user's binding map
    /// (loaded from `~/.config/kod/tui_keys.toml` or a project-local
    /// `.kod-keys.toml`; see [`crate::keybindings`]). A bound character
    /// dispatches through [`TuiLoop::dispatch_key_action`]. Unbound
    /// characters fall through to the small set of fixed controls:
    /// `r` retry, `o` expand the newest tool row, `n`/`N` step through
    /// chat-search matches. Non-character keys (Escape, Ctrl+C, Tab,
    /// F1, arrows, Home/End, PgUp/PgDn) keep their fixed behaviour.
    async fn handle_normal_mode_key(&mut self, key: KeyCode) -> Result<()> {
        if let KeyCode::Char(c) = key
            && let Some(action) = self.keybindings.get(&c).copied()
        {
            return self.dispatch_key_action(action).await;
        }
        match key {
            // Fixed single-char fallbacks not exposed through the
            // configurable binding set.
            KeyCode::Char('r') => {
                self.retry_generation().await?;
            }
            KeyCode::Char('o') => {
                if self.app.expand_newest_tool() {
                    self.app.push_system_message("Expanded newest tool output.");
                }
            }
            KeyCode::Char('n') => {
                if self.app.is_searching()
                    && let Some((pos, total)) = self.app.search_next()
                {
                    self.app
                        .push_system_message(&format!("Search {pos}/{total}"));
                }
            }
            KeyCode::Char('N') => {
                if self.app.is_searching()
                    && let Some((pos, total)) = self.app.search_prev()
                {
                    self.app
                        .push_system_message(&format!("Search {pos}/{total}"));
                }
            }
            KeyCode::Escape => {
                // Do NOT quit unconditionally — only back out of live states.
                if self.app.is_generating() {
                    self.cancel_generation();
                } else if self.app.show_help() {
                    self.app.toggle_help();
                }
                // is_searching already handled in handle_key above
            }
            KeyCode::CtrlC => {
                self.cancel_generation();
            }
            KeyCode::Tab => match self.app.mode() {
                AppMode::Normal => self.app.set_mode(AppMode::AgentPanel),
                AppMode::AgentPanel => self.app.set_mode(AppMode::Normal),
                _ => self.app.set_mode(AppMode::Normal),
            },
            KeyCode::F(1) => {
                self.app.toggle_help();
            }
            KeyCode::Up => {
                self.app.scroll_up(1);
            }
            KeyCode::Down => {
                self.app.scroll_down(1);
            }
            KeyCode::PageUp => {
                self.app.scroll_up(10);
            }
            KeyCode::PageDown => {
                self.app.scroll_down(10);
            }
            KeyCode::Home => {
                self.app.scroll_to_top();
            }
            KeyCode::End => {
                self.app.scroll_to_bottom();
            }
            _ => {}
        }

        Ok(())
    }

    /// Dispatch one configurable keybinding action.
    ///
    /// The behaviour bodies are identical to what the old hardcoded
    /// match arms did; extracting them lets the binding table and the
    /// code path share one implementation.
    async fn dispatch_key_action(&mut self, action: KeyAction) -> Result<()> {
        match action {
            KeyAction::Insert => self.app.set_input_mode(InputMode::Insert),
            KeyAction::Quit => {
                if self.app.is_generating() {
                    self.app.request_confirm(ConfirmKind::Quit);
                } else {
                    self.app.quit();
                }
            }
            KeyAction::Help => self.app.toggle_help(),
            KeyAction::Panel => self.app.set_mode(AppMode::AgentPanel),
            KeyAction::ScrollUp => self.app.scroll_up(1),
            KeyAction::ScrollDown => self.app.scroll_down(1),
            KeyAction::Top => self.app.scroll_to_top(),
            KeyAction::Bottom => self.app.scroll_to_bottom(),
            KeyAction::EditLast => {
                self.app.edit_last_message();
                self.app.set_input_mode(InputMode::Insert);
            }
            KeyAction::Undo => {
                if self.app.undo_clear() {
                    self.app
                        .push_system_message("Restored last cleared messages.");
                } else {
                    self.app.push_system_message("Nothing to undo.");
                }
            }
            KeyAction::ToggleTools => {
                let on = self.app.toggle_show_tools();
                self.app.push_system_message(if on {
                    "Tool outputs shown."
                } else {
                    "Tool outputs hidden."
                });
            }
            KeyAction::SearchPrefix => {
                self.app.set_input("/search ".to_string());
                self.app.set_input_mode(InputMode::Insert);
            }
            KeyAction::CopyLast => {
                if self.app.copy_last_to_clipboard() {
                    self.app
                        .push_system_message("Copied last assistant reply to clipboard.");
                } else {
                    self.app
                        .push_system_message("Nothing to copy — no assistant reply yet.");
                }
            }
        }
        Ok(())
    }'''

patch(loop_path, old_handler, new_handler, "handle_normal_mode_key + dispatch_key_action")

# ========================================================================
# 6. Tests in main_loop::tests
# ========================================================================
patch(
    loop_path,
    '''    #[tokio::test]
    async fn test_tui_lifecycle() {
        let mut tui = TuiLoop::new();

        tui.handle_event(Event::Key(KeyCode::Char('i')))
            .await
            .unwrap();
        assert_eq!(tui.app().input_mode(), &InputMode::Insert);

        tui.handle_event(Event::Key(KeyCode::Escape)).await.unwrap();
        assert_eq!(tui.app().input_mode(), &InputMode::Normal);
    }''',
    '''    #[tokio::test]
    async fn test_tui_lifecycle() {
        let mut tui = TuiLoop::new();

        tui.handle_event(Event::Key(KeyCode::Char('i')))
            .await
            .unwrap();
        assert_eq!(tui.app().input_mode(), &InputMode::Insert);

        tui.handle_event(Event::Key(KeyCode::Escape)).await.unwrap();
        assert_eq!(tui.app().input_mode(), &InputMode::Normal);
    }

    /// The default binding set must keep 'i' as insert, so a fresh
    /// install behaves as documented.
    #[tokio::test]
    async fn test_default_keybinding_insert_fires() {
        let mut tui = TuiLoop::new();
        tui.handle_event(Event::Key(KeyCode::Char('i')))
            .await
            .unwrap();
        assert_eq!(tui.app().input_mode(), &InputMode::Insert);
    }

    /// A user-supplied binding for a previously-unbound key must fire.
    #[tokio::test]
    async fn test_custom_keybinding_is_honored() {
        use crate::keybindings::KeyAction;
        use std::collections::HashMap;

        let mut tui = TuiLoop::new();
        let mut b = HashMap::new();
        b.insert('w', KeyAction::Insert);
        tui.set_keybindings(b);

        // 'w' is not in the defaults; only the custom binding should
        // make it enter insert mode.
        tui.handle_event(Event::Key(KeyCode::Char('w')))
            .await
            .unwrap();
        assert_eq!(tui.app().input_mode(), &InputMode::Insert);
    }

    /// Replacing the default binding for an action with a different key
    /// must disable the old key. Regression: the previous implementation
    /// hardcoded the character arms, so a rebind would silently keep the
    /// old key working and the new key would never fire.
    #[tokio::test]
    async fn test_rebinding_replaces_default() {
        use crate::keybindings::KeyAction;
        use std::collections::HashMap;

        let mut tui = TuiLoop::new();
        let mut b = HashMap::new();
        // Map insert to 'w' only — 'i' must no longer trigger it.
        b.insert('w', KeyAction::Insert);
        tui.set_keybindings(b);

        tui.handle_event(Event::Key(KeyCode::Char('i')))
            .await
            .unwrap();
        assert_eq!(
            tui.app().input_mode(),
            &InputMode::Normal,
            "rebound 'i' should no longer enter insert mode"
        );

        tui.handle_event(Event::Key(KeyCode::Char('w')))
            .await
            .unwrap();
        assert_eq!(tui.app().input_mode(), &InputMode::Insert);
    }''',
    "keybinding dispatch tests",
)

print("All patches applied.")
PYEOF

if [ $? -ne 0 ]; then
    echo "ERROR: patching failed"
    exit 1
fi

echo "Checking compilation"
if ! cargo check --workspace 2>&1; then
    echo "Compilation failed – will skip commit"
    COMPILE_OK=false
fi

if [ "$INCOMPLETE" = true ] || [ "$COMPILE_OK" = false ]; then
    echo "Skipping tests and commit due to incomplete files or compilation errors"
    exit 1
fi

echo "Running kod-tui tests (180s wall clock)"
if ! run_with_timeout 180 cargo test -p kod-tui 2>&1; then
    echo "kod-tui tests failed or hung. Paste the full output for a surgical fix."
    exit 1
fi

echo "Running workspace tests (300s wall clock)"
if ! run_with_timeout 300 cargo test --workspace 2>&1; then
    echo "Workspace tests failed or hung. Paste the full output for a surgical fix."
    exit 1
fi

echo "Running clippy with -D warnings"
if ! cargo clippy --workspace --all-targets -- -D warnings 2>&1; then
    echo "Clippy failed. Paste the full output for a surgical fix."
    exit 1
fi

echo "All checks passed. Committing."
git add -A
git commit -m "feat(tui): honour user keybindings in normal mode

The keybindings module parsed ~/.config/kod/tui_keys.toml and
.kod-keys.toml, exposed a KeyAction map, and had tests. But
handle_normal_mode_key hardcoded every single-char behaviour and
only touched the module via 'let _ = crate::keybindings::
default_bindings()' to silence an unused warning. A user who wrote
a tui_keys.toml and rebound insert from 'i' to 'w' got neither the
new binding nor the old one suppressed — nothing changed.

Wire it up:

- TuiLoop::new loads the binding map via load_bindings().
- A new set_keybindings method lets tests inject a map directly, so
  the tests do not have to poke at ~/.config/kod.
- handle_normal_mode_key looks up Char(c) in the map first. A hit
  dispatches through the new dispatch_key_action, which carries the
  same behaviour bodies the old match arms had. A miss falls
  through to the small set of fixed keys (r retry, o expand,
  n/N search step) and to the non-character keys (Escape, Ctrl+C,
  Tab, F1, arrows, Home/End, PgUp/PgDn).
- 'I' joins the default bindings as a legacy alias for insert, so
  uppercase I keeps working and can be rebound or removed like any
  other binding.

Adds three tests:

- test_default_keybinding_insert_fires: 'i' enters insert on a fresh
  TuiLoop (defaults preserved).
- test_custom_keybinding_is_honored: binding 'w' to insert makes
  'w' enter insert.
- test_rebinding_replaces_default: mapping insert to 'w' must stop
  'i' from entering insert — the previous hardcoded arms would have
  fired anyway."
