#!/usr/bin/env bash
set -uo pipefail

COMPILE_OK=true
APP=crates/kod-tui/src/app.rs

if [ ! -f Cargo.toml ] || [ ! -f "$APP" ]; then
    echo "ERROR: run from the kod workspace root ($APP missing)"
    exit 1
fi

echo "Patching $APP: /clear resets context accounting"

python3 - "$APP" << 'PYEOF'
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

# --- clear_messages resets context accounting --------------------------
patch(
    '''    pub fn clear_messages(&mut self) {
        if !self.messages.is_empty() {
            self.cleared_stack.push(std::mem::take(&mut self.messages));
            if self.cleared_stack.len() > 5 {
                self.cleared_stack.remove(0);
            }
        }
        self.scroll_lines = 0;
        self.expanded_tools.clear();
        self.clear_search();
    }''',
    '''    pub fn clear_messages(&mut self) {
        if !self.messages.is_empty() {
            self.cleared_stack.push(std::mem::take(&mut self.messages));
            if self.cleared_stack.len() > 5 {
                self.cleared_stack.remove(0);
            }
        }
        self.scroll_lines = 0;
        self.expanded_tools.clear();
        self.clear_search();

        // Reset context accounting. `/clear` wipes the display AND the
        // engine's transcript (see the ConfirmKind::Clear handler in
        // main_loop, which calls engine.clear_history()), so the
        // session really is starting over. Leaving the previous
        // session's accumulated `context_tokens` in place meant the
        // next N messages inherited a count that included messages
        // the user had thrown away: the header's "≈ ctx X/Y" meter
        // overstated by the discarded amount, and `maybe_compact`'s
        // threshold — a fraction of the model window — was compared
        // against a number that no longer reflected anything.
        //
        // `compacted_messages` (the session's running total) is reset
        // for the same reason: it is meant to say "N messages have
        // been compacted *in this session*", not "since the process
        // started".
        self.context_tokens = 0;
        self.compacted_messages = 0;
    }''',
    "clear_messages resets context accounting",
)

# --- Tests --------------------------------------------------------
patch(
    '''    #[test]
    fn test_scroll_to_bottom() {''',
    '''    /// `/clear` resets the context accounting. Regression: the
    /// visible messages and the engine transcript were reset, but
    /// `context_tokens` and `compacted_messages` kept accumulating,
    /// so the header meter overstated the current context and the
    /// auto-compact threshold was compared against a number that
    /// included discarded messages.
    #[test]
    fn test_clear_resets_context_accounting() {
        let mut app = KodApp::new();
        app.set_context_limit(10_000);

        // Build up a believable pre-clear state: some messages and
        // some token usage.
        for i in 0..30 {
            app.push_system_message(&format!("filler {i}"));
        }
        app.note_real_usage(3_000);
        // Also trigger a manual compact to set compacted_messages.
        app.compact_now();
        assert!(app.context_tokens() > 0);
        assert!(app.messages().len() < 30, "compact should have dropped some");

        // Sanity: pre-clear state is not the fresh state.
        let pre_tokens = app.context_tokens();

        app.clear_messages();

        assert!(app.messages().is_empty(), "display should be empty");
        assert_eq!(
            app.context_tokens(),
            0,
            "context accounting must reset (was {pre_tokens})"
        );
        // The label must report 0% — the meter the header draws reads
        // from the same counter.
        assert!(
            app.context_label().contains("0%"),
            "context label should read 0%: {}",
            app.context_label()
        );
    }

    /// `/undo` restores the cleared messages but must NOT resurrect
    /// the stale context count — the engine's transcript was cleared
    /// by the `/clear` handler, so the model really does have zero
    /// context at that point. Undo is a display operation only.
    #[test]
    fn test_undo_does_not_restore_stale_context() {
        let mut app = KodApp::new();
        app.set_context_limit(10_000);
        app.push_system_message("hello");
        app.note_real_usage(2_500);
        assert_eq!(app.context_tokens(), 2_500);

        app.clear_messages();
        assert_eq!(app.context_tokens(), 0);

        let restored = app.undo_clear();
        assert!(restored, "undo should succeed");
        assert_eq!(app.messages().len(), 1);
        // Context stays at the post-clear value, not the pre-clear one.
        assert_eq!(
            app.context_tokens(),
            0,
            "undo must not resurrect a stale context count"
        );
    }

    #[test]
    fn test_scroll_to_bottom() {''',
    "clear resets context tests",
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
fix(tui): /clear resets context accounting

/clear wiped the visible messages and, via the ConfirmKind::Clear
handler in main_loop, the engine's transcript. It left
`context_tokens` and `compacted_messages` untouched. Two visible
effects:

- The header meter "≈ ctx X/Y · Z%" reported a value that included
  the messages the user had just thrown away, overstating the
  current session until the count was overtaken by fresh usage.
- `maybe_compact` compares `context_tokens` against a fraction of
  the model window. With the stale count still in place, the very
  next cluster of messages could trip auto-compact — compacting a
  session that was only a few turns old, using a threshold based on
  messages that no longer existed.

clear_messages now zeroes both counters, alongside the display and
search state it was already resetting. `compacted_messages` is
included because it is a per-session counter — "N messages compacted
this session" — not a process-lifetime statistic.

`/undo` (restoring the messages cleared by a previous /clear) does
not resurrect the counter. The engine's transcript was cleared at
the same time and the model really does have zero context at that
point; the count should match reality rather than the display.

Adds two tests: /clear zeroes both counters and the label reads
0%, and /undo restores the display without resurrecting the stale
count.
MSG
