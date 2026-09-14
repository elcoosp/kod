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

ROOT_CARGO=Cargo.toml
TOOLS_CARGO=crates/kod-tools/Cargo.toml
TOOLS_RS=crates/kod-tools/src/tools.rs

for f in "$ROOT_CARGO" "$TOOLS_CARGO" "$TOOLS_RS"; do
    if [ ! -f "$f" ]; then
        echo "ERROR: missing $f — run from the kod workspace root"
        exit 1
    fi
done

echo "Adding regex to workspace deps and rewriting GrepTool to actually use it"

python3 - "$ROOT_CARGO" "$TOOLS_CARGO" "$TOOLS_RS" << 'PYEOF'
import os
import sys

root_cargo, tools_cargo, tools_rs = sys.argv[1], sys.argv[2], sys.argv[3]

def patch(path, old, new, label, expect=1):
    with open(path, "r") as f:
        content = f.read()
    n = content.count(old)
    if n == 0:
        print(f"ERROR: old snippet not found in {path}: {label}")
        sys.exit(2)
    if expect and n != expect:
        print(f"ERROR: expected {expect} occurrence(s) of {label} in {path}, found {n}")
        sys.exit(2)
    patched = content.replace(old, new, expect if expect else n)
    tmp = path + ".tmp"
    with open(tmp, "w") as f:
        f.write(patched)
    os.replace(tmp, path)
    print(f"Patched {path}: {label}")

# --- 1. Root Cargo.toml: add regex to [workspace.dependencies] ----------
patch(
    root_cargo,
    '''globset = "0.4.20"
notify = "8.2.0"
fs4 = "1.1.0"''',
    '''globset = "0.4.20"
regex = "1.13"
notify = "8.2.0"
fs4 = "1.1.0"''',
    "regex in workspace deps",
)

# --- 2. kod-tools Cargo.toml: add regex dep -----------------------------
patch(
    tools_cargo,
    '''ignore = { workspace = true }
globset = { workspace = true }
fs4 = { workspace = true }''',
    '''ignore = { workspace = true }
globset = { workspace = true }
regex = { workspace = true }
fs4 = { workspace = true }''',
    "regex in kod-tools deps",
)

# --- 3. tools.rs: import regex -----------------------------------------
patch(
    tools_rs,
    '''use crate::{Tool, ToolContext};
use kod_error::{KodError, Result};
use kod_types::{ToolCategory, ToolDefinition, ToolId, ToolPermissions, ToolResult};
use serde_json::Value;''',
    '''use crate::{Tool, ToolContext};
use kod_error::{KodError, Result};
use kod_types::{ToolCategory, ToolDefinition, ToolId, ToolPermissions, ToolResult};
use regex::Regex;
use serde_json::Value;''',
    "import regex",
)

# --- 4. tools.rs: update GrepTool description and schema ---------------
patch(
    tools_rs,
    '''            definition: ToolDefinition {
                id: ToolId::new(),
                name: "grep".to_string(),
                description: "Search for a pattern in files".to_string(),
                category: ToolCategory::FileSystem,
                parameters_schema: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "path": {
                            "type": "string",
                            "description": "Directory to search"
                        },
                        "pattern": {
                            "type": "string",
                            "description": "Pattern to search for"
                        },
                        "recursive": {
                            "type": "boolean",
                            "description": "Search recursively"
                        }
                    },
                    "required": ["path", "pattern"]
                }),''',
    '''            definition: ToolDefinition {
                id: ToolId::new(),
                name: "grep".to_string(),
                description: "Search file contents with a regular expression. Respects .gitignore (skips target/, node_modules/, .git, …); results cap at 500 matches. Use \\\\b, \\\\w, [abc], (a|b), etc. — not PCRE lookarounds.".to_string(),
                category: ToolCategory::FileSystem,
                parameters_schema: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "path": {
                            "type": "string",
                            "description": "Directory to search"
                        },
                        "pattern": {
                            "type": "string",
                            "description": "Rust `regex` crate pattern. Metacharacters are active; escape them (e.g. \\\\.) to match literally."
                        },
                        "recursive": {
                            "type": "boolean",
                            "description": "Search recursively"
                        },
                        "case_insensitive": {
                            "type": "boolean",
                            "description": "Match case-insensitively (default false, matching grep). Set true when the case of the target is unknown."
                        }
                    },
                    "required": ["path", "pattern"]
                }),''',
    "GrepTool description and schema",
)

