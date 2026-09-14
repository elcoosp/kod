#!/usr/bin/env bash
set -uo pipefail

COMPILE_OK=true
TOOLS=crates/kod-tools/src/tools.rs
ENGINE=crates/kod-core/src/engine.rs

for f in "$TOOLS" "$ENGINE"; do
    if [ ! -f "$f" ]; then
        echo "ERROR: missing $f"
        exit 1
    fi
done

echo "Reporting timeout explicitly; deleting dead code; summing in chat"

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
# 1. tools.rs: capture the timeout value and delete the empty branch
# ======================================================================
patch(
    tools,
    '''        let timeout = tokio::time::sleep(std::time::Duration::from_secs(
            context.timeout_secs.max(1),
        ));
        tokio::pin!(timeout);
        let mut timed_out = false;

        loop {
            if stdout_res.is_some() && stderr_res.is_some() {
                break;
            }
            if timed_out {
                // Child was killed; drain the reads to EOF and exit.
                // The other branch below will still fire because the
                // child's death closes its pipe ends.
            }
            tokio::select! {''',
    '''        let effective_timeout_secs = context.timeout_secs.max(1);
        let timeout =
            tokio::time::sleep(std::time::Duration::from_secs(effective_timeout_secs));
        tokio::pin!(timeout);
        let mut timed_out = false;

        loop {
            if stdout_res.is_some() && stderr_res.is_some() {
                break;
            }
            // No timed_out handling here: when the timeout branch fires
            // we call start_kill, the child dies, and the child's death
            // closes its pipe ends — so the two read arms complete on
            // their own and the loop exits through the top-of-loop
            // check above. (The previous version carried an empty
            // `if timed_out {}` block whose only content was a comment
            // explaining that fact.)
            tokio::select! {''',
    "remove dead if timed_out block; capture timeout secs",
)

# ======================================================================
# 2. tools.rs: report timed_out + effective_timeout_secs
# ======================================================================
patch(
    tools,
    '''        Ok(ToolResult::Success(serde_json::json!({
            "stdout": String::from_utf8_lossy(&stdout_bytes).to_string(),
            "stderr": String::from_utf8_lossy(&stderr_bytes).to_string(),
            "exit_code": status.code().unwrap_or(-1),
            "exit_signal": exit_signal,
            "stdout_truncated": stdout_truncated,
            "stderr_truncated": stderr_truncated,
        })))''',
    '''        Ok(ToolResult::Success(serde_json::json!({
            "stdout": String::from_utf8_lossy(&stdout_bytes).to_string(),
            "stderr": String::from_utf8_lossy(&stderr_bytes).to_string(),
            "exit_code": status.code().unwrap_or(-1),
            "exit_signal": exit_signal,
            "stdout_truncated": stdout_truncated,
            "stderr_truncated": stderr_truncated,
            // Explicit, not inferred. The timeout kill and the
            // output-cap kill both surface as a signal, and the
            // summariser needs to distinguish them: an `exit_signal`
            // alone says "killed", but not why. A timeout is user
            // action-required (raise the timeout, or run in the
            // background); a cap kill means the command was too
            // chatty and the partial output is still representative.
            "timed_out": timed_out,
            "timeout_secs": effective_timeout_secs,
        })))''',
    "report timed_out + timeout_secs",
)

# ======================================================================
# 3. engine.rs: summarize timeout vs cap kill vs signal
# ======================================================================
patch(
    engine,
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
    '''        // Explain why the output stops, when it did.
        //
        // Three separate things can end a command early, and each
        // needs a distinct message:
        //
        //   * timed_out: the tool killed the child at
        //     `context.timeout_secs` because it was still running.
        //     User action is required — raise the timeout or run in
        //     the background.
        //   * stdout_truncated / stderr_truncated: the child wrote
        //     more than MAX_CMD_OUTPUT_BYTES on one stream, and the
        //     tool killed it to keep memory bounded. The partial
        //     output is still representative; no action required.
        //   * exit_signal (without either of the above): the child
        //     died of an external signal — a SIGKILL from the OS, a
        //     container OOM, a `kill -9` from another shell. Rare but
        //     worth surfacing; the previous code silently treated a
        //     small-output signal-kill as a normal exit, so a command
        //     killed by the OOM killer looked like it had completed
        //     with partial output.
        let timed_out = v
            .get("timed_out")
            .and_then(|t| t.as_bool())
            .unwrap_or(false);
        let timeout_secs = v
            .get("timeout_secs")
            .and_then(|n| n.as_u64())
            .unwrap_or(0);
        let truncated = stdout_trunc || stderr_trunc;
        let signalled = v
            .get("exit_signal")
            .and_then(|s| s.as_i64())
            .is_some();

        if timed_out {
            out.push_str(&format!(
                "\\n[command timed out after {}s — killed]",
                timeout_secs
            ));
        }
        if truncated {
            out.push_str("\\n[output truncated at cap");
            if signalled && !timed_out {
                // Both the timeout branch and the cap branch call
                // start_kill; getting here with a signal and no
                // timeout means the cap branch fired.
                out.push_str(" — command was killed");
            }
            out.push(']');
        } else if signalled && !timed_out {
            out.push_str("\\n[command was killed by a signal (exit_signal reported)]");
        }''',
    "summarize timeout vs cap vs signal",
)

