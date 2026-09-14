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

echo "Patching $TOOLS: NUL-byte binary detection in read_file"

python3 - "$TOOLS" "$ENGINE" << 'PYEOF'
import os
import sys

tools, engine = sys.argv[1], sys.argv[2]

def patch(path, old, new, label, expect=1):
    with open(path, "r") as f:
        src = f.read()
    n = src.count(old)
    if n == 0:
        print(f"  MISS in {path}: {label}")
        return False
    if expect and n != expect:
        print(f"  ERROR: expected {expect} occurrence(s) of {label} in {path}, found {n}")
        sys.exit(2)
    tmp = path + ".tmp"
    with open(tmp, "w") as f:
        f.write(src.replace(old, new, expect if expect else n))
    os.replace(tmp, path)
    print(f"  patched {path}: {label}")
    return True

# ----------------------------------------------------------------------
# 1. read_file execute: detect binary, return preview + flag.
# ----------------------------------------------------------------------
patch(
    tools,
    '''        let resolved = context.resolve_path(path)?;
        context.can_read(&resolved)?;

        // Read with a hard byte cap instead of `read_to_string`, so a
        // huge file (log, generated lock file, binary) can't exhaust
        // memory before the engine's prompt-side truncation kicks in.
        use std::io::Read as _;
        let mut file = std::fs::File::open(&resolved).map_err(KodError::Io)?;
        let mut buf: Vec<u8> = Vec::with_capacity(8192);
        file.by_ref()
            .take(MAX_READ_BYTES as u64 + 1)
            .read_to_end(&mut buf)
            .map_err(KodError::Io)?;
        let truncated = buf.len() > MAX_READ_BYTES;
        if truncated {
            buf.truncate(MAX_READ_BYTES);
        }
        // Truncation may have landed mid-UTF-8; drop the trailing
        // incomplete char instead of returning invalid bytes.
        let content = match std::str::from_utf8(&buf) {
            Ok(s) => s.to_string(),
            Err(e) => String::from_utf8_lossy(&buf[..e.valid_up_to()]).into_owned(),
        };

        Ok(ToolResult::Success(serde_json::json!({
            "path": resolved.to_string_lossy().to_string(),
            "content": content,
            "truncated": truncated,
        })))''',
    '''        let resolved = context.resolve_path(path)?;
        context.can_read(&resolved)?;

        // Total size from metadata (the byte cap below can hide it).
        let total_size = std::fs::metadata(&resolved)
            .map(|m| m.len())
            .unwrap_or(0);

        // Read with a hard byte cap instead of `read_to_string`, so a
        // huge file (log, generated lock file, binary) can't exhaust
        // memory before the engine's prompt-side truncation kicks in.
        use std::io::Read as _;
        let mut file = std::fs::File::open(&resolved).map_err(KodError::Io)?;
        let mut buf: Vec<u8> = Vec::with_capacity(8192);
        file.by_ref()
            .take(MAX_READ_BYTES as u64 + 1)
            .read_to_end(&mut buf)
            .map_err(KodError::Io)?;
        let truncated = buf.len() > MAX_READ_BYTES;
        if truncated {
            buf.truncate(MAX_READ_BYTES);
        }

        // Detect binary content before treating the bytes as text. A
        // NUL byte in the first 1 KB is the standard heuristic for
        // "not text" (git, file(1), ripgrep). The previous code fed
        // the bytes through `from_utf8_lossy`, so a PNG or compiled
        // object came back to the model as a wall of U+FFFD
        // replacement characters, or a short prefix ending at the
        // first invalid sequence — with no indication that the content
        // was anything other than a text file the user had asked about.
        //
        // A UTF-16 file also has NUL bytes (each ASCII char is
        // `XX 00`), and would land here. That is not a regression:
        // the previous behavior decoded UTF-16 as mojibake too,
        // because the crate treats everything as UTF-8. Returning the
        // honest "binary, here is a hex preview" is at least useful
        // for identifying what the file is; if UTF-16 support is
        // wanted, it belongs in a `file(1)`-style encoding probe that
        // this tool does not yet have.
        let probe_len = buf.len().min(1024);
        let looks_binary = buf[..probe_len].contains(&0u8);
        if looks_binary {
            // 64 bytes is enough for a magic number (`\\x89PNG\\r\\n\\x1a\\n`,
            // `\\x7fELF`, `PK\\x03\\x04`) and any short ASCII banner the
            // model might use to identify the format.
            let preview_len = buf.len().min(64);
            let preview_hex: String = buf[..preview_len]
                .iter()
                .map(|b| format!("{b:02x}"))
                .collect::<Vec<_>>()
                .join(" ");
            return Ok(ToolResult::Success(serde_json::json!({
                "path": resolved.to_string_lossy().to_string(),
                "binary": true,
                "size_bytes": total_size,
                "truncated": truncated,
                "preview_hex": preview_hex,
            })));
        }

        // Truncation may have landed mid-UTF-8; drop the trailing
        // incomplete char instead of returning invalid bytes.
        let content = match std::str::from_utf8(&buf) {
            Ok(s) => s.to_string(),
            Err(e) => String::from_utf8_lossy(&buf[..e.valid_up_to()]).into_owned(),
        };

        Ok(ToolResult::Success(serde_json::json!({
            "path": resolved.to_string_lossy().to_string(),
            "content": content,
            "truncated": truncated,
            "binary": false,
        })))''',
    "read_file binary detection",
)

