#!/usr/bin/env bash
set -uo pipefail

TOOLS=crates/kod-tools/src/tools.rs
ENGINE=crates/kod-core/src/engine.rs

for f in "$TOOLS" "$ENGINE"; do
    if [ ! -f "$f" ]; then
        echo "ERROR: missing $f"
        exit 1
    fi
done

python3 - "$TOOLS" "$ENGINE" << 'PYEOF'
import os
import sys

tools, engine = sys.argv[1], sys.argv[2]

def read(p):
    with open(p, "r") as f:
        return f.read()

def write(p, s):
    tmp = p + ".tmp"
    with open(tmp, "w") as f:
        f.write(s)
    os.replace(tmp, p)

# ======================================================================
# 1. tools.rs: insert tests before the final `}` of the file (which
#    closes `mod tests`). Only if not already present.
# ======================================================================
src = read(tools)

if "list_files_on_file_reports_file_kind" in src:
    print("  tools.rs: tests already present")
else:
    new_tests = '''
    /// list_files on a file must return path_kind: "file" with a
    /// single-entry list, not the ambiguous empty-directory shape.
    /// Regression: the walker rooted at a file yielded nothing, so the
    /// result was indistinguishable from an empty directory.
    #[tokio::test]
    async fn list_files_on_file_reports_file_kind() {
        let temp = tempfile::TempDir::new().unwrap();
        std::fs::write(temp.path().join("single.txt"), "content").unwrap();

        let ctx = full_context(temp.path());
        let tool = ListFilesTool::new();
        let params = serde_json::json!({ "path": "single.txt" });
        let result = tool.execute(&params, &ctx).await.unwrap();

        match result {
            ToolResult::Success(v) => {
                assert_eq!(v["path_kind"], "file");
                let files = v["files"].as_array().unwrap();
                assert_eq!(files.len(), 1, "single-entry list expected");
                assert!(files[0].as_str().unwrap().ends_with("single.txt"));
                assert_eq!(v["total"], 1);
                assert_eq!(v["truncated"], false);
            }
            other => panic!("expected success, got {:?}", other),
        }
    }

    /// list_files on a directory still reports directory and its
    /// entries, unchanged.
    #[tokio::test]
    async fn list_files_on_directory_reports_directory_kind() {
        let temp = tempfile::TempDir::new().unwrap();
        std::fs::write(temp.path().join("a.txt"), "").unwrap();
        std::fs::write(temp.path().join("b.txt"), "").unwrap();

        let ctx = full_context(temp.path());
        let tool = ListFilesTool::new();
        let params = serde_json::json!({ "path": "." });
        let result = tool.execute(&params, &ctx).await.unwrap();

        match result {
            ToolResult::Success(v) => {
                assert_eq!(v["path_kind"], "directory");
                assert_eq!(v["files"].as_array().unwrap().len(), 2);
                assert_eq!(v["total"], 2);
            }
            other => panic!("expected success, got {:?}", other),
        }
    }

    /// list_files on a missing path returns a structured error naming
    /// the path, not an empty result.
    #[tokio::test]
    async fn list_files_missing_path_errors() {
        let temp = tempfile::TempDir::new().unwrap();
        let ctx = full_context(temp.path());
        let tool = ListFilesTool::new();
        let params = serde_json::json!({ "path": "does-not-exist" });
        let result = tool.execute(&params, &ctx).await.unwrap();

        match result {
            ToolResult::Error(msg) => {
                assert!(
                    msg.contains("not found"),
                    "message should say not-found: {msg}"
                );
                assert!(msg.contains("does-not-exist"));
            }
            other => panic!("expected ToolResult::Error, got {:?}", other),
        }
    }
'''

    # Find the final closing brace: last occurrence of "\n}\n" or
    # trailing "}\n" at column 0. Prefer the file's final "}\n".
    stripped = src.rstrip()
    if not stripped.endswith("}"):
        print("  ERROR: file does not end with `}`")
        sys.exit(2)
    # Find the position of that final `}`.
    idx = stripped.rfind("}")
    # Insert before it: [..idx] + new_tests + ["}\n"..restored trailing].
    src = stripped[:idx] + new_tests + "\n}\n"
    write(tools, src)
    print("  tools.rs: inserted path_kind tests")

# ======================================================================
# 2. engine.rs: extend the summarize test with the file-branch.
# ======================================================================
src = read(engine)

if "is a file, not a directory" in src:
    print("  engine.rs: summarize test already extended")
else:
    anchor = "        // read_file binary result: one-line summary, no text preview."
    if anchor not in src:
        print("  ERROR: engine.rs summarize test anchor not found")
        sys.exit(2)
    addition = '''        // list_files on a file: reports "is a file", not "1 entry".
        let lf_file = summarize_tool_result(
            "list_files",
            &ToolResult::Success(serde_json::json!({
                "path": "/a/single.txt",
                "path_kind": "file",
                "files": ["/a/single.txt"],
                "total": 1,
                "truncated": false
            })),
        );
        assert!(
            lf_file.contains("is a file, not a directory"),
            "file-shaped list_files summary: {lf_file}"
        );

'''
    src = src.replace(anchor, addition + anchor, 1)
    write(engine, src)
    print("  engine.rs: extended summarize test")

print("Done.")
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
test(tools,core): cover list_files file/directory/missing discriminator

Adds three tests in kod-tools for the list_files path_kind work
that landed in the previous commit: a file target reports
path_kind "file" with a single-entry list; a directory target
reports "directory" with its entries (unchanged behavior); a
missing target returns a structured error naming the path.

Extends the engine's summarize test with the file-shaped
list_files case: "path · is a file, not a directory" rather than
the count form, which would read as if the call had worked.

(Insertion uses the file's final brace rather than a named-test
anchor, so this lands regardless of how earlier edits reordered
the module.)
MSG