# --- 5. tools.rs: replace GrepTool::execute body -----------------------
patch(
    tools_rs,
    '''    async fn execute(&self, params: &Value, context: &ToolContext) -> Result<ToolResult> {
        let path = params["path"]
            .as_str()
            .ok_or_else(|| KodError::InvalidParameters {
                reason: "Missing 'path' parameter".to_string(),
            })?;
        let pattern = params["pattern"]
            .as_str()
            .ok_or_else(|| KodError::InvalidParameters {
                reason: "Missing 'pattern' parameter".to_string(),
            })?;
        let recursive = params["recursive"].as_bool().unwrap_or(false);

        let resolved = context.resolve_path(path)?;
        context.can_read(&resolved)?;

        let glob = if recursive {
            format!("{}/**", path)
        } else {
            format!("{}/*", path)
        };

        let matcher = globset::GlobSetBuilder::new()
            .add(
                globset::Glob::new(&glob)
                    .map_err(|e| KodError::InvalidState(format!("Invalid glob: {}", e)))?,
            )
            .build()
            .map_err(|e| KodError::InvalidState(format!("Invalid globset: {}", e)))?;

        let mut results = Vec::new();

        for file_path in gitaware_walk(&resolved, recursive) {
            if !matcher.is_match(&file_path) {
                continue;
            }

            if results.len() >= MAX_GREP_MATCHES {
                break;
            }
            if file_path.is_file() {
                let content = match std::fs::read_to_string(&file_path) {
                    Ok(c) => c,
                    Err(_) => continue,
                };
                for (line_num, line) in content.lines().enumerate() {
                    if line.contains(pattern) {
                        results.push(serde_json::json!({
                            "file": file_path.to_string_lossy().to_string(),
                            "line": line_num + 1,
                            "text": line.trim(),
                        }));
                        if results.len() >= MAX_GREP_MATCHES {
                            break;
                        }
                    }
                }
            }
        }

        Ok(ToolResult::Success(serde_json::json!({
            "pattern": pattern,
            "results": results,
            "truncated": results.len() >= MAX_GREP_MATCHES,
        })))
    }''',
    '''    async fn execute(&self, params: &Value, context: &ToolContext) -> Result<ToolResult> {
        let path = params["path"]
            .as_str()
            .ok_or_else(|| KodError::InvalidParameters {
                reason: "Missing 'path' parameter".to_string(),
            })?;
        let pattern = params["pattern"]
            .as_str()
            .ok_or_else(|| KodError::InvalidParameters {
                reason: "Missing 'pattern' parameter".to_string(),
            })?;
        let recursive = params["recursive"].as_bool().unwrap_or(false);
        let case_insensitive = params["case_insensitive"].as_bool().unwrap_or(false);

        let resolved = context.resolve_path(path)?;
        context.can_read(&resolved)?;

        // Compile the caller's pattern as a regex. An invalid pattern
        // becomes a ToolResult::Error so the model sees its own mistake
        // instead of the whole tool loop stalling.
        let regex = match Regex::new(pattern) {
            Ok(r) => r,
            Err(e) => {
                return Ok(ToolResult::Error(format!(
                    "invalid regex {:?}: {}",
                    pattern, e
                )));
            }
        };
        // `regex` has no inline (?i) rebuild helper, so recompile with
        // the case-insensitive flag when requested.
        let regex = if case_insensitive {
            let folded = format!("(?i){}", pattern);
            match Regex::new(&folded) {
                Ok(r) => r,
                Err(e) => {
                    return Ok(ToolResult::Error(format!(
                        "invalid regex {:?}: {}",
                        pattern, e
                    )));
                }
            }
        } else {
            regex
        };

        // `gitaware_walk` already roots at `resolved` and depth-limits
        // when non-recursive, so its yielded paths are the search set.
        // The old code additionally filtered with a glob built from the
        // *user-supplied* `path` — which never matched the absolute
        // paths the walker returns, so grep silently returned nothing
        // for relative-path calls. Dropped.
        let mut results = Vec::new();

        for file_path in gitaware_walk(&resolved, recursive) {
            if results.len() >= MAX_GREP_MATCHES {
                break;
            }
            if !file_path.is_file() {
                continue;
            }
            let content = match std::fs::read_to_string(&file_path) {
                Ok(c) => c,
                Err(_) => continue,
            };
            for (line_num, line) in content.lines().enumerate() {
                if regex.is_match(line) {
                    results.push(serde_json::json!({
                        "file": file_path.to_string_lossy().to_string(),
                        "line": line_num + 1,
                        "text": line.trim(),
                    }));
                    if results.len() >= MAX_GREP_MATCHES {
                        break;
                    }
                }
            }
        }

        Ok(ToolResult::Success(serde_json::json!({
            "pattern": pattern,
            "case_insensitive": case_insensitive,
            "results": results,
            "truncated": results.len() >= MAX_GREP_MATCHES,
        })))
    }''',
    "GrepTool::execute regex body",
)

