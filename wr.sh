#!/usr/bin/env bash
set -uo pipefail

COMPILE_OK=true
TARGET=crates/kod-core/src/engine.rs

if [ ! -f Cargo.toml ] || [ ! -f "$TARGET" ]; then
    echo "ERROR: run from the kod workspace root ($TARGET missing)"
    exit 1
fi

echo "Patching $TARGET: JSON-aware cap on tool results fed to the model"

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

# --- 1. Add cap_rendered_result helper -------------------------------
patch(
    '''/// Truncate a UTF-8 string to at most `max` bytes, rounding down to the
/// nearest char boundary. Returns the input unchanged when it already
/// fits. Use this instead of `&s[..max]` — the raw slice panics when
/// `max` lands mid-codepoint, which any non-ASCII tool output can hit
/// (a file containing "café", an error message with an em-dash, any
/// emoji in a directory listing).
pub(crate) fn truncate_chars(s: &str, max: usize) -> &str {''',
    '''/// Bytes of headroom reserved when capping a JSON tool result: enough
/// room for the surrounding `{"path": "…", "content": "…", "truncated":
/// …}` scaffolding after we trim the big string fields.
const JSON_CAP_HEADROOM: usize = 512;

/// Fields in a tool-result JSON object that are typically the reason
/// a rendered result exceeds the prompt cap. Trimmed in place so the
/// surrounding JSON stays valid.
const CAPPABLE_FIELDS: [&str; 3] = ["content", "stdout", "stderr"];

/// Cap a rendered `ToolResult::Success` to at most `cap` bytes without
/// cutting mid-JSON.
///
/// The simple byte cut this replaces produced, for a large `read_file`
/// result:
///
///   `{"path": "/foo.rs", "content": "fn main() {\\n    ...
///    … [truncated 90000 bytes]`
///
/// — invalid JSON, with the `"truncated": true` flag past the cut and
/// therefore unreachable. The model could not tell that the content
/// was incomplete, and any downstream parser would have rejected the
/// string outright.
///
/// This helper trims the long string fields *inside* the object (with
/// a per-field marker), re-serializes, and only falls back to a byte
/// cut if the reserialized form is somehow still over. Non-string
/// fields (paths, line numbers, flags, error codes) always survive.
pub(crate) fn cap_rendered_result(result: &ToolResult, cap: usize) -> String {
    let ToolResult::Success(v) = result else {
        // Callers route only Success through this helper; the fallback
        // is defensive.
        return format!("{result:?}");
    };
    let raw = v.to_string();
    if raw.len() <= cap {
        return raw;
    }
    if let Some(obj) = v.as_object() {
        let mut trimmed = obj.clone();
        // Split the budget across the fields that may each need a
        // marker. Two long fields (read_file's content + nothing;
        // execute_command's stdout + stderr) is the worst case.
        let per_field = cap.saturating_sub(JSON_CAP_HEADROOM) / 2;
        let mut changed = false;
        for key in CAPPABLE_FIELDS {
            if let Some(serde_json::Value::String(s)) = trimmed.get_mut(key)
                && s.len() > per_field
            {
                let removed = s.len() - per_field;
                *s = format!(
                    "{}… [truncated {} of {} bytes]",
                    truncate_chars(s, per_field),
                    removed,
                    s.len(),
                );
                changed = true;
            }
        }
        if changed
            && let Ok(reserialized) = serde_json::to_string(&trimmed)
        {
            if reserialized.len() <= cap {
                return reserialized;
            }
            return format!(
                "{}… [truncated {} bytes]",
                truncate_chars(&reserialized, cap),
                reserialized.len().saturating_sub(cap),
            );
        }
    }
    format!(
        "{}… [truncated {} bytes]",
        truncate_chars(&raw, cap),
        raw.len().saturating_sub(cap),
    )
}

/// Truncate a UTF-8 string to at most `max` bytes, rounding down to the
/// nearest char boundary. Returns the input unchanged when it already
/// fits. Use this instead of `&s[..max]` — the raw slice panics when
/// `max` lands mid-codepoint, which any non-ASCII tool output can hit
/// (a file containing "café", an error message with an em-dash, any
/// emoji in a directory listing).
pub(crate) fn truncate_chars(s: &str, max: usize) -> &str {''',
    "cap_rendered_result helper",
)