# ----------------------------------------------------------------------
# 2. engine.rs summarize_success: handle the binary result shape.
# ----------------------------------------------------------------------
patch(
    engine,
    '''fn summarize_success(name: &str, v: &serde_json::Value) -> String {
    // read_file: path + size + short preview only. The full content still
    // reaches the model through the tool-result feedback block — the chat
    // row stays lean while the agent loses nothing.
    if name == "read_file"
        && let Some(content) = v.get("content").and_then(|s| s.as_str())
    {''',
    '''fn summarize_success(name: &str, v: &serde_json::Value) -> String {
    // Binary read_file: no text preview, just a name-and-size line.
    // The model sees the hex preview through the tool-result feedback
    // block; the chat row is a one-liner.
    if name == "read_file"
        && v.get("binary").and_then(|b| b.as_bool()).unwrap_or(false)
    {
        let path = v
            .get("path")
            .and_then(|p| p.as_str())
            .map(shorten_path)
            .unwrap_or_else(|| name.to_string());
        let size = v
            .get("size_bytes")
            .and_then(|s| s.as_u64())
            .unwrap_or(0);
        return format!(
            "{} · binary ({} bytes) — not shown as text",
            path, size
        );
    }

    // read_file: path + size + short preview only. The full content still
    // reaches the model through the tool-result feedback block — the chat
    // row stays lean while the agent loses nothing.
    if name == "read_file"
        && let Some(content) = v.get("content").and_then(|s| s.as_str())
    {''',
    "summarize_success binary branch",
)

# ----------------------------------------------------------------------
# 3. Tests in tools.rs.
# ----------------------------------------------------------------------
with open(tools, "r") as f:
    tools_src = f.read()

if "read_file_detects_binary_content" not in tools_src:
    anchor = '''    #[tokio::test]
    async fn read_file_small_file_is_not_truncated() {'''
    if anchor not in tools_src:
        print("  ERROR: test anchor not found in tools.rs")
        sys.exit(2)
    new_tests = '''    /// A file with NUL bytes in the first 1 KB must be returned as
    /// binary (flag + hex preview), not decoded as UTF-8 lossy.
    /// Regression: the previous code passed the bytes through
    /// from_utf8_lossy, so the model saw a wall of U+FFFD characters
    /// and had no way to know the file was a PNG, an ELF, or a
    /// UTF-16 text file.
    #[tokio::test]
    async fn read_file_detects_binary_content() {
        let temp = tempfile::TempDir::new().unwrap();
        let path = temp.path().join("img.png");
        // PNG magic + a few NUL-containing bytes.
        let bytes: Vec<u8> = vec![
            0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A, // PNG signature
            0x00, 0x00, 0x00, 0x0D, // IHDR length
            0x49, 0x48, 0x44, 0x52, // "IHDR"
        ];
        std::fs::write(&path, &bytes).unwrap();

        let ctx = full_context(temp.path());
        let tool = ReadFileTool::new();
        let params = serde_json::json!({ "path": "img.png" });
        let result = tool.execute(&params, &ctx).await.unwrap();

        match result {
            ToolResult::Success(v) => {
                assert_eq!(v["binary"], true, "binary flag should be set");
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

    /// A UTF-8 file with non-ASCII content (accented letters, emoji)
    /// must NOT be misclassified as binary. The heuristic is NUL
    /// bytes, not "any non-ASCII".
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

    #[tokio::test]
    async fn read_file_small_file_is_not_truncated() {'''
    tools_src = tools_src.replace(anchor, new_tests, 1)
    with open(tools, "w") as f:
        f.write(tools_src)
    print("  added binary detection tests to tools.rs")
