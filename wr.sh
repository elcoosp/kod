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
TARGET=crates/kod-tui/src/app.rs

if [ ! -f Cargo.toml ] || [ ! -f "$TARGET" ]; then
    echo "ERROR: run from the kod workspace root ($TARGET missing)"
    exit 1
fi

echo "Patching $TARGET: add model-not-found, context-length, malformed-args hints"

python3 - "$TARGET" << 'PYEOF'
import os
import sys

target = sys.argv[1]
with open(target, "r") as f:
    content = f.read()

old = '''    /// Turn a raw provider/transport error into something the user can act
    /// on. The original text is always kept — advice is appended.
    pub fn friendly_error(error: &str, fail_count: usize) -> String {
        let lower = error.to_lowercase();
        let advice = if lower.contains("connection refused")
            || lower.contains("connection reset")
            || lower.contains("failed to connect")
            || lower.contains("connection closed")
        {
            " Could not reach the model server — is it running? For Ollama: `ollama serve`, then check `base_url` in the kod config."
        } else if lower.contains("401")
            || lower.contains("unauthorized")
            || lower.contains("api key")
        {
            " Looks like an auth problem — check `api_key` in the kod config."
        } else if lower.contains("404") {
            " Endpoint not found — check `base_url` ends with `/v1` for OpenAI-compatible servers."
        } else if lower.contains("timed out")
            || lower.contains("timeout")
            || lower.contains("deadline")
        {
            " The request timed out — the model may still be loading (first run pulls weights). Wait a minute and `/retry`."
        } else {
            // Any other error carries no situational advice. This also
            // covers "cancelled by user" (which the engine treats as a
            // normal stop, not a failure) and matches the previous
            // behavior of returning an empty advice string for both.
            ""
        };
        let mut out = format!("Error: {error}");
        out.push_str(advice);
        if fail_count >= 2 {
            out.push_str(
                " (offline mode: generation keeps failing — fix the server, then `/retry`)",
            );
        }
        out
    }'''

new = '''    /// Turn a raw provider/transport error into something the user can act
    /// on. The original text is always kept — advice is appended.
    ///
    /// Ordering matters: "model not found" is often reported by OpenAI-
    /// compatible servers as a 404, and context-length errors sometimes
    /// arrive wrapped in a `400` or `422`. Match the most specific
    /// phrasing first.
    pub fn friendly_error(error: &str, fail_count: usize) -> String {
        let lower = error.to_lowercase();
        let advice = if lower.contains("model not found")
            || lower.contains("model `")
            || lower.contains("unknown model")
            || lower.contains("no such model")
            || lower.contains("model does not exist")
        {
            // The most common first-run mistake: config or `/model <name>`
            // names a model the server has never pulled. The fix is one
            // command, so name it.
            " The model named in the config or `/model` is not on the server. \
             Pull it first (e.g. `ollama pull codellama:13b`), or run `/model <name>` \
             with a model the server already has."
        } else if lower.contains("context length")
            || lower.contains("context window")
            || lower.contains("too many tokens")
            || lower.contains("maximum context")
            || lower.contains("exceeds the maximum")
        {
            " The prompt exceeded the model's context window. Run `/compact` to trim \
             the session, or start a fresh chat with `/clear`."
        } else if (lower.contains("json") && lower.contains("parse"))
            || lower.contains("invalid tool")
            || lower.contains("malformed function")
            || lower.contains("tool_call")
        {
            " The model returned a tool call that could not be parsed. Retrying \
             usually helps — if it persists, the model may not support tool calling \
             at all (try a larger or newer model, or a codellama/qwen2.5-coder build)."
        } else if lower.contains("connection refused")
            || lower.contains("connection reset")
            || lower.contains("failed to connect")
            || lower.contains("connection closed")
        {
            " Could not reach the model server — is it running? For Ollama: `ollama serve`, then check `base_url` in the kod config."
        } else if lower.contains("401")
            || lower.contains("unauthorized")
            || lower.contains("api key")
        {
            " Looks like an auth problem — check `api_key` in the kod config."
        } else if lower.contains("404") {
            " Endpoint not found — check `base_url` ends with `/v1` for OpenAI-compatible servers."
        } else if lower.contains("timed out")
            || lower.contains("timeout")
            || lower.contains("deadline")
        {
            " The request timed out — the model may still be loading (first run pulls weights). Wait a minute and `/retry`."
        } else {
            // Any other error carries no situational advice. This also
            // covers "cancelled by user" (which the engine treats as a
            // normal stop, not a failure) and matches the previous
            // behavior of returning an empty advice string for both.
            ""
        };
        let mut out = format!("Error: {error}");
        out.push_str(advice);
        if fail_count >= 2 {
            out.push_str(
                " (offline mode: generation keeps failing — fix the server, then `/retry`)",
            );
        }
        out
    }'''