# --- 2. Route Success results through the helper --------------------
patch(
    '''            let rendered = match &result {
                // list_files raw JSON is one quoted path per entry; a repo
                // with a target/ dir produces 40k+ entries and the model
                // sees 4000 bytes of quoted paths ending in "[truncated
                // 1523k chars]" — no count, no sense of scale.
                // summarize_tool_result renders "4852 entries in src/:
                // · main.rs · lib.rs … and 4840 more", which is what the
                // model can actually reason about. read_file and grep
                // keep their raw payloads — the model needs the content
                // and the (file, line, text) tuples, not a preview.
                ToolResult::Success(_) if call.tool_name == "list_files" => {
                    summarize_tool_result(&call.tool_name, &result)
                }
                ToolResult::Success(v) => v.to_string(),
                ToolResult::Error(e) => format!("error: {e}"),
                ToolResult::RequiresConfirmation { description, .. } => {
                    format!("requires confirmation (auto-skipped in TUI): {description}")
                }
            };
            // Cap huge outputs so context survives. Slicing must be
            // char-boundary aware — the old `&rendered[..4000]` panicked
            // whenever byte 4000 landed inside a multibyte codepoint
            // (any file containing non-ASCII text).
            let rendered = if rendered.len() > 8000 {
                format!(
                    "{}… [truncated {} bytes]",
                    truncate_chars(&rendered, 8000),
                    rendered.len() - truncate_chars(&rendered, 8000).len()
                )
            } else {
                rendered
            };''',
    '''            // Cap for the prompt block the model sees. Byte count, not
            // tokens, but at the workspace's 4-chars-per-token rule of
            // thumb this is ≈2k tokens — comfortably under any model's
            // per-round budget once history, identity, and tools are
            // added on top.
            const RENDERED_RESULT_CAP: usize = 8_000;
            let rendered = match &result {
                // list_files raw JSON is one quoted path per entry; a repo
                // with a target/ dir produces 40k+ entries and the model
                // sees a few KB of quoted paths ending in "[truncated
                // 1523k chars]" — no count, no sense of scale.
                // summarize_tool_result renders "4852 entries in src/:
                // · main.rs · lib.rs … and 4840 more", which is what the
                // model can actually reason about.
                ToolResult::Success(_) if call.tool_name == "list_files" => {
                    summarize_tool_result(&call.tool_name, &result)
                }
                // read_file and grep keep their structured payloads —
                // the model needs the actual content and (file, line,
                // text) tuples. cap_rendered_result trims the long
                // string fields *inside* the JSON rather than cutting
                // the serialized form mid-token, so the model always
                // gets parseable JSON with every metadata field
                // (path, line numbers, the `truncated` flag) intact.
                ToolResult::Success(_) => cap_rendered_result(&result, RENDERED_RESULT_CAP),
                ToolResult::Error(e) => format!("error: {e}"),
                ToolResult::RequiresConfirmation { description, .. } => {
                    format!("requires confirmation (auto-skipped in TUI): {description}")
                }
            };''',
    "run_tool_calls JSON-aware cap",
)

