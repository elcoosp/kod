#!/usr/bin/env bash
set -uo pipefail

TOOLS=crates/kod-tools/src/tools.rs

if [ ! -f Cargo.toml ] || [ ! -f "$TOOLS" ]; then
    echo "ERROR: run from the kod workspace root"
    exit 1
fi

echo "=== Pre-state: write_file error paths ==="
grep -n "create_dir_all(parent).map_err\|open(&resolved)" "$TOOLS"
grep -n "write!.*map_err(KodError::Io)\|std::fs::write(&resolved.*map_err" "$TOOLS"

echo
python3 - "$TOOLS" << 'PYEOF'
import os
import sys

path = sys.argv[1]
with open(path) as f:
    src = f.read()

def replace_once(text, old, new, label):
    n = text.count(old)
    if n == 0:
        print(f"  MISS: {label}")
        return text, False
    if n != 1:
        print(f"  ERROR: {label} appears {n} times, expected 1")
        sys.exit(2)
    print(f"  patched: {label}")
    return text.replace(old, new, 1), True

# ----------------------------------------------------------------------
# 1. The whole write block, routed through describe_path_error.
# ----------------------------------------------------------------------
old_block = '''        // Create parent directories if the target is in a new tree.
        // The model frequently writes to a fresh path — a new module
        // under `src/handlers/`, a first skill file under
        // `~/.agents/skills/` — and `std::fs::write` fails with ENOENT
        // when the parent does not exist. The model's natural response
        // is to give up, because it has no tool for creating
        // directories. Idempotent and cheap: `create_dir_all` on an
        // existing tree is a no-op.
        if let Some(parent) = resolved.parent() {
            std::fs::create_dir_all(parent).map_err(KodError::Io)?;
        }

        if append {
            let mut file = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(&resolved)
                .map_err(KodError::Io)?;
            use std::io::Write;
            write!(file, "{}", content).map_err(KodError::Io)?;
        } else {
            std::fs::write(&resolved, content).map_err(KodError::Io)?;
        }'''

new_block = '''        // Create parent directories if the target is in a new tree.
        // The model frequently writes to a fresh path — a new module
        // under `src/handlers/`, a first skill file under
        // `~/.agents/skills/` — and `std::fs::write` fails with ENOENT
        // when the parent does not exist. The model's natural response
        // is to give up, because it has no tool for creating
        // directories. Idempotent and cheap: `create_dir_all` on an
        // existing tree is a no-op.
        //
        // Failures come back as Ok(ToolResult::Error(...)) rather than
        // Err(KodError::Io): every one of them is a "the write could
        // not be done, here is why, here is what to check" — the shape
        // the model should reason about — rather than "the tool itself
        // broke." Same shape the read_file fix uses, via the same
        // describe_path_error helper. The engine treats both shapes
        // the same when it renders the tool result block, so the
        // observable difference is only in the type system; the
        // consistent shape is what lets a future caller distinguish
        // model-facing errors from tool-level failures without
        // inspecting the message text.
        if let Some(parent) = resolved.parent()
            && let Err(e) = std::fs::create_dir_all(parent)
        {
            return Ok(ToolResult::Error(describe_path_error(parent, &e)));
        }

        if append {
            let mut file = match std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(&resolved)
            {
                Ok(f) => f,
                Err(e) => {
                    return Ok(ToolResult::Error(describe_path_error(&resolved, &e)));
                }
            };
            use std::io::Write;
            if let Err(e) = write!(file, "{}", content) {
                return Ok(ToolResult::Error(describe_path_error(&resolved, &e)));
            }
        } else if let Err(e) = std::fs::write(&resolved, content) {
            return Ok(ToolResult::Error(describe_path_error(&resolved, &e)));
        }'''

src, ok = replace_once(src, old_block, new_block, "write_file error paths")
if not ok:
    print("  (block already patched or anchor differs; nothing written)")
    sys.exit(0)

# ----------------------------------------------------------------------
# 2. Test: write to a location whose parent is a file, not a directory.
#    `create_dir_all` fails there with a clear kind (NotADirectory on
#    Linux, "other" on some platforms), and the tool should report it
#    as a ToolResult::Error rather than propagating KodError::Io.
# ----------------------------------------------------------------------
if "test_write_file_parent_is_a_file_reports_error" not in src:
    anchor = '''    #[tokio::test]
    async fn read_file_detects_binary_content() {'''
    if anchor not in src:
        print("  WARN: test anchor not found; skipping test add")
    else:
        new_test = '''    /// Writing under a path whose parent is a file (not a directory)
    /// must return Ok(ToolResult::Error(...)) with a message the model
    /// can act on — the same shape read_file uses for directories and
    /// missing files. Regression: the previous code propagated
    /// KodError::Io, forcing callers to handle two shapes for the same
    /// class of "the model asked for something that cannot work."
    #[tokio::test]
    async fn test_write_file_parent_is_a_file_reports_error() {
        let temp = tempfile::TempDir::new().unwrap();
        // A file named "blocker" — the write target below has it as a
        // parent directory, which create_dir_all will reject.
        std::fs::write(temp.path().join("blocker"), "not a dir").unwrap();

        let ctx = full_context(temp.path());
        let tool = WriteFileTool::new();
        let params = serde_json::json!({
            "path": "blocker/child.txt",
            "content": "hello"
        });
        let result = tool.execute(&params, &ctx).await.unwrap();

        match result {
            ToolResult::Error(msg) => {
                assert!(
                    msg.contains("blocker"),
                    "error should name the path: {msg}"
                );
            }
            other => panic!("expected ToolResult::Error, got {:?}", other),
        }
    }

    #[tokio::test]
    async fn read_file_detects_binary_content() {'''
        src = src.replace(anchor, new_test, 1)
        print("  patched: write_file test")

with open(path, "w") as f:
    f.write(src)
print("Wrote", path)
PYEOF

if [ $? -ne 0 ]; then
    echo "ERROR: patching failed"
    exit 1
fi

echo
echo "cargo check --workspace --all-targets 2>&1 | tail -8"
if ! cargo check --workspace --all-targets 2>&1 | tail -8; then
    echo "Compilation failed"
    exit 1
fi

echo
echo "cargo test -p kod-tools --quiet 2>&1 | tail -12"
if ! cargo test -p kod-tools --quiet 2>&1 | tail -12; then
    echo "kod-tools tests failed"
    exit 1
fi

cat > /tmp/kod_commit_msg.txt <<'MSG'
fix(tools): write_file errors stay model-facing

write_file's three failure points (create_dir_all, open for append,
write) all propagated KodError::Io. So did read_file before its
earlier fix, and for the same reason: the codebase had no
convention for "the model asked for something that cannot work" vs
"the tool itself broke."

Route every write_file failure through describe_path_error and
return Ok(ToolResult::Error(...)). The engine renders both shapes
identically in the tool result block, so the observable difference
is only in the type system — and the consistent shape is what lets
a future caller distinguish the two classes without string-matching
the message.

Adds test_write_file_parent_is_a_file_reports_error: a target whose
parent is a regular file, which create_dir_all rejects; the tool
must report a ToolResult::Error naming the offending path rather
than propagating a raw I/O error.
MSG

git add -A
git commit -F /tmp/kod_commit_msg.txt
rm -f /tmp/kod_commit_msg.txt