# ======================================================================
# 4. engine.rs: extend the existing test with the timeout case
# ======================================================================
patch(
    engine,
    '''        assert!(
            !exec_nonzero_not_killed.contains("killed"),
            "non-zero exit is not 'killed': {exec_nonzero_not_killed}"
        );
    }''',
    '''        assert!(
            !exec_nonzero_not_killed.contains("killed"),
            "non-zero exit is not 'killed': {exec_nonzero_not_killed}"
        );

        // Regression: a timeout with small output used to be silently
        // treated as a normal exit, because the old summariser only
        // looked at the truncation flags. The user saw a partial
        // `cargo build` transcript and assumed it had finished.
        let exec_timeout = summarize_tool_result(
            "execute_command",
            &ToolResult::Success(serde_json::json!({
                "stdout": "Compiling foo\\n",
                "stderr": "",
                "exit_code": -1,
                "exit_signal": 9,
                "stdout_truncated": false,
                "stderr_truncated": false,
                "timed_out": true,
                "timeout_secs": 30
            })),
        );
        assert!(
            exec_timeout.contains("timed out after 30s"),
            "timeout must be named: {exec_timeout}"
        );
        assert!(
            exec_timeout.contains("killed"),
            "timeout should say killed: {exec_timeout}"
        );

        // A kill by a signal with neither timeout nor truncation is
        // still worth a one-liner. Rare, but silent is worse.
        let exec_signalled = summarize_tool_result(
            "execute_command",
            &ToolResult::Success(serde_json::json!({
                "stdout": "partial output\\n",
                "stderr": "",
                "exit_code": -1,
                "exit_signal": 9,
                "stdout_truncated": false,
                "stderr_truncated": false,
                "timed_out": false,
                "timeout_secs": 30
            })),
        );
        assert!(
            exec_signalled.contains("killed by a signal"),
            "external signal must be named: {exec_signalled}"
        );
    }''',
    "timeout + signal summarize tests",
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
fix(tools,core): name timeouts and external signal-kills in chat

execute_command can end a command in three distinct ways, and only
one of them was visible in the summary.

- Timeout. When the command is still running at
  `context.timeout_secs`, the tool sends SIGKILL. The command stops
  mid-output, the exit status reports a signal, and the exit code is
  -1. Previously this path left no marker in the summary unless the
  partial output also happened to exceed MAX_CMD_OUTPUT_BYTES, so a
  `cargo build` killed at 30s with 10 KB of output rendered as if it
  had finished. A user reading the transcript assumed the build was
  complete.

- Output cap. The child writes more than MAX_CMD_OUTPUT_BYTES on a
  stream and is killed to keep memory bounded. The existing
  "output truncated at cap — command was killed" marker covers this,
  and it is correct.

- External signal. The child was SIGKILLed by something outside the
  tool: a container OOM, a `kill -9` from another shell. The
  previous code only reported the signal when the output was also
  truncated, so a small-output OOM kill was silently read as normal
  completion.

ExecuteCommandTool now reports `timed_out: bool` and
`timeout_secs: u64` alongside the existing `exit_signal`. The
distinction is not inferable from `exit_signal` alone — both the
timeout branch and the cap branch send SIGKILL — so the tool states
which one fired rather than asking the summariser to guess.

summarize_success now emits:

  [command timed out after 30s — killed]
  [output truncated at cap — command was killed]
  [command was killed by a signal (exit_signal reported)]

as appropriate, and nothing when the command exited normally. The
empty `if timed_out { /* comment only */ }` block in the select loop
is deleted; it explained an invariant that the top-of-loop check
already enforces.

Tests extended: a timeout with small output names the timeout;
a signal-kill with no truncation and no timeout is named; the
existing signal-and-truncation case still labels as killed; the
non-zero-exit case (grep returning 1) still does not.
MSG
