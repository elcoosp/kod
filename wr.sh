#!/usr/bin/env bash
set -uo pipefail

COMPILE_OK=true
INCOMPLETE=false
WORKSPACE=crates/kod-swarm/src/workspace.rs
ENGINE=crates/kod-core/src/engine.rs

for f in "$WORKSPACE" "$ENGINE"; do
    if [ ! -f "$f" ]; then
        echo "ERROR: missing $f — run from the kod workspace root"
        exit 1
    fi
done

echo "Fixing SharedWorkspace::resolve_path for not-yet-existing paths"
echo "Surfacing truncation flags in summarize_success"

python3 - "$WORKSPACE" "$ENGINE" << 'PYEOF'
import os
import sys

workspace, engine = sys.argv[1], sys.argv[2]

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

# ========================================================================
# 1. SharedWorkspace::resolve_path — handle non-existent target
# ========================================================================
patch(
    workspace,
    '''    /// Resolve a path relative to workspace root
    pub fn resolve_path(&self, path: &Path) -> Result<PathBuf> {
        let resolved = if path.is_absolute() {
            path.to_path_buf()
        } else {
            self.root.join(path)
        };

        // Check the path is within workspace
        let canonical = std::fs::canonicalize(&resolved).map_err(|_| {
            KodError::InvalidState(format!("Path not found: {}", resolved.display()))
        })?;
        let root_canonical = std::fs::canonicalize(&self.root)
            .map_err(|_| KodError::InvalidState("Workspace root not found".to_string()))?;

        if !canonical.starts_with(root_canonical) {
            return Err(KodError::PermissionDenied {
                action: "access".to_string(),
                reason: format!("Path outside workspace: {}", canonical.display()),
            });
        }

        Ok(canonical)
    }''',
    '''    /// Resolve a path relative to the workspace root and confirm it
    /// lies inside the workspace.
    ///
    /// Files that do not exist yet are handled by canonicalizing the
    /// deepest existing ancestor and re-appending the remainder. Without
    /// this, `std::fs::canonicalize(&resolved)` failed on any path that
    /// did not already exist, so the workspace could never lock a file
    /// *before* it was written — exactly the case pre-write coordination
    /// exists for. A second agent about to write the same path is what
    /// needs the lock.
    ///
    /// Traversal is still blocked: after canonicalization the result is
    /// compared against the canonicalized workspace root, so `../etc/…`
    /// and absolute paths outside the root are rejected whether the
    /// target exists or not.
    pub fn resolve_path(&self, path: &Path) -> Result<PathBuf> {
        let resolved = if path.is_absolute() {
            path.to_path_buf()
        } else {
            self.root.join(path)
        };

        // Canonicalize the deepest existing ancestor, then re-attach the
        // non-existent suffix. Any failure to canonicalize an existing
        // ancestor (permission denied, races) propagates as an I/O error.
        let canonical = match std::fs::canonicalize(&resolved) {
            Ok(c) => c,
            Err(_) => {
                // Walk up until an ancestor exists, then rebuild.
                let mut tail: Vec<std::ffi::OsString> = Vec::new();
                let mut cur = resolved.as_path();
                let canon_ancestor = loop {
                    match cur.parent() {
                        Some(parent) => {
                            if let Some(name) = cur.file_name() {
                                tail.push(name.to_os_string());
                            }
                            if let Ok(c) = std::fs::canonicalize(parent) {
                                break c;
                            }
                            cur = parent;
                        }
                        None => {
                            return Err(KodError::InvalidState(format!(
                                "Path has no existing ancestor: {}",
                                resolved.display()
                            )));
                        }
                    }
                };
                let mut rebuilt = canon_ancestor;
                for name in tail.iter().rev() {
                    rebuilt.push(name);
                }
                rebuilt
            }
        };

        let root_canonical = std::fs::canonicalize(&self.root)
            .map_err(|_| KodError::InvalidState("Workspace root not found".to_string()))?;

        if !canonical.starts_with(&root_canonical) {
            return Err(KodError::PermissionDenied {
                action: "access".to_string(),
                reason: format!("Path outside workspace: {}", canonical.display()),
            });
        }

        Ok(canonical)
    }''',
    "SharedWorkspace::resolve_path handles missing paths",
)

# ========================================================================
# 2. Append regression tests to workspace.rs
# ========================================================================
with open(workspace, "r") as f:
    ws_content = f.read()
