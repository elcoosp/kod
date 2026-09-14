#!/usr/bin/env bash
set -uo pipefail

TOOLS=crates/kod-tools/src/tools.rs

if [ ! -f Cargo.toml ] || [ ! -f "$TOOLS" ]; then
    echo "ERROR: run from the kod workspace root ($TOOLS missing)"
    exit 1
fi

echo "Patching $TOOLS: structured errors for directory / permission / not-found"

python3 - "$TOOLS" << 'PYEOF'
import os
import sys

target = sys.argv[1]
with open(target, "r") as f:
    src = f.read()

def patch(old, new, label, expect=1):
    global src
    n = src.count(old)
    if n == 0:
        print(f"  MISS: {label}")
        return False
    if expect and n != expect:
        print(f"  ERROR: expected {expect} occurrence(s) of {label}, found {n}")
        sys.exit(2)
    src = src.replace(old, new, expect if expect else n)
    print(f"  patched: {label}")
    return True

# ----------------------------------------------------------------------
# 1. Add a helper that classifies an io::Error on a resolved path into
#    a structured tool-appropriate response.
# ----------------------------------------------------------------------
patch(
    '''/// Read up to `cap` bytes from an async reader. Returns the bytes read
/// and whether the source had more (reading hit the cap).''',
    '''/// Map a path-level IO error into a message the model can act on.
///
/// The default Display of std::io::Error on a directory read is
/// "Is a directory (os error 21)" — technically accurate, but it does
/// not tell the model what to do next. Same for the bare "No such file
/// or directory" and "Permission denied". Return a short label plus a
/// one-line suggestion, so the model recovers instead of giving up or
/// (worse) re-issuing the same call.
fn describe_path_error(path: &std::path::Path, err: &std::io::Error) -> String {
    let kind = err.kind();
    let p = path.display();
    match kind {
        std::io::ErrorKind::NotFound => format!(
            "not found: {p}. Check the path — a typo or a directory you have \\
             not listed yet is the common cause.",
        ),
        std::io::ErrorKind::PermissionDenied => format!(
            "permission denied: {p}. The process does not have read access.",
        ),
        std::io::ErrorKind::IsADirectory => format!(
            "is a directory, not a file: {p}. Use list_files to see its \\
             contents, or read a specific file inside it.",
        ),
        _ => {
            // Windows reports directory reads as "other"; check metadata
            // to give the same advice.
            if path.is_dir() {
                format!(
                    "is a directory, not a file: {p}. Use list_files to see \\
                     its contents, or read a specific file inside it.",
                )
            } else {
                format!("{}: {p}", err)
            }
        }
    }
}

/// Read up to `cap` bytes from an async reader. Returns the bytes read
/// and whether the source had more (reading hit the cap).''',
    "describe_path_error helper",
)

# ----------------------------------------------------------------------
# 2. Apply it at the open site in read_file.
# ----------------------------------------------------------------------
patch(
    '''        // Read with a hard byte cap instead of `read_to_string`, so a
        // huge file (log, generated lock file, binary) can't exhaust
        // memory before the engine's prompt-side truncation kicks in.
        use std::io::Read as _;
        let mut file = std::fs::File::open(&resolved).map_err(KodError::Io)?;''',
    '''        // Reject directories up front with a structured message. The
        // downstream open would fail with "Is a directory" (or a
        // Windows-specific "other" error), which the model cannot
        // distinguish from a missing file or a permission problem. A
        // clear "is a directory — use list_files" is the recovery the
        // model actually needs.
        if resolved.is_dir() {
            return Ok(ToolResult::Error(describe_path_error(
                &resolved,
                &std::io::Error::new(
                    std::io::ErrorKind::IsADirectory,
                    "is a directory",
                ),
            )));
        }

        // Read with a hard byte cap instead of `read_to_string`, so a
        // huge file (log, generated lock file, binary) can't exhaust
        // memory before the engine's prompt-side truncation kicks in.
        use std::io::Read as _;
        let mut file = std::fs::File::open(&resolved).map_err(|e| {
            KodError::Io(std::io::Error::new(e.kind(), describe_path_error(&resolved, &e)))
        })?;''',
    "read_file directory guard + open error",
)