# --- 6. tools.rs: append grep tests to the inline test module ----------
old_tail = '''    #[cfg(unix)]
    #[tokio::test]
    async fn execute_command_small_output_is_not_truncated() {
        let temp = tempfile::TempDir::new().unwrap();
        let ctx = full_context(temp.path());
        let tool = ExecuteCommandTool::new();
        let params = serde_json::json!({ "command": "echo hello" });
        let result = tool.execute(&params, &ctx).await.unwrap();

        match result {
            ToolResult::Success(v) => {
                assert_eq!(v["stdout_truncated"], false);
                assert_eq!(v["stdout"], "hello\\n");
                assert_eq!(v["exit_code"], 0);
            }
            other => panic!("expected success, got {:?}", other),
        }
    }
}'''

new_tail = '''    #[cfg(unix)]
    #[tokio::test]
    async fn execute_command_small_output_is_not_truncated() {
        let temp = tempfile::TempDir::new().unwrap();
        let ctx = full_context(temp.path());
        let tool = ExecuteCommandTool::new();
        let params = serde_json::json!({ "command": "echo hello" });
        let result = tool.execute(&params, &ctx).await.unwrap();

        match result {
            ToolResult::Success(v) => {
                assert_eq!(v["stdout_truncated"], false);
                assert_eq!(v["stdout"], "hello\\n");
                assert_eq!(v["exit_code"], 0);
            }
            other => panic!("expected success, got {:?}", other),
        }
    }

    fn grep_ctx(dir: &std::path::Path) -> ToolContext {
        ToolContext::new(dir).with_permissions(ToolPermissions {
            read_files: true,
            ..Default::default()
        })
    }

    #[tokio::test]
    async fn grep_finds_literal_pattern_from_relative_path() {
        // Regression: the previous implementation built a glob from the
        // user-supplied relative `path` and matched it against the
        // absolute paths returned by the walker, so calling grep with
        // path="." or path="src" silently returned zero matches.
        let temp = tempfile::TempDir::new().unwrap();
        std::fs::write(temp.path().join("a.txt"), "needle\\nhay\\n").unwrap();
        std::fs::write(temp.path().join("b.txt"), "hay only\\n").unwrap();

        let ctx = grep_ctx(temp.path());
        let tool = GrepTool::new();
        let params = serde_json::json!({ "path": ".", "pattern": "needle" });
        let result = tool.execute(&params, &ctx).await.unwrap();

        match result {
            ToolResult::Success(v) => {
                let hits = v["results"].as_array().unwrap();
                assert_eq!(hits.len(), 1, "got {:?}", hits);
                assert_eq!(hits[0]["text"], "needle");
            }
            other => panic!("expected success, got {:?}", other),
        }
    }

    #[tokio::test]
    async fn grep_treats_pattern_as_regex() {
        let temp = tempfile::TempDir::new().unwrap();
        std::fs::write(
            temp.path().join("code.rs"),
            "fn main() {}\\nfn helper(x: u32) -> u32 { x }\\nlet n = 42;\\n",
        )
        .unwrap();

        let ctx = grep_ctx(temp.path());
        let tool = GrepTool::new();
        // Match any `fn <name>(` definition.
        let params = serde_json::json!({
            "path": ".",
            "pattern": r"fn\\s+\\w+\\s*\\("
        });
        let result = tool.execute(&params, &ctx).await.unwrap();

        match result {
            ToolResult::Success(v) => {
                let hits = v["results"].as_array().unwrap();
                assert_eq!(hits.len(), 2, "got {:?}", hits);
            }
            other => panic!("expected success, got {:?}", other),
        }
    }

    #[tokio::test]
    async fn grep_case_insensitive_flag_works() {
        let temp = tempfile::TempDir::new().unwrap();
        std::fs::write(temp.path().join("mixed.txt"), "TODO\\ntodo\\nTodo\\n").unwrap();

        let ctx = grep_ctx(temp.path());
        let tool = GrepTool::new();

        let params = serde_json::json!({
            "path": ".",
            "pattern": "todo"
        });
        let result = tool.execute(&params, &ctx).await.unwrap();
        match result {
            ToolResult::Success(v) => {
                assert_eq!(v["results"].as_array().unwrap().len(), 1);
            }
            other => panic!("expected success, got {:?}", other),
        }

        let params = serde_json::json!({
            "path": ".",
            "pattern": "todo",
            "case_insensitive": true
        });
        let result = tool.execute(&params, &ctx).await.unwrap();
        match result {
            ToolResult::Success(v) => {
                assert_eq!(v["results"].as_array().unwrap().len(), 3);
            }
            other => panic!("expected success, got {:?}", other),
        }
    }

    #[tokio::test]
    async fn grep_invalid_regex_returns_error_result() {
        let temp = tempfile::TempDir::new().unwrap();
        std::fs::write(temp.path().join("a.txt"), "anything").unwrap();

        let ctx = grep_ctx(temp.path());
        let tool = GrepTool::new();
        let params = serde_json::json!({ "path": ".", "pattern": "[" });
        let result = tool.execute(&params, &ctx).await.unwrap();

        match result {
            ToolResult::Error(msg) => {
                assert!(msg.contains("invalid regex"), "got: {msg}");
            }
            other => panic!("expected ToolResult::Error, got {:?}", other),
        }
    }
}'''