else:
    print("  binary detection tests already present")

# ----------------------------------------------------------------------
# 4. Extend engine.rs summarize test with the binary branch.
# ----------------------------------------------------------------------
with open(engine, "r") as f:
    engine_src = f.read()

if "binary (4096 bytes) — not shown as text" not in engine_src:
    anchor = '''        // read_file without the flag: no marker.'''
    if anchor not in engine_src:
        print("  ERROR: summarize test anchor not found")
        sys.exit(2)
    addition = '''        // read_file binary result: one-line summary, no text preview.
        let bin = summarize_tool_result(
            "read_file",
            &ToolResult::Success(serde_json::json!({
                "path": "/a/img.png",
                "binary": true,
                "size_bytes": 4096,
                "truncated": false,
                "preview_hex": "89 50 4e 47 0d 0a 1a 0a"
            })),
        );
        assert!(
            bin.contains("binary (4096 bytes) — not shown as text"),
            "binary summary shape: {bin}"
        );

        // read_file without the flag: no marker.'''
    engine_src = engine_src.replace(anchor, addition, 1)
    with open(engine, "w") as f:
        f.write(engine_src)
    print("  added binary branch to summarize test")
else:
    print("  binary summarize test already present")
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
feat(tools): detect binary files in read_file; return hex preview

read_file treated every file as UTF-8. A PNG, a compiled object, an
ELF, a UTF-16 text file — all four reached the model as either a
wall of U+FFFD replacement characters (from from_utf8_lossy) or a
short prefix ending at the first invalid byte, with no indication
that the content was anything other than a text file the user had
asked about. The model then tried to reason about the mojibake.

Add a NUL-byte probe on the first 1 KB — the standard
file(1)/git/ripgrep heuristic for "not text". If a NUL is present,
return ToolResult::Success with:

  { "path": ..., "binary": true, "size_bytes": N,
    "truncated": false, "preview_hex": "89 50 4e 47 ..." }

The hex preview is the first 64 bytes, enough for the model to
identify a magic number (PNG, ELF, ZIP, PDF) and decide what to do.
No "content" field on this shape, so a caller that keys on `content`
cannot accidentally treat the preview as text.

A UTF-16 file also has NUL bytes and lands in the binary branch.
That is not a regression: the previous code decoded UTF-16 as
mojibake too (the crate is UTF-8-only). Returning an honest "this
is binary, here is the identifier" is more useful than the previous
empty-or-garbage result. Real UTF-16 support belongs in an encoding
probe this tool does not have.

Text files are unchanged except for an explicit `"binary": false`
flag, so consumers can rely on the field being present.

summarize_success gains a matching branch: a binary read_file
renders as "path · binary (N bytes) — not shown as text" in chat,
so the TUI does not try to print the hex preview as a paragraph.

Adds three tests: a PNG-shaped file is detected binary with the
signature in the hex preview; a plain .rs file reports binary:
false and its content; a UTF-8 file with accented characters is
NOT misclassified (the heuristic is NUL bytes, not "any
non-ASCII"). Extends the engine's summarize test with the binary
branch shape.
MSG
