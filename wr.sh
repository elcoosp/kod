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

echo "Patching $TARGET: char-boundary-safe truncation + list_files prompt summary"

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

# --- 1. Insert truncate_chars helper before format_call_brief -----------
patch(
    '''/// One-line brief for a tool call: `execute_command cargo test …`,
/// `read_file path=…`. Used for the live "running" indicator.''',
    '''/// Truncate a UTF-8 string to at most `max` bytes, rounding down to the
/// nearest char boundary. Returns the input unchanged when it already
/// fits. Use this instead of `&s[..max]` — the raw slice panics when
/// `max` lands mid-codepoint, which any non-ASCII tool output can hit
/// (a file containing "café", an error message with an em-dash, any
/// emoji in a directory listing).
pub(crate) fn truncate_chars(s: &str, max: usize) -> &str {
    if s.len() <= max {
        return s;
    }
    let mut end = max;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    &s[..end]
}

/// One-line brief for a tool call: `execute_command cargo test …`,
/// `read_file path=…`. Used for the live "running" indicator.''',
    "truncate_chars helper",
)

# --- 2. format_call_brief: first.len() > 100 slice ----------------------
patch(
    '''            let first = cmd.lines().next().unwrap_or(cmd).trim();
            let short = if first.len() > 100 {
                format!("{}…", &first[..100])
            } else {
                first.to_string()
            };''',
    '''            let first = cmd.lines().next().unwrap_or(cmd).trim();
            let short = format!("{}…", truncate_chars(first, 100));''',
    "format_call_brief: command preview",
)
# Guard so we only append the ellipsis when truncation happened.
patch(
    '''            let short = format!("{}…", truncate_chars(first, 100));''',
    '''            let short = if first.len() > 100 {
                format!("{}…", truncate_chars(first, 100))
            } else {
                first.to_string()
            };''',
    "format_call_brief: command preview (conditional ellipsis)",
)

# --- 3. format_call_brief: key-value preview ----------------------------
patch(
    '''                let short = if s.len() > 80 {
                    format!("{}…", &s[..80])
                } else {
                    s
                };
                return format!("{name} {key}={short}");''',
    '''                let short = if s.len() > 80 {
                    format!("{}…", truncate_chars(&s, 80))
                } else {
                    s
                };
                return format!("{name} {key}={short}");''',
    "format_call_brief: key=value preview",
)

# --- 4. format_call_brief: raw-args fallback ----------------------------
patch(
    '''    let short = if raw.len() > 80 {
        format!("{}...", &raw[..80])
    } else {
        raw
    };
    format!("{name} {short}")''',
    '''    let short = if raw.len() > 80 {
        format!("{}...", truncate_chars(&raw, 80))
    } else {
        raw
    };
    format!("{name} {short}")''',
    "format_call_brief: raw-args fallback",
)

# --- 5. format_tool_header: key=value preview ---------------------------
patch(
    '''                let short = if s.len() > 60 {
                    format!("{}…", &s[..60])
                } else {
                    s
                };''',
    '''                let short = if s.len() > 60 {
                    format!("{}…", truncate_chars(&s, 60))
                } else {
                    s
                };''',
    "format_tool_header: key=value preview",
)

# --- 6. run_tool_calls: list_files summary + char-boundary truncation ---
patch(
    '''            let rendered = match &result {
                ToolResult::Success(v) => v.to_string(),
                ToolResult::Error(e) => format!("error: {e}"),
                ToolResult::RequiresConfirmation { description, .. } => {
                    format!("requires confirmation (auto-skipped in TUI): {description}")
                }
            };
            // Cap huge outputs (directory dumps) so context survives.
            let rendered = if rendered.len() > 4000 {
                format!(
                    "{}… [truncated {} chars]",
                    &rendered[..4000],
                    rendered.len() - 4000
                )
            } else {
                rendered
            };''',
    '''            let rendered = match &result {
                // list_files raw JSON is one quoted path per entry; a repo
                // with a target/ dir produces 40k+ entries and the model
                // sees 4000 bytes of quoted paths ending in "[truncated
                // 1523k chars]" — no count, no sense of scale.
                // summarize_tool_result renders "4852 entries in src/:
                // · main.rs · lib.rs … and 4840 more", which is what the
                // model can actually reason about. read_file and grep
                // keep their raw payloads — the model needs the content
                // and the (file, line, text) tuples, not a preview.
                ToolResult::Success(_) if call.tool_name == "list_files" => {
                    summarize_tool_result(&call.tool_name, &result)
                }
                ToolResult::Success(v) => v.to_string(),
                ToolResult::Error(e) => format!("error: {e}"),
                ToolResult::RequiresConfirmation { description, .. } => {
                    format!("requires confirmation (auto-skipped in TUI): {description}")
                }
            };
            // Cap huge outputs so context survives. Slicing must be
            // char-boundary aware — the old `&rendered[..4000]` panicked
            // whenever byte 4000 landed inside a multibyte codepoint
            // (any file containing non-ASCII text).
            let rendered = if rendered.len() > 8000 {
                format!(
                    "{}… [truncated {} bytes]",
                    truncate_chars(&rendered, 8000),
                    rendered.len() - truncate_chars(&rendered, 8000).len()
                )
            } else {
                rendered
            };''',
    "run_tool_calls: list_files summary + char-safe truncation",
)

