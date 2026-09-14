#!/usr/bin/env bash
set -uo pipefail

LLM=crates/kod-config/src/llm.rs

echo "=== Current context_window handling in validate() ==="
awk '/if self.context_window == 0/,/^        }$/' "$LLM" | head -15

echo
echo "Patching $LLM"

python3 - "$LLM" << 'PYEOF'
import os
import sys

target = sys.argv[1]
with open(target, "r") as f:
    src = f.read()

def patch(old, new, label, expect=1):
    global src
    n = src.count(old)
    if n == 0:
        print(f"  SKIP (anchor absent): {label}")
        return False
    if expect and n != expect:
        print(f"  ERROR: expected {expect} occurrence(s) of {label}, found {n}")
        sys.exit(2)
    src = src.replace(old, new, expect if expect else n)
    print(f"  patched: {label}")
    return True

# ----------------------------------------------------------------------
# 1. Replace the context_window clamp with a floor that makes the whole
#    pipeline coherent.
# ----------------------------------------------------------------------
patch(
    '''        // Context window: the codebase's own caps (`render_history`'s
        // MAX_HISTORY_CHARS, the TUI meter's floor, per-tool prompt
        // caps) all assume a positive window. 0 means "unknown" to some
        // users, but every consumer here treats it as "0 tokens
        // available", which then silently discards every memory and
        // history entry. Clamp to the default.
        if self.context_window == 0 {
            tracing::warn!(
                "llm.context_window is 0; falling back to {}",
                LlmConfig::default().context_window
            );
            self.context_window = LlmConfig::default().context_window;
        }''',
    '''        // Context window. Three downstream floors have to be
        // consistent with this value, or the session behaves
        // incoherently:
        //
        //   - `KodApp::set_context_limit` floors the meter at 1000
        //     tokens.
        //   - `KodEngine::set_history_budget` floors the rendered
        //     history at `MIN_HISTORY_CHAR_BUDGET = 4000` chars
        //     (~1000 tokens at 4 chars/token), and the TUI passes
        //     `context_window * 3`.
        //   - `KodApp::maybe_compact` fires when the accumulated
        //     token estimate exceeds 4/5 of the (floored) window.
        //
        // A user who writes `context_window = 500` gets a meter that
        // says 1000, a history budget that clamps to 4000 chars, and
        // an auto-compact threshold based on 1000 — three numbers that
        // all pretend the window is bigger than the user set, and the
        // history budget alone already overshoots the window. The
        // session works but its accounting lies.
        //
        // Clamp to a coherent minimum instead of letting the incoherent
        // case run. The floor is 2000 tokens: high enough that
        // `2000 * 3 = 6000` chars of history clears the engine's 4000
        // char floor with headroom, and the meter's 4/5 threshold
        // (1600 tokens) leaves room for the prompt scaffolding and a
        // tool result or two before compaction fires.
        //
        // 0 is treated separately: a config file that predates the
        // field, or a user who wrote `context_window = 0` meaning
        // "unknown, use the default", gets `LlmConfig::default()`'s
        // 8192 rather than the floor.
        const MIN_CONTEXT_WINDOW: usize = 2_000;
        if self.context_window == 0 {
            tracing::warn!(
                "llm.context_window is 0; falling back to {}",
                LlmConfig::default().context_window
            );
            self.context_window = LlmConfig::default().context_window;
        } else if self.context_window < MIN_CONTEXT_WINDOW {
            tracing::warn!(
                value = self.context_window,
                floor = MIN_CONTEXT_WINDOW,
                "llm.context_window is below the coherent minimum; clamping to {}",
                MIN_CONTEXT_WINDOW
            );
            self.context_window = MIN_CONTEXT_WINDOW;
        }''',
    "context_window coherent floor",
)

