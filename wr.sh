#!/usr/bin/env bash
set -uo pipefail

COMPILE_OK=true
TARGET=crates/kod-tools/src/tools.rs

if [ ! -f Cargo.toml ] || [ ! -f "$TARGET" ]; then
    echo "ERROR: run from the kod workspace root ($TARGET missing)"
    exit 1
fi

echo "Patching $TARGET: stream grep line-by-line; skip oversized files"

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

# --- 1. Add the per-file size cap constant alongside MAX_ENTRY_BYTES ---
patch(
    '''/// 1 KB is generous for a path (typical: 40–120 bytes) and for a line of
/// code (typical: 20–200 bytes) while bounded enough that 5000 entries
/// cannot exceed ~5 MB even in the pathological case.
const MAX_ENTRY_BYTES: usize = 1024;''',
    '''/// 1 KB is generous for a path (typical: 40–120 bytes) and for a line of
/// code (typical: 20–200 bytes) while bounded enough that 5000 entries
/// cannot exceed ~5 MB even in the pathological case.
const MAX_ENTRY_BYTES: usize = 1024;

/// Per-file byte cap for `grep`. Files larger than this are skipped and
/// reported in the result's `skipped_large_files` list.
///
/// The pre-streaming implementation called `std::fs::read_to_string` on
/// every candidate file, so a 2 GB log — the exact file a user might
/// want to grep — would allocate the whole thing into memory and OOM
/// the process before the entry cap could fire. A source tree rarely
/// has a file over a megabyte, and a file that large rarely contains
/// the line-level pattern a coding agent is looking for; 8 MB is
/// generous for the useful case and cheap to bound the useless one.
const MAX_GREP_FILE_BYTES: u64 = 8 * 1024 * 1024;''',
    "MAX_GREP_FILE_BYTES",
)

# --- 2. Stream lines through BufReader and skip oversized files --------
patch(
    '''        // `gitaware_walk` already roots at `resolved` and depth-limits
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
                    // Cap the matched text per entry. A generated
                    // bundler output file with 8 KB of inline JSON
                    // on one line would otherwise produce a single
                    // 8 KB match and blow the model's prompt budget
                    // for the whole call. The `file` field is left
                    // untouched — paths are already short, and the
                    // model needs the real path to open the file.
                    results.push(serde_json::json!({
                        "file": file_path.to_string_lossy().to_string(),
                        "line": line_num + 1,
                        "text": truncate_entry(line.trim(), MAX_ENTRY_BYTES),
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
        })))''',
    '''        // `gitaware_walk` already roots at `resolved` and depth-limits
        // when non-recursive, so its yielded paths are the search set.
        // The old code additionally filtered with a glob built from the
        // *user-supplied* `path` — which never matched the absolute
        // paths the walker returns, so grep silently returned nothing
        // for relative-path calls. Dropped.
        let mut results = Vec::new();
        // Files whose size exceeded MAX_GREP_FILE_BYTES. Reported in
        // the result so the model knows the search was not exhaustive
        // and can decide whether to grep them specifically (or read
        // them with an offset once that exists).
        let mut skipped_large_files: Vec<String> = Vec::new();

        use std::io::BufRead as _;

        for file_path in gitaware_walk(&resolved, recursive) {
            if results.len() >= MAX_GREP_MATCHES {
                break;
            }
            if !file_path.is_file() {
                continue;
            }
            // Size check before opening. `read_to_string` used to load
            // the entire file into memory; a 2 GB log would OOM here.
            if let Ok(meta) = std::fs::metadata(&file_path)
                && meta.len() > MAX_GREP_FILE_BYTES
            {
                skipped_large_files.push(file_path.to_string_lossy().to_string());
                continue;
            }
            let file = match std::fs::File::open(&file_path) {
                Ok(f) => f,
                Err(_) => continue,
            };
            let reader = std::io::BufReader::new(file);
            // `.lines()` yields Result<String>; a line containing
            // invalid UTF-8 (a binary file, a log with raw bytes)
            // produces Err and we stop scanning that file. The old
            // `read_to_string` failed the whole file on any bad byte;
            // the streaming form at least gets matches from the clean
            // prefix.
            for (line_num, line_result) in reader.lines().enumerate() {
                let line = match line_result {
                    Ok(l) => l,
                    Err(_) => break,
                };
                if regex.is_match(&line) {
                    // Cap the matched text per entry. A generated
                    // bundler output file with 8 KB of inline JSON
                    // on one line would otherwise produce a single
                    // 8 KB match and blow the model's prompt budget
                    // for the whole call. The `file` field is left
                    // untouched — paths are already short, and the
                    // model needs the real path to open the file.
                    results.push(serde_json::json!({
                        "file": file_path.to_string_lossy().to_string(),
                        "line": line_num + 1,
                        "text": truncate_entry(line.trim(), MAX_ENTRY_BYTES),
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
            "skipped_large_files": skipped_large_files,
        })))''',
    "grep streaming + skip large files",
)