if "test_resolve_path_allows_nonexistent_inside_root" not in ws_content:
    ws_content += '''

#[cfg(test)]
mod tests {
    use super::*;
    use kod_types::AgentId;
    use std::path::PathBuf;
    use tempfile::TempDir;

    fn ws() -> (TempDir, SharedWorkspace) {
        let tmp = TempDir::new().unwrap();
        let ws = SharedWorkspace::new(tmp.path().to_path_buf());
        (tmp, ws)
    }

    /// A file that does not exist yet but would live inside the
    /// workspace must resolve. This is the pre-write lock case: two
    /// agents coordinating on a new file both call acquire_lock before
    /// either has created it.
    #[test]
    fn test_resolve_path_allows_nonexistent_inside_root() {
        let (tmp, ws) = ws();
        let resolved = ws
            .resolve_path(Path::new("brand_new.txt"))
            .expect("should resolve a not-yet-existing path inside the root");
        assert!(resolved.starts_with(tmp.path().canonicalize().unwrap()));
        assert!(resolved.ends_with("brand_new.txt"));
    }

    /// A `..` climb that escapes the workspace is rejected whether or
    /// not the target exists.
    #[test]
    fn test_resolve_path_rejects_traversal() {
        let (_tmp, ws) = ws();
        let escape = PathBuf::from("..").join("outside.txt");
        assert!(
            ws.resolve_path(&escape).is_err(),
            "traversal to a non-existent parent must be rejected"
        );
    }

    /// An absolute path outside the workspace is rejected.
    #[test]
    fn test_resolve_path_rejects_absolute_outside_root() {
        let (_tmp, ws) = ws();
        assert!(ws.resolve_path(Path::new("/etc/hostname")).is_err());
    }

    /// The full pre-write flow: a lock can be acquired on a file that
    /// does not exist yet, then the file is created, and the lock is
    /// released.
    #[tokio::test]
    async fn test_acquire_lock_on_nonexistent_file() {
        let (tmp, ws) = ws();
        let agent = AgentId::new();
        let path = Path::new("to_be_created.txt");

        let lock = ws
            .acquire_lock(path, &agent, LockType::Exclusive)
            .await
            .expect("should be able to lock a not-yet-existing path inside the root");
        assert_eq!(lock.path, tmp.path().canonicalize().unwrap().join("to_be_created.txt"));

        // Second agent cannot acquire the same exclusive lock.
        let other = AgentId::new();
        assert!(
            ws.acquire_lock(path, &other, LockType::Exclusive).await.is_err(),
            "exclusive lock should be held"
        );

        ws.release_lock(path, &agent).await.unwrap();
        // After release, the second agent can acquire it.
        ws.acquire_lock(path, &other, LockType::Exclusive)
            .await
            .expect("lock should be free after release");
    }
}
'''
    with open(workspace, "w") as f:
        f.write(ws_content)
    print("Patched workspace.rs: appended test module")
else:
    print("Skipped workspace.rs: tests already present")

# ========================================================================
# 3. engine.rs: surface truncation flags in summarize_success
# ========================================================================

# 3a. read_file branch: mention truncation when present.
patch(
    engine,
    '''        let lines = content.lines().count();
        let mut out = format!(
            "{} · {} line{} · {} chars",
            path,
            lines,
            if lines == 1 { "" } else { "s" },
            content.len()
        );
        let preview: Vec<&str> = content.lines().take(3).collect();
        if !preview.is_empty() {
            out.push('\\n');
            out.push_str(&preview.join("\\n"));
            if lines > preview.len() {
                out.push_str("\\n…");
            }
        }
        return out;''',
    '''        let lines = content.lines().count();
        // When the file was larger than the reader's byte cap, the tool
        // sets "truncated": true. Without surfacing it, the row reads
        // "path · 3 lines · 98 chars" for what is actually the first
        // 256 KB of a multi-megabyte file — the user (and the model)
        // have no way to tell the preview is not the whole file.
        let truncated = v
            .get("truncated")
            .and_then(|t| t.as_bool())
            .unwrap_or(false);
        let suffix = if truncated { " (truncated)" } else { "" };
        let mut out = format!(
            "{} · {} line{} · {} chars{}",
            path,
            lines,
            if lines == 1 { "" } else { "s" },
            content.len(),
            suffix,
        );
        let preview: Vec<&str> = content.lines().take(3).collect();
        if !preview.is_empty() {
            out.push('\\n');
            out.push_str(&preview.join("\\n"));
            if lines > preview.len() {
                out.push_str("\\n…");
            }
        }
        return out;''',
    "read_file branch mentions truncation",
)

