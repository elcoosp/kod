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

src = read(tools)

tests = [
    ("read_file_detects_binary_content", '''
    /// A file with NUL bytes in the first 1 KB must be returned as
    /// binary (flag + hex preview), not decoded as UTF-8 lossy.
    #[tokio::test]
    async fn read_file_detects_binary_content() {
        let temp = tempfile::TempDir::new().unwrap();
        let path = temp.path().join("img.png");
        let bytes: Vec<u8> = vec![
            0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A,
            0x00, 0x00, 0x00, 0x0D,
            0x49, 0x48, 0x44, 0x52,
        ];
        std::fs::write(&path, &bytes).unwrap();

        let ctx = full_context(temp.path());
        let tool = ReadFileTool::new();
        let params = serde_json::json!({ "path": "img.png" });
        let result = tool.execute(&params, &ctx).await.unwrap();

        match result {
            ToolResult::Success(v) => {
                assert_eq!(v["binary"], true);
                assert_eq!(v["size_bytes"], bytes.len() as u64);
                assert!(v.get("content").is_none(), "no text content field");
                let hex = v["preview_hex"].as_str().unwrap();
                assert!(
                    hex.starts_with("89 50 4e 47"),
                    "hex preview should start with the PNG signature: {hex}"
                );
            }
            other => panic!("expected success, got {:?}", other),
        }
    }
'''),
    ("read_file_text_file_reports_not_binary", '''
    /// A plain text file must be returned as text with binary: false.
    #[tokio::test]
    async fn read_file_text_file_reports_not_binary() {
        let temp = tempfile::TempDir::new().unwrap();
        std::fs::write(temp.path().join("code.rs"), "fn main() {}\\n").unwrap();

        let ctx = full_context(temp.path());
        let tool = ReadFileTool::new();
        let params = serde_json::json!({ "path": "code.rs" });
        let result = tool.execute(&params, &ctx).await.unwrap();

        match result {
            ToolResult::Success(v) => {
                assert_eq!(v["binary"], false);
                assert_eq!(v["content"], "fn main() {}\\n");
            }
            other => panic!("expected success, got {:?}", other),
        }
    }
'''),
    ("read_file_multibyte_utf8_is_not_binary", '''
    /// A UTF-8 file with non-ASCII content must NOT be misclassified
    /// as binary. The heuristic is NUL bytes, not "any non-ASCII".
    #[tokio::test]
    async fn read_file_multibyte_utf8_is_not_binary() {
        let temp = tempfile::TempDir::new().unwrap();
        std::fs::write(
            temp.path().join("accented.txt"),
            "café au lait — un été\\n",
        )
        .unwrap();

        let ctx = full_context(temp.path());
        let tool = ReadFileTool::new();
        let params = serde_json::json!({ "path": "accented.txt" });
        let result = tool.execute(&params, &ctx).await.unwrap();

        match result {
            ToolResult::Success(v) => {
                assert_eq!(v["binary"], false);
                assert!(v["content"].as_str().unwrap().contains("café"));
            }
            other => panic!("expected success, got {:?}", other),
        }
    }
'''),
    ("read_file_directory_returns_actionable_error", '''
    /// read_file on a directory must return an actionable error, not a
    /// raw "Is a directory" that the model cannot distinguish from a
    /// missing file or a permission problem.
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
                assert!(msg.contains("is a directory"), "message: {msg}");
                assert!(msg.contains("list_files"), "message: {msg}");
            }
            other => panic!("expected ToolResult::Error, got {:?}", other),
        }
    }
'''),
    ("read_file_missing_returns_actionable_error", '''
    /// read_file on a missing path must say so, with a suggestion
    /// rather than a re-run of the same failing call.
    #[tokio::test]
    async fn read_file_missing_returns_actionable_error() {
        let temp = tempfile::TempDir::new().unwrap();
        let ctx = full_context(temp.path());
        let tool = ReadFileTool::new();
        let params = serde_json::json!({ "path": "nope.txt" });
        let result = tool.execute(&params, &ctx).await.unwrap();

        match result {
            ToolResult::Error(msg) => {
                assert!(msg.contains("not found"), "message: {msg}");
                assert!(msg.contains("nope.txt"), "message: {msg}");
            }
            other => panic!("expected ToolResult::Error, got {:?}", other),
        }
    }
'''),
    ("list_files_on_file_reports_file_kind", '''
    /// list_files on a file must return path_kind: "file" with a
    /// single-entry list, not the ambiguous empty-directory shape.
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
                assert_eq!(files.len(), 1);
                assert!(files[0].as_str().unwrap().ends_with("single.txt"));
                assert_eq!(v["total"], 1);
            }
            other => panic!("expected success, got {:?}", other),
        }
    }
'''),
    ("list_files_on_directory_reports_directory_kind", '''
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
'''),
    ("list_files_missing_path_errors", '''
    /// list_files on a missing path returns a structured error.
    #[tokio::test]
    async fn list_files_missing_path_errors() {
        let temp = tempfile::TempDir::new().unwrap();
        let ctx = full_context(temp.path());
        let tool = ListFilesTool::new();
        let params = serde_json::json!({ "path": "does-not-exist" });
        let result = tool.execute(&params, &ctx).await.unwrap();

        match result {
            ToolResult::Error(msg) => {
                assert!(msg.contains("not found"), "message: {msg}");
                assert!(msg.contains("does-not-exist"), "message: {msg}");
            }
            other => panic!("expected ToolResult::Error, got {:?}", other),
        }
    }
'''),
]

missing = [(m, b) for (m, b) in tests if m not in src]
print(f"Missing in tools.rs: {len(missing)}")
for m, _ in missing:
    print(f"  - {m}")

if missing:
    stripped = src.rstrip()
    if not stripped.endswith("}"):
        print("ERROR: tools.rs does not end with `}`")
        sys.exit(2)
    idx = stripped.rfind("}")
    body = "\n".join(b for _, b in missing)
    write(tools, stripped[:idx] + body + "\n}\n")
    print("Inserted tests into tools.rs")
else:
    print("tools.rs: nothing to insert")

src = read(engine)
if "is a file, not a directory" in src:
    print("engine.rs: summarize test already covers the file branch")
else:
    anchor = "        // read_file binary result: one-line summary, no text preview."
    if anchor not in src:
        print("WARN: engine.rs summarize-test anchor absent; skipping")
    else:
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
        write(engine, src.replace(anchor, addition + anchor, 1))
        print("Extended summarize test in engine.rs")
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

cat > /tmp/kod_commit_msg.txt <<'MSG'
test(tools,core): add missing tests for binary detection and path errors

Several tests that accompanied the binary-detection and
structured-path-error work never landed — earlier script runs
aborted before their write step, or their anchors no longer
matched the file. This commit inserts whichever are missing,
gated on presence so a re-run duplicates nothing:

  read_file_detects_binary_content
  read_file_text_file_reports_not_binary
  read_file_multibyte_utf8_is_not_binary
  read_file_directory_returns_actionable_error
  read_file_missing_returns_actionable_error
  list_files_on_file_reports_file_kind
  list_files_on_directory_reports_directory_kind
  list_files_missing_path_errors

Plus the engine summarize-test extension for the file-shaped
list_files case.

Insertion uses the file's final closing brace as the anchor rather
than a named test, so this lands regardless of how earlier edits
reordered the tests module.
MSG

git add -A
git commit -F /tmp/kod_commit_msg.txt
rm -f /tmp/kod_commit_msg.txt