# --- 7. Tests for the helper and the list_files routing -----------------
patch(
    '''    #[test]
    fn test_summarize_tool_result_shapes() {''',
    '''    #[test]
    fn test_truncate_chars_respects_boundaries() {
        // "café": the é is two UTF-8 bytes. Asking for a byte offset
        // mid-codepoint must round down to the previous boundary.
        let s = "café au lait";
        assert_eq!(truncate_chars(s, 100), s);
        assert_eq!(truncate_chars(s, 3), "caf");
        // 4 bytes lands between 0xc3 and 0xa9 — mid-é. Round down to 3.
        assert_eq!(truncate_chars(s, 4), "caf");
        // 5 bytes ends exactly after the é.
        assert_eq!(truncate_chars(s, 5), "café");
        // Degenerate: max 0 returns the empty string.
        assert_eq!(truncate_chars(s, 0), "");
    }

    #[test]
    fn test_truncate_does_not_panic_mid_multibyte() {
        // Reproduce the exact panic the old `&rendered[..4000]` could
        // hit: 3999 ASCII bytes, then a 2-byte 'é' so that byte offset
        // 4000 is the middle of the codepoint.
        let mut s = "a".repeat(3999);
        s.push('é');
        s.push_str("tail");
        assert_eq!(s.len(), 3999 + 2 + 4);
        // Must not panic.
        let cut = truncate_chars(&s, 4000);
        assert_eq!(cut.len(), 3999, "rounded down to the boundary before é");
        assert!(cut.is_char_boundary(cut.len()));
    }

    /// list_files routes through summarize_tool_result so the model
    /// sees "N entries in …" instead of a truncated quoted-path dump.
    #[tokio::test]
    async fn test_run_tool_calls_summarizes_list_files() {
        use tempfile::TempDir;
        let temp = TempDir::new().unwrap();
        // Two files; the summary should name both and say "2 entries".
        std::fs::write(temp.path().join("alpha.txt"), "").unwrap();
        std::fs::write(temp.path().join("beta.txt"), "").unwrap();

        let db_path = temp.path().join("test.redb");
        let cfg = RouterConfig {
            working_dir: temp.path().to_path_buf(),
            enable_memory: false,
            enable_swarm: false,
            max_skills_per_query: 3,
        };
        let engine = KodEngine::new(cfg, db_path).unwrap();
        engine.start().await.unwrap();

        let calls = vec![ToolCall {
            tool_name: "list_files".to_string(),
            arguments: serde_json::json!({ "path": "." }),
        }];
        let round = engine.run_tool_calls(&calls).await;
        assert_eq!(round.results.len(), 1);
        let block = &round.prompt_block;
        assert!(
            block.contains("2 entr"),
            "list_files summary missing count: {block}"
        );
        assert!(block.contains("alpha.txt"), "got: {block}");
        assert!(block.contains("beta.txt"), "got: {block}");
    }

    #[test]
    fn test_summarize_tool_result_shapes() {''',
    "truncate_chars + list_files tests",
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
git commit -m "fix(core): char-boundary-safe truncation, summarize list_files for prompt

Two related fixes in the tool-result rendering path:

1. Panic on non-ASCII tool output. Four call sites truncated strings
   with a raw byte slice (\\&s[..N]) after a length check. When N lands
   inside a multibyte codepoint — a file containing 'café', an error
   with an em-dash, any directory name with an accented letter — the
   slice panics with 'byte index is not a char boundary' and takes the
   whole agentic loop down mid-generation. The old 4000-byte cap in
   run_tool_calls was the most exposed: it ran on every tool result.

   Add pub(crate) truncate_chars(s, max) -> &str that rounds the cut
   down to the nearest char boundary, and route all four sites through
   it (format_call_brief x2, format_tool_header, run_tool_calls).
   Also bump the result cap from 4000 to 8000 bytes so a mid-size code
   file survives intact.

2. list_files was useless to the model. Its raw JSON is one quoted
   path per entry. A repo with target/ produces 40k+ entries; the
   4000-byte cap chopped the JSON mid-string and the model saw
   '[truncated 1523k chars]' with no entry count. list_files now goes
   through summarize_tool_result in the prompt block, matching what
   the TUI already shows — '4852 entries in src/: · main.rs · lib.rs
   … and 4840 more' — which the model can reason about.

read_file and grep keep their raw payloads: the model needs the
content and the (file, line, text) tuples, not a preview.

Adds three tests: char-boundary rounding for truncate_chars, a
regression that reproduces the mid-codepoint panic without panicking,
and a list_files round that asserts the prompt block carries the
entry count and file names."
