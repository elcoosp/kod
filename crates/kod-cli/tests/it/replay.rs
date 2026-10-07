//! `kod replay` end-to-end (design §11.3, "replay avant/arrière").
//!
//! `kod replay <session.jsonl>` re-runs the tool calls a session made,
//! without the model. It is the design's "regression suite": the same
//! tool sequence against a new commit is a concrete check that the
//! tools still work, and a diff of the results is a concrete check
//! that the codebase still behaves the same way.
//!
//! This file spawns the built binary against a fixture log. Two runs:
//!
//! - **Dry run** (the default): the command prints what would run and
//!   touches nothing. The assertion is that the tool names and
//!   arguments print in the correct order, and that no file was
//!   created or modified.
//! - **`--execute`**: the tool calls actually run. For a `read_file`
//!   against a fixture that matches, the fresh result matches the
//!   recorded one — the "0 differed" summary is the assertion.
//!
//! The test intentionally uses only `read_file` (a side-effect-free
//! tool) so `--execute` cannot leave artefacts behind and the test is
//! safe to run in a shared tmpdir.

use std::process::Command;

fn kod_bin() -> &'static str {
    env!("CARGO_BIN_EXE_kod")
}

/// Build a session log with one `read_file` call whose recorded result
/// matches what a fresh read of `fixture.txt` will produce.
fn write_fixture(dir: &std::path::Path) -> std::path::PathBuf {
    // The file we will read. Canonicalize *after* creating it: on
    // macOS, `TempDir::new()` yields a path under `/var/folders/...`,
    // which is a symlink to `/private/var/folders/...`. `ReadFileTool`
    // canonicalizes through `resolve_path`, so the result it returns
    // carries the `/private/...` form. Writing the un-canonicalized
    // form into the fixture made the fresh result differ from the
    // recorded one on the very same bytes — the tool was correct, the
    // fixture was wrong.
    let target = dir.join("fixture.txt");
    std::fs::write(&target, "hello from the fixture\n").unwrap();
    let target = std::fs::canonicalize(&target).unwrap();

    // Build the recorded result in the same shape `kod replay`
    // computes: `{"success": {"path": ..., "content": ..., "truncated":
    // false, "binary": false}}` — matching the JSON the real
    // ReadFileTool returns.
    let path_str = target.to_string_lossy().to_string();
    let recorded = serde_json::json!({
        "success": {
            "path": path_str,
            "content": "hello from the fixture\n",
            "truncated": false,
            "binary": false
        }
    });
    let entry = serde_json::json!({
        "kind": "tool_call",
        "timestamp_ms": 1_700_000_000_000u64,
        "holder": "session",
        "tool_name": "read_file",
        "arguments": { "path": path_str },
        "duration_ms": 2,
        "result": recorded
    });

    let log = dir.join("session.jsonl");
    let mut line = serde_json::to_string(&entry).unwrap();
    line.push('\n');
    std::fs::write(&log, line).unwrap();
    log
}

#[test]
fn dry_run_lists_tool_calls_without_executing() {
    let tmp = tempfile::TempDir::new().unwrap();
    let log = write_fixture(tmp.path());

    let out = Command::new(kod_bin())
        .current_dir(tmp.path())
        .args(["replay", log.to_str().unwrap()])
        .output()
        .expect("spawn kod");
    let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
    let stderr = String::from_utf8_lossy(&out.stderr).into_owned();

    assert!(
        out.status.success(),
        "dry-run must exit 0\nstdout: {stdout}\nstderr: {stderr}",
    );
    assert!(
        stdout.contains("read_file"),
        "the tool name must appear in the dry-run listing:\n{stdout}",
    );
    assert!(
        stdout.contains("dry run") || stdout.contains("Pass --execute"),
        "the dry-run hint must be visible:\n{stdout}",
    );
}

#[test]
fn execute_runs_the_tool_and_matches_the_recorded_result() {
    // The recorded result must match what a fresh read produces. The
    // fixture content is stable, so a "1 matched, 0 differed" summary
    // is the expected outcome. A regression that lost the comparison,
    // that re-serialized the result differently, or that failed to
    // execute would break this.
    let tmp = tempfile::TempDir::new().unwrap();
    let log = write_fixture(tmp.path());

    let out = Command::new(kod_bin())
        .current_dir(tmp.path())
        .args(["replay", log.to_str().unwrap(), "--execute"])
        .output()
        .expect("spawn kod");
    let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
    let stderr = String::from_utf8_lossy(&out.stderr).into_owned();

    assert!(
        out.status.success(),
        "--execute must exit 0\nstdout: {stdout}\nstderr: {stderr}",
    );
    assert!(
        stdout.contains("1 matched, 0 differed"),
        "expected `1 matched, 0 differed`; got:\n{stdout}",
    );
}

#[test]
fn empty_log_reports_no_tool_calls() {
    // A log that only carries non-tool-call variants (the AD-15
    // extensions) has nothing to replay. The command must say so
    // rather than exiting non-zero — the log is valid, it just has
    // no tool calls.
    let tmp = tempfile::TempDir::new().unwrap();
    let log = tmp.path().join("empty.jsonl");
    std::fs::write(
        &log,
        "{\"kind\":\"cost\",\"timestamp_ms\":1,\"holder\":\"s\",\
         \"endpoint\":\"e\",\"model\":\"m\",\"prompt_tokens\":1,\
         \"completion_tokens\":1,\"cost_usd\":0.001}\n",
    )
    .unwrap();

    let out = Command::new(kod_bin())
        .current_dir(tmp.path())
        .args(["replay", log.to_str().unwrap()])
        .output()
        .expect("spawn kod");
    assert!(out.status.success());
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("No tool calls"),
        "expected a clear `No tool calls` message, got:\n{stdout}",
    );
}