# ----------------------------------------------------------------------
# 3. Tests.
# ----------------------------------------------------------------------
if "read_file_directory_returns_actionable_error" not in src:
    anchor = '''    #[tokio::test]
    async fn read_file_small_file_is_not_truncated() {'''
    if anchor not in src:
        print("  ERROR: test anchor not found")
        sys.exit(2)
    new_tests = '''    /// read_file on a directory must return an error message the
    /// model can act on ("use list_files"), not a raw "Is a directory"
    /// IO error that it cannot distinguish from "file not found".
    #[tokio::test]
    async fn read_file_directory_returns_actionable_error() {
        let temp = tempfile::TempDir::new().unwrap();
        std::fs::create_dir_all(temp.path().join("subdir")).unwrap();
        std::fs::write(temp.path().join("subdir/x.txt"), "x").unwrap();

        let ctx = full_context(temp.path());
        let tool = ReadFileTool::new();
        let params = serde_json::json!({ "path": "subdir" });
        let result = tool.execute(&params, &ctx).await.unwrap();

        match result {
            ToolResult::Error(msg) => {
                assert!(
                    msg.contains("is a directory"),
                    "message should name the problem: {msg}"
                );
                assert!(
                    msg.contains("list_files"),
                    "message should suggest the fix: {msg}"
                );
            }
            other => panic!("expected ToolResult::Error, got {:?}", other),
        }
    }

    /// read_file on a missing path must say so, and the message must
    /// suggest checking the path rather than re-running the same call.
    #[tokio::test]
    async fn read_file_missing_returns_actionable_error() {
        let temp = tempfile::TempDir::new().unwrap();
        let ctx = full_context(temp.path());
        let tool = ReadFileTool::new();
        let params = serde_json::json!({ "path": "nope.txt" });
        let result = tool.execute(&params, &ctx).await.unwrap();

        match result {
            ToolResult::Error(msg) => {
                assert!(
                    msg.contains("not found"),
                    "message should say not-found: {msg}"
                );
                assert!(
                    msg.contains("nope.txt"),
                    "message should name the path: {msg}"
                );
            }
            other => panic!("expected ToolResult::Error, got {:?}", other),
        }
    }

    #[tokio::test]
    async fn read_file_small_file_is_not_truncated() {'''
    src = src.replace(anchor, new_tests, 1)
    print("  added directory + missing-path tests")
else:
    print("  tests already present")

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

echo
echo "Committing."
git add -A
git commit -F - <<'MSG'
fix(tools): structured read_file errors for directory / missing paths

read_file on a directory returned a raw IO error propagated through
KodError::Io, whose Display is "Is a directory (os error 21)" on
Unix and a platform-specific "other" error on Windows. The model saw
a string it could not act on: no hint that the target was a
directory (as opposed to missing, unreadable, or a symlink loop), no
suggestion to use list_files instead, nothing to reason about.

Add describe_path_error(path, err) that classifies the common
path-level IO error kinds (NotFound, PermissionDenied,
IsADirectory) into a short label plus a one-line recovery hint, and
route read_file's directory check and file-open failure through it.

The directory check runs before the open, so the model gets a
uniform message on both platforms rather than the Unix "Is a
directory" versus Windows "other" split. Missing paths now say
"not found: PATH. Check the path — a typo or a directory you have
not listed yet is the common cause.", which points at the two real
causes instead of re-issuing the same call.

The behaviour of every other read_file path is unchanged: the file
opens, the size is read, the binary probe runs, the content is
returned.

Adds two tests: a directory target produces "is a directory" and
"list_files" in the message; a missing target produces "not found"
and names the path.
MSG
