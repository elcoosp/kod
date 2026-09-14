#!/usr/bin/env bash
set -uo pipefail

COMPILE_OK=true
TARGET=crates/kod-core/src/engine.rs

if [ ! -f Cargo.toml ] || [ ! -f "$TARGET" ]; then
    echo "ERROR: run from the kod workspace root ($TARGET missing)"
    exit 1
fi

echo "Patching $TARGET: tool-result block name; MAX_TOOL_ROUNDS notice"

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

# --- 1. Fix the block name mismatch -----------------------------------
patch(
    '''            prompt.push_str(&format!(
                "\\n## Tool use\\n\\nYou have these tools (function calls, rooted at the working directory above):\\n{}\\nCall them when you need facts from this machine instead of guessing. Tool outputs return as `## Tool result` blocks — then answer the user.\\n",
                names.join("\\n")
            ));''',
    '''            prompt.push_str(&format!(
                // The block name matches the actual header emitted by
                // `run_tool_calls` (`## Tool results`). Telling the model
                // to look for `## Tool result` — the previous singular
                // spelling — invited it to miss the results block and
                // re-call the same tool.
                "\\n## Tool use\\n\\nYou have these tools (function calls, rooted at the working directory above):\\n{}\\nCall them when you need facts from this machine instead of guessing. Tool outputs return as `## Tool results` blocks — then answer the user.\\n",
                names.join("\\n")
            ));''',
    "## Tool results block name",
)

# --- 2. Reduce MAX_TOOL_ROUNDS and add a caller-visible stop notice ---
patch(
    '''/// Max agentic tool rounds per `process()` call before forcing a summary.
const MAX_TOOL_ROUNDS: usize = 150;''',
    '''/// Max agentic tool rounds per `process()` call before forcing a
/// summary. A single agentic pass typically uses 3–15 rounds for a
/// non-trivial task; 40 is a generous safety margin that catches a
/// runaway loop (a small model that keeps re-calling `read_file` on
/// the same path, unable to recognize it is done) well before the
/// user has waited minutes for nothing. When the cap is hit the loop
/// appends a notice to the transcript so the model — and the user —
/// can see why generation stopped short of a final answer.
const MAX_TOOL_ROUNDS: usize = 40;

/// Appended to the transcript when the tool loop hits
/// [`MAX_TOOL_ROUNDS`] without a text-only reply. Without it, the
/// model's next prompt would open with the appearance of a normal
/// conversation and might keep re-calling tools; with it, the model
/// is prompted to summarize what it has done so far, and any user
/// reading the reply sees why the loop stopped short of a final
/// answer.
const TOOL_ROUNDS_EXHAUSTED_NOTE: &str =
    "\\n\\n[tool-round limit reached — no further tool calls will run this turn. \
     Summarize what has been done so far and what remains.]";''',
    "MAX_TOOL_ROUNDS 150 -> 40 + exhausted note",
)