n = content.count(old)
if n != 1:
    print(f"ERROR: expected 1 occurrence of friendly_error body, found {n}")
    sys.exit(2)
content = content.replace(old, new, 1)

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

echo "Patching test file for the new error classes"
TEST_FILE=crates/kod-tui/tests/app.rs

python3 - "$TEST_FILE" << 'PYEOF'
import os
import sys

target = sys.argv[1]
with open(target, "r") as f:
    content = f.read()

if "test_friendly_error_advice" in content:
    print("Skipped: friendly_error tests already present")
    sys.exit(0)

content += '''

/// Friendly-error coverage: each branch of KodApp::friendly_error must
/// fire on the phrasing OpenAI-compatible servers actually produce,
/// and the raw error text must always be preserved.
#[test]
fn test_friendly_error_advice() {
    use kod_tui::app::KodApp;

    // 1. Model not found — advice should name `ollama pull`.
    let m = KodApp::friendly_error(
        "provider error: model `codellama:13b` not found",
        0,
    );
    assert!(m.contains("Error:"), "raw text preserved: {m}");
    assert!(m.contains("ollama pull"), "model advice missing: {m}");

    // 2. Context length — advice should name /compact.
    let m = KodApp::friendly_error(
        "400 Bad Request: maximum context length is 8192 tokens",
        0,
    );
    assert!(m.contains("/compact"), "context advice missing: {m}");

    // 3. Malformed tool call — advice should mention tool calling.
    let m = KodApp::friendly_error(
        "provider error: invalid tool call: expected value at line 1",
        0,
    );
    assert!(
        m.contains("tool call") && m.contains("tool calling"),
        "tool-parse advice missing: {m}"
    );

    // 4. Connectivity — advice should mention ollama serve.
    let m = KodApp::friendly_error(
        "connection refused: 127.0.0.1:11434",
        0,
    );
    assert!(m.contains("ollama serve"), "connection advice missing: {m}");

    // 5. Auth — advice should mention api_key.
    let m = KodApp::friendly_error("401 Unauthorized", 0);
    assert!(m.contains("api_key"), "auth advice missing: {m}");

    // 6. Timeout — advice should mention /retry.
    let m = KodApp::friendly_error("request timed out after 300s", 0);
    assert!(m.contains("/retry"), "timeout advice missing: {m}");

    // 7. Unknown error — raw text preserved, no advice appended.
    let m = KodApp::friendly_error("something else went wrong", 0);
    assert_eq!(m, "Error: something else went wrong");

    // 8. fail_count >= 2 appends the offline-mode footer to any branch.
    let m = KodApp::friendly_error("connection refused", 3);
    assert!(m.contains("offline mode"), "offline footer missing: {m}");

    // 9. Specific-branch precedence: a message containing both
    // "model not found" and "404" should pick the model-not-found
    // advice, not the endpoint-not-found one.
    let m = KodApp::friendly_error(
        "404 Not Found: model not found in registry",
        0,
    );
    assert!(
        m.contains("ollama pull"),
        "model-not-found should win over 404: {m}"
    );
}
'''

with open(target, "w") as f:
    f.write(content)
    print("Patched tests/app.rs: friendly_error coverage")

PYEOF

if [ $? -ne 0 ]; then
    echo "ERROR: patching tests failed"
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
git commit -m "feat(tui): broaden friendly_error with three common failure classes

KodApp::friendly_error mapped connectivity, auth, 404, and timeout
errors to actionable advice. Three common failures were falling
through to the raw text with no guidance:

- 'model not found' (a config or /model name the server has never
  pulled). Advice names 'ollama pull <name>' explicitly.
- Context-length errors ('context length', 'too many tokens',
  'maximum context'), wrapped in a 400 or 422 by most servers.
  Advice names /compact and /clear.
- Malformed tool calls ('invalid tool call', JSON parse errors
  around tool_call payloads). Advice names the /retry path and
  hints that a small model may not support tool calling at all.

Order matters: 'model not found' is sometimes reported as a 404, and
context-length errors are wrapped in 400/422, so the specific
phrasings are checked before the generic status-code branches.
Adds test_friendly_error_advice covering each branch, the raw-text
preservation, the offline footer, and the specific-over-generic
precedence."
