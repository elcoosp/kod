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
TARGET=crates/kod-tui/src/main_loop.rs

if [ ! -f Cargo.toml ] || [ ! -f "$TARGET" ]; then
    echo "ERROR: run from the kod workspace root ($TARGET missing)"
    exit 1
fi

echo "Patching $TARGET: /goal dispatches directly instead of pushing a fake Enter"

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

# --- 1. /goal arm: replace synthetic Enter with direct dispatch --------
patch(
    '''                } else {
                    self.app.set_goal(rest);
                    self.app.push_system_message(&format!(
                        "Goal set: {rest}\\nEvery prompt now works turn by turn until GOAL MET. Esc cancels; /steer redirects; /goal clear stops."
                    ));
                    // Start working immediately: reload the goal as the next
                    // prompt and queue an Enter behind this command, so the
                    // loop picks it up and dispatches without extra keystrokes.
                    self.app.set_input(rest.to_string());
                    self.app.set_input_mode(InputMode::Insert);
                    self.event_handler.push_event(Event::Key(KeyCode::Enter));
                }''',
    '''                } else {
                    self.app.set_goal(rest);
                    self.app.push_system_message(&format!(
                        "Goal set: {rest}\\nEvery prompt now works turn by turn until GOAL MET. Esc cancels; /steer redirects; /goal clear stops."
                    ));
                    // Start working immediately: the goal itself becomes
                    // the first prompt. Call dispatch_prompt directly
                    // instead of pushing a synthetic Enter into the event
                    // queue — the previous approach relied on Enter's
                    // Insert-mode binding and could misfire if the user
                    // changed keybindings or was mid-typing.
                    //
                    // Box::pin breaks the dispatch → handle_command →
                    // (goal arm) → dispatch recursion the same way
                    // retry_generation does.
                    self.app.set_input(rest.to_string());
                    Box::pin(self.dispatch_prompt()).await?;
                }''',
    "/goal direct dispatch",
)

# --- 2. Update the test that asserted the old Insert-mode + queued Enter
patch(
    '''    #[tokio::test]
    async fn test_goal_set_show_clear() {
        let mut tui = TuiLoop::new();
        tui.handle_command("/goal ship the fix").await.unwrap();
        assert_eq!(tui.app().goal(), Some("ship the fix"));
        // Setting a goal also queues its first prompt: input prefilled,
        // Insert mode on, so the queued Enter dispatches without keystrokes.
        assert_eq!(tui.app().input(), "ship the fix");
        assert!(matches!(
            tui.app().input_mode(),
            crate::app::InputMode::Insert
        ));
        tui.handle_command("/goal").await.unwrap();
        let last = tui.app().messages().last().unwrap();
        assert!(
            last.content.contains("ship the fix"),
            "got: {}",
            last.content
        );
        tui.handle_command("/goal clear").await.unwrap();
        assert_eq!(tui.app().goal(), None);
    }''',
    '''    #[tokio::test]
    async fn test_goal_set_show_clear() {
        let mut tui = TuiLoop::new();
        tui.handle_command("/goal ship the fix").await.unwrap();
        assert_eq!(tui.app().goal(), Some("ship the fix"));
        // Setting a goal dispatches it as the first prompt immediately.
        // Without a live engine (this test has none) dispatch_prompt
        // records the user message and returns without generating.
        // The input box is cleared by submit_input, and the goal text
        // appears in the chat as a user message.
        assert_eq!(tui.app().input(), "");
        let user_msg = tui
            .app()
            .messages()
            .iter()
            .find(|m| m.role == kod_types::MessageRole::User)
            .expect("goal text should be recorded as a user message");
        assert_eq!(user_msg.content, "ship the fix");

        tui.handle_command("/goal").await.unwrap();
        let last = tui.app().messages().last().unwrap();
        assert!(
            last.content.contains("ship the fix"),
            "got: {}",
            last.content
        );
        tui.handle_command("/goal clear").await.unwrap();
        assert_eq!(tui.app().goal(), None);
    }''',
    "test_goal_set_show_clear updated",
)

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

echo "Checking compilation"
if ! cargo check --workspace 2>&1; then
    echo "Compilation failed – will skip commit"
    COMPILE_OK=false
fi

if [ "$INCOMPLETE" = true ] || [ "$COMPILE_OK" = false ]; then
    echo "Skipping tests and commit due to incomplete files or compilation errors"
    exit 1
fi

echo "Running kod-tui tests (120s wall clock)"
if ! run_with_timeout 120 cargo test -p kod-tui 2>&1; then
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
git commit -m "fix(tui): /goal dispatches its first prompt directly, no fake Enter

The /goal command set the goal, prefilled the input box with the goal
text, switched to Insert mode, and pushed a synthetic Event::Key(Enter)
into the event queue to trigger dispatch. That is a hack:

- It routes the immediate dispatch through the Insert-mode Enter
  binding, so a user who remaps Enter (or changes the input submode)
  silently breaks /goal's 'start working now' behavior.
- It races with real keystrokes. A user typing while the queue drains
  could see their input appended to the goal text or the wrong mode
  active when the synthetic Enter is processed.
- The queue hop makes reasoning about ordering between /goal and any
  user keystroke immediate afterwards harder than it needs to be.

We are already inside an async handler. Call dispatch_prompt directly
(Box::pin, same pattern as retry_generation, to break the dispatch →
handle_command → goal-arm → dispatch recursion). The goal text still
lands in the chat as the first user message, the input box is cleared
by submit_input, and the input mode is left exactly where the user had
it — we no longer force Insert.

Updates test_goal_set_show_clear to assert the new observable state
(empty input, goal text recorded as a user message) instead of the
pre-filled input + Insert mode the hack produced."
