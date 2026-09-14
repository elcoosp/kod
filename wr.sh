#!/usr/bin/env bash
set -uo pipefail

COMPILE_OK=true
TOOLS=crates/kod-tools/src/tools.rs
ENGINE=crates/kod-core/src/engine.rs

for f in "$TOOLS" "$ENGINE"; do
    if [ ! -f "$f" ]; then
        echo "ERROR: missing $f — run from the kod workspace root"
        exit 1
    fi
done

echo "Reporting exit signal on Unix; capping grep skipped_large_files"

python3 - "$TOOLS" "$ENGINE" << 'PYEOF'
import os
import sys

tools, engine = sys.argv[1], sys.argv[2]

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

# ======================================================================
# 1. tools.rs: cap skipped_large_files
# ======================================================================
patch(
    tools,
    '''/// Per-file byte cap for `grep`. Files larger than this are skipped and
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
    '''/// Per-file byte cap for `grep`. Files larger than this are skipped and
/// reported in the result's `skipped_large_files` list.
///
/// The pre-streaming implementation called `std::fs::read_to_string` on
/// every candidate file, so a 2 GB log — the exact file a user might
/// want to grep — would allocate the whole thing into memory and OOM
/// the process before the entry cap could fire. A source tree rarely
/// has a file over a megabyte, and a file that large rarely contains
/// the line-level pattern a coding agent is looking for; 8 MB is
/// generous for the useful case and cheap to bound the useless one.
const MAX_GREP_FILE_BYTES: u64 = 8 * 1024 * 1024;

/// Cap on the `skipped_large_files` list returned with a grep result.
/// A repo with a build tree full of large artifacts (a `target/` that
/// .gitignore does not cover, a vendored dataset, a `node_modules/`
/// with binary blobs) can have hundreds of files over
/// [`MAX_GREP_FILE_BYTES`]. Listing every one of them would reproduce
/// the exact problem the size cap was meant to solve — a tool result
/// dominated by paths. 50 is enough to answer "which files were too
/// big?" for the common case; `skipped_large_files_total` carries the
/// real count when more were skipped.
const MAX_SKIPPED_LARGE_FILES: usize = 50;''',
    "MAX_SKIPPED_LARGE_FILES constant",
)

# ======================================================================
# 2. tools.rs: use the cap in the grep execute body
# ======================================================================
patch(
    tools,
    '''        let mut results = Vec::new();
        // Files whose size exceeded MAX_GREP_FILE_BYTES. Reported in
        // the result so the model knows the search was not exhaustive
        // and can decide whether to grep them specifically (or read
        // them with an offset once that exists).
        let mut skipped_large_files: Vec<String> = Vec::new();''',
    '''        let mut results = Vec::new();
        // Files whose size exceeded MAX_GREP_FILE_BYTES. Reported in
        // the result so the model knows the search was not exhaustive
        // and can decide whether to grep them specifically. Capped at
        // MAX_SKIPPED_LARGE_FILES with the true count in
        // `skipped_large_files_total` — an uncapped list would turn
        // into the very "tool result dominated by paths" problem the
        // size limit exists to prevent.
        let mut skipped_large_files: Vec<String> = Vec::new();
        let mut skipped_large_total: usize = 0;''',
    "grep skipped_large_total counter",
)

patch(
    tools,
    '''            if let Ok(meta) = std::fs::metadata(&file_path)
                && meta.len() > MAX_GREP_FILE_BYTES
            {
                skipped_large_files.push(file_path.to_string_lossy().to_string());
                continue;
            }''',
    '''            if let Ok(meta) = std::fs::metadata(&file_path)
                && meta.len() > MAX_GREP_FILE_BYTES
            {
                skipped_large_total += 1;
                if skipped_large_files.len() < MAX_SKIPPED_LARGE_FILES {
                    skipped_large_files.push(file_path.to_string_lossy().to_string());
                }
                continue;
            }''',
    "grep counts every skip, lists the first 50",
)

patch(
    tools,
    '''        Ok(ToolResult::Success(serde_json::json!({
            "pattern": pattern,
            "case_insensitive": case_insensitive,
            "results": results,
            "truncated": results.len() >= MAX_GREP_MATCHES,
            "skipped_large_files": skipped_large_files,
        })))''',
    '''        Ok(ToolResult::Success(serde_json::json!({
            "pattern": pattern,
            "case_insensitive": case_insensitive,
            "results": results,
            "truncated": results.len() >= MAX_GREP_MATCHES,
            "skipped_large_files": skipped_large_files,
            "skipped_large_files_total": skipped_large_total,
        })))''',
    "grep reports skipped total",
)

# ======================================================================
# 3. tools.rs: expose exit signal on Unix
# ======================================================================
patch(
    tools,
    '''        let status = child.wait().await.map_err(KodError::Io)?;

        Ok(ToolResult::Success(serde_json::json!({
            "stdout": String::from_utf8_lossy(&stdout_bytes).to_string(),
            "stderr": String::from_utf8_lossy(&stderr_bytes).to_string(),
            "exit_code": status.code().unwrap_or(-1),
            "stdout_truncated": stdout_truncated,
            "stderr_truncated": stderr_truncated,
        })))''',
    '''        let status = child.wait().await.map_err(KodError::Io)?;

        // On Unix, distinguish "exited with code N" from "killed by
        // signal N". A process terminated by the truncation path's
        // start_kill() reports a signal, not an exit code; a process
        // that finished on its own — even with a non-zero exit, like
        // `grep` returning 1 for no matches — reports a code. The
        // downstream summariser used to infer "killed" from
        // `exit_code != 0`, which mislabelled every `grep` result
        // whose output happened to be truncated. Reporting the signal
        // directly removes the guess. Windows does not have exit
        // signals in the same sense; the field is omitted there.
        #[cfg(unix)]
        let exit_signal: Option<i32> = {
            use std::os::unix::process::ExitStatusExt;
            status.signal()
        };
        #[cfg(not(unix))]
        let exit_signal: Option<i32> = None;

        Ok(ToolResult::Success(serde_json::json!({
            "stdout": String::from_utf8_lossy(&stdout_bytes).to_string(),
            "stderr": String::from_utf8_lossy(&stderr_bytes).to_string(),
            "exit_code": status.code().unwrap_or(-1),
            "exit_signal": exit_signal,
            "stdout_truncated": stdout_truncated,
            "stderr_truncated": stderr_truncated,
        })))''',
    "execute_command reports exit_signal",
)

# ======================================================================
# 4. engine.rs: use exit_signal for the killed label
# ======================================================================
patch(
    engine,
    '''        if stdout_trunc || stderr_trunc {
            out.push_str(&format!(
                "\\n[output truncated at cap{}]",
                if v.get("exit_code").and_then(|c| c.as_i64()).unwrap_or(0) != 0 {
                    " — command was killed"
                } else {
                    ""
                }
            ));
        }''',
    '''        if stdout_trunc || stderr_trunc {
            // Only say "killed" when the tool actually reports a
            // signal. Previously this inferred "killed" from
            // `exit_code != 0`, so a `grep` with no matches (exit 1)
            // whose stdout happened to be truncated was labelled
            // "command was killed" — a small lie the reader has no way
            // to detect. The tool now reports `exit_signal` explicitly
            // on Unix; Windows omits it (no exit signals in the same
            // sense), and the label is simply omitted there.
            let killed = v
                .get("exit_signal")
                .and_then(|s| s.as_i64())
                .is_some();
            out.push_str(&format!(
                "\\n[output truncated at cap{}]",
                if killed { " — command was killed" } else { "" }
            ));
        }''',
    "summarize uses exit_signal",
)

# ======================================================================
# 5. engine.rs test: exit_signal drives the killed label
# ======================================================================
patch(
    engine,
    '''        // execute_command killed by the cap: exit code non-zero, label.
        let exec_killed = summarize_tool_result(
            "execute_command",
            &ToolResult::Success(serde_json::json!({
                "stdout": "y\\n",
                "stderr": "",
                "exit_code": -1,
                "stdout_truncated": true,
                "stderr_truncated": false
            })),
        );
        assert!(
            exec_killed.contains("killed"),
            "killed command must be labelled: {exec_killed}"
        );
    }''',
    '''        // Truncated output with a real signal (killed by us): label.
        let exec_killed = summarize_tool_result(
            "execute_command",
            &ToolResult::Success(serde_json::json!({
                "stdout": "y\\n",
                "stderr": "",
                "exit_code": -1,
                "exit_signal": 9,
                "stdout_truncated": true,
                "stderr_truncated": false
            })),
        );
        assert!(
            exec_killed.contains("killed"),
            "signalled command must be labelled: {exec_killed}"
        );

        // Regression: a normal non-zero exit code with truncated
        // output must NOT be labelled "killed". `grep` returning 1 for
        // no matches and a check that happened to exceed the cap is
        // the exact shape that mislabelled before.
        let exec_nonzero_not_killed = summarize_tool_result(
            "execute_command",
            &ToolResult::Success(serde_json::json!({
                "stdout": "x\\n",
                "stderr": "",
                "exit_code": 1,
                "exit_signal": null,
                "stdout_truncated": true,
                "stderr_truncated": false
            })),
        );
        assert!(
            exec_nonzero_not_killed.contains("output truncated at cap"),
            "truncation must still be reported: {exec_nonzero_not_killed}"
        );
        assert!(
            !exec_nonzero_not_killed.contains("killed"),
            "non-zero exit is not 'killed': {exec_nonzero_not_killed}"
        );
    }''',
    "summarize killed label tests",
)

print("All patches applied.")
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
fix(tools,core): report exit signal; cap grep's skipped-file list

Two honesty fixes on the tool-result path.

1. summarize_success labelled a truncated command "— command was
   killed" whenever the exit code was non-zero. Non-zero exit is
   normal: `grep` returns 1 for no matches, `diff` returns 1 for
   differences, `test` returns 1 for false. Any such command whose
   output happened to exceed the 64 KB cap was rendered as if the
   tool had terminated it, which is misleading and not something the
   reader can detect.

   ExecuteCommandTool now reports `exit_signal` on Unix (from
   ExitStatusExt::signal()) alongside the existing `exit_code`.
   Windows omits it — exit signals do not exist in the same sense
   there. summarize_success labels "killed" only when a signal is
   present; truncation is reported either way, so the reader still
   knows the output was cut.

   Test covers both the signalled case (label present) and the
   non-zero-exit case (label absent, truncation still stated) — the
   exact shape that mislabelled before.

2. grep's `skipped_large_files` accumulated one entry per file over
   MAX_GREP_FILE_BYTES. A repo with a big target/ tree or a vendored
   dataset can have hundreds, at which point the "which files were
   skipped" list becomes the tool result dominated by paths the size
   cap was meant to prevent. Cap the list at
   MAX_SKIPPED_LARGE_FILES = 50 and add
   `skipped_large_files_total` for the true count. Consumers that
   only care about the fact of the skip read the total; consumers
   that want a sample read the first 50.
MSG
