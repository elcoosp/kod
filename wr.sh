#!/usr/bin/env bash
set -uo pipefail

ENGINE=crates/kod-core/src/engine.rs

echo "=== Diagnostic: current tool-loop exhaustion handling ==="
grep -n "TOOL_ROUNDS_EXHAUSTED\|MAX_TOOL_ROUNDS\|tool-round limit" "$ENGINE" || echo "  no exhaustion note present"

echo
echo "=== Current run_collected_loop tail ==="
awk '/async fn run_collected_loop/,/^    \}$/' "$ENGINE" | tail -30

echo
echo "=== Current run_streaming_loop tail ==="
awk '/async fn run_streaming_loop/,/^    \}$/' "$ENGINE" | tail -35

echo
echo "Patching $ENGINE"

python3 - "$ENGINE" << 'PYEOF'
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
# 1. Add the exhausted-note constant next to MAX_TOOL_ROUNDS.
# ----------------------------------------------------------------------
if "TOOL_ROUNDS_EXHAUSTED_NOTE" not in src:
    patch(
        '''/// Max agentic tool rounds per `process()` call before forcing a
/// summary. A single agentic pass typically uses 3–15 rounds for a
/// non-trivial task; 40 is a generous safety margin that catches a
/// runaway loop (a small model that keeps re-calling `read_file` on
/// the same path, unable to recognize it is done) well before the
/// user has waited minutes for nothing.
const MAX_TOOL_ROUNDS: usize = 40;''',
        '''/// Max agentic tool rounds per `process()` call before forcing a
/// summary. A single agentic pass typically uses 3–15 rounds for a
/// non-trivial task; 40 is a generous safety margin that catches a
/// runaway loop (a small model that keeps re-calling `read_file` on
/// the same path, unable to recognize it is done) well before the
/// user has waited minutes for nothing.
const MAX_TOOL_ROUNDS: usize = 40;

/// Notice appended to the conversation when the tool loop hits
/// [`MAX_TOOL_ROUNDS`] without a text-only reply. The loop calls the
/// provider one more time afterwards to request a summary; this note
/// is what steers that summary toward "what got done and what
/// remains" instead of "the model answers as if nothing unusual
/// happened." Without it the last tool-result block is the only
/// context for the summary, and a small model tends to summarize
/// that one result rather than the whole run.
const TOOL_ROUNDS_EXHAUSTED_NOTE: &str =
    "\\n\\n[tool-round limit reached — no further tool calls will run this turn. \\
     Summarize what has been done so far and what remains.]";''',
        "TOOL_ROUNDS_EXHAUSTED_NOTE constant",
    )

# ----------------------------------------------------------------------
# 2. run_collected_loop: append the note after the loop if we exited
#    on the round cap (empty final_text + tool_calls present).
# ----------------------------------------------------------------------
patch(
    '''                    let section = self.run_tool_calls(&calls).await;
                    tool_calls.extend(calls);
                    tool_results.extend(section.results);
                    pending.push_str(&format!("\\n\\n{}", section.prompt_block));
                    self.apply_steers(pending).await;
                }
            }
        }
        Ok((final_text, tool_calls, tool_results, last_usage))
    }

    /// Append queued steer notes to the running conversation (each once).''',
    '''                    let section = self.run_tool_calls(&calls).await;
                    tool_calls.extend(calls);
                    tool_results.extend(section.results);
                    pending.push_str(&format!("\\n\\n{}", section.prompt_block));
                    self.apply_steers(pending).await;
                }
            }
        }
        // Exited on the round cap rather than a text-only reply. Tell
        // the transcript — the caller asks the model for a summary
        // after this returns, and this note is what makes that
        // summary "what got done" rather than a recap of the last
        // tool result.
        if final_text.trim().is_empty() && !tool_calls.is_empty() {
            pending.push_str(TOOL_ROUNDS_EXHAUSTED_NOTE);
        }
        Ok((final_text, tool_calls, tool_results, last_usage))
    }

    /// Append queued steer notes to the running conversation (each once).''',
    "collected loop exhausted note",
)

# ----------------------------------------------------------------------
# 3. run_streaming_loop: same, plus a visible notice on the chunk
#    stream so the TUI can render it.
# ----------------------------------------------------------------------
patch(
    '''            tool_calls.extend(calls);
            tool_results.extend(section.results);
            pending.push_str(&format!("\\n\\n{}", section.prompt_block));
            self.apply_steers(pending).await;
            // Tool is done, result reinjected — next provider call is pure
            // LLM thinking, not tool execution. Tell the UI to drop the
            // "tool: …" line so a slow model doesn't look like a stuck tool.
            let _ = chunk_tx.send(thinking_marker()).await;
        }
        Ok((final_text, tool_calls, tool_results, last_usage))
    }''',
    '''            tool_calls.extend(calls);
            tool_results.extend(section.results);
            pending.push_str(&format!("\\n\\n{}", section.prompt_block));
            self.apply_steers(pending).await;
            // Tool is done, result reinjected — next provider call is pure
            // LLM thinking, not tool execution. Tell the UI to drop the
            // "tool: …" line so a slow model doesn't look like a stuck tool.
            let _ = chunk_tx.send(thinking_marker()).await;
        }
        // Exited on the round cap (empty text + tool calls present).
        // Append the note for the model, and send a visible line down
        // the chunk stream so the user sees why generation stopped
        // short of a final answer.
        if final_text.trim().is_empty() && !tool_calls.is_empty() {
            pending.push_str(TOOL_ROUNDS_EXHAUSTED_NOTE);
            let _ = chunk_tx
                .send(format!(
                    "\\n\\n[tool-round limit ({MAX_TOOL_ROUNDS}) reached — summarising progress]\\n"
                ))
                .await;
        }
        Ok((final_text, tool_calls, tool_results, last_usage))
    }''',
    "streaming loop exhausted note + stream notice",
)

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
feat(core): announce tool-round-limit exhaustion

MAX_TOOL_ROUNDS (now 40) is a safety valve against a runaway
agentic loop — a small model that keeps re-calling read_file on the
same path, unable to recognize it is done. The loop exits cleanly
at the cap, but silently: the caller then asks the model for a
summary, and the model sees only the last tool-result block as
context. The result is a plausible-looking final answer that has
nothing to do with what actually happened over the previous 40
rounds.

Add TOOL_ROUNDS_EXHAUSTED_NOTE, appended to the transcript when the
loop exits on the cap (final text empty, tool calls present). The
note tells the model "no further tool calls will run this turn —
summarize what has been done so far and what remains," which
steers the follow-up summary toward a run-level recap rather than a
recap of the last round.

In the streaming path, also send a one-line notice down the chunk
channel:

  [tool-round limit (40) reached — summarising progress]

so the TUI renders a system line alongside the model's summary.
Without it, the user sees tool calls stop and a summary appear
with no explanation of why generation didn't reach a final answer —
indistinguishable from a normal completion.

No public API change.
MSG

git add -A
git commit -F /tmp/kod_commit_msg.txt
rm -f /tmp/kod_commit_msg.txt