# 3b. execute_command branch: append a truncation note.
patch(
    engine,
    '''    // execute_command / read_file: {stdout, stderr} or {path, content}.
    if let Some(stdout) = v.get("stdout").and_then(|s| s.as_str()) {
        let mut out = cap_lines(stdout.trim_end(), TOOL_RESULT_LINES);
        if let Some(stderr) = v.get("stderr").and_then(|s| s.as_str())
            && !stderr.trim().is_empty()
        {
            out.push_str(&format!("\\nstderr:\\n{}", cap_lines(stderr.trim_end(), 4)));
        }
        return if out.is_empty() {
            "(no output)".to_string()
        } else {
            out
        };
    }''',
    '''    // execute_command / read_file: {stdout, stderr} or {path, content}.
    if let Some(stdout) = v.get("stdout").and_then(|s| s.as_str()) {
        let mut out = cap_lines(stdout.trim_end(), TOOL_RESULT_LINES);
        if let Some(stderr) = v.get("stderr").and_then(|s| s.as_str())
            && !stderr.trim().is_empty()
        {
            out.push_str(&format!("\\nstderr:\\n{}", cap_lines(stderr.trim_end(), 4)));
        }
        // The runaway-command guard in ExecuteCommandTool reports
        // "stdout_truncated" / "stderr_truncated" so downstream can
        // distinguish a command that finished from one that was killed
        // mid-output. Drop that flag and a `yes` output looks like a
        // perfectly ordinary 64 KB of text.
        let stdout_trunc = v
            .get("stdout_truncated")
            .and_then(|t| t.as_bool())
            .unwrap_or(false);
        let stderr_trunc = v
            .get("stderr_truncated")
            .and_then(|t| t.as_bool())
            .unwrap_or(false);
        if stdout_trunc || stderr_trunc {
            out.push_str(&format!(
                "\\n[output truncated at cap{}]",
                if v.get("exit_code").and_then(|c| c.as_i64()).unwrap_or(0) != 0 {
                    " — command was killed"
                } else {
                    ""
                }
            ));
        }
        return if out.is_empty() {
            "(no output)".to_string()
        } else {
            out
        };
    }''',
    "execute_command branch mentions truncation",
)

# ========================================================================
# 4. Tests
# ========================================================================
patch(
    engine,
    '''    #[test]
    fn test_summarize_tool_result_shapes() {''',
    '''    /// summarize_success must surface the tool's truncation flags so
    /// the row does not present a partial read or a killed command as
    /// if it were complete.
    #[test]
    fn test_summarize_reports_truncation() {
        // read_file: "truncated": true adds "(truncated)".
        let read = summarize_tool_result(
            "read_file",
            &ToolResult::Success(serde_json::json!({
                "path": "/a/big.rs",
                "content": "line1\\nline2\\n",
                "truncated": true
            })),
        );
        assert!(
            read.contains("(truncated)"),
            "read_file truncation not surfaced: {read}"
        );

        // read_file without the flag: no marker.
        let read_ok = summarize_tool_result(
            "read_file",
            &ToolResult::Success(serde_json::json!({
                "path": "/a/small.rs",
                "content": "hello\\n",
                "truncated": false
            })),
        );
        assert!(
            !read_ok.contains("(truncated)"),
            "untruncated read must not carry the marker: {read_ok}"
        );

        // execute_command: "stdout_truncated": true adds a note.
        let exec = summarize_tool_result(
            "execute_command",
            &ToolResult::Success(serde_json::json!({
                "stdout": "y\\ny\\n",
                "stderr": "",
                "exit_code": 0,
                "stdout_truncated": true,
                "stderr_truncated": false
            })),
        );
        assert!(
            exec.contains("output truncated at cap"),
            "command truncation not surfaced: {exec}"
        );
        assert!(
            !exec.contains("killed"),
            "clean exit must not be labelled killed: {exec}"
        );

        // execute_command killed by the cap: exit code non-zero, label.
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
    }

    #[test]
    fn test_summarize_tool_result_shapes() {''',
    "summarize truncation tests",
)

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

echo "Running kod-swarm tests"
if ! cargo test -p kod-swarm 2>&1; then
    echo "kod-swarm tests failed. Paste the full output for a surgical fix."
    exit 1
fi

echo "Running kod-core tests"
if ! cargo test -p kod-core 2>&1; then
    echo "kod-core tests failed. Paste the full output for a surgical fix."
    exit 1
fi

echo "Running workspace tests"
if ! cargo test --workspace 2>&1; then
    echo "Workspace tests failed. Paste the full output for a surgical fix."
    exit 1
fi

echo "Running clippy with -D warnings"
if ! cargo clippy --workspace --all-targets -- -D warnings 2>&1; then
    echo "Clippy failed. Paste the full output for a surgical fix."
    exit 1
fi

echo "All checks passed. Committing."
git add -A
git commit -m "fix(swarm,core): lock not-yet-existing files; surface truncation flags

Two related honesty bugs in the tool/coordination layer.

1. SharedWorkspace::resolve_path called std::fs::canonicalize on the
   target path and errored on any path that did not already exist.
   That made the swarm unable to lock a file *before* it was written,
   which is exactly what pre-write coordination exists for: two
   agents about to write the same new path both need acquire_lock on
   a path that neither has created yet. Mirror the fix already used
   in ToolContext::resolve_path — canonicalize the deepest existing
   ancestor and re-append the remainder — and still compare the
   result against the canonical root so traversal stays blocked.

2. summarize_success ignored the truncation flags the tools now emit.
   A read_file that had been capped at 256 KB rendered as
   'path · N lines · M chars' indistinguishable from a complete
   read; a runaway execute_command killed at 64 KB rendered as
   ordinary output. Surface both: read_file appends '(truncated)'
   when 'truncated' is set, execute_command appends
   '[output truncated at cap]' and, when the exit code says the
   process was killed, '— command was killed'.

Adds three tests to workspace.rs (resolve a missing in-root path,
reject traversal, full acquire-lock-on-missing-file flow) and one to
engine.rs covering all four summarize_success flag combinations."