# ----------------------------------------------------------------------
# 2. Tests.
# ----------------------------------------------------------------------
if "test_validate_clamps_tiny_context_window" not in src:
    anchor = '''    #[test]
    fn test_validate_floors_timeout() {'''
    if anchor not in src:
        print("  ERROR: test anchor not found")
        sys.exit(2)
    new_tests = '''    /// A `context_window` below the coherent floor must be clamped up,
    /// not passed through. Regression: a window of 500 produced an
    /// incoherent session where the meter's floor (1000), the history
    /// budget's floor (4000 chars ≈ 1000 tokens), and the auto-compact
    /// threshold (4/5 of the floored window) all disagreed about how
    /// much room the model had.
    #[test]
    fn test_validate_clamps_tiny_context_window() {
        let mut c = LlmConfig {
            context_window: 500,
            ..LlmConfig::default()
        };
        c.validate();
        assert_eq!(
            c.context_window, 2_000,
            "tiny window must clamp to the coherent floor"
        );

        // Boundary: 1999 clamps, 2000 does not.
        let mut c = LlmConfig {
            context_window: 1_999,
            ..LlmConfig::default()
        };
        c.validate();
        assert_eq!(c.context_window, 2_000);

        let mut c = LlmConfig {
            context_window: 2_000,
            ..LlmConfig::default()
        };
        c.validate();
        assert_eq!(c.context_window, 2_000);

        // Above the floor is honored.
        let mut c = LlmConfig {
            context_window: 131_072,
            ..LlmConfig::default()
        };
        c.validate();
        assert_eq!(c.context_window, 131_072);

        // 0 is a special case that maps to the default, not the floor.
        let mut c = LlmConfig {
            context_window: 0,
            ..LlmConfig::default()
        };
        c.validate();
        assert_eq!(c.context_window, LlmConfig::default().context_window);
    }

    #[test]
    fn test_validate_floors_timeout() {'''
    src = src.replace(anchor, new_tests, 1)
    print("  added context_window floor test")
else:
    print("  test already present")

tmp = target + ".tmp"
with open(tmp, "w") as f:
    f.write(src)
os.replace(tmp, target)
print("Wrote", target)
PYEOF

if [ $? -ne 0 ]; then
    echo "ERROR: patching failed"
    exit 1
fi

echo
echo "cargo check --workspace --all-targets 2>&1 | tail -15"
if ! cargo check --workspace --all-targets 2>&1 | tail -15; then
    echo "Compilation failed"
    exit 1
fi

cat > /tmp/kod_commit_msg.txt <<'MSG'
fix(config): clamp context_window to a coherent minimum

Three downstream floors were all silently flooring a too-small
`context_window` up to values that did not agree with each other:

  - KodApp::set_context_limit floors the meter at 1000 tokens.
  - KodEngine::set_history_budget floors the history at 4000 chars
    (~1000 tokens), and the TUI passes context_window * 3.
  - KodApp::maybe_compact fires at 4/5 of the (floored) window.

A config with `context_window = 500` therefore produced a session
that reported a 1000-token window, kept 4000 chars of history
(which alone approaches the window), and fired auto-compact against
a threshold derived from the 1000-token floor. Every number was
larger than the user's actual window and none of them agreed.

Clamp in LlmConfig::validate to MIN_CONTEXT_WINDOW = 2000 tokens.
At 2000:
  - history budget = 2000 * 3 = 6000 chars, comfortably above the
    engine's 4000-char floor;
  - meter threshold = 1600 tokens, leaving headroom for the prompt
    scaffolding and a tool result or two before compaction;
  - the meter, the history cap, and the compaction threshold all
    derive from the same 2000-token value.

0 remains a special case: it maps to LlmConfig::default()'s 8192
(a user who wrote `context_window = 0` meant "unknown, use the
default"), not to the floor.

Adds test_validate_clamps_tiny_context_window, which covers 500,
the 1999/2000 boundary, an above-floor value, and 0.
MSG

git add -A
git commit -F /tmp/kod_commit_msg.txt
rm -f /tmp/kod_commit_msg.txt
