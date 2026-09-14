#!/usr/bin/env bash
set -uo pipefail

COMPILE_OK=true
LOOP=crates/kod-tui/src/main_loop.rs

if [ ! -f Cargo.toml ] || [ ! -f "$LOOP" ]; then
    echo "ERROR: run from the kod workspace root ($LOOP missing)"
    exit 1
fi

echo "Wiring persistent prompt history into TuiLoop"

python3 - "$LOOP" << 'PYEOF'
import os
import sys

target = sys.argv[1]
with open(target, "r") as f:
    content = f.read()

def patch(old, new, label, expect=1):
    global content
    n = content.count(old)
    if n == 0:
        print(f"ERROR: old snippet not found: {label}")
        sys.exit(2)
    if expect and n != expect:
        print(f"ERROR: expected {expect} occurrence(s) of {label}, found {n}")
        sys.exit(2)
    content = content.replace(old, new, expect if expect else n)
    print(f"Patched: {label}")

# ======================================================================
# 1. TuiLoop struct: add persist_history flag.
# ======================================================================
patch(
    '''    /// Char → action map for normal-mode single-key commands. Loaded at
    /// construction from `~/.config/kod/tui_keys.toml` and a project-
    /// local `.kod-keys.toml` (see [`crate::keybindings`]); a missing or
    /// corrupt file falls back to the built-in defaults. Tests override
    /// it via [`TuiLoop::set_keybindings`].
    keybindings: std::collections::HashMap<char, KeyAction>,
}''',
    '''    /// Char → action map for normal-mode single-key commands. Loaded at
    /// construction from `~/.config/kod/tui_keys.toml` and a project-
    /// local `.kod-keys.toml` (see [`crate::keybindings`]); a missing or
    /// corrupt file falls back to the built-in defaults. Tests override
    /// it via [`TuiLoop::set_keybindings`].
    keybindings: std::collections::HashMap<char, KeyAction>,
    /// Whether this loop reads and writes `~/.kod/tui_history.json`.
    ///
    /// Set to true only by [`TuiLoop::run`] — the production entry
    /// point. Tests drive `handle_event`/`dispatch_prompt` directly
    /// without calling `run`, so they neither touch the user's real
    /// history file nor observe a stale one. The load/persist helpers
    /// on `KodApp` exist and are correct; they simply had no caller
    /// before this — the doc comment on the persistence module
    /// promised cross-restart history that did not happen.
    persist_history: bool,
}''',
    "TuiLoop persist_history field",
)

# ======================================================================
# 2. TuiLoop::new: init to false.
# ======================================================================
patch(
    '''            gen_task: None,
            keybindings: load_bindings(),
        }
    }''',
    '''            gen_task: None,
            keybindings: load_bindings(),
            persist_history: false,
        }
    }''',
    "TuiLoop::new init",
)

# ======================================================================
# 3. TuiLoop::run: load history and arm persistence.
# ======================================================================
patch(
    '''        self.init_engine(model).await?;
        self.init_terminal().await?;
        let result = self.main_loop().await;''',
    '''        self.init_engine(model).await?;

        // Load persisted prompt history and arm the save path.
        // KodApp::load_persistent_history reads
        // ~/.kod/tui_history.json (or KOD_TUI_STATE_DIR) best-effort —
        // a missing or corrupt file just means an empty history. The
        // matching persist_history_entry runs from dispatch_prompt
        // below, gated on the persist_history flag so tests do not
        // touch the real file.
        self.app.load_persistent_history();
        self.persist_history = true;

        self.init_terminal().await?;
        let result = self.main_loop().await;''',
    "run() loads history and arms persist",
)

# ======================================================================
# 4. dispatch_prompt: persist after a successful submit.
# ======================================================================
patch(
    '''        self.app.submit_input();

        // Remember the prompt so /retry (and the `r` key) can resend it
        // after a failure. The previous code only set last_prompt from
        // retry_generation itself, so last_prompt() was always None on
        // first use and /retry always answered 'Nothing to retry'.
        self.app.set_last_prompt(&input);''',
    '''        self.app.submit_input();

        // Remember the prompt for /retry (and the `r` key). The previous
        // code only set last_prompt from retry_generation itself, so
        // last_prompt() was always None on first use and /retry always
        // answered 'Nothing to retry'.
        self.app.set_last_prompt(&input);

        // Append the raw prompt to the cross-session history file, so
        // Up-arrow in the next session recalls what was typed this one.
        // Skipped in tests (persist_history is only true after run()),
        // and skipped for slash commands by living below the earlier
        // `input.trim_start().starts_with('/')` early return — a
        // recalled `/help` in the history would be noise next time.
        if self.persist_history {
            self.app.persist_history_entry(&input);
        }''',
    "dispatch_prompt persists plain prompts",
)

tmp = target + ".tmp"
with open(tmp, "w") as f:
    f.write(content)
os.replace(tmp, target)
print("Wrote", target)
PYEOF

if [ $? -ne 0 ]; then
    echo "ERROR: patching failed"
    exit 1
fi

echo "cargo check --workspace --all-targets"
if ! cargo check --workspace --all-targets 2>&1; then
    echo "Compilation failed"
    exit 1
fi

echo "cargo clippy --workspace --all-targets -- -D warnings"
if ! cargo clippy --workspace --all-targets -- -D warnings 2>&1; then
    echo "Clippy failed"
    exit 1
fi

echo "Committing."
git add -A
git commit -F - <<'MSG'
fix(tui): load and persist prompt history across sessions

KodApp::load_persistent_history and KodApp::persist_history_entry
existed, were documented as the cross-restart mechanism in the
module-level comment, and had no callers. TuiLoop::new never loaded
the file and dispatch_prompt never wrote it, so every restart lost
the Up-arrow history and ~/.kod/tui_history.json was either absent
or frozen at whatever version an earlier build had written.

Wire it up:

- TuiLoop gains a `persist_history: bool` field, initialised false
  in `new()`. Tests drive handle_event/dispatch_prompt directly and
  must not touch the user's real history file — they never set the
  flag, so the save path is inert for them.
- TuiLoop::run sets `persist_history = true` after init_engine and
  calls `self.app.load_persistent_history()` in the same block.
  Loading is best-effort (missing or corrupt file = empty history),
  matching the rest of the persistence code.
- dispatch_prompt calls `persist_history_entry` after submit_input
  when the flag is set. The call sits below the slash-command early
  return, so `/model X` and friends do not enter the persisted
  history — a recalled slash command from a previous session is
  noise, not something a user wants to press Up to find.

No public API change; one new private field.
MSG
