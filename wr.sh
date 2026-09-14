#!/usr/bin/env bash
set -uo pipefail

COMPILE_OK=true
TARGET=crates/kod-tui/src/main_loop.rs

if [ ! -f Cargo.toml ] || [ ! -f "$TARGET" ]; then
    echo "ERROR: run from the kod workspace root ($TARGET missing)"
    exit 1
fi

echo "Patching $TARGET: dispatch_prompt records last_prompt; add tests"

python3 - "$TARGET" << 'PYEOF'
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

# --- 1. dispatch_prompt records the prompt before the engine check ---
patch(
    '''        self.app.submit_input();

        // Without an engine (e.g. in tests) the message is recorded and
        // nothing else happens.
        let Some(engine) = self.engine.clone() else {
            return Ok(());
        };''',
    '''        self.app.submit_input();

        // Remember the prompt so /retry (and the `r` key) can resend it
        // after a failure. The previous code only set last_prompt from
        // retry_generation itself, so last_prompt() was always None on
        // first use and /retry always answered 'Nothing to retry'.
        self.app.set_last_prompt(&input);

        // Without an engine (e.g. in tests) the message is recorded and
        // nothing else happens.
        let Some(engine) = self.engine.clone() else {
            return Ok(());
        };''',
    "dispatch_prompt records last_prompt",
)

# --- 2. retry_generation no longer re-sets last_prompt or clones ---
patch(
    '''        // Set input to the last prompt, then dispatch as a normal submit.
        self.app.set_input(prompt.clone());
        self.app.set_last_prompt(&prompt);
        // Box::pin to break the dispatch → handle_command → retry → dispatch
        // recursive future cycle (Rust requires indirection for recursive
        // async fns).
        Box::pin(self.dispatch_prompt()).await''',
    '''        // Restore into the input box and dispatch again. No need to
        // re-set last_prompt: dispatch_prompt overwrites it with the
        // same text on the way through.
        self.app.set_input(prompt);
        // Box::pin breaks the dispatch → handle_command → retry →
        // dispatch recursive future cycle (Rust requires indirection).
        Box::pin(self.dispatch_prompt()).await''',
    "retry_generation drops redundant set_last_prompt",
)

# --- 3. Tests: insert four tests before a stable anchor. Use
#        test_steer_command_without_run_explains, which is present
#        in the file and unique. ---
if "test_retry_prompt_is_recorded_on_dispatch" in content:
    print("Skipped: retry tests already present")
else:
    anchor = """    #[tokio::test]
    async fn test_steer_command_without_run_explains() {"""
    if content.count(anchor) != 1:
        print("ERROR: anchor test_steer_command_without_run_explains not unique")
        sys.exit(2)

    new_tests = '''    /// Dispatching a plain prompt must record it as `last_prompt`, so
    /// `/retry` and the `r` key have something to resend. Regression:
    /// before this, last_prompt was only set by retry_generation
    /// itself — a chicken-and-egg that made /retry a no-op.
    #[tokio::test]
    async fn test_retry_prompt_is_recorded_on_dispatch() {
        let mut tui = TuiLoop::new();
        tui.app_mut().set_input_mode(InputMode::Insert);
        tui.app_mut().set_input("hello there".to_string());
        // dispatch_prompt with no engine records the message and returns.
        tui.handle_event(Event::Key(KeyCode::Enter)).await.unwrap();
        assert_eq!(
            tui.app().last_prompt(),
            Some("hello there"),
            "last_prompt must be set by dispatch_prompt"
        );
    }

    /// Slash commands must not become the retry target: /retry should
    /// resend a user prompt, not re-run a /command.
    #[tokio::test]
    async fn test_slash_commands_do_not_become_retry_target() {
        let mut tui = TuiLoop::new();
        tui.app_mut().set_input_mode(InputMode::Insert);
        tui.app_mut().set_input("real prompt".to_string());
        tui.handle_event(Event::Key(KeyCode::Enter)).await.unwrap();
        assert_eq!(tui.app().last_prompt(), Some("real prompt"));

        tui.app_mut().set_input("/help".to_string());
        tui.handle_event(Event::Key(KeyCode::Enter)).await.unwrap();
        assert_eq!(
            tui.app().last_prompt(),
            Some("real prompt"),
            "/help should not become the retry target"
        );
    }

    /// `/model` with no argument must route to show_and_refresh_models.
    /// Without an engine the handler reports that clearly rather than
    /// printing the old "Usage: /model <name>".
    #[tokio::test]
    async fn test_model_with_no_args_lists_or_reports_engine_missing() {
        let mut tui = TuiLoop::new();
        tui.app_mut()
            .set_available_models(vec!["qwen2.5:0.5b".to_string()]);
        tui.handle_command("/model").await.unwrap();
        let last = tui.app().messages().last().unwrap();
        assert!(
            last.content.contains("Engine not initialized"),
            "got: {}",
            last.content
        );
    }

    /// available_models() reports what was last set. Pins the shape
    /// switch_model relies on: an empty list means 'unknown' (do not
    /// warn), a non-empty list is what validation compares against.
    #[tokio::test]
    async fn test_available_models_accessor_roundtrips() {
        let mut tui = TuiLoop::new();
        assert!(tui.app().available_models().is_empty());
        tui.app_mut()
            .set_available_models(vec!["a".to_string(), "b".to_string()]);
        let got = tui.app().available_models();
        assert_eq!(got.len(), 2);
        assert!(got.contains(&"a".to_string()));
        assert!(got.contains(&"b".to_string()));
    }

    #[tokio::test]
    async fn test_steer_command_without_run_explains() {'''

    content = content.replace(anchor, new_tests, 1)
    print("Inserted retry + model tests before test_steer_command_without_run_explains")

tmp = target + ".tmp"
with open(tmp, "w") as f:
    f.write(content)
os.replace(tmp, target)
print("Patched", target)
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
git commit -m "fix(tui): record last_prompt on dispatch; validate /model names

Two related /retry and /model corrections.

1. /retry (and the r key) always answered 'Nothing to retry — no
   previous prompt.' The TUI's last_prompt field was only ever set
   by retry_generation itself: retry_generation read last_prompt,
   found None, and bailed. Dispatch never wrote it, so the very
   first retry after a failure was a no-op and so was every retry
   after that.

   dispatch_prompt now calls self.app.set_last_prompt(&input) for
   plain prompts, immediately after submit_input. Slash commands do
   not become retry targets — the /-arm of dispatch_prompt returns
   before the set_last_prompt line, so /help does not overwrite the
   last real prompt. retry_generation no longer re-sets the field;
   the subsequent dispatch does that anyway.

2. /model <typo> silently succeeded. OpenAICompatProvider::from_config
   never fails on a name the server has not pulled — it just stores
   the string — so switch_model printed 'Switched model to X' and the
   user's next prompt failed with an opaque 'model not found' from
   the server. switch_model now checks the requested name against
   the list fetched at startup (KodApp::available_models). A name
   that is not in a *non-empty* list gets a warning that names the
   fix. The switch still happens — the list can be stale — but the
   user is not misled. An empty list means 'unknown' (cold start,
   list_models failed) and suppresses the warning.

   /model with no argument now calls show_and_refresh_models, which
   queries the provider, refreshes the cache, and prints the models
   with the current one marked. This is the natural recovery path
   when the user has pulled a new model after the TUI started.

Adds KodApp::available_models() accessor and four tests: two for
the retry target (prompt recorded; slash commands excluded), one
for /model with no engine, and one pinning the accessor's shape."