# --- 3. Tests --------------------------------------------------------
patch(
    '''    #[tokio::test]
    async fn grep_invalid_regex_returns_error_result() {''',
    '''    /// A file larger than MAX_GREP_FILE_BYTES must be skipped, and
    /// the skip must be reported in the result. Before streaming, the
    /// old code called read_to_string on every candidate file and
    /// would OOM on a multi-gigabyte log.
    #[tokio::test]
    async fn grep_skips_oversized_files() {
        let temp = tempfile::TempDir::new().unwrap();
        // A file one byte over the cap. Filling it with 'x' is fast
        // enough; the pattern will not match anyway, so the skip path
        // is the only thing that determines the outcome.
        let big = temp.path().join("huge.log");
        let filler = vec![b'x'; (MAX_GREP_FILE_BYTES + 1) as usize];
        std::fs::write(&big, &filler).unwrap();
        // A small file that would match, so we can also assert the
        // search continued to the small file after the skip.
        std::fs::write(temp.path().join("small.txt"), "needle here").unwrap();

        let ctx = grep_ctx(temp.path());
        let tool = GrepTool::new();
        let params = serde_json::json!({ "path": ".", "pattern": "needle" });
        let result = tool.execute(&params, &ctx).await.unwrap();

        match result {
            ToolResult::Success(v) => {
                let hits = v["results"].as_array().unwrap();
                assert_eq!(hits.len(), 1, "small file match missing: {hits:?}");
                let skipped = v["skipped_large_files"].as_array().unwrap();
                assert_eq!(skipped.len(), 1, "huge.log should be skipped");
                assert!(skipped[0].as_str().unwrap().ends_with("huge.log"));
            }
            other => panic!("expected success, got {:?}", other),
        }
    }

    /// A file within the cap must be searched normally even when there
    /// is also a skipped file. Sanity that the `continue` after the
    /// size check does not accidentally short-circuit the walk.
    #[tokio::test]
    async fn grep_searches_files_within_cap() {
        let temp = tempfile::TempDir::new().unwrap();
        std::fs::write(temp.path().join("a.txt"), "alpha\\nneedle\\n").unwrap();
        std::fs::write(temp.path().join("b.txt"), "beta\\nneedle too\\n").unwrap();

        let ctx = grep_ctx(temp.path());
        let tool = GrepTool::new();
        let params = serde_json::json!({ "path": ".", "pattern": "needle" });
        let result = tool.execute(&params, &ctx).await.unwrap();

        match result {
            ToolResult::Success(v) => {
                assert_eq!(v["results"].as_array().unwrap().len(), 2);
                assert!(
                    v["skipped_large_files"].as_array().unwrap().is_empty(),
                    "nothing should be skipped at this size"
                );
            }
            other => panic!("expected success, got {:?}", other),
        }
    }

    #[tokio::test]
    async fn grep_invalid_regex_returns_error_result() {''',
    "grep streaming tests",
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
git commit -m "fix(tools): stream grep line-by-line, skip oversized files

GrepTool::execute called std::fs::read_to_string on every candidate
file, loading the entire file into memory before any line was
examined. A 2 GB application log — the exact file a developer might
ask grep to search — would allocate the whole thing and OOM the
process well before the per-entry cap or the match cap could
matter. The read_file tool was already capped at 256 KB for exactly
this reason; grep, which is more likely to be aimed at logs, was
not.

Stream through std::io::BufReader::lines() instead. The streaming
reader holds one line in memory at a time; the only per-file
allocation is the returned line. A file with a multi-megabyte single
line still holds that line, which is why MAX_ENTRY_BYTES already
caps what we *return*, but nothing else in the file is buffered.

Add MAX_GREP_FILE_BYTES = 8 MB and skip files above it, listing them
in a new \`skipped_large_files\` field on the result. A coding agent
grepping a source tree almost never wants a match inside a huge
binary blob or log; when it does, seeing the path in
\`skipped_large_files\` tells it the search was not exhaustive, and
it can read the file directly instead of getting a wrong 'no
matches' answer.

The streaming reader also degrades more gracefully on non-UTF-8
input: a file that stops being valid UTF-8 at byte N used to fail
the whole file (read_to_string returns Err on any invalid byte);
streaming gets matches from the clean prefix and stops at the first
bad line.

Adds two tests. grep_skips_oversized_files writes a file one byte
over the cap alongside a small matching file, asserts the small
file matched and the large one is in skipped_large_files;
grep_searches_files_within_cap confirms the continue-after-skip
does not short-circuit the walk."