patch(tools_rs, old_tail, new_tail, "grep tests")

print("All patches applied.")
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

echo "Running kod-tools tests (120s wall clock)"
if ! run_with_timeout 120 cargo test -p kod-tools 2>&1; then
    echo "kod-tools tests failed or hung. Paste the full output for a surgical fix."
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
git commit -m "fix(tools): make grep actually regex, fix broken path filter

Two bugs in one tool:

1. The schema described 'pattern' as 'Pattern to search for' but the
   implementation did a literal substring match (line.contains). A
   model writing grep { pattern: 'fn \\w+\\(' } got zero hits with no
   hint that metacharacters were inert. Now compiles the pattern with
   the regex crate and returns ToolResult::Error on an invalid
   pattern, so the model sees its own mistake instead of a silent
   empty result. Adds an optional case_insensitive flag, default
   false to match grep semantics.

2. The search set was already produced by gitaware_walk, which is
   rooted at the resolved path and depth-limited for non-recursive
   calls. The old code additionally filtered with a glob built from
   the user-supplied relative path ('src/**') and matched it against
   the walker's absolute paths ('/abs/cwd/src/main.rs'), so grep
   returned nothing whenever it was called with a relative path. The
   redundant glob filter is removed.

Also adds four tests: literal match from a relative path, regex
metacharacter matching, the case_insensitive flag, and invalid-regex
error reporting. Workspace deps gain regex = '1.13' (already present
transitively via globset, so no new compile cost)."