# --- 3. Emit the notice in run_collected_loop -------------------------
patch(
    '''            match provider
                .generate_with_tools(pending, definitions, options)
                .await?
            {
                GenerationResponse::Text { content, usage } => {
                    last_usage = usage.or(last_usage);
                    final_text.push_str(&content);
                    break;
                }
                GenerationResponse::ToolCalls { calls, usage } => {
                    last_usage = usage.or(last_usage);
                    if calls.is_empty() {
                        break;
                    }
                    let section = self.run_tool_calls(&calls).await;
                    tool_calls.extend(calls);
                    tool_results.extend(section.results);
                    pending.push_str(&format!("\\n\\n{}", section.prompt_block));
                    self.apply_steers(pending).await;
                }
                GenerationResponse::Mixed {
                    content,
                    calls,
                    usage,
                } => {
                    last_usage = usage.or(last_usage);
                    final_text.push_str(&content);
                    if calls.is_empty() {
                        break;
                    }
                    let section = self.run_tool_calls(&calls).await;
                    tool_calls.extend(calls);
                    tool_results.extend(section.results);
                    pending.push_str(&format!("\\n\\n{}", section.prompt_block));
                    self.apply_steers(pending).await;
                }
            }
        }
        Ok((final_text, tool_calls, tool_results, last_usage))
    }''',
    '''            match provider
                .generate_with_tools(pending, definitions, options)
                .await?
            {
                GenerationResponse::Text { content, usage } => {
                    last_usage = usage.or(last_usage);
                    final_text.push_str(&content);
                    break;
                }
                GenerationResponse::ToolCalls { calls, usage } => {
                    last_usage = usage.or(last_usage);
                    if calls.is_empty() {
                        break;
                    }
                    let section = self.run_tool_calls(&calls).await;
                    tool_calls.extend(calls);
                    tool_results.extend(section.results);
                    pending.push_str(&format!("\\n\\n{}", section.prompt_block));
                    self.apply_steers(pending).await;
                }
                GenerationResponse::Mixed {
                    content,
                    calls,
                    usage,
                } => {
                    last_usage = usage.or(last_usage);
                    final_text.push_str(&content);
                    if calls.is_empty() {
                        break;
                    }
                    let section = self.run_tool_calls(&calls).await;
                    tool_calls.extend(calls);
                    tool_results.extend(section.results);
                    pending.push_str(&format!("\\n\\n{}", section.prompt_block));
                    self.apply_steers(pending).await;
                }
            }
        }
        // If we exited because we ran out of rounds (as opposed to the
        // model producing a Text reply), tell the transcript. The
        // caller will ask the model for a summary since final_text is
        // empty; the note steers that summary toward "what got done".
        if tool_calls.len() >= MAX_TOOL_ROUNDS && final_text.trim().is_empty() {
            pending.push_str(TOOL_ROUNDS_EXHAUSTED_NOTE);
        }
        Ok((final_text, tool_calls, tool_results, last_usage))
    }''',
    "collected loop exhausted note",
)

# --- 4. Emit the notice in run_streaming_loop -------------------------
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
        // Ran out of rounds without a text-only reply: let the caller's
        // summary prompt know why, and send a visible notice down the
        // stream so the TUI can show a system line alongside whatever
        // summary the model produces.
        if tool_calls.len() >= MAX_TOOL_ROUNDS && final_text.trim().is_empty() {
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

print("All patches applied.")
PYEOF

if [ $? -ne 0 ]; then
    echo "ERROR: patching failed"
    exit 1
fi

echo "cargo check --workspace"
if ! cargo check --workspace 2>&1; then
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
git commit -m "fix(core): correct tool-results block name; announce tool-round limit

Two small fixes in the tool-loop plumbing.

1. ground_prompt told the model: 'Tool outputs return as ## Tool
   result blocks'. The block that run_tool_calls actually emits is
   '## Tool results' (plural). A model told to look for a singular
   header that never appears in the prompt has no way to know the
   results it just triggered are the very next section — and a small
   model re-calls the same tool rather than reading the block. Fix
   the spelling in the grounding text to match the emitter.

2. MAX_TOOL_ROUNDS was 150 — three orders of magnitude more than the
   3–15 rounds a real agentic pass uses — and when the loop hit the
   cap it exited silently. The user saw tool calls stop with no
   explanation, the model's next prompt had the shape of a normal
   conversation, and the summary prompt could not tell the difference
   between 'the model finished' and 'we ran out of rounds'.

   Reduce the ceiling to 40 (still generous; a runaway loop typically
   gets there in seconds) and add TOOL_ROUNDS_EXHAUSTED_NOTE, which
   the loop appends to the transcript and, in the streaming path,
   sends down the chunk channel as a visible system line. The model's
   forced summary now opens with explicit context, and the TUI shows
   a 'tool-round limit (40) reached — summarising progress' row
   alongside the reply instead of leaving the user guessing.

No public API change."