# --- 3. Tests --------------------------------------------------------
patch(
    '''    #[test]
    fn test_summarize_reports_truncation() {''',
    '''    /// cap_rendered_result must produce valid JSON for an oversized
    /// result: the model has to be able to parse every metadata field
    /// even when the content itself is trimmed.
    #[test]
    fn test_cap_rendered_result_keeps_json_valid() {
        // A read_file result whose content is much larger than the cap.
        let big_content = "x".repeat(50_000);
        let result = ToolResult::Success(serde_json::json!({
            "path": "/a/big.rs",
            "content": big_content,
            "truncated": false
        }));
        let rendered = cap_rendered_result(&result, 8_000);
        assert!(
            rendered.len() <= 8_000,
            "rendered {} bytes > cap 8000",
            rendered.len()
        );
        // The critical assertion: the output must parse as JSON.
        let parsed: serde_json::Value =
            serde_json::from_str(&rendered).expect("capped result must be valid JSON");
        assert_eq!(parsed["path"], "/a/big.rs");
        assert_eq!(parsed["truncated"], false);
        let content = parsed["content"].as_str().expect("content is a string");
        assert!(
            content.contains("truncated"),
            "content should carry a per-field truncation marker"
        );
    }

    /// A result that already fits the cap is returned unchanged.
    #[test]
    fn test_cap_rendered_result_small_is_untouched() {
        let result = ToolResult::Success(serde_json::json!({
            "path": "/a/small.rs",
            "content": "hello\\n",
            "truncated": false
        }));
        let raw = match &result {
            ToolResult::Success(v) => v.to_string(),
            _ => unreachable!(),
        };
        let rendered = cap_rendered_result(&result, 8_000);
        assert_eq!(rendered, raw);
    }

    /// execute_command with both stdout and stderr over the per-field
    /// budget must still produce valid JSON with all four fields present.
    #[test]
    fn test_cap_rendered_result_trims_both_streams() {
        let result = ToolResult::Success(serde_json::json!({
            "stdout": "a".repeat(20_000),
            "stderr": "b".repeat(20_000),
            "exit_code": 3,
            "stdout_truncated": false,
            "stderr_truncated": false
        }));
        let rendered = cap_rendered_result(&result, 8_000);
        assert!(rendered.len() <= 8_000);
        let parsed: serde_json::Value =
            serde_json::from_str(&rendered).expect("capped result must be valid JSON");
        assert_eq!(parsed["exit_code"], 3);
        assert_eq!(parsed["stdout_truncated"], false);
        assert_eq!(parsed["stderr_truncated"], false);
        assert!(parsed["stdout"].as_str().unwrap().contains("truncated"));
        assert!(parsed["stderr"].as_str().unwrap().contains("truncated"));
    }

    #[test]
    fn test_summarize_reports_truncation() {''',
    "cap_rendered_result tests",
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

echo "cargo check --workspace"
if ! cargo check --workspace 2>&1; then
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
git commit -m "fix(core): JSON-aware cap for tool results fed to the model

The 8000-byte prompt cap in run_tool_calls cut the rendered
ToolResult as a raw byte string. For read_file that string is JSON:

  {\"path\": \"/foo.rs\", \"content\": \"fn main() {\\\\n    ...

After the cut the model saw:

  {\"path\": \"/foo.rs\", \"content\": \"fn main() {\\\\n    ...
  … [truncated 90000 bytes]

Two problems. First, it is not valid JSON — any downstream parser
(or a model that tries to re-read the structure) fails. Second, the
\`truncated\` flag on read_file lives *after* \`content\` in the
serialized form, so for any file larger than the cap the model never
saw whether the file itself had been truncated by the reader's own
256 KB limit or was simply cut by the prompt cap.

Add cap_rendered_result, which:
- If the serialized form fits the cap, returns it unchanged.
- Otherwise trims the long string fields (content, stdout, stderr)
  *inside* the object with a per-field '… [truncated N of M bytes]'
  marker, and re-serializes. Paths, line numbers, exit codes, and
  the truncated flags always survive.
- Falls back to a byte cut only if the reserialized form is somehow
  still over.

Route every Success result through it, except list_files (which
already goes through summarize_tool_result — the count-and-names
form is more useful than a JSON dump).

Adds three tests: a 50 KB read_file stays valid JSON under an 8 KB
cap and keeps its path + truncated flag; a small result is returned
unchanged; a two-stream execute_command result over the per-field
budget stays valid with exit_code and both truncated flags intact."
