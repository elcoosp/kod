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
TARGET=crates/kod-core/src/engine.rs

if [ ! -f Cargo.toml ] || [ ! -f "$TARGET" ]; then
    echo "ERROR: run from the kod workspace root ($TARGET missing)"
    exit 1
fi

echo "Patching $TARGET: char-boundary-safe truncation in record_turn"

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

# --- 1. record_turn: use truncate_chars, and mark the cut char-aware ---
patch(
    '''    /// Remember one turn, truncating long texts and keeping only the most
    /// recent [`MAX_HISTORY_TURNS`] turns.
    async fn record_turn(&self, user: bool, text: &str) {
        let text = text.trim();
        if text.is_empty() {
            return;
        }
        let short = if text.len() > MAX_TURN_CHARS {
            format!("{}… [truncated]", &text[..MAX_TURN_CHARS])
        } else {
            text.to_string()
        };
        let mut history = self.history.write().await;
        history.push(HistoryTurn { user, text: short });
        let excess = history.len().saturating_sub(MAX_HISTORY_TURNS);
        if excess > 0 {
            history.drain(..excess);
        }
    }''',
    '''    /// Remember one turn, truncating long texts and keeping only the most
    /// recent [`MAX_HISTORY_TURNS`] turns.
    ///
    /// Uses [`truncate_chars`] rather than a raw byte slice. `&text[..N]`
    /// panics when N lands inside a multibyte codepoint, which every
    /// non-ASCII turn (a prompt in Japanese, an answer quoting "café",
    /// any emoji) can hit — and the panic took down the whole agentic
    /// loop on the *second* turn, since record_turn runs on both sides
    /// of every prompt.
    async fn record_turn(&self, user: bool, text: &str) {
        let text = text.trim();
        if text.is_empty() {
            return;
        }
        let short = if text.len() > MAX_TURN_CHARS {
            format!("{}… [truncated]", truncate_chars(text, MAX_TURN_CHARS))
        } else {
            text.to_string()
        };
        let mut history = self.history.write().await;
        history.push(HistoryTurn { user, text: short });
        let excess = history.len().saturating_sub(MAX_HISTORY_TURNS);
        if excess > 0 {
            history.drain(..excess);
        }
    }''',
    "record_turn uses truncate_chars",
)

# --- 2. Regression test in the existing tests module -------------------
patch(
    '''    #[test]
    fn test_truncate_does_not_panic_mid_multibyte() {''',
    '''    /// record_turn runs on both sides of every prompt. A turn longer
    /// than MAX_TURN_CHARS whose 1500th byte falls inside a multibyte
    /// codepoint used to panic and abort the whole loop.
    #[tokio::test]
    async fn test_record_turn_does_not_panic_mid_multibyte() {
        let temp = TempDir::new().unwrap();
        let db_path = temp.path().join("test.redb");
        let cfg = RouterConfig {
            working_dir: temp.path().to_path_buf(),
            enable_memory: false,
            enable_swarm: false,
            max_skills_per_query: 3,
        };
        let engine = KodEngine::new(cfg, db_path).unwrap();
        engine.start().await.unwrap();

        // 1499 ASCII bytes, then 'é' (2 bytes) so byte offset 1500 is
        // the middle of the codepoint, then more content to exceed the
        // cap. MAX_TURN_CHARS is 1500.
        let mut prompt = "a".repeat(1499);
        prompt.push('é');
        prompt.push_str(&"x".repeat(100));
        assert!(prompt.len() > 1500);
        assert!(!prompt.is_char_boundary(1500));

        // Must not panic. The stored text ends at the last safe boundary
        // before the é, with the truncation marker appended.
        engine.record_turn(true, &prompt).await;

        let rendered = engine.render_history().await;
        assert!(rendered.contains("User:"), "history should carry the turn");
        assert!(
            rendered.contains("[truncated]"),
            "history should mark truncation"
        );
    }

    #[test]
    fn test_truncate_does_not_panic_mid_multibyte() {''',
    "record_turn regression test",
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

echo "Checking compilation"
if ! cargo check --workspace 2>&1; then
    echo "Compilation failed – will skip commit"
    COMPILE_OK=false
fi

if [ "$INCOMPLETE" = true ] || [ "$COMPILE_OK" = false ]; then
    echo "Skipping tests and commit due to incomplete files or compilation errors"
    exit 1
fi

echo "Running kod-core tests (180s wall clock)"
if ! run_with_timeout 180 cargo test -p kod-core 2>&1; then
    echo "kod-core tests failed or hung. Paste the full output for a surgical fix."
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
git commit -m "fix(core): char-boundary-safe truncation in record_turn

The truncate_chars helper landed for run_tool_calls but record_turn
still used the raw byte slice '&text[..MAX_TURN_CHARS]'. That panics
the moment byte 1500 lands inside a multibyte codepoint, and
record_turn runs on both sides of every prompt — so any turn
containing non-ASCII past the 1500-byte mark aborted the agentic
loop mid-session. Same class of bug we already fixed once; this is
the second occurrence.

Route record_turn through truncate_chars and add a regression test
that builds a 1599-byte prompt with a 2-byte 'é' at offset 1499, so
1500 is not a char boundary, and asserts record_turn completes and
the history records the turn with a truncation marker."
