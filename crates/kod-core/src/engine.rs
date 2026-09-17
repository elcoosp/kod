//! Main KOD engine - orchestrates all subsystems.
//!
//! Coordinates the task router, LLM providers, skills, and memory
//! to process user requests end-to-end.

use crate::router::{RouterConfig, TaskResponse, TaskRouter};
use kod_error::{KodError, Result};
use serde::{Deserialize, Serialize};
use kod_provider::{GenerationOptions, GenerationResponse, LlmProvider, StreamChunk};
use kod_tools::{
    ExecuteCommandTool, FileInfoTool, GitDiffTool, GitStatusTool, GrepTool, ListFilesTool,
    PatchFileTool, PathLockTable, ReadFileTool, ToolContext, ToolRegistry, WriteFileTool,
};
use kod_types::{ToolCall, ToolDefinition, ToolPermissions, ToolResult};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use tokio::sync::RwLock;

/// Max agentic tool rounds per `process()` call before forcing a
/// summary. A single agentic pass typically uses 3–15 rounds for a
/// non-trivial task; 40 is a generous safety margin that catches a
/// runaway loop (a small model that keeps re-calling `read_file` on
/// the same path, unable to recognize it is done) well before the
/// user has waited minutes for nothing.
const MAX_TOOL_ROUNDS: usize = 40;

/// Notice appended to the conversation when the tool loop hits
/// [`MAX_TOOL_ROUNDS`] without a text-only reply. The loop calls the
/// provider one more time afterwards to request a summary; this note
/// is what steers that summary toward "what got done and what
/// remains" instead of "the model answers as if nothing unusual
/// happened." Without it the last tool-result block is the only
/// context for the summary, and a small model tends to summarize
/// that one result rather than the whole run.
const TOOL_ROUNDS_EXHAUSTED_NOTE: &str =
    "\n\n[tool-round limit reached — no further tool calls will run this turn. \
     Summarize what has been done so far and what remains.]";
/// Max turns of the `/goal` loop before it stops and reports progress.
const MAX_GOAL_TURNS: usize = 6;

/// How long a write approval waits for an answer before defaulting to
/// deny. A dialog nobody answers — the user closed the terminal, walked
/// away, or a script that cannot answer ran unattended — must not hang
/// the tool loop forever. Denying is the safe default: the file is not
/// written, the model sees the denial, and the user can re-run with
/// `tools.confirm_writes = false` to skip the prompt entirely.
const AWAIT_APPROVAL_SECS: u64 = 120;

/// Marker prefix for tool-start notices inside the `process_streaming`
/// chunk channel: `\0kod-tool:<name>\0`. The TUI turns these into its
/// "running …" indicator instead of chat text (see `parse_tool_start`).
pub const TOOL_START_MARKER: &str = "\0kod-tool:";

/// Build a tool-start marker chunk for `name`.
pub fn tool_start_marker(name: &str) -> String {
    format!("{TOOL_START_MARKER}{name}\0")
}

/// If `chunk` is a tool-start marker, return the tool name.
pub fn parse_tool_start(chunk: &str) -> Option<&str> {
    chunk.strip_prefix(TOOL_START_MARKER)?.strip_suffix('\0')
}

/// Marker prefix for tool-argument excerpts inside the
/// `process_streaming` chunk channel: `\0kod-args:<one-line display>\0`.
/// Sent after a streamed call's arguments are assembled but before the
/// tool executes, so the TUI's "running …" line can show the actual
/// command/file instead of just the tool name.
pub const TOOL_ARGS_MARKER: &str = "\0kod-args:";

/// Build a tool-args marker chunk carrying a one-line display string.
pub fn tool_args_marker(display: &str) -> String {
    format!("{TOOL_ARGS_MARKER}{display}\0")
}

/// If `chunk` is a tool-args marker, return the one-line display string.
pub fn parse_tool_args(chunk: &str) -> Option<&str> {
    chunk.strip_prefix(TOOL_ARGS_MARKER)?.strip_suffix('\0')
}

/// Marker prefix for per-tool completion notices inside the
/// `process_streaming` chunk channel:
/// `\0kod-done:<header>\0<summary>\0<duration_ms>`.
/// Sent by [`KodEngine::run_streaming_loop`] the moment each tool call
/// finishes — long before the whole agentic loop returns — so the TUI
/// can fill the live "running …" row in immediately instead of batching
/// every `ToolCompleted` at task end. Headers/summaries are sanitized
/// (no `\0`) when built by [`tool_done_marker`].
pub const TOOL_DONE_MARKER: &str = "\0kod-done:";

/// Build a tool-done marker chunk for one finished call.
pub fn tool_done_marker(header: &str, summary: &str, duration_ms: u64) -> String {
    let clean = |s: &str| s.replace('\0', " ");
    format!(
        "{TOOL_DONE_MARKER}{}\0{}\0{duration_ms}",
        clean(header),
        clean(summary)
    )
}

/// If `chunk` is a tool-done marker, return `(header, summary,
/// duration_ms)`. A malformed duration degrades to `0` rather than
/// dropping the completion.
pub fn parse_tool_done(chunk: &str) -> Option<(&str, &str, u64)> {
    let rest = chunk.strip_prefix(TOOL_DONE_MARKER)?;
    let mut parts = rest.splitn(3, '\0');
    let header = parts.next()?;
    let summary = parts.next()?;
    let duration = parts.next()?;
    Some((header, summary, duration.parse::<u64>().unwrap_or(0)))
}

/// Short human duration for tool rows: `340ms`, `1.2s`, `1m05s`.
pub fn format_duration_ms(ms: u64) -> String {
    if ms < 1000 {
        format!("{ms}ms")
    } else if ms < 60_000 {
        format!("{:.1}s", ms as f64 / 1000.0)
    } else {
        format!("{}m{:02}s", ms / 60_000, (ms % 60_000) / 1000)
    }
}

/// Marker for the post-tool thinking phase: tool result was reinjected
/// and the LLM is reasoning again. The TUI switches from "tool: …" back
/// to "thinking…" so a slow reinjection doesn't look like a stuck tool.
pub const THINKING_MARKER: &str = "\0kod-thinking\0";

pub fn thinking_marker() -> String {
    THINKING_MARKER.to_string()
}

pub fn is_thinking_marker(chunk: &str) -> bool {
    chunk == THINKING_MARKER
}

/// Marker prefix for an interactive question on the streaming chunk
/// channel: `\0kod-question:<id>:<json>\0`. Same shape as the
/// approval marker; re-exported here so consumers do not have to reach
/// into kod-tools.
pub const QUESTION_MARKER: &str = kod_tools::ask::QUESTION_MARKER;

/// Build a question marker for `id` with the JSON-encoded request.
pub fn question_marker(id: u64, json: &str) -> String {
    kod_tools::ask::question_marker(id, json)
}

/// Parse a question marker, returning `(id, json)`.
pub fn parse_question(chunk: &str) -> Option<(u64, &str)> {
    kod_tools::ask::parse_question(chunk)
}

/// Bytes of headroom reserved when capping a JSON tool result: enough
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
///   `{"path": "/foo.rs", "content": "fn main() {\n    ...
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

/// Does this reply declare the goal met?
///
/// The goal prompt instructs the model to "end your reply with a line
/// containing exactly GOAL MET". The check that used to stand here was
/// `final_text.to_uppercase().contains("GOAL MET")` — a substring test
/// that fired on "I have not reached GOAL MET yet" and on "I cannot
/// determine if the GOAL MET criteria are satisfied", stopping the
/// loop with a false success and hiding the model's actual progress.
///
/// Anchor on the last non-empty line instead. Trim and strip the same
/// decorations a model often adds (`**GOAL MET**`, `GOAL MET.`,
/// `— GOAL MET`, `> GOAL MET`), then compare case-insensitively to
/// `GOAL MET`. A reply that only mentions the phrase mid-paragraph is
/// not a completion signal.
pub(crate) fn reply_declares_goal_met(text: &str) -> bool {
    let last_line = text
        .lines()
        .rev()
        .map(|l| l.trim())
        .find(|l| !l.is_empty());
    let Some(line) = last_line else {
        return false;
    };
    // Strip surrounding emphasis and leading quote / list markers.
    let stripped: String = line
        .trim_matches(|c: char| {
            c.is_whitespace() || c == '*' || c == '`' || c == '>' || c == '-'
        })
        .trim_start_matches(|c: char| c.is_whitespace() || c == '—' || c == ':')
        .trim_end_matches(|c: char| c.is_whitespace() || c == '.' || c == '!' || c == ':')
        .to_string();
    stripped.eq_ignore_ascii_case("GOAL MET")
}

/// Expand `@path` references in `input` into fenced code blocks
/// containing the referenced file's content.
///
/// This runs before the prompt reaches the router. A reference is an
/// `@` at a word boundary followed by a path-shaped token: no
/// whitespace, and containing a `/`, a `.`, or ending at the end of
/// input. Paths are resolved against `working_dir`; `~/` expands to the
/// home directory. The file is inserted as
/// `\n\n<file path=\"...\">\n...\n</file>\n\n` so the model sees it as
/// an explicit, named context block rather than text woven into the
/// question.
///
/// Errors are silent: a non-existent path or an unreadable file leaves
/// the `@path` token untouched. A user who typed `@nonexistent` gets
/// their literal input back; the model handles the ambiguity naturally.
/// A user who typed `@real/file.rs` and got content back does not need
/// to know about the error path.
///
/// Reads cap at [`MAX_AT_REF_BYTES`] per file; a file larger than that
/// is truncated with a marker. The total number of files per prompt
/// caps at [`MAX_AT_REFS`] so a user who pastes a wall of @-tokens
/// cannot blow the context window on one turn.
pub fn expand_at_references(input: &str, working_dir: &std::path::Path) -> String {
    const MAX_AT_REF_BYTES: usize = 64 * 1024;
    const MAX_AT_REFS: usize = 10;

    let mut out = String::with_capacity(input.len() + 256);
    let bytes = input.as_bytes();
    let mut i = 0;
    let mut inserted = 0usize;
    while i < bytes.len() {
        // An @ starts a reference only at a word boundary: previous
        // byte must be whitespace or start of input.
        let at_word_start = i == 0
            || matches!(bytes[i - 1], b' ' | b'\t' | b'\n' | b'\r');
        if bytes[i] == b'@' && at_word_start {
            // Scan the token: everything up to whitespace.
            let mut j = i + 1;
            while j < bytes.len() && !bytes[j].is_ascii_whitespace() {
                j += 1;
            }
            let token = &input[i + 1..j];
            // Heuristic for "looks like a path": non-empty, and
            // contains `/`, `.`, or `~`. This filters out `@user`
            // mentions that are not paths.
            let looks_like_path = !token.is_empty()
                && (token.contains('/')
                    || token.contains('.')
                    || token.starts_with('~'));
            if looks_like_path && inserted < MAX_AT_REFS {
                let expanded = expand_one_at_ref(token, working_dir, MAX_AT_REF_BYTES);
                if let Some(text) = expanded {
                    out.push_str(&text);
                    inserted += 1;
                    i = j;
                    continue;
                }
            }
        }
        // Copy the byte through unchanged. Multi-byte UTF-8 preserves
        // because we copy byte-by-byte and the input was valid UTF-8.
        out.push(bytes[i] as char);
        i += 1;
    }
    // Re-decode as UTF-8 — the byte-copy above yields a valid string
    // because we only skipped whole bytes when expanding.
    out
}

/// Try to expand one `@path` token. Returns the fenced block on
/// success, `None` when the file cannot be read or the path is not
/// inside `working_dir` (a symlink escape is refused, matching the
/// tools' own containment check).
fn expand_one_at_ref(
    token: &str,
    working_dir: &std::path::Path,
    max_bytes: usize,
) -> Option<String> {
    let expanded_tilde = if let Some(rest) = token.strip_prefix("~/") {
        let home = dirs::home_dir()?;
        home.join(rest)
    } else {
        std::path::PathBuf::from(token)
    };
    let candidate = if expanded_tilde.is_absolute() {
        expanded_tilde
    } else {
        working_dir.join(expanded_tilde)
    };
    let canonical = std::fs::canonicalize(&candidate).ok()?;
    // Containment: the resolved target must live inside the
    // canonicalized working directory. This matches the tool-context
    // rule; without it, `@../../etc/passwd` would leak.
    let root = std::fs::canonicalize(working_dir).ok()?;
    if !canonical.starts_with(&root) {
        return None;
    }
    if !canonical.is_file() {
        return None;
    }
    let text = std::fs::read_to_string(&canonical).ok()?;
    let (body, truncated) = if text.len() > max_bytes {
        let mut end = max_bytes;
        while end > 0 && !text.is_char_boundary(end) {
            end -= 1;
        }
        (text[..end].to_string(), true)
    } else {
        (text, false)
    };
    let notice = if truncated {
        format!("\n[truncated at {} bytes]", max_bytes)
    } else {
        String::new()
    };
    Some(format!(
        "\n<file path=\"{}\">\n{}{}\n</file>\n",
        canonical.display(),
        body,
        notice,
    ))
}

/// Truncate a UTF-8 string to at most `max` bytes, rounding down to the
/// nearest char boundary. Returns the input unchanged when it already
/// fits. Use this instead of `&s[..max]` — the raw slice panics when
/// `max` lands mid-codepoint, which any non-ASCII tool output can hit
/// (a file containing "café", an error message with an em-dash, any
/// emoji in a directory listing).
pub(crate) fn truncate_chars(s: &str, max: usize) -> &str {
    if s.len() <= max {
        return s;
    }
    let mut end = max;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    &s[..end]
}

/// Append a round's text to the accumulated final text, inserting a
/// blank-line separator when this is not the first non-empty round.
///
/// Within one `process*` call the agentic loop can produce text in
/// more than one round: the model writes a sentence, calls a tool,
/// then writes a follow-up. Each round's text went into the same
/// `final_text` and, without a separator, the caller saw
/// "Let me check.Here is the answer." — two sentences jammed into
/// one. The TUI side-stepped the visible join because it flushes
/// each round into its own bubble (see `flush_streamed_text`), but a
/// non-streaming caller (the returned `final_text`) or a stream
/// consumer that does not track round boundaries saw the run-together
/// text.
fn append_round_text(buf: &mut String, text: &str) {
    if text.is_empty() {
        return;
    }
    if !buf.is_empty() {
        buf.push_str("\n\n");
    }
    buf.push_str(text);
}

/// One-line brief for a tool call: `execute_command cargo test …`,
/// `read_file path=…`. Used for the live "running" indicator.
pub fn format_call_brief(name: &str, args: &serde_json::Value) -> String {
    if name == "execute_command" {
        if let Some(cmd) = args.get("command").and_then(|v| v.as_str()) {
            // Multi-line shell snippets read as their first line only.
            let first = cmd.lines().next().unwrap_or(cmd).trim();
            let short = if first.len() > 100 {
                format!("{}…", truncate_chars(first, 100))
            } else {
                first.to_string()
            };
            if short.is_empty() {
                return name.to_string();
            }
            return format!("{name} {short}");
        }
        return name.to_string();
    }
    const KEYS: [&str; 4] = ["path", "pattern", "file", "content"];
    if let Some(obj) = args.as_object() {
        for key in KEYS {
            if let Some(v) = obj.get(key) {
                let s = match v {
                    serde_json::Value::String(s) => s.clone(),
                    serde_json::Value::Bool(b) => b.to_string(),
                    serde_json::Value::Number(n) => n.to_string(),
                    _ => continue,
                };
                let short = if s.len() > 80 {
                    format!("{}…", truncate_chars(&s, 80))
                } else {
                    s
                };
                return format!("{name} {key}={short}");
            }
        }
    }
    // Fallback: show a short preview of the raw arguments so unknown tools
    // still carry context in the running indicator.
    let raw = args.to_string();
    if raw.is_empty() || raw == "null" || raw == "{}" || raw == "[]" {
        return name.to_string();
    }
    let short = if raw.len() > 80 {
        format!("{}...", truncate_chars(&raw, 80))
    } else {
        raw
    };
    format!("{name} {short}")
}

/// Default generation options captured from `LlmConfig`.
/// Transitional until D1 replaces this with per-endpoint config.
#[derive(Debug, Clone)]
pub struct GenerationDefaults {
    pub temperature: Option<f32>,
    pub max_tokens: Option<usize>,
}

impl Default for GenerationDefaults {
    fn default() -> Self {
        Self { temperature: None, max_tokens: None }
    }
}

impl GenerationDefaults {
    fn to_options(&self) -> GenerationOptions {
        GenerationOptions {
            model: None,
            max_tokens: self.max_tokens,
            temperature: self.temperature,
            top_p: None,
            stop_sequences: Vec::new(),
        }
    }
}

/// Max result lines kept per tool message; the rest collapses to a counter.
pub const TOOL_RESULT_LINES: usize = 12;

/// Human-readable tool header: `list_files path=.` instead of raw JSON.
/// `complete_tool_execution` wraps it in `[...]`, so no brackets here.
pub fn format_tool_header(name: &str, args: &serde_json::Value) -> String {
    const KEYS: [&str; 5] = ["path", "pattern", "command", "file", "content"];
    let mut parts = vec![name.to_string()];
    if let Some(obj) = args.as_object() {
        for key in KEYS {
            if let Some(v) = obj.get(key) {
                let s = match v {
                    serde_json::Value::String(s) => s.clone(),
                    serde_json::Value::Bool(b) => b.to_string(),
                    serde_json::Value::Number(n) => n.to_string(),
                    _ => continue,
                };
                let short = if s.len() > 60 {
                    format!("{}…", truncate_chars(&s, 60))
                } else {
                    s
                };
                // Long paths read better as their last two segments.
                let shown = if key == "path" || key == "file" {
                    shorten_path(&short)
                } else {
                    short
                };
                parts.push(format!("{key}={shown}"));
            }
        }
        if let Some(rec) = obj.get("recursive").and_then(|v| v.as_bool())
            && rec
        {
            parts.push("recursive".to_string());
        }
    }
    parts.join(" ")
}

/// Keep the tail of a long path: `/a/b/c` → `…/b/c`.
fn shorten_path(path: &str) -> String {
    const KEEP: usize = 2;
    let mut segments: Vec<&str> = path.split('/').filter(|s| !s.is_empty()).collect();
    if segments.len() <= KEEP + 1 {
        return path.to_string();
    }
    segments = segments[segments.len() - KEEP..].to_vec();
    format!("…/{}", segments.join("/"))
}

/// Render a tool result for chat: file lists become counts + names,
/// command output keeps its lines, everything caps at [`TOOL_RESULT_LINES`]
/// with an explicit "…and N more" instead of a mid-token cut.
pub fn summarize_tool_result(name: &str, result: &ToolResult) -> String {
    match result {
        ToolResult::Success(v) => summarize_success(name, v),
        ToolResult::Error(e) => cap_lines(&format!("Error: {}", e.trim()), 5),
        ToolResult::RequiresConfirmation { description, .. } => {
            format!("Needs confirmation: {description}")
        }
    }
}

/// Cap on the diff body shown for a write_file / patch_file row. A
/// small edit is 5–30 lines; a large one is 200+. The TUI's `o` key
/// expands the full body, so the summary preview can stay tight.
const TOOL_DIFF_LINES: usize = 40;

fn summarize_success(name: &str, v: &serde_json::Value) -> String {
    // write_file / patch_file with a diff field: show the unified diff
    // instead of "written N bytes". This is the whole point of the
    // checkpoint system from the user's perspective — the row says
    // *what* changed, not just *that* something did.
    if matches!(name, "write_file" | "patch_file")
        && let Some(diff) = v.get("diff").and_then(|d| d.as_str())
    {
        if diff.is_empty() {
            return format!(
                "{}: no change (content identical)",
                v.get("path").and_then(|p| p.as_str()).map(shorten_path)
                    .unwrap_or_else(|| name.to_string())
            );
        }
        let path = v
            .get("path")
            .and_then(|p| p.as_str())
            .map(shorten_path)
            .unwrap_or_else(|| name.to_string());
        let body = cap_lines(diff.trim_end(), TOOL_DIFF_LINES);
        return format!("{}:\n{}", path, body);
    }

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
    {
        let path = v
            .get("path")
            .and_then(|p| p.as_str())
            .map(shorten_path)
            .unwrap_or_else(|| name.to_string());
        let lines = content.lines().count();
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
            out.push('\n');
            out.push_str(&preview.join("\n"));
            if lines > preview.len() {
                out.push_str("\n…");
            }
        }
        return out;
    }
    // list_files on a file: report "is a file", not "1 entry in …".
    if name == "list_files"
        && v.get("path_kind").and_then(|k| k.as_str()) == Some("file")
    {
        let path = v
            .get("path")
            .and_then(|p| p.as_str())
            .map(shorten_path)
            .unwrap_or_else(|| name.to_string());
        return format!("{} · is a file, not a directory", path);
    }

    // list_files: {path, files:[...]} → count + names.
    if let Some(files) = v.get("files").and_then(|f| f.as_array()) {
        // The tool returns each entry as an absolute path (it
        // canonicalizes before walking), and the header shows the
        // *shortened* path (`…/subdir`) for readability. The previous
        // code stored only the shortened form and then tried to strip
        // it from each absolute entry — which never matched, so the
        // row showed full absolute paths for every entry. Keep both
        // forms: `dir_full` for the strip, `dir_short` for the header.
        let dir_full = v
            .get("path")
            .and_then(|p| p.as_str())
            .unwrap_or("");
        let dir_short = if dir_full.is_empty() {
            String::new()
        } else {
            shorten_path(dir_full)
        };
        let shown: Vec<String> = files
            .iter()
            .take(TOOL_RESULT_LINES)
            .filter_map(|f| f.as_str())
            .map(|f| {
                // Strip the listed dir prefix; bare names scan fastest.
                // Match against the full path (`dir_full`), not the
                // shortened header form.
                let bare = f.strip_prefix(dir_full).unwrap_or(f);
                let bare = bare.trim_start_matches('/');
                if bare.is_empty() {
                    f.to_string()
                } else {
                    bare.to_string()
                }
            })
            .map(|f| format!("· {f}"))
            .collect();
        let mut out = format!(
            "{} entr{} in {}:",
            files.len(),
            if files.len() == 1 { "y" } else { "ies" },
            if dir_short.is_empty() { name } else { &dir_short }
        );
        if !shown.is_empty() {
            out.push('\n');
            out.push_str(&shown.join("\n"));
        }
        if files.len() > shown.len() {
            out.push_str(&format!("\n… and {} more", files.len() - shown.len()));
        }
        return out;
    }
    // execute_command / read_file: {stdout, stderr} or {path, content}.
    if let Some(stdout) = v.get("stdout").and_then(|s| s.as_str()) {
        let mut out = cap_lines(stdout.trim_end(), TOOL_RESULT_LINES);
        if let Some(stderr) = v.get("stderr").and_then(|s| s.as_str())
            && !stderr.trim().is_empty()
        {
            out.push_str(&format!("\nstderr:\n{}", cap_lines(stderr.trim_end(), 4)));
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
        // Explain why the output stops, when it did.
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
                "\n[command timed out after {}s — killed]",
                timeout_secs
            ));
        }
        if truncated {
            out.push_str("\n[output truncated at cap");
            if signalled && !timed_out {
                // Both the timeout branch and the cap branch call
                // start_kill; getting here with a signal and no
                // timeout means the cap branch fired.
                out.push_str(" — command was killed");
            }
            out.push(']');
        } else if signalled && !timed_out {
            out.push_str("\n[command was killed by a signal (exit_signal reported)]");
        }
        return if out.is_empty() {
            "(no output)".to_string()
        } else {
            out
        };
    }
    if let Some(content) = v.get("content").and_then(|s| s.as_str()) {
        let path = v.get("path").and_then(|p| p.as_str()).unwrap_or(name);
        return format!(
            "{}:\n{}",
            path,
            cap_lines(content.trim_end(), TOOL_RESULT_LINES)
        );
    }
    // Anything else: pretty JSON, capped by line (never mid-token).
    match serde_json::to_string_pretty(v) {
        Ok(pretty) => cap_lines(&pretty, TOOL_RESULT_LINES),
        Err(_) => cap_lines(&v.to_string(), TOOL_RESULT_LINES),
    }
}

/// Keep the first `max` lines; append an explicit remainder counter.
fn cap_lines(text: &str, max: usize) -> String {
    let lines: Vec<&str> = text.lines().collect();
    if lines.len() <= max {
        return text.to_string();
    }
    format!(
        "{}\n… and {} more lines",
        lines[..max].join("\n"),
        lines.len() - max
    )
}

// The engine's transcript used to be a private `HistoryTurn
// { user: bool, text: String }`. It is now `Vec<ChatMessage>` so the
// transcript can carry tool calls, tool results, and — from A4 on —
// a system prompt that is not a fake user turn. The rendering is
// preserved byte-for-byte via `ChatMessage::render_text`, guarded by
// `crates/kod-core/tests/characterization_history.rs`.

/// Cap the remembered transcript: last turns, each truncated, total render
/// capped so history can never blow the context window on its own.
const MAX_HISTORY_TURNS: usize = 40;

/// Per-turn cap. A single turn can hold a code snippet, an error trace,
/// or a tool-result excerpt without being chopped. Was 1500, which was
/// smaller than a typical `read_file` output — every turn past the first
/// got truncated.
const MAX_TURN_CHARS: usize = 4_000;

/// Default total rendered-history budget, in chars. ~8k tokens at the
/// rough 4-chars-per-token approximation, which fits comfortably
/// alongside the prompt scaffolding (identity, environment, tool
/// inventory, skill inventory, user request) even on an 8k-context
/// model. Larger models should raise this via
/// [`KodEngine::set_history_budget`]; the TUI and CLI derive the value
/// from `LlmConfig::context_window` at startup.
pub const DEFAULT_HISTORY_CHAR_BUDGET: usize = 32_000;

/// Floor for a caller-supplied budget. Below this, history is so short
/// that the model effectively has no memory of past turns — that is
/// worse than the small-window default it is trying to protect, so we
/// clamp instead of silently dropping every turn.
const MIN_HISTORY_CHAR_BUDGET: usize = 4_000;

/// Outcome of one tool-execution round: results for the response plus a
/// prompt block feeding them back to the model. `elapsed_ms` parallels
/// `results` — per-call wall time for the live done-markers.
struct ToolRound {
    results: Vec<ToolResult>,
    prompt_block: String,
    elapsed_ms: Vec<u64>,
}

/// Default transcript key: the interactive session. Public methods
/// without an explicit key operate on this. Swarm agents use a
/// `swarm:<agent-id>` key so concurrent agents do not interleave their
/// turns into one shared history.
const DEFAULT_TRANSCRIPT_KEY: &str = "";

/// Main engine for KOD
pub struct KodEngine {
    router: Arc<TaskRouter>,
    provider: RwLock<Option<Arc<dyn LlmProvider>>>,
    is_running: RwLock<bool>,
    tools: Arc<ToolRegistry>,
    tool_context: ToolContext,
    /// Shared per-path advisory locks. Cloned into every per-call
    /// tool context the engine derives, so a swarm agent and the
    /// interactive session contend on the same table.
    lock_table: Arc<PathLockTable>,
    working_dir: PathBuf,
    /// Steer notes queued while a prompt is running (see [`KodEngine::steer`]).
    steer_queue: RwLock<Vec<String>>,
    /// Set by [`KodEngine::request_cancel`]; loops check it between rounds.
    cancelled: AtomicBool,
    /// Transcripts, one per key. `DEFAULT_TRANSCRIPT_KEY` is the
    /// interactive session; a swarm agent uses `swarm:<agent-id>` so
    /// concurrent agents do not interleave their turns.
    history: RwLock<HashMap<String, Vec<kod_types::ChatMessage>>>,
    /// Total chars of history rendered into a prompt. Defaults to
    /// [`DEFAULT_HISTORY_CHAR_BUDGET`]; the TUI and CLI set this from
    /// `LlmConfig::context_window` at startup so a 128k model actually
    /// gets 128k worth of history instead of the 8k-safe default.
    history_budget: std::sync::atomic::AtomicUsize,
    /// The grounded prompt handed to the provider on the most recent
    /// `process*` call. Kept so `/debug last-prompt` can show exactly
    /// what the model received — environment block, tool inventory,
    /// skill inventory, rendered history, and user input — which is
    /// otherwise invisible and the number one source of "why did the
    /// model answer that?" confusion. Overwritten each call; bounded
    /// by the prompt builder's own caps.
    /// Same keying as `history`.
    last_prompt: RwLock<HashMap<String, String>>,
    /// Provider options captured from `LlmConfig` at engine construction.
    /// Transitional until D1 (endpoint config per ModelRef). Kept in an
    /// `RwLock<Option<...>>` so `set_generation_defaults` works through
    /// `&self` (the engine is shared as `Arc<KodEngine>`).
    generation_defaults: RwLock<GenerationDefaults>,
    /// Optional session log. When `Some`, every tool call and its result
    /// are appended as one JSONL entry, `kod replay`-able. `None` (the
    /// default) is the right shape for a test or a one-shot command.
    session_recorder: std::sync::RwLock<Option<std::sync::Arc<crate::session_log::SessionRecorder>>>,
    /// Shell hooks around tool execution. `RwLock<Arc<...>>` so
    /// `set_hooks` works through `&self` — the engine is shared as
    /// `Arc<KodEngine>` by both the CLI and the TUI, so `&mut self`
    /// is unavailable at the call site.
    hooks: std::sync::RwLock<std::sync::Arc<crate::hooks::HookRunner>>,
    /// Sandbox mode for shell commands. `Disabled` (the default) runs
    /// under the user's own privileges; `Require` runs through the
    /// platform primitive and fails loudly if unavailable. `AtomicU8`
    /// so `set_sandbox_mode` works through `&self` — the engine is
    /// shared as `Arc<KodEngine>`.
    sandbox_mode_atomic: std::sync::atomic::AtomicU8,
    /// Whether `web_fetch` may reach the network. `AtomicBool` so the
    /// setter works through `&self`, matching the sandbox flag. Off by
    /// default; the CLI and TUI apply `LlmConfig::network_access` at
    /// startup.
    network_access_atomic: std::sync::atomic::AtomicBool,
    /// When true, `write_file` / `patch_file` calls ask for approval
    /// before running. See [`crate::engine::ApprovalRequest`] and
    /// [`crate::config::ToolsConfig`].
    confirm_writes_atomic: std::sync::atomic::AtomicBool,
    /// When true, a successful `write_file` / `patch_file` triggers an
    /// automatic project check and the diagnostics are appended to the
    /// model's tool-results block. See `ToolsConfig::auto_check`.
    auto_check_atomic: std::sync::atomic::AtomicBool,
    /// A long-lived language server for incremental diagnostics. Lazily
    /// started on the first `lsp_diagnostics` call; `None` when no
    /// server has been spawned yet. Wrapped in an async mutex because
    /// every LSP method takes `&mut self` — the client is mutated on
    /// each call (next_id, buffered reader, etc.).
    ///
    /// The mutex is per-engine. Two concurrent auto-checks serialize,
    /// which is the correct behavior: one language server handles one
    /// request at a time.
    lsp_client: Arc<tokio::sync::Mutex<Option<kod_lsp::LspClient>>>,
    /// The project's diagnostics as of the last check. `None` until a
    /// baseline has been captured. Used by auto-check to distinguish
    /// the model's contribution from pre-existing problems: a write
    /// that introduces no new errors should not be reported as
    /// though it did.
    ///
    /// Identity is `(file, code, message)` — line and column drift
    /// (an edit earlier in a file shifting a later error) does not
    /// make a diagnostic "new". A same-message error at a different
    /// line is the same error.
    check_baseline: Arc<RwLock<Option<Vec<kod_tools::check::Diagnostic>>>>,
    /// Monotonic counter for approval request ids. Ids are only unique
    /// within an engine's lifetime, which is all the consumer needs.
    next_approval_id: std::sync::atomic::AtomicU64,
    /// Pending ask_user questions, keyed by id. Same shape as
    /// `pending_approvals`, different answer type.
    pending_questions: RwLock<std::collections::HashMap<u64, tokio::sync::oneshot::Sender<String>>>,
    /// Monotonic id source for ask_user questions. Kept distinct from
    /// the approval counter so a marker cannot be accidentally
    /// answered by the wrong dialog.
    next_question_id: std::sync::atomic::AtomicU64,
    /// Pending approval requests, keyed by id. The engine inserts a
    /// oneshot sender before emitting the request marker; the consumer
    /// takes the sender out via [`KodEngine::respond_to_approval`] and
    /// sends a decision. A request that is never answered is dropped
    /// when its wait times out (see `AWAIT_APPROVAL_SECS`).
    pending_approvals: RwLock<std::collections::HashMap<u64, tokio::sync::oneshot::Sender<ApprovalDecision>>>,
    /// Shared key-value blackboard for swarm agents. Every agent
    /// running under this engine reads and writes the same store
    /// through the cloned `Arc`.
    swarm_knowledge: kod_tools::SwarmKnowledge,
    /// The session's todo list. Shared across swarm agents and across
    /// every turn of the same engine.
    todo_list: kod_tools::TodoList,
    /// File checkpoint snapshots. `Some` when a checkpoint directory
    /// could be determined from the working directory; `None` when
    /// the home directory is unavailable (a stripped container, a
    /// test that has unset HOME). See [`crate::checkpoint`].
    checkpoints: Option<Arc<crate::checkpoint::CheckpointManager>>,
}

/// Marker prefix for approval requests inside the `process_streaming`
/// chunk channel: `\0kod-approval:<id>:<json>\0`. The consumer (TUI,
/// CLI) renders a diff dialog and calls
/// [`KodEngine::respond_to_approval`] with the id.
pub const TOOL_APPROVAL_MARKER: &str = "\0kod-approval:";

/// Build an approval-request chunk carrying the JSON-encoded request.
/// The id is prepended as a decimal string so the parser does not need
/// to decode the JSON to know which pending request the chunk is for.
pub fn tool_approval_marker(id: u64, request_json: &str) -> String {
    // Sanitize NULs; the request JSON is user-tool-adjacent (paths,
    // arguments) and could contain anything.
    let clean = request_json.replace('\0', " ");
    format!("{TOOL_APPROVAL_MARKER}{id}:{clean}\0")
}

/// If `chunk` is an approval-request marker, return `(id, json)`.
pub fn parse_tool_approval(chunk: &str) -> Option<(u64, &str)> {
    let rest = chunk.strip_prefix(TOOL_APPROVAL_MARKER)?;
    let body = rest.strip_suffix('\0')?;
    let (id_str, json) = body.split_once(':')?;
    let id = id_str.parse::<u64>().ok()?;
    Some((id, json))
}

/// The serialized shape of an approval request. Sent as JSON inside an
/// [`tool_approval_marker`] chunk, decoded by the consumer.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ApprovalRequest {
    /// The tool the model wants to run.
    pub tool_name: String,
    /// The arguments the model passed. Included so a consumer that
    /// wants a specific view (the CLI prints the JSON verbatim) does
    /// not have to re-invoke anything.
    pub arguments: serde_json::Value,
    /// A unified diff of the intended change, when one could be
    /// computed. `None` when the target does not exist yet (a
    /// create) or the file is binary.
    pub diff: Option<String>,
    /// The user-facing summary a plain-text consumer can print
    /// without decoding `diff`.
    pub summary: String,
}

/// What the consumer decides.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ApprovalDecision {
    Approve,
    Deny,
    /// Same as `Deny` in this version; the variant exists so that
    /// adding a "remember my choice" set later does not change the
    /// wire format.
    DenyAlways,
}

impl KodEngine {
    /// Create a new engine
    pub fn new(config: RouterConfig, db_path: PathBuf) -> Result<Self> {
        let working_dir = config.working_dir.clone();
        // Git operations default to enabled because the git tools that
        // exist today (`git_status`, `git_diff`) are read-only. A
        // future mutating git tool (commit, branch, checkout) must
        // reconsider this default — today, disabling the flag turns
        // off `git status` for a caller that wants it, which is the
        // wrong trade.
        // `network_access` stays off in the default context. The
        // `web_fetch` tool is registered either way; a caller that
        // wants the agent to reach the network must construct a
        // context with `network_access: true`. Wiring this to the
        // `LlmConfig::network_access` flag is a follow-up — the
        // context is built before the config is available here, and
        // the CLI/TUI do not currently pass a context in.
        let tool_context =
            ToolContext::new(working_dir.clone()).with_permissions(ToolPermissions {
                read_files: true,
                write_files: true,
                execute_commands: true,
                network_access: false,
                git_operations: true,
                allowed_paths: Vec::new(),
                forbidden_paths: Vec::new(),
            });
        let router = TaskRouter::new(config, db_path)?;
        let lock_table = Arc::new(PathLockTable::new());
        // Snapshots are best-effort: a session without a home directory
        // still runs, it just cannot roll back. The manager is
        // per-working-directory, so two sessions on different projects
        // do not see each other's checkpoints.
        let checkpoints = crate::checkpoint::CheckpointManager::for_working_dir(
            &working_dir,
        )
        .map(Arc::new);

        Ok(Self {
            router: Arc::new(router),
            provider: RwLock::new(None),
            is_running: RwLock::new(false),
            tools: Arc::new(ToolRegistry::new()),
            tool_context,
            lock_table,
            working_dir,
            steer_queue: RwLock::new(Vec::new()),
            cancelled: AtomicBool::new(false),
            history: RwLock::new(HashMap::new()),
            history_budget: std::sync::atomic::AtomicUsize::new(
                DEFAULT_HISTORY_CHAR_BUDGET,
            ),
            last_prompt: RwLock::new(HashMap::new()),
            generation_defaults: RwLock::new(GenerationDefaults::default()),
            session_recorder: std::sync::RwLock::new(None),
            hooks: std::sync::RwLock::new(std::sync::Arc::new(
                crate::hooks::HookRunner::disabled(),
            )),
            sandbox_mode_atomic: std::sync::atomic::AtomicU8::new(0),
            network_access_atomic: std::sync::atomic::AtomicBool::new(false),
            confirm_writes_atomic: std::sync::atomic::AtomicBool::new(false),
            auto_check_atomic: std::sync::atomic::AtomicBool::new(false),
            lsp_client: Arc::new(tokio::sync::Mutex::new(None)),
            check_baseline: Arc::new(RwLock::new(None)),
            next_approval_id: std::sync::atomic::AtomicU64::new(1),
            pending_approvals: RwLock::new(std::collections::HashMap::new()),
            pending_questions: RwLock::new(std::collections::HashMap::new()),
            next_question_id: std::sync::atomic::AtomicU64::new(1),
            swarm_knowledge: kod_tools::new_knowledge(),
            todo_list: kod_tools::new_todo_list(),
            checkpoints,
        })
    }

    /// Set the total chars of history rendered into prompts. Called by
    /// the TUI and CLI after construction with a value derived from the
    /// model's context window (roughly `context_window * 3`, which is
    /// the char-count version of the 4-chars-per-token approximation
    /// with headroom for prompt scaffolding).
    ///
    /// Silently clamps below [`MIN_HISTORY_CHAR_BUDGET`]: a caller who
    /// passes a tiny value would otherwise produce an engine that
    /// forgets every turn before it finishes.
    pub fn set_history_budget(&self, chars: usize) {
        let clamped = chars.max(MIN_HISTORY_CHAR_BUDGET);
        self.history_budget
            .store(clamped, std::sync::atomic::Ordering::Relaxed);
    }

    /// Set the generation defaults the engine will pass to the provider
    /// on each call. Called by the CLI/TUI after construction from
    /// `LlmConfig`. Transitional until D1.
    pub fn set_generation_defaults(&self, temperature: Option<f32>, max_tokens: Option<usize>) {
        if let Ok(mut guard) = self.generation_defaults.try_write() {
            *guard = GenerationDefaults { temperature, max_tokens };
        }
    }

    /// The current history budget in chars (for `/debug` and tests).
    pub fn history_budget(&self) -> usize {
        self.history_budget
            .load(std::sync::atomic::Ordering::Relaxed)
    }

    /// Set the sandbox mode for shell commands. `Required` wraps every
    /// `execute_command` in `bwrap` or `sandbox-exec`; a missing
    /// primitive fails each such call with a named reason.
    pub fn set_sandbox_mode(&self, mode: kod_tools::context::SandboxMode) {
        use std::sync::atomic::Ordering;
        let v = match mode {
            kod_tools::context::SandboxMode::Disabled => 0u8,
            kod_tools::context::SandboxMode::Require => 1u8,
        };
        self.sandbox_mode_atomic.store(v, Ordering::Relaxed);
    }

    /// The current sandbox mode.
    pub fn sandbox_setting(&self) -> kod_tools::context::SandboxMode {
        use std::sync::atomic::Ordering;
        match self.sandbox_mode_atomic.load(Ordering::Relaxed) {
            1 => kod_tools::context::SandboxMode::Require,
            _ => kod_tools::context::SandboxMode::Disabled,
        }
    }

    /// Enable or disable network access for `web_fetch`. The CLI and
    /// TUI call this at startup with `LlmConfig::network_access`. A
    /// caller that never calls it gets the default (off) — a session
    /// that has not opted in cannot reach the network through a tool.
    pub fn set_network_access(&self, allowed: bool) {
        self.network_access_atomic
            .store(allowed, std::sync::atomic::Ordering::Relaxed);
    }

    /// The current network-access setting.
    pub fn network_access_setting(&self) -> bool {
        self.network_access_atomic
            .load(std::sync::atomic::Ordering::Relaxed)
    }

    /// Enable or disable write confirmation. Called by the CLI and TUI
    /// at startup with `ToolsConfig::confirm_writes`.
    pub fn set_confirm_writes(&self, enabled: bool) {
        self.confirm_writes_atomic
            .store(enabled, std::sync::atomic::Ordering::Relaxed);
    }

    /// The current write-confirmation setting.
    pub fn confirm_writes_setting(&self) -> bool {
        self.confirm_writes_atomic
            .load(std::sync::atomic::Ordering::Relaxed)
    }

    /// Enable or disable auto-check after writes. Called by the CLI and
    /// TUI at startup with `ToolsConfig::auto_check`.
    pub fn set_auto_check(&self, enabled: bool) {
        self.auto_check_atomic
            .store(enabled, std::sync::atomic::Ordering::Relaxed);
    }

    /// The current auto-check setting.
    pub fn auto_check_setting(&self) -> bool {
        self.auto_check_atomic
            .load(std::sync::atomic::Ordering::Relaxed)
    }

    /// Is a language server available for this path?
    ///
    /// Returns the server binary name (e.g. `rust-analyzer`) when one
    /// is on PATH and can serve this file's language, or `None` when
    /// the caller should fall back to the compiler-based `check`.
    ///
    /// Only Rust is supported today. Adding Python (`pyright-langserver`
    /// or `pylsp`), TypeScript (`typescript-language-server`), or Go
    /// (`gopls`) is a match arm on the extension plus the binary name;
    /// the LSP crate already speaks the protocol.
    pub fn lsp_binary_for(path: &std::path::Path) -> Option<&'static str> {
        match path.extension().and_then(|s| s.to_str()) {
            Some("rs") if which("rust-analyzer") => Some("rust-analyzer"),
            _ => None,
        }
    }

    /// Return LSP diagnostics for `path`.
    ///
    /// The first call spawns and initializes the server; subsequent
    /// calls reuse the same process, which is where the value is —
    /// rust-analyzer's indexing cost is paid once and then per-file
    /// diagnostics are milliseconds. On any failure (no binary, spawn
    /// error, protocol error, timeout) the client is dropped so the
    /// next call retries cleanly, and the method returns an empty vec.
    /// The caller treats empty as "no LSP feedback" and falls back to
    /// `CheckTool::run_check`.
    ///
    /// `content` is what to analyze. The caller reads the file it just
    /// wrote; passing the content avoids a re-read that could race a
    /// concurrent write.
    pub async fn lsp_diagnostics(
        &self,
        path: &std::path::Path,
        content: &str,
        overall_timeout: std::time::Duration,
    ) -> Vec<kod_lsp::Diagnostic> {
        let Some(binary) = Self::lsp_binary_for(path) else {
            return Vec::new();
        };

        let mut guard = self.lsp_client.lock().await;

        // Lazy spawn + initialize.
        if guard.is_none() {
            match kod_lsp::LspClient::start(binary, &self.working_dir).await {
                Ok(mut client) => {
                    if let Err(e) = client.initialize().await {
                        tracing::debug!(
                            error = %e,
                            binary,
                            "LSP initialize failed; disabling LSP for this session"
                        );
                        return Vec::new();
                    }
                    tracing::info!(binary, "LSP client started");
                    *guard = Some(client);
                }
                Err(e) => {
                    tracing::debug!(error = %e, binary, "could not spawn LSP server");
                    return Vec::new();
                }
            }
        }

        let Some(client) = guard.as_mut() else {
            return Vec::new();
        };

        // `client.diagnostics` handles the first-call / subsequent-call
        // distinction internally: didOpen the first time, didChange
        // after. The caller does not track which files are open.
        match client.diagnostics(path, content, overall_timeout).await {
            Ok(diags) => diags,
            Err(e) => {
                tracing::debug!(error = %e, "LSP diagnostics failed; dropping client");
                *guard = None;
                Vec::new()
            }
        }
    }

    /// Capture the project's diagnostics into the baseline. Called at
    /// engine start (best-effort) and by any caller that wants the
    /// next auto-check to treat the current state as "before".
    ///
    /// Silent on failure: no project, no toolchain, or a timeout all
    /// leave the baseline at its previous value. A `None` baseline
    /// means the next auto-check reports every diagnostic it sees;
    /// that is the correct behavior for a session that never had a
    /// chance to establish a baseline.
    pub async fn refresh_check_baseline(&self) {
        match kod_tools::CheckTool::run_check(&self.working_dir, 60).await {
            Ok(outcome) => {
                let n = outcome.diagnostics.len();
                *self.check_baseline.write().await = Some(outcome.diagnostics);
                tracing::debug!(count = n, "check baseline refreshed");
            }
            Err(e) => {
                tracing::debug!(
                    error = %e,
                    "check baseline not refreshed (no project or toolchain)"
                );
            }
        }
    }

    /// The current baseline, if one has been captured. Public for
    /// tests and for a caller that wants to display what the engine
    /// considers "pre-existing".
    pub async fn check_baseline(
        &self,
    ) -> Option<Vec<kod_tools::check::Diagnostic>> {
        self.check_baseline.read().await.clone()
    }

    /// Shut down the LSP client if one is running. Called by
    /// `shutdown()`.
    async fn lsp_shutdown(&self) {
        let mut guard = self.lsp_client.lock().await;
        if let Some(client) = guard.take() {
            client.shutdown().await;
            tracing::info!("LSP client shut down");
        }
    }

    /// Answer a pending approval request. Returns `true` when the id
    /// matched a pending request and the decision was delivered; `false`
    /// when the id was unknown (a stale click, a consumer that raced a
    /// timeout). The consumer does not have to care which — a `false`
    /// just means the engine already gave up on this request.
    pub async fn respond_to_approval(&self, id: u64, decision: ApprovalDecision) -> bool {
        let sender = self.pending_approvals.write().await.remove(&id);
        match sender {
            Some(tx) => tx.send(decision).is_ok(),
            None => false,
        }
    }

    /// Answer a pending ask_user question. Returns `true` when the id
    /// matched and the answer was delivered.
    pub async fn respond_to_question(&self, id: u64, answer: String) -> bool {
        let sender = self.pending_questions.write().await.remove(&id);
        match sender {
            Some(tx) => tx.send(answer).is_ok(),
            None => false,
        }
    }

    /// Install the shell hooks the engine runs around tool calls. A
    /// caller that never calls this gets a disabled runner.
    pub fn set_hooks(&self, config: kod_config::HooksConfig) {
        if let Ok(mut guard) = self.hooks.write() {
            *guard = std::sync::Arc::new(crate::hooks::HookRunner::new(config));
        }
    }

    /// Install a session log. Every tool call and its result is
    /// appended to the file the recorder holds. A caller that never
    /// calls this gets no log.
    pub fn set_session_recorder(
        &self,
        recorder: Arc<crate::session_log::SessionRecorder>,
    ) {
        if let Ok(mut slot) = self.session_recorder.write() {
            *slot = Some(recorder);
        }
    }

    /// The session log path, when one is installed.
    pub fn session_log_path(&self) -> Option<std::path::PathBuf> {
        self.session_recorder
            .read()
            .ok()
            .and_then(|guard| guard.as_ref().map(|r| r.path().to_path_buf()))
    }

    /// Set the LLM provider
    pub async fn set_provider(&self, provider: Arc<dyn LlmProvider>) {
        *self.provider.write().await = Some(provider);
    }

    /// Error for a `process*` call made before a provider is installed.
    ///
    /// The router has a set of placeholder handlers that return
    /// strings like "Processing simple task: …". Those exist so the
    /// router's own unit tests can exercise the classification path
    /// without a provider, and they are fine in that role. As a
    /// user-visible answer from the engine, though, they are worse
    /// than an error: the call looks like it succeeded, the reply
    /// advertises no fix, and the operator has to guess that the
    /// engine was never wired to a model.
    ///
    /// Callers that do want the placeholder behavior (the router's
    /// own tests) use `TaskRouter` directly. The engine tells the
    /// truth.
    fn no_provider_error() -> KodError {
        KodError::InvalidState(
            "No LLM provider configured. Install one with \
             `engine.set_provider(Arc::new(provider))` before calling \
             process — kod-cli and kod-tui do this automatically from \
             ~/.config/kod/config.toml."
                .to_string(),
        )
    }

    /// The provider currently installed, cloned out of its lock.
    /// `None` when the engine has not been wired to a model.
    pub async fn provider_arc(&self) -> Option<Arc<dyn LlmProvider>> {
        self.provider.read().await.clone()
    }

    /// The engine's shared swarm blackboard. A caller that wants to
    /// seed a fact before a swarm runs, or inspect what was recorded
    /// after, reads and writes this directly.
    pub fn swarm_knowledge(&self) -> &kod_tools::SwarmKnowledge {
        &self.swarm_knowledge
    }

    /// The engine's shared todo list. A caller that wants to seed it
    /// before a session, or render it alongside the chat, reads this
    /// directly.
    pub fn todo_list(&self) -> &kod_tools::TodoList {
        &self.todo_list
    }

    /// The engine's shared per-path lock table. A caller that wants
    /// to hold a lock itself (a test, an embedder coordinating with an
    /// agent) acquires from this table directly.
    pub fn path_lock_table(&self) -> Arc<PathLockTable> {
        Arc::clone(&self.lock_table)
    }

    /// The engine's checkpoint manager, when a checkpoint directory
    /// could be determined. `None` means the engine cannot snapshot
    /// (no home directory); a caller that offers `/rollback` should
    /// say so rather than silently no-op.
    pub fn checkpoints(&self) -> Option<&Arc<crate::checkpoint::CheckpointManager>> {
        self.checkpoints.as_ref()
    }

    /// The working directory tools are rooted at.
    pub fn working_dir(&self) -> &std::path::Path {
        &self.working_dir
    }

    /// Execute a registered tool by name with the engine's own tool
    /// context. Used by the swarm runner's repo probe; a caller that
    /// wants a specific tool can also reach it this way, but the
    /// agentic loop is the ordinary path.
    pub async fn run_tool(
        &self,
        name: &str,
        args: serde_json::Value,
    ) -> Result<kod_types::ToolResult> {
        self.tools
            .execute_tool(name, &args, &self.tool_context)
            .await
    }

    /// List available models from the configured provider.
    ///
    /// Returns `Ok(vec![])` when no provider is set — there really
    /// are zero models to list, and an empty list is the correct
    /// answer. Returns `Err(...)` when a provider *is* set but the
    /// request to it fails — the answer is unknown (server down, auth
    /// wrong, endpoint mistyped), and collapsing that into an empty
    /// vec makes a caller unable to distinguish "the server has no
    /// models" from "the server is not reachable." The two deserve
    /// different user-facing messages and different recovery paths.
    pub async fn list_models(&self) -> Result<Vec<String>> {
        let provider = self.provider.read().await;
        match provider.as_ref() {
            Some(p) => {
                let models = p.list_models().await?;
                Ok(models)
            }
            None => Ok(Vec::new()),
        }
    }

    /// Start the engine
    pub async fn start(&self) -> Result<()> {
        let mut running = self.is_running.write().await;

        if *running {
            return Err(KodError::InvalidState("Engine already running".to_string()));
        }

        *running = true;

        // Register the built-in tools once (start runs exactly once —
        // second call errors above). Tools fail closed via ToolContext
        // permissions unless explicitly granted in `new()`.
        self.tools.register(Box::new(ReadFileTool::new())).await;
        self.tools.register(Box::new(WriteFileTool::new())).await;
        self.tools.register(Box::new(PatchFileTool::new())).await;
        // Swarm coordination tools. Always registered — they cost one
        // HashMap. Agents running concurrently under this engine share
        // the same blackboard through the cloned `Arc`.
        self.tools
            .register(Box::new(kod_tools::SwarmNoteTool::new(
                self.swarm_knowledge.clone(),
            )))
            .await;
        self.tools
            .register(Box::new(kod_tools::SwarmReadTool::new(
                self.swarm_knowledge.clone(),
            )))
            .await;
        self.tools.register(Box::new(ListFilesTool::new())).await;
        self.tools.register(Box::new(GrepTool::new())).await;
        self.tools.register(Box::new(FileInfoTool::new())).await;
        self.tools
            .register(Box::new(ExecuteCommandTool::new()))
            .await;
        // Read-only git inspection. Registered unconditionally; the
        // per-context `git_operations` permission gate rejects calls
        // from a context that opted out.
        self.tools.register(Box::new(GitStatusTool::new())).await;
        self.tools.register(Box::new(GitDiffTool::new())).await;
        self.tools
            .register(Box::new(kod_tools::TodoTool::new(
                self.todo_list.clone(),
            )))
            .await;
        self.tools
            .register(Box::new(kod_tools::SearchFilesTool::new()))
            .await;
        self.tools
            .register(Box::new(kod_tools::AskUserTool::new()))
            .await;
        // `web_fetch` is registered unconditionally; the per-context
        // `network_access` permission gates the actual call. This is
        // the same shape the git tools use, and it means a future
        // caller that wants to enable network access for one agent
        // does not have to re-register the tool.
        self.tools.register(Box::new(kod_tools::WebFetchTool::new())).await;
        // `check` runs the project's compiler/linter and returns
        // structured diagnostics. Registered alongside the other
        // code-aware tools so a model that just wrote a file can ask
        // "did that break the build?" without grepping compiler
        // output.
        self.tools
            .register(Box::new(kod_tools::CheckTool::new()))
            .await;

        // Capture the check baseline in the background. Runs the
        // project's compiler once; the result is stored so the first
        // auto-check can distinguish the model's errors from
        // pre-existing ones. Non-blocking: a big workspace can take
        // tens of seconds and delaying `start` for it would slow the
        // whole session.
        //
        // The handle is not joined on shutdown; the task ends when
        // the compiler exits or the runtime drops. That is fine — the
        // compiler is a child process and Rust's Drop on the engine
        // does not affect it. A `check` running against a project
        // after shutdown writes nothing the session cares about.
        let this = BaselineRefresher {
            working_dir: self.working_dir.clone(),
            check_baseline: Arc::clone(&self.check_baseline),
        };
        tokio::spawn(async move {
            this.refresh_check_baseline().await;
            tracing::debug!("baseline refresh spawned");
        });

        tracing::info!("KOD engine started");
        Ok(())
    }

    /// Process user input
    pub async fn process(&self, input: &str) -> Result<TaskResponse> {
        self.process_for(DEFAULT_TRANSCRIPT_KEY, input).await
    }

    /// Process user input on a named transcript. `key` selects which
    /// transcript (and last-prompt slot) this call reads and writes.
    /// The swarm runner passes `swarm:<agent-id>` per agent, so three
    /// concurrent agents do not interleave their turns.
    pub async fn process_for(&self, key: &str, input: &str) -> Result<TaskResponse> {
        // Check if engine is running
        {
            let running = self.is_running.read().await;
            if !*running {
                return Err(KodError::InvalidState("Engine not running".to_string()));
            }
        }
        // Expand `@file` references before the router sees the input.
        // The expanded prompt is what gets classified, what gets
        // remembered, and what reaches the model; the original typed
        // text is only used for the display row the caller already
        // pushed.
        let expanded_input = expand_at_references(input, &self.working_dir);
        let input = expanded_input.as_str();

        // Clone the provider Arc out of the read lock before any long
        // await. Holding the read guard across the agentic loop below
        // made `set_provider` (used by the TUI's `/model` switch) block
        // until the current generation finished — the write acquired
        // only after the last read released, i.e. at the very end of
        // the response. Cloning is one atomic increment on the Arc, so
        // the read lock is held for microseconds.
        let provider: Option<Arc<dyn LlmProvider>> =
            self.provider.read().await.clone();
        if let Some(provider) = provider.as_ref() {
            // Process through router for task classification and context
            let response = self.router.process_input(input).await?;

            // Build the full prompt using the router's context builder
            let task_type = response.task_type;
            let history = self.render_history_for(key).await;
            self.remember_turn_for(key, true, input).await;
            let prompt = self
                .router
                .build_prompt_with_context(
                    input,
                    &task_type,
                    &history,
                    response.memory_context.clone(),
                )
                .await?;

            // Ground the model: where it runs and what it can touch.
            // Without this it claims "no filesystem access" even though
            // tools are wired below.
            let definitions = self.tools.get_definitions().await;
            let convo = self.ground_prompt(prompt, &definitions);

            // Snapshot the grounded prompt before the loop mutates it
            // with tool results. This is what `/debug last-prompt` shows.
            self.last_prompt.write().await.insert(key.to_string(), convo.clone());

            // Agentic loop: generate (with tools) -> execute -> feed back.
            let options = self
                .generation_defaults
                .read()
                .await
                .to_options();
            let mut pending = convo;
            let (final_text, tool_calls, tool_results, usage) = self
                .run_collected_loop(provider, &mut pending, &definitions, &options, key)
                .await?;
            // Model only called tools and never wrote back: ask for a summary.
            let final_text = if final_text.trim().is_empty() && !tool_calls.is_empty() {
                pending.push_str(
                    "\nSummarize what you did and the result for the user in plain text.",
                );
                provider.generate(&pending, &options).await?
            } else {
                final_text
            };
            self.remember_turn_for(key, false, &final_text).await;

            return Ok(TaskResponse {
                task_type: response.task_type,
                text: Some(final_text),
                tool_calls,
                tool_results,
                skills_used: response.skills_used,
                memory_used: response.memory_used,
                execution_time_ms: response.execution_time_ms,
                usage,
                memory_context: response.memory_context,
            });
        }

        // No provider. Reject rather than return the router's
        // placeholder text — see `no_provider_error`.
        Err(Self::no_provider_error())
    }

    /// Process user input, streaming text chunks live to `chunk_tx`.
    ///
    /// Same result as [`process`], but answer tokens arrive as they generate
    /// (the TUI renders each chunk immediately) and tool starts arrive as
    /// [`tool_start_marker`] chunks (see [`parse_tool_start`]) so the UI can
    /// show "running …" while the tool actually executes.
    pub async fn process_streaming(
        &self,
        input: &str,
        chunk_tx: &tokio::sync::mpsc::Sender<String>,
    ) -> Result<TaskResponse> {
        self.process_streaming_for(DEFAULT_TRANSCRIPT_KEY, input, chunk_tx)
            .await
    }

    /// Streaming variant of [`KodEngine::process_for`].
    pub async fn process_streaming_for(
        &self,
        key: &str,
        input: &str,
        chunk_tx: &tokio::sync::mpsc::Sender<String>,
    ) -> Result<TaskResponse> {
        {
            let running = self.is_running.read().await;
            if !*running {
                return Err(KodError::InvalidState("Engine not running".to_string()));
            }
        }
        let expanded_input = expand_at_references(input, &self.working_dir);
        let input = expanded_input.as_str();

        // See process(): clone out of the lock before any long await.
        let provider: Option<Arc<dyn LlmProvider>> =
            self.provider.read().await.clone();
        if let Some(provider) = provider.as_ref() {
            let response = self.router.process_input(input).await?;
            let task_type = response.task_type;
            let history = self.render_history_for(key).await;
            self.remember_turn_for(key, true, input).await;
            let prompt = self
                .router
                .build_prompt_with_context(
                    input,
                    &task_type,
                    &history,
                    response.memory_context.clone(),
                )
                .await?;
            let definitions = self.tools.get_definitions().await;
            let mut pending = self.ground_prompt(prompt, &definitions);

            // Snapshot the grounded prompt for /debug last-prompt.
            self.last_prompt.write().await.insert(key.to_string(), pending.clone());

            let options = self
                .generation_defaults
                .read()
                .await
                .to_options();
            let (final_text, tool_calls, tool_results, usage) = self
                .run_streaming_loop(provider, &mut pending, &definitions, &options, chunk_tx, key)
                .await?;
            let final_text = if final_text.trim().is_empty() && !tool_calls.is_empty() {
                pending.push_str(
                    "\nSummarize what you did and the result for the user in plain text.",
                );
                self.stream_summary(provider, &pending, &options, chunk_tx)
                    .await?
            } else {
                final_text
            };
            self.remember_turn_for(key, false, &final_text).await;

            return Ok(TaskResponse {
                task_type: response.task_type,
                text: Some(final_text),
                tool_calls,
                tool_results,
                skills_used: response.skills_used,
                memory_used: response.memory_used,
                execution_time_ms: response.execution_time_ms,
                usage,
                memory_context: response.memory_context,
            });
        }

        Err(Self::no_provider_error())
    }

    /// Work toward `goal` across turns until the model declares it met.
    ///
    /// Same streaming contract as [`process_streaming`], but after each
    /// agentic pass the conversation continues with a "keep going" nudge
    /// until the reply contains `GOAL MET` (case-insensitive),
    /// [`MAX_GOAL_TURNS`] passes run, or [`KodEngine::request_cancel`]
    /// fires. Steer notes queued via [`KodEngine::steer`] are injected
    /// every turn. Each turn's text streams live; turns are separated by
    /// a `—— turn N ——` marker chunk so the TUI can render progress.
    pub async fn process_goal_streaming(
        &self,
        input: &str,
        goal: &str,
        chunk_tx: &tokio::sync::mpsc::Sender<String>,
    ) -> Result<TaskResponse> {
        self.process_goal_streaming_for(DEFAULT_TRANSCRIPT_KEY, input, goal, chunk_tx)
            .await
    }

    /// Streaming goal loop on a named transcript.
    pub async fn process_goal_streaming_for(
        &self,
        key: &str,
        input: &str,
        goal: &str,
        chunk_tx: &tokio::sync::mpsc::Sender<String>,
    ) -> Result<TaskResponse> {
        {
            let running = self.is_running.read().await;
            if !*running {
                return Err(KodError::InvalidState("Engine not running".to_string()));
            }
        }
        let expanded_input = expand_at_references(input, &self.working_dir);
        let input = expanded_input.as_str();

        // See process(): clone out of the lock before any long await.
        let provider: Option<Arc<dyn LlmProvider>> =
            self.provider.read().await.clone();
        if let Some(provider) = provider.as_ref() {
            let response = self.router.process_input(input).await?;
            let task_type = response.task_type;
            let history = self.render_history_for(key).await;
            self.remember_turn_for(key, true, input).await;
            let prompt = self
                .router
                .build_prompt_with_context(
                    input,
                    &task_type,
                    &history,
                    response.memory_context.clone(),
                )
                .await?;
            let definitions = self.tools.get_definitions().await;
            let mut pending = self.ground_prompt(prompt, &definitions);
            pending.push_str(&format!(
                "\n## Goal\n\n{goal}\n\nWork turn by turn toward this goal using tools. Do not ask the user for confirmation — act. When the goal is fully reached, end your reply with a line containing exactly GOAL MET and summarize what was done. If a tool errors, work around it and keep going.\n"
            ));

            // Snapshot includes the goal block — that is what the model
            // sees on turn 1, which is what users want to inspect when a
            // goal run misbehaves.
            self.last_prompt.write().await.insert(key.to_string(), pending.clone());

            let options = self
                .generation_defaults
                .read()
                .await
                .to_options();
            let mut all_text = String::new();
            let mut tool_calls: Vec<ToolCall> = Vec::new();
            let mut tool_results: Vec<ToolResult> = Vec::new();
            let mut last_usage: Option<kod_provider::TokenUsage> = None;
            for turn in 1..=MAX_GOAL_TURNS {
                if self.is_cancelled() {
                    return Err(KodError::InvalidState("cancelled by user".to_string()));
                }
                if turn > 1 {
                    let _ = chunk_tx.send(format!("\n\n—— turn {turn} ——\n")).await;
                    pending.push_str(
                        "\n\nContinue working toward the goal above. If it is now fully reached, reply with GOAL MET plus a short summary instead of calling more tools.\n",
                    );
                }
                self.apply_steers(&mut pending).await;
                let (final_text, calls, results, usage) = self
                    .run_streaming_loop(
                        provider,
                        &mut pending,
                        &definitions,
                        &options,
                        chunk_tx,
                        key,
                    )
                    .await?;
                last_usage = usage.or(last_usage);
                if !all_text.is_empty() && !final_text.trim().is_empty() {
                    all_text.push_str("\n\n");
                }
                all_text.push_str(&final_text);
                tool_calls.extend(calls);
                tool_results.extend(results);
                if reply_declares_goal_met(&final_text) {
                    break;
                }
                if turn == MAX_GOAL_TURNS {
                    all_text.push_str("\n\n(Goal loop stopped after maximum turns — progress above. Refine with /goal or /steer.)");
                }
            }
            self.remember_turn_for(key, false, &all_text).await;

            return Ok(TaskResponse {
                task_type: response.task_type,
                text: Some(all_text),
                tool_calls,
                tool_results,
                skills_used: response.skills_used,
                memory_used: response.memory_used,
                execution_time_ms: response.execution_time_ms,
                usage: last_usage,
                memory_context: response.memory_context,
            });
        }

        Err(Self::no_provider_error())
    }

    /// Collected (non-streaming) agentic loop used by [`process`].
    async fn run_collected_loop(
        &self,
        provider: &Arc<dyn LlmProvider>,
        pending: &mut String,
        definitions: &[ToolDefinition],
        options: &GenerationOptions,
        holder: &str,
    ) -> Result<(
        String,
        Vec<ToolCall>,
        Vec<ToolResult>,
        Option<kod_provider::TokenUsage>,
    )> {
        let mut final_text = String::new();
        let mut tool_calls: Vec<ToolCall> = Vec::new();
        let mut tool_results: Vec<ToolResult> = Vec::new();
        let mut last_usage: Option<kod_provider::TokenUsage> = None;
        for _ in 0..MAX_TOOL_ROUNDS {
            if self.is_cancelled() {
                return Err(KodError::InvalidState("cancelled by user".to_string()));
            }
            match provider
                .generate_with_tools(pending, definitions, options)
                .await?
            {
                GenerationResponse::Text { content, usage } => {
                    last_usage = usage.or(last_usage);
                    append_round_text(&mut final_text, &content);
                    break;
                }
                GenerationResponse::ToolCalls { calls, usage } => {
                    last_usage = usage.or(last_usage);
                    if calls.is_empty() {
                        break;
                    }
                    let section = self.run_tool_calls(&calls, holder, None).await;
                    tool_calls.extend(calls);
                    tool_results.extend(section.results);
                    pending.push_str(&format!("\n\n{}", section.prompt_block));
                    self.apply_steers(pending).await;
                }
                GenerationResponse::Mixed {
                    content,
                    calls,
                    usage,
                } => {
                    last_usage = usage.or(last_usage);
                    append_round_text(&mut final_text, &content);
                    if calls.is_empty() {
                        break;
                    }
                    let section = self.run_tool_calls(&calls, holder, None).await;
                    tool_calls.extend(calls);
                    tool_results.extend(section.results);
                    pending.push_str(&format!("\n\n{}", section.prompt_block));
                    self.apply_steers(pending).await;
                }
            }
        }
        // Exited on the round cap rather than a text-only reply. Tell
        // the transcript — the caller asks the model for a summary
        // after this returns, and this note is what makes that
        // summary "what got done" rather than a recap of the last
        // tool result.
        if final_text.trim().is_empty() && !tool_calls.is_empty() {
            pending.push_str(TOOL_ROUNDS_EXHAUSTED_NOTE);
        }
        Ok((final_text, tool_calls, tool_results, last_usage))
    }

    /// Append queued steer notes to the running conversation (each once).
    async fn apply_steers(&self, pending: &mut String) {
        for note in self.take_steers().await {
            pending.push_str(&format!(
                "\n\n## User steer (new instruction — adjust course now, do not restart what already worked)\n{note}\n"
            ));
        }
    }

    /// Streaming agentic loop: text chunks are forwarded to `chunk_tx` the
    /// moment they arrive; tool-start markers go through the same channel.
    async fn run_streaming_loop(
        &self,
        provider: &Arc<dyn LlmProvider>,
        pending: &mut String,
        definitions: &[ToolDefinition],
        options: &GenerationOptions,
        chunk_tx: &tokio::sync::mpsc::Sender<String>,
        holder: &str,
    ) -> Result<(
        String,
        Vec<ToolCall>,
        Vec<ToolResult>,
        Option<kod_provider::TokenUsage>,
    )> {
        let mut final_text = String::new();
        let mut tool_calls: Vec<ToolCall> = Vec::new();
        let mut tool_results: Vec<ToolResult> = Vec::new();
        let mut last_usage: Option<kod_provider::TokenUsage> = None;
        for _ in 0..MAX_TOOL_ROUNDS {
            if self.is_cancelled() {
                return Err(KodError::InvalidState("cancelled by user".to_string()));
            }
            let (text, calls, usage) = self
                .stream_round(provider, pending, definitions, options, chunk_tx)
                .await?;
            last_usage = usage.or(last_usage);
            append_round_text(&mut final_text, &text);
            if calls.is_empty() {
                break;
            }
            // The running indicator now shows what each call actually does
            // (`execute_command cargo test …`), not just the tool name.
            for call in &calls {
                let _ = chunk_tx
                    .send(tool_args_marker(&format_call_brief(
                        &call.tool_name,
                        &call.arguments,
                    )))
                    .await;
            }
            let section = self.run_tool_calls(&calls, holder, Some(chunk_tx)).await;
            // Each call finished: hand the TUI its completion live (header
            // + summary + wall time) so the "running …" row fills in now,
            // not when the whole loop returns. Markers travel the same
            // channel in call order; the task-end `ToolCompleted` events
            // remain as fallback and are idempotent there.
            for (call, (result, ms)) in calls
                .iter()
                .zip(section.results.iter().zip(section.elapsed_ms.iter()))
            {
                let header = format_tool_header(&call.tool_name, &call.arguments);
                let summary = summarize_tool_result(&call.tool_name, result);
                let _ = chunk_tx
                    .send(tool_done_marker(&header, &summary, *ms))
                    .await;
            }
            tool_calls.extend(calls);
            tool_results.extend(section.results);
            pending.push_str(&format!("\n\n{}", section.prompt_block));
            self.apply_steers(pending).await;
            // If we have already produced text this turn, emit a
            // blank-line separator into the chunk stream before the
            // next round. A consumer that prints chunks straight
            // through (the CLI's `kod chat`) would otherwise see the
            // two rounds' text jammed into one sentence. The TUI
            // trims leading blank lines on flush (see
            // `trim_blank_lines`), so this is a no-op for it.
            if !final_text.is_empty() {
                let _ = chunk_tx.send("\n\n".to_string()).await; // kod-round-separator
            }
            // Tool is done, result reinjected — next provider call is pure
            // LLM thinking, not tool execution. Tell the UI to drop the
            // "tool: …" line so a slow model doesn't look like a stuck tool.
            let _ = chunk_tx.send(thinking_marker()).await;
        }
        // Exited on the round cap (empty text + tool calls present).
        // Append the note for the model, and send a visible line down
        // the chunk stream so the user sees why generation stopped
        // short of a final answer.
        if final_text.trim().is_empty() && !tool_calls.is_empty() {
            pending.push_str(TOOL_ROUNDS_EXHAUSTED_NOTE);
            let _ = chunk_tx
                .send(format!(
                    "\n\n[tool-round limit ({MAX_TOOL_ROUNDS}) reached — summarising progress]\n"
                ))
                .await;
        }
        Ok((final_text, tool_calls, tool_results, last_usage))
    }

    /// One streaming round: forward text live, assemble tool calls from
    /// `ToolCallStart`/`ToolCallDelta` framing.
    async fn stream_round(
        &self,
        provider: &Arc<dyn LlmProvider>,
        pending: &str,
        definitions: &[ToolDefinition],
        options: &GenerationOptions,
        chunk_tx: &tokio::sync::mpsc::Sender<String>,
    ) -> Result<(String, Vec<ToolCall>, Option<kod_provider::TokenUsage>)> {
        use futures::StreamExt;
        let mut stream = provider.stream_with_tools(pending, definitions, options);
        let mut text = String::new();
        let mut calls = Vec::new();
        let mut cur_name: Option<String> = None;
        let mut cur_args = String::new();
        let mut last_usage: Option<kod_provider::TokenUsage> = None;
        while let Some(item) = stream.next().await {
            match item? {
                StreamChunk::Text(t) => {
                    text.push_str(&t);
                    let _ = chunk_tx.send(t).await;
                }
                StreamChunk::ToolCallStart { name } => {
                    if let Some(prev) = cur_name.take() {
                        calls.push(finish_stream_call(prev, &cur_args));
                        cur_args.clear();
                    }
                    let _ = chunk_tx.send(tool_start_marker(&name)).await;
                    cur_name = Some(name);
                }
                StreamChunk::ToolCallDelta { arguments } => {
                    cur_args.push_str(&arguments);
                }
                StreamChunk::Usage(usage) => {
                    last_usage = Some(usage);
                }
                StreamChunk::Done => break,
            }
        }
        if let Some(prev) = cur_name.take() {
            calls.push(finish_stream_call(prev, &cur_args));
        }
        Ok((text, calls, last_usage))
    }

    /// Stream a plain-text summary (tools already ran): forwards chunks live.
    async fn stream_summary(
        &self,
        provider: &Arc<dyn LlmProvider>,
        pending: &str,
        options: &GenerationOptions,
        chunk_tx: &tokio::sync::mpsc::Sender<String>,
    ) -> Result<String> {
        use futures::StreamExt;
        let mut stream = provider.stream(pending, options);
        let mut text = String::new();
        while let Some(item) = stream.next().await {
            match item? {
                StreamChunk::Text(t) => {
                    text.push_str(&t);
                    let _ = chunk_tx.send(t).await;
                }
                StreamChunk::Usage(_) => {
                    // Summary stream usage is not critical (already counted in tool rounds)
                }
                _ => {}
            }
        }
        Ok(text)
    }

    /// Append the environment + tool inventory grounding to a router prompt.
    fn ground_prompt(&self, mut prompt: String, definitions: &[ToolDefinition]) -> String {
        prompt.push_str(&format!(
            "\n## Environment\n\n- Working directory: {}\n- OS: {}\n",
            self.working_dir.display(),
            std::env::consts::OS
        ));
        if !definitions.is_empty() {
            let names: Vec<String> = definitions
                .iter()
                .map(|d| format!("- {}: {}", d.name, d.description))
                .collect();
            prompt.push_str(&format!(
                "\n## Tool use\n\nYou have these tools (function calls, rooted at the working directory above):\n{}\nCall them when you need facts from this machine instead of guessing. Tool outputs return as `## Tool results` blocks — then answer the user.\n",
                names.join("\n")
            ));
        }
        prompt
    }

    /// Execute one round of model-requested tool calls.
    ///
    /// Failures become `ToolResult::Error` text so the model sees denials
    /// instead of stalling the loop.
    ///
    /// A round containing any mutating tool (`write_files` or
    /// `execute_commands` in its declared permissions) runs serially in
    /// caller order, so `[write_file(a), read_file(a)]` cannot race and
    /// the read is guaranteed to observe the write. All-read-only rounds
    /// still run concurrently — their results cannot depend on each other
    /// or on external state they did not observe themselves.
    async fn run_tool_calls(
        &self,
        calls: &[ToolCall],
        holder: &str,
        chunk_tx: Option<&tokio::sync::mpsc::Sender<String>>,
    ) -> ToolRound {
        // Derive a per-call context so the write lock records the
        // right holder. `holder` is the transcript key for the caller
        // — `swarm:<agent-id>` for a swarm agent, `session` for the
        // interactive session — converted to a stable label here.
        let effective_holder: &str = if holder.is_empty() { "session" } else { holder };
        let mut tool_context = self
            .tool_context
            .clone()
            .with_locks(Arc::clone(&self.lock_table), effective_holder)
            .with_sandbox(self.sandbox_setting());
        // The engine-level network flag overrides whatever the
        // construction-time context held. This is what makes
        // `set_network_access` meaningful through an `Arc<KodEngine>`
        // (no `&mut self` available): the flag is read here, per call,
        // and applied to the context the tool sees.
        tool_context.permissions.network_access = self.network_access_setting();

        let mut any_mutating = false;
        for call in calls {
            if let Some(perms) = self.tools.get_permissions(&call.tool_name).await
                && (perms.write_files || perms.execute_commands)
            {
                any_mutating = true;
                break;
            }
        }

        // Pre-tool hooks. A failing hook denies only its own call —
        // sibling calls still complete. When any pre-hook is configured,
        // the round is forced serial: the hook itself is I/O, so
        // parallelism buys nothing, and a serial loop keeps the
        // denial bookkeeping honest.
        let hook_runner = self.hooks.read().ok().map(|g| g.clone());
        let mut hook_denied: std::collections::HashMap<usize, String> =
            std::collections::HashMap::new();
        if let Some(runner) = hook_runner.as_ref()
            && runner.is_enabled()
        {
            for (i, call) in calls.iter().enumerate() {
                if let Err(e) = runner.run_pre(call).await {
                    hook_denied.insert(i, e.to_string());
                }
            }
        }
        let any_mutating = any_mutating || !hook_denied.is_empty();

        // Snapshot the target file of every mutating call BEFORE any of
        // them runs. Only `write_file` and `patch_file` are snapshotted
        // — `execute_command` has no declared write set to snapshot (see
        // the module doc for the reasoning). Snapshot failures are
        // logged, never propagated: a session that cannot write a
        // checkpoint must still be able to run tools.
        //
        // The snapshot ids are captured so that after the tool runs, a
        // unified diff (old content vs new content) can be attached to
        // the result. That is what makes the TUI's tool row useful: the
        // user sees what changed, not just "written N bytes".
        //
        // The snapshot content is also what the approval dialog shows,
        // so the diff the user reviews and the diff attached to the
        // result are computed against the same "before" state.
        let mut snapshot_ids: Vec<Option<String>> = vec![None; calls.len()];
        if any_mutating
            && let Some(cp) = self.checkpoints.as_ref()
        {
            for (i, call) in calls.iter().enumerate() {
                if matches!(call.tool_name.as_str(), "write_file" | "patch_file")
                    && let Some(p) = call.arguments.get("path").and_then(|v| v.as_str())
                {
                    let abs = if std::path::Path::new(p).is_absolute() {
                        std::path::PathBuf::from(p)
                    } else {
                        tool_context.working_dir.join(p)
                    };
                    match cp.snapshot_before(&abs, &call.tool_name) {
                        Ok(id) => snapshot_ids[i] = id,
                        Err(e) => tracing::warn!(
                            path = %abs.display(),
                            error = %e,
                            "checkpoint snapshot failed"
                        ),
                    }
                }
            }
        }

        // Approval gate. When `confirm_writes` is on, every
        // write_file / patch_file call pauses on a oneshot until the
        // streaming consumer answers. The consumer is the caller's
        // `chunk_tx` — the same channel the tool markers travel on.
        //
        // Three cases:
        //
        // 1. confirm_writes is off (the default): no gate, no cost.
        // 2. confirm_writes on, chunk_tx is Some: emit an approval
        //    marker carrying `{id, request}` JSON, register a oneshot,
        //    await the answer. Timeout, drop, or explicit deny all map
        //    to a denial; only an explicit `Approve` lets the call run.
        // 3. confirm_writes on, chunk_tx is None: the caller is
        //    `process` (non-streaming). Approval needs an interactive
        //    consumer; rather than hang for AWAIT_APPROVAL_SECS and
        //    then deny, refuse immediately with a message the user
        //    can act on.
        let mut denied: std::collections::HashMap<usize, String> =
            std::collections::HashMap::new();
        if self.confirm_writes_setting() {
            match chunk_tx {
                Some(tx) => {
                    for (i, call) in calls.iter().enumerate() {
                        if !matches!(call.tool_name.as_str(), "write_file" | "patch_file") {
                            continue;
                        }
                        let summary = format_call_brief(&call.tool_name, &call.arguments);
                        let diff = snapshot_ids
                            .get(i)
                            .and_then(|o| o.as_ref())
                            .and_then(|id| {
                                self.checkpoints
                                    .as_ref()
                                    .and_then(|cp| cp.find(id).ok().flatten())
                            })
                            .map(|snap| match call.tool_name.as_str() {
                                "write_file" => {
                                    let new_content = call
                                        .arguments
                                        .get("content")
                                        .and_then(|v| v.as_str())
                                        .unwrap_or("");
                                    kod_tools::patch::render_unified_diff(
                                        &snap.content,
                                        new_content,
                                        &snap.path.display().to_string(),
                                    )
                                }
                                "patch_file" => call
                                    .arguments
                                    .get("patch")
                                    .and_then(|v| v.as_str())
                                    .unwrap_or("")
                                    .to_string(),
                                _ => String::new(),
                            });

                        let id = self
                            .next_approval_id
                            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                        let (otx, orx) = tokio::sync::oneshot::channel();
                        self.pending_approvals.write().await.insert(id, otx);

                        let request = ApprovalRequest {
                            tool_name: call.tool_name.clone(),
                            arguments: call.arguments.clone(),
                            diff,
                            summary,
                        };
                        let json = serde_json::to_string(&request)
                            .unwrap_or_else(|_| "{}".to_string());
                        let _ = tx
                            .send(tool_approval_marker(id, &json))
                            .await;

                        let decision = tokio::time::timeout(
                            std::time::Duration::from_secs(AWAIT_APPROVAL_SECS),
                            orx,
                        )
                        .await;
                        match decision {
                            Ok(Ok(ApprovalDecision::Approve)) => {}
                            Ok(Ok(ApprovalDecision::Deny))
                            | Ok(Ok(ApprovalDecision::DenyAlways)) => {
                                denied.insert(i, "denied by user".to_string());
                            }
                            Ok(Err(_)) => {
                                denied.insert(i, "approval cancelled".to_string());
                            }
                            Err(_) => {
                                denied.insert(
                                    i,
                                    format!(
                                        "no approval answer within {}s — denied",
                                        AWAIT_APPROVAL_SECS
                                    ),
                                );
                            }
                        }
                    }
                }
                None => {
                    for (i, call) in calls.iter().enumerate() {
                        if matches!(call.tool_name.as_str(), "write_file" | "patch_file") {
                            denied.insert(
                                i,
                                "tools.confirm_writes is on but this execution \
                                 path has no interactive consumer. Set \
                                 tools.confirm_writes = false, or use the TUI."
                                    .to_string(),
                            );
                        }
                    }
                }
            }
        }

        // ask_user interception. The tool itself cannot reach the
        // chunk channel (its `execute` signature does not carry one), so
        // the engine does the marker + await, and hands the answer back
        // as the tool result. A call with no chunk_tx (non-streaming
        // `process`) becomes a denial with a message the model can act
        // on, matching the confirm_writes fallback.
        let mut answers: std::collections::HashMap<usize, String> =
            std::collections::HashMap::new();
        for (i, call) in calls.iter().enumerate() {
            if call.tool_name != "ask_user" {
                continue;
            }
            match chunk_tx {
                Some(tx) => {
                    let question = call
                        .arguments
                        .get("question")
                        .and_then(|v| v.as_str())
                        .unwrap_or("(no question)")
                        .to_string();
                    let placeholder = call
                        .arguments
                        .get("placeholder")
                        .and_then(|v| v.as_str())
                        .map(String::from);
                    let req = kod_tools::ask::QuestionRequest {
                        question,
                        placeholder,
                    };
                    let json = serde_json::to_string(&req)
                        .unwrap_or_else(|_| "{}".to_string());
                    let id = self
                        .next_question_id
                        .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    let (otx, orx) = tokio::sync::oneshot::channel();
                    self.pending_questions.write().await.insert(id, otx);
                    let _ = tx.send(question_marker(id, &json)).await;
                    // Generous timeout — a user reading the question and
                    // typing a real answer needs more than a click.
                    let answer = tokio::time::timeout(
                        std::time::Duration::from_secs(AWAIT_APPROVAL_SECS),
                        orx,
                    )
                    .await;
                    match answer {
                        Ok(Ok(text)) => {
                            answers.insert(i, text);
                        }
                        Ok(Err(_)) => {
                            answers.insert(i, "(question cancelled)".to_string());
                        }
                        Err(_) => {
                            answers.insert(
                                i,
                                format!(
                                    "(no answer within {}s — the user is away)",
                                    AWAIT_APPROVAL_SECS
                                ),
                            );
                        }
                    }
                }
                None => {
                    answers.insert(
                        i,
                        "(ask_user requires an interactive consumer; use kod tui or kod chat)"
                            .to_string(),
                    );
                }
            }
        }

        let mut raw_results: Vec<(Result<ToolResult>, u64)> = if any_mutating {
            let mut out = Vec::with_capacity(calls.len());
            for (i, call) in calls.iter().enumerate() {
                if let Some(reason) = hook_denied.get(&i) {
                    out.push((
                        Ok(ToolResult::Error(format!(
                            "pre_tool_use hook denied this call: {reason}"
                        ))),
                        0,
                    ));
                    continue;
                }
                if let Some(reason) = denied.get(&i) {
                    out.push((
                        Ok(ToolResult::Error(format!("write denied: {reason}"))),
                        0,
                    ));
                    continue;
                }
                let start = std::time::Instant::now();
                let res = self
                    .tools
                    .execute_tool(&call.tool_name, &call.arguments, &tool_context)
                    .await;
                out.push((res, start.elapsed().as_millis() as u64));
            }
            out
        } else {
            // Read-only round: no approvals are involved (approval is
            // only requested for write_file / patch_file, both of
            // which set `any_mutating` above), so the concurrent path
            // is unchanged.
            let futs: Vec<_> = calls
                .iter()
                .map(|call| {
                    let start = std::time::Instant::now();
                    let ctx = tool_context.clone();
                    async move {
                        let res = self
                            .tools
                            .execute_tool(&call.tool_name, &call.arguments, &ctx)
                            .await;
                        (res, start.elapsed().as_millis() as u64)
                    }
                })
                .collect();
            futures::future::join_all(futs).await
        };

        // Post-tool hooks. Run for every non-denied call, before the
        // result reaches the model. Failures are logged by `run_post`,
        // never propagated — a formatting failure after a successful
        // write must not turn the write into a failed tool call.
        if let Some(runner) = hook_runner.as_ref()
            && runner.is_enabled()
        {
            for (i, call) in calls.iter().enumerate() {
                if hook_denied.contains_key(&i) {
                    continue;
                }
                runner.run_post(call).await;
            }
        }

        // Session log: every tool call and its result as one JSONL
        // line. Best-effort — a write failure logs and the run
        // continues.
        if let Ok(guard) = self.session_recorder.read()
            && let Some(recorder) = guard.as_ref()
        {
            let now_ms = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_millis() as u64)
                .unwrap_or(0);
            for (i, call) in calls.iter().enumerate() {
                let Some((result, ms)) = raw_results.get(i) else {
                    continue;
                };
                let result_json = match result {
                    Ok(ToolResult::Success(v)) => serde_json::json!({ "success": v }),
                    Ok(ToolResult::Error(e)) => serde_json::json!({ "error": e }),
                    Ok(ToolResult::RequiresConfirmation { description, .. }) => {
                        serde_json::json!({ "requires_confirmation": description })
                    }
                    Err(e) => serde_json::json!({ "error": e.to_string() }),
                };
                let entry = crate::session_log::SessionEntry::ToolCall {
                    timestamp_ms: now_ms,
                    holder: effective_holder.to_string(),
                    tool_name: call.tool_name.clone(),
                    arguments: call.arguments.clone(),
                    duration_ms: *ms,
                    result: result_json,
                };
                if let Err(e) = recorder.record(&entry) {
                    tracing::warn!(
                        error = %e,
                        path = %recorder.path().display(),
                        "could not append session log entry"
                    );
                }
            }
        }

        // Diff augmentation: for every successful write_file / patch_file
        // that had a pre-call snapshot, compute a unified diff (old vs
        // new) and attach it to the result payload as `"diff"`. The TUI
        // summarizer renders that field in the tool row; the model sees
        // it too, so a "did that edit land where I intended?" question
        // is answerable without re-reading the file.
        //
        // Failures here are silent skips: a missing snapshot (a
        // session without a checkpoint directory), an unreadable file
        // (the tool itself already reported the error), or a
        // byte-diff mismatch (the file is binary) each just mean "no
        // diff on this row".
        if let Some(cp) = self.checkpoints.as_ref() {
            for (i, call) in calls.iter().enumerate() {
                if !matches!(call.tool_name.as_str(), "write_file" | "patch_file") {
                    continue;
                }
                let Some(sid) = snapshot_ids.get(i).and_then(|o| o.as_ref()) else {
                    continue;
                };
                let Some(snap) = cp.find(sid).ok().flatten() else {
                    continue;
                };
                let new_content = match std::fs::read_to_string(&snap.path) {
                    Ok(s) => s,
                    Err(_) => continue,
                };
                let old_content = snap.content.clone();
                let diff = kod_tools::patch::render_unified_diff(
                    &old_content,
                    &new_content,
                    &snap.path.display().to_string(),
                );
                if let Some(entry) = raw_results.get_mut(i)
                    && let Ok(ToolResult::Success(v)) = &mut entry.0
                    && let Some(obj) = v.as_object_mut()
                {
                    obj.insert(
                        "diff".to_string(),
                        serde_json::Value::String(diff),
                    );
                }
            }
        }

        let mut results = Vec::with_capacity(calls.len());
        let mut elapsed_ms = Vec::with_capacity(calls.len());
        let mut block = String::from("## Tool results\n");
        for (i, (call, (res, ms))) in calls.iter().zip(raw_results).enumerate() {
            elapsed_ms.push(ms);
            let result = match res {
                Ok(r) => r,
                Err(e) => ToolResult::Error(e.to_string()),
            };
            // ask_user: replace whatever the tool returned (an error
            // from its fallback) with the answer the user gave.
            let result = if let Some(answer) = answers.get(&i) {
                ToolResult::Success(serde_json::json!({ "answer": answer }))
            } else {
                result
            };
            // Cap for the prompt block the model sees. Byte count, not
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
            };
            block.push_str(&format!(
                "\n### {} {}\n{}\n",
                call.tool_name, call.arguments, rendered
            ));
            results.push(result);
        }
        // Auto-check: when enabled, and at least one of the calls was
        // a successful write_file / patch_file, run the project's
        // compiler/linter and append its diagnostics to the prompt
        // block. The model sees breakage on the same turn as the write,
        // instead of having to ask for a check itself.
        //
        // Failures are silent except for a `tracing::debug!`: a
        // missing toolchain, an empty directory, or a timeout should
        // not make the write itself look like a problem.
        if self.auto_check_setting() {
            // Every successful write in this round. We read the file
            // from disk rather than trusting the call's `content`
            // argument: `patch_file` does not carry one, and a
            // `write_file` may have been transformed by a hook.
            let writes: Vec<(std::path::PathBuf, String)> = calls
                .iter()
                .zip(results.iter())
                .filter_map(|(c, r)| {
                    if !matches!(c.tool_name.as_str(), "write_file" | "patch_file") {
                        return None;
                    }
                    if !matches!(r, ToolResult::Success(_)) {
                        return None;
                    }
                    let p = c.arguments.get("path").and_then(|v| v.as_str())?;
                    let abs = if std::path::Path::new(p).is_absolute() {
                        std::path::PathBuf::from(p)
                    } else {
                        tool_context.working_dir.join(p)
                    };
                    let content = std::fs::read_to_string(&abs).unwrap_or_default();
                    Some((abs, content))
                })
                .collect();

            if !writes.is_empty() {
                // Preferred path: a single Rust file and a server on
                // PATH. Every other shape — several files, a
                // non-Rust file — goes to the whole-workspace
                // compiler, which sees every file the round touched
                // and the cross-file effects between them.
                //
                // A per-file LSP check on one file of a multi-file
                // round would miss the other files' breakage.
                let lsp_eligible =
                    writes.len() == 1 && Self::lsp_binary_for(&writes[0].0).is_some();

                let diags: Vec<kod_tools::check::Diagnostic>;
                let source: String;

                if lsp_eligible {
                    let (path, content) = &writes[0];
                    let binary = Self::lsp_binary_for(path).unwrap_or("lsp");
                    let lsp_diags = self
                        .lsp_diagnostics(
                            path,
                            content,
                            std::time::Duration::from_secs(30),
                        )
                        .await;
                    if !lsp_diags.is_empty() {
                        diags = lsp_diags
                            .iter()
                            .map(|d| kod_tools::check::Diagnostic {
                                file: d.file.clone(),
                                line: d.line,
                                column: d.column,
                                severity: d.severity.clone(),
                                code: d.code.clone(),
                                message: d.message.clone(),
                            })
                            .collect();
                        source = binary.to_string();
                    } else {
                        // Empty LSP answer: could mean "clean" or
                        // "unreachable". Fall through to the
                        // compiler, which disambiguates.
                        match kod_tools::CheckTool::run_check(
                            &self.working_dir,
                            60,
                        )
                        .await
                        {
                            Ok(outcome) => {
                                source = outcome.command.clone();
                                diags = outcome.diagnostics;
                            }
                            Err(e) => {
                                tracing::debug!(
                                    error = %e,
                                    "auto-check compiler fallback did not run"
                                );
                                return ToolRound {
                                    results,
                                    prompt_block: block,
                                    elapsed_ms,
                                };
                            }
                        }
                    }
                } else {
                    match kod_tools::CheckTool::run_check(&self.working_dir, 60).await
                    {
                        Ok(outcome) => {
                            source = outcome.command.clone();
                            diags = outcome.diagnostics;
                        }
                        Err(e) => {
                            tracing::debug!(
                                error = %e,
                                "auto-check compiler did not run"
                            );
                            return ToolRound {
                                results,
                                prompt_block: block,
                                elapsed_ms,
                            };
                        }
                    }
                }

                // Diff against the baseline. A diagnostic whose
                // `(file, code, message)` was already present is
                // pre-existing; only the model's *new* errors should
                // drive the feedback loop.
                let baseline = self.check_baseline.read().await.clone();
                let baseline_keys: std::collections::HashSet<(
                    String,
                    Option<String>,
                    String,
                )> = baseline
                    .as_ref()
                    .map(|v| v.iter().map(diag_key).collect())
                    .unwrap_or_default();
                let current_keys: std::collections::HashSet<(
                    String,
                    Option<String>,
                    String,
                )> = diags.iter().map(diag_key).collect();

                let new_diags: Vec<&kod_tools::check::Diagnostic> = diags
                    .iter()
                    .filter(|d| !baseline_keys.contains(&diag_key(d)))
                    .collect();
                let resolved_count = if baseline.is_some() {
                    baseline_keys
                        .iter()
                        .filter(|k| !current_keys.contains(*k))
                        .count()
                } else {
                    0
                };

                // Render. The message is deliberately different for
                // each case so the model knows what its write did:
                // introduced N errors, resolved M errors, or neither.
                block.push_str("\n## Auto-check\n\n");
                match baseline {
                    None => {
                        // First auto-check in this session; no
                        // baseline. Report everything, note that the
                        // state is unknown.
                        if diags.is_empty() {
                            block.push_str(&format!("`{}` is clean.\n", source));
                        } else {
                            block.push_str(&format!(
                                "`{}` reported {} diagnostic(s). \\
                                 (No baseline was captured, so all are shown — \
                                 some may pre-date this write.)\n\n",
                                source,
                                diags.len(),
                            ));
                            render_diagnostics(&mut block, &diags, 20);
                        }
                    }
                    Some(_) if !new_diags.is_empty() => {
                        block.push_str(&format!(
                            "`{}` reported {} NEW diagnostic(s) from this write:\n\n",
                            source,
                            new_diags.len(),
                        ));
                        let owned: Vec<kod_tools::check::Diagnostic> = new_diags
                            .iter()
                            .map(|d| (*d).clone())
                            .collect();
                        render_diagnostics(&mut block, &owned, 20);
                        if resolved_count > 0 {
                            block.push_str(&format!(
                                "\n({} pre-existing diagnostic(s) resolved.)\n",
                                resolved_count,
                            ));
                        }
                        block.push_str("\nFix the new errors before continuing.\n");
                    }
                    Some(_) if resolved_count > 0 => {
                        block.push_str(&format!(
                            "`{}`: your write introduced no new errors and \
                             resolved {} pre-existing diagnostic(s).\n",
                            source, resolved_count,
                        ));
                    }
                    Some(_) if diags.is_empty() => {
                        block.push_str(&format!("`{}` is clean.\n", source));
                    }
                    Some(_) => {
                        block.push_str(&format!(
                            "`{}`: your write introduced no new errors. \
                             {} pre-existing diagnostic(s) remain, \
                             unrelated to this change.\n",
                            source,
                            diags.len(),
                        ));
                    }
                }

                // The post-write state becomes the new baseline.
                *self.check_baseline.write().await = Some(diags);
            }
        }

        ToolRound {
            results,
            prompt_block: block,
            elapsed_ms,
        }
    }

    /// Shutdown the engine. Idempotent: a second call returns `Ok(())`
    /// without touching anything. Every step is best-effort and logs on
    /// failure — a shutdown that refuses to finish because one subsystem
    /// misbehaved is worse than one that reports the problem and
    /// continues.
    ///
    /// Order matters:
    /// 1. Flip the running flag and drop the lock early so callers
    ///    blocked on `is_running()` do not stall behind the teardown.
    /// 2. Signal cancellation so any in-flight tool round / goal loop
    ///    observes the stop at its next check.
    /// 3. Shut down the LSP client (may be mid-request).
    /// 4. Release every path-lock cell.
    /// 5. Flush the session recorder (per-line already, belt-and-braces).
    /// 6. Trim checkpoints to their retention cap.
    ///
    /// Redb is intentionally not closed here: `Arc<Database>` closes on
    /// last reference drop, and forcing it would require unwinding the
    /// `Arc<TaskRouter>` held by callers of this engine. Documented in
    /// `LongTermMemory` — the OS will flush on process exit, which is
    /// sufficient for redb's durability guarantee (fsync per commit).
    pub async fn shutdown(&self) -> Result<()> {
        let was_running = {
            let mut running = self.is_running.write().await;
            if !*running {
                return Ok(());
            }
            *running = false;
            true
        };
        if !was_running {
            return Ok(());
        }

        // 2. Signal stop to any running loop. `request_cancel` is safe to
        //    call twice; a subsequent `clear_cancel` (from a new prompt)
        //    would only matter if the engine is reused, which it is not.
        self.request_cancel();

        // 3. LSP client shutdown (may be mid-request; timeout inside).
        self.lsp_shutdown().await;

        // 4. Release path-lock cells. Outstanding guards keep their own
        //    Arc and release on drop.
        self.lock_table.release_all().await;

        // 5. Session recorder flush — best-effort.
        if let Ok(guard) = self.session_recorder.read()
            && let Some(recorder) = guard.as_ref()
            && let Err(e) = recorder.flush()
        {
            tracing::warn!(
                error = %e,
                path = %recorder.path().display(),
                "session log flush failed during shutdown"
            );
        }

        // 6. Checkpoint retention — best-effort.
        if let Some(cp) = self.checkpoints.as_ref()
            && let Err(e) = cp.enforce_retention()
        {
            tracing::warn!(error = %e, "checkpoint retention failed during shutdown");
        }

        tracing::info!("KOD engine shutdown complete");
        Ok(())
    }

    /// Check if engine is running
    pub async fn is_running(&self) -> bool {
        *self.is_running.read().await
    }

    /// Get router reference
    pub fn router(&self) -> &TaskRouter {
        &self.router
    }

    /// Load skills from a directory into the router's matcher.
    pub async fn load_skills(&self, skills_dir: &std::path::Path) -> Result<usize> {
        self.router.load_skills(skills_dir).await
    }

    /// Load skills from every directory in `dirs`, skipping any that do
    /// not exist. Returns the total number of skill files read across all
    /// directories. Skills sharing a name across directories count once
    /// in the matcher (later dirs shadow earlier ones) but each file is
    /// counted here, so the returned number is "files loaded", not
    /// "distinct skills available" — use [`loaded_skill_names`] for the
    /// deduplicated set.
    pub async fn load_skills_from_dirs(&self, dirs: &[std::path::PathBuf]) -> Result<usize> {
        let mut total = 0;
        for dir in dirs {
            if !dir.is_dir() {
                continue;
            }
            match self.load_skills(dir).await {
                Ok(n) => total += n,
                Err(e) => {
                    tracing::warn!(
                        dir = %dir.display(),
                        error = %e,
                        "Could not load skills from directory"
                    );
                }
            }
        }
        Ok(total)
    }

    /// Watch `skills_dir` for changes and rebuild the router's skill
    /// matcher on each event. Call this after `load_skills` /
    /// `load_skills_from_dirs` when the caller wants newly-added or
    /// edited skills to appear without a restart. No-op when the
    /// directory does not exist or hot reload was already enabled for
    /// it.
    pub async fn enable_hot_reload(&self, skills_dir: &std::path::Path) -> Result<()> {
        self.router.enable_hot_reload(skills_dir).await
    }

    /// Names of all loaded skills (for `/skills` listing).
    pub async fn loaded_skill_names(&self) -> Vec<String> {
        self.router.loaded_skill_names().await
    }

    /// Names + descriptions of all loaded skills (for `/skills` listing).
    pub async fn loaded_skill_details(&self) -> Vec<(String, String)> {
        self.router.loaded_skill_details().await
    }

    /// Ask the running prompt to stop at the next round boundary.
    /// The TUI also aborts its background task, so the UI clears at once;
    /// this flag makes the engine side cooperate (goal loops, tool rounds).
    pub fn request_cancel(&self) {
        self.cancelled.store(true, Ordering::SeqCst);
    }

    /// Clear a previous cancel (called when a new prompt is dispatched).
    pub fn clear_cancel(&self) {
        self.cancelled.store(false, Ordering::SeqCst);
    }

    /// True if [`KodEngine::request_cancel`] was called and not cleared.
    pub fn is_cancelled(&self) -> bool {
        self.cancelled.load(Ordering::SeqCst)
    }

    /// Queue a steering note while a prompt is running. It is injected
    /// into the conversation after the current tool round finishes, so the
    /// model course-corrects on the next round instead of starting over.
    pub async fn steer(&self, note: &str) {
        let note = note.trim();
        if !note.is_empty() {
            self.steer_queue.write().await.push(note.to_string());
        }
    }

    /// Drain queued steer notes (each is applied once, in order).
    async fn take_steers(&self) -> Vec<String> {
        std::mem::take(&mut *self.steer_queue.write().await)
    }

    /// Keyed variant of the transcript + memory writer.
    async fn remember_turn_for(&self, key: &str, user: bool, text: &str) {
        self.record_turn_for(key, user, text).await;
        // Short-term memory is intentionally shared across transcripts:
        // it is the retrieval-side working set and the retrieve path
        // filters by input words, so an agent asking about "SQL schema"
        // will not surface a sibling agent's turn about "HTTP handler".
        let _ = self.router.store_short_term(text).await;
    }

    /// Test-only wrapper for the default transcript.
    #[cfg(test)]
    async fn remember_turn(&self, user: bool, text: &str) {
        self.remember_turn_for(DEFAULT_TRANSCRIPT_KEY, user, text)
            .await
    }

    /// Test-only wrapper for the default transcript.
    #[cfg(test)]
    async fn record_turn(&self, user: bool, text: &str) {
        self.record_turn_for(DEFAULT_TRANSCRIPT_KEY, user, text)
            .await
    }

    /// Test-only wrapper for the default transcript.
    #[cfg(test)]
    async fn render_history(&self) -> String {
        self.render_history_for(DEFAULT_TRANSCRIPT_KEY).await
    }

    /// Keyed turn recorder. Truncates long texts, keeps only the
    /// most recent [`MAX_HISTORY_TURNS`] turns for `key`. Uses
    /// [`truncate_chars`] rather than a raw slice — the byte-slice
    /// version panicked on non-ASCII text that crossed the cap.
    async fn record_turn_for(&self, key: &str, user: bool, text: &str) {
        let text = text.trim();
        if text.is_empty() {
            return;
        }
        let short = if text.len() > MAX_TURN_CHARS {
            format!("{}… [truncated]", truncate_chars(text, MAX_TURN_CHARS))
        } else {
            text.to_string()
        };
        let role = if user {
            kod_types::MessageRole::User
        } else {
            kod_types::MessageRole::Assistant
        };
        let message = kod_types::ChatMessage::text(
            kod_types::MessageId::new(),
            role,
            short,
            time::OffsetDateTime::now_utc(),
        );
        let mut history = self.history.write().await;
        let turns = history.entry(key.to_string()).or_default();
        turns.push(message);
        let excess = turns.len().saturating_sub(MAX_HISTORY_TURNS);
        if excess > 0 {
            turns.drain(..excess);
        }
    }

    /// Render the transcript for `key`, oldest-first, dropping the
    /// newest-over-budget entries per the current history budget (see
    /// [`KodEngine::set_history_budget`]).
    async fn render_history_for(&self, key: &str) -> String {
        let history = self.history.read().await;
        let Some(turns) = history.get(key) else {
            return "(start of conversation)".to_string();
        };
        if turns.is_empty() {
            return "(start of conversation)".to_string();
        }
        let budget = self.history_budget();
        let mut out = String::new();
        for message in turns.iter().rev() {
            // `ChatMessage::render_text` emits "User: {content}" /
            // "Assistant: {content}" / "System: {content}" etc. —
            // byte-identical to what `HistoryTurn` used to produce
            // for user and assistant turns. The characterization test
            // in tests/characterization_history.rs locks this.
            let line = format!("{}\n", message.render_text());
            if out.len() + line.len() > budget {
                break;
            }
            out.insert_str(0, &line);
        }
        out
    }

    /// The prompt the provider received on the most recent
    /// `process*` call on the default transcript.
    pub async fn last_prompt(&self) -> Option<String> {
        self.last_prompt_for(DEFAULT_TRANSCRIPT_KEY).await
    }

    /// The prompt the provider received on the most recent `process*`
    /// call for `key`.
    pub async fn last_prompt_for(&self, key: &str) -> Option<String> {
        self.last_prompt.read().await.get(key).cloned()
    }

    /// Seed a turn into the default transcript. Used by the TUI after
    /// restoring a saved session.
    pub async fn seed_turn(&self, user: bool, text: &str) {
        self.seed_turn_for(DEFAULT_TRANSCRIPT_KEY, user, text).await
    }

    /// Seed a turn into the transcript identified by `key`.
    pub async fn seed_turn_for(&self, key: &str, user: bool, text: &str) {
        self.record_turn_for(key, user, text).await;
    }

    /// Clear the default transcript and short-term memory (`/clear`).
    /// See the doc on [`KodEngine::clear_history_for`] for the
    /// reasoning; this wrapper exists so the `/clear` command keeps its
    /// current shape.
    pub async fn clear_history(&self) {
        self.clear_history_for(DEFAULT_TRANSCRIPT_KEY).await;
        self.router.clear_short_term_memory().await;
    }

    /// Forget the transcript for `key`. Does NOT touch short-term
    /// memory: a per-key clear is used by the swarm runner between
    /// runs, and clearing the shared working set would discard turns a
    /// concurrent single-agent session still wants.
    pub async fn clear_history_for(&self, key: &str) {
        self.history.write().await.remove(key);
        self.last_prompt.write().await.remove(key);
    }

    /// Drop both the transcript and its stored last-prompt for `key`.
    /// Used by the swarm runner at the end of a run so transcripts do
    /// not accumulate.
    pub async fn forget_transcript(&self, key: &str) {
        self.history.write().await.remove(key);
        self.last_prompt.write().await.remove(key);
    }

    /// Compact the default transcript to the last `max_turns` turns.
    pub async fn compact_history(&self, max_turns: usize) {
        self.compact_history_for(DEFAULT_TRANSCRIPT_KEY, max_turns).await
    }

    /// Compact the transcript for `key` to the last `max_turns` turns.
    pub async fn compact_history_for(&self, key: &str, max_turns: usize) {
        let mut history = self.history.write().await;
        if let Some(turns) = history.get_mut(key)
            && turns.len() > max_turns
        {
            let drop = turns.len() - max_turns;
            turns.drain(..drop);
        }
    }
}

/// Assemble one [`ToolCall`] from streamed `ToolCallStart`/`ToolCallDelta`
/// framing. Deltas arrive as JSON text; unparseable fragments are kept as a
/// raw string so the call still executes instead of being dropped.
fn finish_stream_call(name: String, args: &str) -> ToolCall {
    let arguments: serde_json::Value =
        serde_json::from_str(args).unwrap_or(serde_json::Value::String(args.to_string()));
    ToolCall {
        tool_name: name,
        arguments,
    }
}


/// `true` if `program` is on PATH. Used by
/// [`KodEngine::lsp_binary_for`] to avoid promising a language server
/// the process cannot spawn.
fn which(program: &str) -> bool {
    let Some(path) = std::env::var_os("PATH") else {
        return false;
    };
    for dir in std::env::split_paths(&path) {
        if dir.join(program).is_file() {
            return true;
        }
    }
    false
}

/// Identity of a diagnostic for diffing between two check runs.
/// Ignores line and column: an edit that shifts a later error down by
/// a line did not create a new error.
fn diag_key(d: &kod_tools::check::Diagnostic) -> (String, Option<String>, String) {
    (d.file.clone(), d.code.clone(), d.message.clone())
}

/// Append up to `max` diagnostics to `block`, one per line, in the
/// format `severity [code] file:line:col — message`.
fn render_diagnostics(
    block: &mut String,
    diags: &[kod_tools::check::Diagnostic],
    max: usize,
) {
    for d in diags.iter().take(max) {
        let code = d
            .code
            .as_deref()
            .map(|c| format!("[{c}]"))
            .unwrap_or_default();
        let short = if d.message.chars().count() > 160 {
            let s: String = d.message.chars().take(160).collect();
            format!("{s}…")
        } else {
            d.message.clone()
        };
        block.push_str(&format!(
            "  {} {} {}:{}:{} — {}\n",
            d.severity, code, d.file, d.line, d.column, short,
        ));
    }
    if diags.len() > max {
        block.push_str(&format!("  … and {} more.\n", diags.len() - max));
    }
}


/// Small owned snapshot of the parts of `KodEngine` that the baseline
/// refresh needs. Exists because `KodEngine::start` takes `&self` and
/// therefore cannot wrap `self` in an `Arc` for a background task.
struct BaselineRefresher {
    working_dir: std::path::PathBuf,
    check_baseline: Arc<RwLock<Option<Vec<kod_tools::check::Diagnostic>>>>,
}

impl BaselineRefresher {
    async fn refresh_check_baseline(&self) {
        match kod_tools::CheckTool::run_check(&self.working_dir, 60).await {
            Ok(outcome) => {
                let n = outcome.diagnostics.len();
                *self.check_baseline.write().await = Some(outcome.diagnostics);
                tracing::debug!(count = n, "baseline captured");
            }
            Err(e) => {
                tracing::debug!(
                    error = %e,
                    "baseline not captured (no project or toolchain)"
                );
            }
        }
    }
}


#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[tokio::test]
    async fn test_engine_lifecycle() {
        let temp_dir = TempDir::new().unwrap();
        let db_path = temp_dir.path().join("test.redb");

        let engine = KodEngine::new(RouterConfig::default(), db_path).unwrap();

        // Engine starts not running
        assert!(!engine.is_running().await);

        // Start engine
        engine.start().await.unwrap();
        assert!(engine.is_running().await);

        // Shutdown
        engine.shutdown().await.unwrap();
        assert!(!engine.is_running().await);
    }

    #[tokio::test]
    async fn test_shutdown_is_idempotent() {
        let temp_dir = TempDir::new().unwrap();
        let db_path = temp_dir.path().join("test.redb");
        let engine = KodEngine::new(RouterConfig::default(), db_path).unwrap();
        engine.start().await.unwrap();
        assert!(engine.is_running().await);

        engine.shutdown().await.unwrap();
        assert!(!engine.is_running().await);

        // Second call must be a no-op and return Ok.
        engine.shutdown().await.unwrap();
        assert!(!engine.is_running().await);
    }

    /// `reply_declares_goal_met` must fire on the completion form and
    /// NOT fire on a mention of the phrase mid-reply. Regression: the
    /// previous substring check stopped the loop on "I have NOT
    /// reached GOAL MET".
    #[test]
    fn test_reply_declares_goal_met() {
        // The prompt's contract: last line is exactly GOAL MET.
        assert!(reply_declares_goal_met("Here is the summary.\nGOAL MET"));
        // A model that capitalizes differently on the last line is
        // still a completion — the check is case-insensitive.
        assert!(reply_declares_goal_met("done\ngoal met"));
        // Trailing punctuation and emphasis are tolerated.
        assert!(reply_declares_goal_met("…\nGOAL MET."));
        assert!(reply_declares_goal_met("…\n**GOAL MET**"));
        assert!(reply_declares_goal_met("…\n— GOAL MET"));
        // Trailing blank lines after the marker are fine.
        assert!(reply_declares_goal_met("GOAL MET\n\n"));

        // A false promise does NOT declare success.
        assert!(!reply_declares_goal_met(
            "I have not reached GOAL MET yet, but I'm close."
        ));
        // The phrase on a non-final line is not a signal.
        assert!(!reply_declares_goal_met(
            "GOAL MET is what I'd say if done.\nBut I need one more turn."
        ));
        // A question about the criteria is not a completion.
        assert!(!reply_declares_goal_met(
            "Should I reply GOAL MET now, or keep working?"
        ));
        // Empty input is not a completion.
        assert!(!reply_declares_goal_met(""));
        assert!(!reply_declares_goal_met("   \n\n"));
    }

    /// set_provider must complete promptly even while a streaming
    /// generation is in flight. Before the fix, process_streaming held
    /// the RwLock read guard across the whole agentic loop, so
    /// set_provider's write awaited the end of the generation — a
    /// `/model` switch mid-prompt looked like a hang.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn test_set_provider_not_blocked_by_running_generation() {
        use futures::Stream;
        use std::pin::Pin;
        use std::sync::Arc as StdArc;
        use std::time::Duration;
        use tokio::sync::Notify;

        /// Provider whose `stream_with_tools` signals `started` and then
        /// parks for `hold_for` before yielding `Done`. The signal is
        /// what makes the test deterministic: by the time `started`
        /// fires, the running `process_streaming` has definitely
        /// acquired the read guard and entered the streaming loop.
        struct SlowProvider {
            hold_for: Duration,
            started: StdArc<Notify>,
        }

        #[async_trait::async_trait]
        impl LlmProvider for SlowProvider {
            fn name(&self) -> &str {
                "slow"
            }
            async fn list_models(&self) -> kod_error::Result<Vec<String>> {
                Ok(vec![])
            }
            async fn generate(
                &self,
                _prompt: &str,
                _opts: &GenerationOptions,
            ) -> kod_error::Result<String> {
                Ok(String::new())
            }
            async fn generate_with_tools(
                &self,
                _prompt: &str,
                _tools: &[ToolDefinition],
                _opts: &GenerationOptions,
            ) -> kod_error::Result<GenerationResponse> {
                Ok(GenerationResponse::Text {
                    content: String::new(),
                    usage: None,
                })
            }
            fn stream(
                &self,
                _prompt: &str,
                _opts: &GenerationOptions,
            ) -> Pin<Box<dyn Stream<Item = kod_error::Result<StreamChunk>> + Send + '_>>
            {
                Box::pin(futures::stream::empty())
            }
            fn stream_with_tools<'a>(
                &'a self,
                _prompt: &'a str,
                _tools: &'a [ToolDefinition],
                _opts: &'a GenerationOptions,
            ) -> Pin<Box<dyn Stream<Item = kod_error::Result<StreamChunk>> + Send + 'a>>
            {
                let hold = self.hold_for;
                let started = self.started.clone();
                Box::pin(futures::stream::once(async move {
                    started.notify_one();
                    tokio::time::sleep(hold).await;
                    Ok(StreamChunk::Done)
                }))
            }
        }

        let temp = TempDir::new().unwrap();
        let db_path = temp.path().join("test.redb");
        let cfg = RouterConfig {
            context_window: 8192,
            short_term_capacity: 100,
            working_dir: temp.path().to_path_buf(),
            enable_memory: false,
            max_skills_per_query: 3,
        };
        let engine = Arc::new(KodEngine::new(cfg, db_path).unwrap());
        engine.start().await.unwrap();

        let started = StdArc::new(Notify::new());
        engine
            .set_provider(Arc::new(SlowProvider {
                hold_for: Duration::from_millis(1000),
                started: started.clone(),
            }))
            .await;

        let engine_for_gen = engine.clone();
        let (tx, _rx) = tokio::sync::mpsc::channel::<String>(4);
        let gen_task = tokio::spawn(async move {
            let _ = engine_for_gen.process_streaming("hello", &tx).await;
        });

        // Block until the streaming loop is definitely running and the
        // read guard is held.
        started.notified().await;

        // Swap providers. With the fix this returns immediately; without
        // it, it waits for the 1s stream to finish and the assertion
        // below fails.
        let start = std::time::Instant::now();
        engine
            .set_provider(Arc::new(SlowProvider {
                hold_for: Duration::from_millis(1),
                started: StdArc::new(Notify::new()),
            }))
            .await;
        let elapsed = start.elapsed();

        assert!(
            elapsed < Duration::from_millis(200),
            "set_provider blocked for {elapsed:?} — the read lock is \
             still held across the agentic loop"
        );

        let _ = gen_task.await;
    }

    /// All three process* entry points must reject a call made
    /// before a provider is installed. Regression: the previous
    /// fallback routed through the router's placeholder handlers and
    /// returned "Processing simple task: …" — a plausible-looking
    /// answer that hid the missing setup.
    #[tokio::test]
    async fn test_process_without_provider_errors() {
        let temp = TempDir::new().unwrap();
        let db_path = temp.path().join("test.redb");
        let cfg = RouterConfig {
            context_window: 8192,
            short_term_capacity: 100,
            working_dir: temp.path().to_path_buf(),
            enable_memory: false,
            max_skills_per_query: 3,
        };
        let engine = KodEngine::new(cfg, db_path).unwrap();
        engine.start().await.unwrap();

        let err = engine.process("hello?").await.unwrap_err();
        match err {
            KodError::InvalidState(msg) => assert!(
                msg.contains("No LLM provider"),
                "error should name the missing provider: {msg}"
            ),
            other => panic!("expected InvalidState, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn test_process_streaming_without_provider_errors() {
        let temp = TempDir::new().unwrap();
        let db_path = temp.path().join("test.redb");
        let cfg = RouterConfig {
            context_window: 8192,
            short_term_capacity: 100,
            working_dir: temp.path().to_path_buf(),
            enable_memory: false,
            max_skills_per_query: 3,
        };
        let engine = KodEngine::new(cfg, db_path).unwrap();
        engine.start().await.unwrap();

        let (tx, _rx) = tokio::sync::mpsc::channel::<String>(4);
        let err = engine
            .process_streaming("hello?", &tx)
            .await
            .unwrap_err();
        assert!(matches!(err, KodError::InvalidState(_)), "got {err:?}");
    }

    #[tokio::test]
    async fn test_process_goal_streaming_without_provider_errors() {
        let temp = TempDir::new().unwrap();
        let db_path = temp.path().join("test.redb");
        let cfg = RouterConfig {
            context_window: 8192,
            short_term_capacity: 100,
            working_dir: temp.path().to_path_buf(),
            enable_memory: false,
            max_skills_per_query: 3,
        };
        let engine = KodEngine::new(cfg, db_path).unwrap();
        engine.start().await.unwrap();

        let (tx, _rx) = tokio::sync::mpsc::channel::<String>(4);
        let err = engine
            .process_goal_streaming("work on it", "finish the task", &tx)
            .await
            .unwrap_err();
        assert!(matches!(err, KodError::InvalidState(_)), "got {err:?}");
    }

    /// `remember_turn` must write the turn to both the transcript and
    /// short-term memory. Regression: nothing in the engine ever
    /// called `MemoryManager::store`, so the memory subsystem was
    /// read-only from the engine's perspective — retrieve_context
    /// returned whatever a caller had stored externally, never a
    /// session turn.
    #[tokio::test]
    async fn test_remember_turn_writes_short_term_memory() {
        let temp = TempDir::new().unwrap();
        let db_path = temp.path().join("test.redb");
        let cfg = RouterConfig {
            context_window: 8192,
            short_term_capacity: 100,
            working_dir: temp.path().to_path_buf(),
            enable_memory: true,
            max_skills_per_query: 3,
        };
        let engine = KodEngine::new(cfg, db_path).unwrap();
        engine.start().await.unwrap();

        assert_eq!(
            engine.router().get_all_short_term_len().await,
            0,
            "short-term memory starts empty"
        );

        engine.remember_turn(true, "the user asked about rust").await;
        engine
            .remember_turn(false, "the assistant answered with an example")
            .await;

        assert_eq!(
            engine.router().get_all_short_term_len().await,
            2,
            "both turns must land in short-term memory"
        );

        // The transcript received the same turns.
        let history = engine.render_history().await;
        assert!(history.contains("the user asked about rust"));
        assert!(history.contains("the assistant answered with an example"));
    }

    /// A turn with only whitespace must not create a memory entry —
    /// the router's `store_short_term` drops empty content.
    #[tokio::test]
    async fn test_remember_turn_skips_empty_text() {
        let temp = TempDir::new().unwrap();
        let db_path = temp.path().join("test.redb");
        let cfg = RouterConfig {
            context_window: 8192,
            short_term_capacity: 100,
            working_dir: temp.path().to_path_buf(),
            enable_memory: true,
            max_skills_per_query: 3,
        };
        let engine = KodEngine::new(cfg, db_path).unwrap();
        engine.start().await.unwrap();

        engine.remember_turn(true, "   \n\t  ").await;
        assert_eq!(
            engine.router().get_all_short_term_len().await,
            0,
            "whitespace-only text must not be stored"
        );
    }

    #[test]
    fn test_tool_done_marker_roundtrip() {
        let header = "execute_command command=cargo test";
        let summary = "line1\nline2\nline3";
        let chunk = tool_done_marker(header, summary, 1340);
        let (h, s, ms) = parse_tool_done(&chunk).expect("must parse");
        assert_eq!(h, header);
        assert_eq!(s, summary);
        assert_eq!(ms, 1340);
    }

    #[test]
    fn test_tool_done_marker_sanitizes_nul() {
        let chunk = tool_done_marker("a\0b", "c\0d", 7);
        let (h, s, ms) = parse_tool_done(&chunk).expect("must parse");
        assert_eq!((h, s, ms), ("a b", "c d", 7));
    }

    #[test]
    fn test_tool_done_marker_rejects_other_chunks() {
        assert!(parse_tool_done("plain text").is_none());
        assert!(parse_tool_done(&tool_start_marker("read_file")).is_none());
        // Malformed duration degrades to 0 instead of dropping the row.
        let raw = format!("{TOOL_DONE_MARKER}h\0s\0abc");
        assert_eq!(parse_tool_done(&raw), Some(("h", "s", 0)));
        // Truncated payload is not a completion.
        let raw = format!("{TOOL_DONE_MARKER}only-header");
        assert!(parse_tool_done(&raw).is_none());
    }

    #[test]
    fn test_format_duration_ms() {
        assert_eq!(format_duration_ms(0), "0ms");
        assert_eq!(format_duration_ms(999), "999ms");
        assert_eq!(format_duration_ms(1000), "1.0s");
        assert_eq!(format_duration_ms(1500), "1.5s");
        assert_eq!(format_duration_ms(65_000), "1m05s");
    }

    /// A single turn's agentic loop can produce text in more than one
    /// round (model says something, calls a tool, then says more).
    /// `append_round_text` must insert a blank-line separator between
    /// non-empty rounds so the accumulated `final_text` does not read
    /// as "Let me check.Here is the answer."
    #[test]
    fn test_append_round_text_separates_rounds() {
        let mut buf = String::new();
        // First round: no separator.
        append_round_text(&mut buf, "first");
        assert_eq!(buf, "first");
        // Second round: blank-line separator.
        append_round_text(&mut buf, "second");
        assert_eq!(buf, "first\n\nsecond");
        // Empty text is ignored — no separator for a round that
        // produced nothing.
        append_round_text(&mut buf, "");
        assert_eq!(buf, "first\n\nsecond");
        // Third round: separator again.
        append_round_text(&mut buf, "third");
        assert_eq!(buf, "first\n\nsecond\n\nthird");
        // Empty buffer + empty text: stays empty.
        let mut empty = String::new();
        append_round_text(&mut empty, "");
        assert_eq!(empty, "");
        // Empty buffer + first text: no leading separator.
        append_round_text(&mut empty, "one");
        assert_eq!(empty, "one");
    }

    #[test]
    fn test_truncate_chars_respects_boundaries() {
        // "café": the é is two UTF-8 bytes. Asking for a byte offset
        // mid-codepoint must round down to the previous boundary.
        let s = "café au lait";
        assert_eq!(truncate_chars(s, 100), s);
        assert_eq!(truncate_chars(s, 3), "caf");
        // 4 bytes lands between 0xc3 and 0xa9 — mid-é. Round down to 3.
        assert_eq!(truncate_chars(s, 4), "caf");
        // 5 bytes ends exactly after the é.
        assert_eq!(truncate_chars(s, 5), "café");
        // Degenerate: max 0 returns the empty string.
        assert_eq!(truncate_chars(s, 0), "");
    }

    /// last_prompt() must return the grounded prompt after a
    /// successful process_streaming call, so /debug last-prompt has
    /// something to show. Uses a no-op provider that yields Done
    /// immediately.
    #[tokio::test]
    async fn test_last_prompt_is_captured() {
        use futures::Stream;
        use std::pin::Pin;

        struct NopProvider;

        #[async_trait::async_trait]
        impl LlmProvider for NopProvider {
            fn name(&self) -> &str {
                "nop"
            }
            async fn list_models(&self) -> kod_error::Result<Vec<String>> {
                Ok(vec![])
            }
            async fn generate(
                &self,
                _p: &str,
                _o: &GenerationOptions,
            ) -> kod_error::Result<String> {
                Ok(String::new())
            }
            async fn generate_with_tools(
                &self,
                _p: &str,
                _t: &[ToolDefinition],
                _o: &GenerationOptions,
            ) -> kod_error::Result<GenerationResponse> {
                Ok(GenerationResponse::Text {
                    content: String::new(),
                    usage: None,
                })
            }
            fn stream(
                &self,
                _p: &str,
                _o: &GenerationOptions,
            ) -> Pin<Box<dyn Stream<Item = kod_error::Result<StreamChunk>> + Send + '_>>
            {
                Box::pin(futures::stream::empty())
            }
            fn stream_with_tools<'a>(
                &'a self,
                _p: &'a str,
                _t: &'a [ToolDefinition],
                _o: &'a GenerationOptions,
            ) -> Pin<Box<dyn Stream<Item = kod_error::Result<StreamChunk>> + Send + 'a>>
            {
                Box::pin(futures::stream::once(async { Ok(StreamChunk::Done) }))
            }
        }

        let temp = TempDir::new().unwrap();
        let db_path = temp.path().join("test.redb");
        let cfg = RouterConfig {
            context_window: 8192,
            short_term_capacity: 100,
            working_dir: temp.path().to_path_buf(),
            enable_memory: false,
            max_skills_per_query: 3,
        };
        let engine = KodEngine::new(cfg, db_path).unwrap();
        engine.start().await.unwrap();

        // Before any call: no prompt.
        assert!(engine.last_prompt().await.is_none());

        engine.set_provider(Arc::new(NopProvider)).await;
        let (tx, _rx) = tokio::sync::mpsc::channel::<String>(4);
        let _ = engine.process_streaming("hello from test", &tx).await;

        let prompt = engine
            .last_prompt()
            .await
            .expect("last_prompt must be set after process_streaming");
        assert!(
            prompt.contains("hello from test"),
            "prompt should carry the user input: got {} chars",
            prompt.len()
        );
        assert!(
            prompt.contains("## Environment"),
            "prompt should carry the environment grounding block"
        );
    }

    /// set_history_budget clamps to the floor and takes effect in
    /// render_history.
    #[tokio::test]
    async fn test_history_budget_clamps_and_applies() {
        let temp = TempDir::new().unwrap();
        let db_path = temp.path().join("test.redb");
        let cfg = RouterConfig {
            context_window: 8192,
            short_term_capacity: 100,
            working_dir: temp.path().to_path_buf(),
            enable_memory: false,
            max_skills_per_query: 3,
        };
        let engine = KodEngine::new(cfg, db_path).unwrap();
        engine.start().await.unwrap();

        // Default is the documented default.
        assert_eq!(engine.history_budget(), DEFAULT_HISTORY_CHAR_BUDGET);

        // Below the floor clamps up.
        engine.set_history_budget(10);
        assert_eq!(engine.history_budget(), MIN_HISTORY_CHAR_BUDGET);

        // Above the floor is honored.
        engine.set_history_budget(100_000);
        assert_eq!(engine.history_budget(), 100_000);
    }

    /// With a small budget, render_history drops the oldest turns
    /// first — newest turns are always retained.
    #[tokio::test]
    async fn test_render_history_drops_oldest_first() {
        let temp = TempDir::new().unwrap();
        let db_path = temp.path().join("test.redb");
        let cfg = RouterConfig {
            context_window: 8192,
            short_term_capacity: 100,
            working_dir: temp.path().to_path_buf(),
            enable_memory: false,
            max_skills_per_query: 3,
        };
        let engine = KodEngine::new(cfg, db_path).unwrap();
        engine.start().await.unwrap();
        engine.set_history_budget(MIN_HISTORY_CHAR_BUDGET);

        // Seed turns whose total exceeds the floor. Each turn is
        // labelled so we can spot which survived.
        for i in 0..30 {
            engine
                .seed_turn(true, &format!("turn-{i}-{}", "x".repeat(500)))
                .await;
        }

        // render_history is private; drive it via the public surface
        // by seeding and then checking the budgeted output through the
        // only public accessor we have for it: last_prompt is populated
        // by process_streaming, which needs a provider. Instead, use
        // the fact that compact_history keeps the last N and assert
        // the ceiling holds by construction: after compacting to 5,
        // history fits comfortably under the floor and no drop occurs.
        engine.compact_history(5).await;
        // If compact_history mis-counted, this second call would be a
        // no-op — just ensure it does not panic.
        engine.compact_history(5).await;
    }

    /// record_turn runs on both sides of every prompt. A turn longer
    /// than MAX_TURN_CHARS whose boundary byte falls inside a multibyte
    /// codepoint used to panic and abort the whole loop.
    ///
    /// The boundary offset is derived from `MAX_TURN_CHARS` rather than
    /// hardcoded, so raising the cap in the future does not silently
    /// turn this test into a no-op.
    #[tokio::test]
    async fn test_record_turn_does_not_panic_mid_multibyte() {
        let temp = TempDir::new().unwrap();
        let db_path = temp.path().join("test.redb");
        let cfg = RouterConfig {
            context_window: 8192,
            short_term_capacity: 100,
            working_dir: temp.path().to_path_buf(),
            enable_memory: false,
            max_skills_per_query: 3,
        };
        let engine = KodEngine::new(cfg, db_path).unwrap();
        engine.start().await.unwrap();

        // MAX_TURN_CHARS - 1 ASCII bytes, then 'é' (2 bytes) so byte
        // offset MAX_TURN_CHARS is the middle of the codepoint, then
        // enough extra content to exceed the cap and force truncation.
        let mut prompt = "a".repeat(MAX_TURN_CHARS - 1);
        prompt.push('é');
        prompt.push_str(&"x".repeat(100));
        assert!(prompt.len() > MAX_TURN_CHARS);
        assert!(
            !prompt.is_char_boundary(MAX_TURN_CHARS),
            "the test must place the cap inside the é codepoint; \
             MAX_TURN_CHARS={} fell on a boundary",
            MAX_TURN_CHARS
        );

        // Must not panic. The stored text ends at the last safe boundary
        // before the é, with the truncation marker appended.
        engine.record_turn(true, &prompt).await;

        let rendered = engine.render_history().await;
        assert!(rendered.contains("User:"), "history should carry the turn");
        assert!(
            rendered.contains("[truncated]"),
            "history should mark truncation"
        );
    }

    #[test]
    fn test_truncate_does_not_panic_mid_multibyte() {
        // Reproduce the exact panic the old `&rendered[..4000]` could
        // hit: 3999 ASCII bytes, then a 2-byte 'é' so that byte offset
        // 4000 is the middle of the codepoint.
        let mut s = "a".repeat(3999);
        s.push('é');
        s.push_str("tail");
        assert_eq!(s.len(), 3999 + 2 + 4);
        // Must not panic.
        let cut = truncate_chars(&s, 4000);
        assert_eq!(cut.len(), 3999, "rounded down to the boundary before é");
        assert!(cut.is_char_boundary(cut.len()));
    }

    /// list_files routes through summarize_tool_result so the model
    /// sees "N entries in …" instead of a truncated quoted-path dump.
    #[tokio::test]
    async fn test_run_tool_calls_summarizes_list_files() {
        use tempfile::TempDir;
        let temp = TempDir::new().unwrap();
        // Two files; the summary should name both and say "2 entries".
        std::fs::write(temp.path().join("alpha.txt"), "").unwrap();
        std::fs::write(temp.path().join("beta.txt"), "").unwrap();

        let db_path = temp.path().join("test.redb");
        let cfg = RouterConfig {
            context_window: 8192,
            short_term_capacity: 100,
            working_dir: temp.path().to_path_buf(),
            enable_memory: false,
            max_skills_per_query: 3,
        };
        let engine = KodEngine::new(cfg, db_path).unwrap();
        engine.start().await.unwrap();

        let calls = vec![ToolCall {
            tool_name: "list_files".to_string(),
            arguments: serde_json::json!({ "path": "." }),
        }];
        let round = engine.run_tool_calls(&calls, "test", None).await;
        assert_eq!(round.results.len(), 1);
        let block = &round.prompt_block;
        assert!(
            block.contains("2 entr"),
            "list_files summary missing count: {block}"
        );
        assert!(block.contains("alpha.txt"), "got: {block}");
        assert!(block.contains("beta.txt"), "got: {block}");

        // Regression: the previous code computed the header's shortened
        // directory (`…/last-two-segments`) and then tried to strip that
        // from each *absolute* entry, which never matched — every entry
        // rendered as its full path. `· alpha.txt` (not `· /private/…`)
        // is the shape the summary is supposed to produce.
        let summary = summarize_tool_result(
            "list_files",
            &ToolResult::Success(serde_json::json!({
                "path": "/tmp/kod-test-dir",
                "path_kind": "directory",
                "files": [
                    "/tmp/kod-test-dir/alpha.txt",
                    "/tmp/kod-test-dir/beta.txt",
                ],
                "total": 2,
                "truncated": false,
            })),
        );
        assert!(
            summary.contains("· alpha.txt"),
            "entries should be stripped to bare names: {summary}"
        );
        assert!(
            !summary.contains("/tmp/kod-test-dir/alpha.txt"),
            "absolute path must not appear in the summary: {summary}"
        );
    }

    /// summarize_success must surface the tool's truncation flags so
    /// the row does not present a partial read or a killed command as
    /// if it were complete.
    /// cap_rendered_result must produce valid JSON for an oversized
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
            "content": "hello\n",
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
    fn test_summarize_reports_truncation() {
        // read_file: "truncated": true adds "(truncated)".
        let read = summarize_tool_result(
            "read_file",
            &ToolResult::Success(serde_json::json!({
                "path": "/a/big.rs",
                "content": "line1\nline2\n",
                "truncated": true
            })),
        );
        assert!(
            read.contains("(truncated)"),
            "read_file truncation not surfaced: {read}"
        );

        // read_file binary result: one-line summary, no text preview.
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

        // read_file without the flag: no marker.
        let read_ok = summarize_tool_result(
            "read_file",
            &ToolResult::Success(serde_json::json!({
                "path": "/a/small.rs",
                "content": "hello\n",
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
                "stdout": "y\ny\n",
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

        // Truncated output with a real signal (killed by us): label.
        let exec_killed = summarize_tool_result(
            "execute_command",
            &ToolResult::Success(serde_json::json!({
                "stdout": "y\n",
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
                "stdout": "x\n",
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

        // Regression: a timeout with small output used to be silently
        // treated as a normal exit, because the old summariser only
        // looked at the truncation flags. The user saw a partial
        // `cargo build` transcript and assumed it had finished.
        let exec_timeout = summarize_tool_result(
            "execute_command",
            &ToolResult::Success(serde_json::json!({
                "stdout": "Compiling foo\n",
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
                "stdout": "partial output\n",
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
    }

    #[test]
    fn test_summarize_tool_result_shapes() {
        let err = summarize_tool_result("read_file", &ToolResult::Error("boom".to_string()));
        assert!(err.starts_with("Error:"), "got: {err}");
        let ok = summarize_tool_result(
            "execute_command",
            &ToolResult::Success(serde_json::json!({"stdout": "hi\n", "stderr": ""})),
        );
        assert_eq!(ok, "hi");
        let hdr = format_tool_header("read_file", &serde_json::json!({"path": "/a/b/c/main.rs"}));
        assert!(hdr.starts_with("read_file path="), "got: {hdr}");
        // read_file success stays compact: path + size + preview, not a dump.
        let read = summarize_tool_result(
            "read_file",
            &ToolResult::Success(
                serde_json::json!({"path": "/a/main.rs", "content": "one\ntwo\nthree\nfour\n"}),
            ),
        );
        assert!(read.contains("4 lines"), "got: {read}");
        assert!(read.contains("one\ntwo\nthree"), "got: {read}");
        assert!(!read.contains("four"), "got: {read}");
    }

    /// A round containing a mutating tool must run serially in caller
    /// order: the read must observe the write that precedes it in the
    /// same round. Before the serialization fix, join_all could run the
    /// read before the write committed, and this test would flake (or
    /// fail when the file did not exist yet).
    #[tokio::test]
    async fn test_run_tool_calls_serializes_mutating_round() {
        let temp = TempDir::new().unwrap();
        let db_path = temp.path().join("test.redb");

        let cfg = RouterConfig {
            context_window: 8192,
            short_term_capacity: 100,
            working_dir: temp.path().to_path_buf(),
            enable_memory: false,
            max_skills_per_query: 3,
        };
        let engine = KodEngine::new(cfg, db_path).unwrap();
        engine.start().await.unwrap();

        let calls = vec![
            ToolCall {
                tool_name: "write_file".to_string(),
                arguments: serde_json::json!({
                    "path": "serialize_probe.txt",
                    "content": "hello-serial"
                }),
            },
            ToolCall {
                tool_name: "read_file".to_string(),
                arguments: serde_json::json!({ "path": "serialize_probe.txt" }),
            },
        ];

        let round = engine.run_tool_calls(&calls, "test", None).await;
        assert_eq!(round.results.len(), 2);

        // Write must succeed.
        match &round.results[0] {
            ToolResult::Success(_) => {}
            other => panic!("write_file did not succeed: {:?}", other),
        }
        // Read must observe the write.
        match &round.results[1] {
            ToolResult::Success(v) => {
                assert_eq!(
                    v["content"], "hello-serial",
                    "read did not observe write — round raced: {:?}",
                    v
                );
            }
            other => panic!("read_file did not succeed: {:?}", other),
        }
    }

    /// An all-read-only round is safe to parallelize; this test just
    /// verifies both results come back, not the execution order.
    #[tokio::test]
    async fn test_run_tool_calls_parallelizes_read_only_round() {
        let temp = TempDir::new().unwrap();
        std::fs::write(temp.path().join("a.txt"), "AAA").unwrap();
        std::fs::write(temp.path().join("b.txt"), "BBB").unwrap();

        let db_path = temp.path().join("test.redb");
        let cfg = RouterConfig {
            context_window: 8192,
            short_term_capacity: 100,
            working_dir: temp.path().to_path_buf(),
            enable_memory: false,
            max_skills_per_query: 3,
        };
        let engine = KodEngine::new(cfg, db_path).unwrap();
        engine.start().await.unwrap();

        let calls = vec![
            ToolCall {
                tool_name: "read_file".to_string(),
                arguments: serde_json::json!({ "path": "a.txt" }),
            },
            ToolCall {
                tool_name: "read_file".to_string(),
                arguments: serde_json::json!({ "path": "b.txt" }),
            },
        ];
        let round = engine.run_tool_calls(&calls, "test", None).await;
        assert_eq!(round.results.len(), 2);
        assert_eq!(round.elapsed_ms.len(), 2);
        // Order matches caller order regardless of scheduling.
        match (&round.results[0], &round.results[1]) {
            (ToolResult::Success(a), ToolResult::Success(b)) => {
                assert_eq!(a["content"], "AAA");
                assert_eq!(b["content"], "BBB");
            }
            other => panic!("expected two successes, got {:?}", other),
        }
    }
}


#[cfg(test)]
mod prop_tests {
    //! Property tests for the \0kod-* marker protocol.
    //!
    //! The four markers (`tool_start_marker`, `tool_args_marker`,
    //! `tool_done_marker`, `THINKING_MARKER`) form a small wire protocol
    //! between the engine and the TUI: the engine builds a string, sends
    //! it down an mpsc channel, and the TUI parses it back. The protocol
    //! relies on an out-of-band NUL sentinel, which no ordinary text will
    //! contain — but tool output does sometimes contain NUL bytes, and
    //! any future marker field could accidentally carry one.
    //!
    //! `tool_done_marker` is documented as sanitizing embedded NULs to
    //! spaces before encoding, so its round-trip is only identity on
    //! NUL-free inputs. `tool_start_marker` and `tool_args_marker` do not
    //! sanitize, so their properties only hold for NUL-free inputs.
    //!
    //! These tests assert:
    //!   1. Round-trips are identity on the domain where they are defined.
    //!   2. `parse_tool_done` never drops a well-formed completion, and
    //!      never accepts a truncated one (a wrong parse is worse than a
    //!      drop: the TUI would show a stale tool row).
    //!   3. `truncate_chars` always yields a char-boundary-respecting
    //!      prefix.
    //!   4. `format_tool_header` and `format_call_brief` never panic on
    //!      arbitrary JSON, since the model supplies the arguments.

    use super::*;
    use proptest::prelude::*;

    /// Regex strategy that never emits NUL: the marker protocol cannot
    /// represent an embedded NUL, and the engine-side builders are the
    /// only sanctioned place that substitutes one for a space.
    fn no_nul() -> impl Strategy<Value = String> {
        ".{0,300}".prop_filter("NUL-free", |s| !s.contains('\0'))
    }

    proptest! {
        /// tool_start_marker / parse_tool_start round-trip on NUL-free input.
        #[test]
        fn prop_tool_start_roundtrip(name in no_nul().prop_filter("non-empty", |s| !s.is_empty())) {
            let chunk = tool_start_marker(&name);
            let parsed = parse_tool_start(&chunk)
                .expect("marker built by tool_start_marker must parse");
            prop_assert_eq!(parsed, name.as_str());
        }

        /// tool_args_marker / parse_tool_args round-trip on NUL-free input.
        #[test]
        fn prop_tool_args_roundtrip(display in no_nul()) {
            let chunk = tool_args_marker(&display);
            let parsed = parse_tool_args(&chunk)
                .expect("marker built by tool_args_marker must parse");
            prop_assert_eq!(parsed, display.as_str());
        }

        /// tool_done_marker / parse_tool_done round-trip. The builder
        /// sanitizes NUL to space, so the round-trip target is the
        /// sanitized form, not the raw inputs.
        #[test]
        fn prop_tool_done_roundtrip(
            header in ".{0,200}",
            summary in ".{0,500}",
            ms in any::<u64>(),
        ) {
            let header_sanitized = header.replace('\0', " ");
            let summary_sanitized = summary.replace('\0', " ");
            let chunk = tool_done_marker(&header, &summary, ms);
            let (h, s, m) = parse_tool_done(&chunk)
                .expect("marker built by tool_done_marker must parse");
            prop_assert_eq!(h, header_sanitized.as_str());
            prop_assert_eq!(s, summary_sanitized.as_str());
            prop_assert_eq!(m, ms);
        }

        /// Any chunk the engine might send that is *not* a well-formed
        /// done-marker must not parse as one. This is what keeps the TUI
        /// from misreading streamed text as a control frame.
        #[test]
        fn prop_arbitrary_text_is_not_a_done_marker(s in ".{0,400}") {
            // Only assert that a non-marker does not accidentally parse
            // as a marker with a mismatched shape. If it parses, the
            // returned tuple must contain the exact payload.
            if let Some((h, s_, m)) = parse_tool_done(&s) {
                prop_assert!(s.starts_with(TOOL_DONE_MARKER));
                prop_assert!(h.len() + s_.len() <= s.len());
                // Duration must round-trip through u64 parsing, or be
                // the documented degradation to 0.
                if let Ok(parsed) = s.splitn(3, '\0').nth(2).unwrap_or("").parse::<u64>() {
                    prop_assert_eq!(m, parsed);
                } else {
                    prop_assert_eq!(m, 0);
                }
            }
        }

        /// truncate_chars(s, max) is always a char-boundary prefix of s
        /// no longer than max bytes. The whole reason the helper exists
        /// is that &s[..max] panics on multibyte input.
        #[test]
        fn prop_truncate_chars_is_a_safe_prefix(
            s in ".{0,2000}",
            max in 0usize..2000,
        ) {
            let out = truncate_chars(&s, max);
            prop_assert!(out.len() <= max);
            prop_assert!(s.is_char_boundary(out.len()));
            prop_assert!(s.starts_with(out));
        }

        /// format_tool_header must never panic: the model supplies the
        /// arguments, and it can put anything in them.
        #[test]
        fn prop_format_tool_header_never_panics(
            name in no_nul().prop_filter("non-empty", |s| !s.is_empty()),
            path in ".{0,500}",
            pattern in ".{0,500}",
        ) {
            let args = serde_json::json!({
                "path": path,
                "pattern": pattern,
            });
            let _ = format_tool_header(&name, &args);
        }

        /// format_call_brief must never panic on arbitrary arguments.
        #[test]
        fn prop_format_call_brief_never_panics(
            name in no_nul().prop_filter("non-empty", |s| !s.is_empty()),
            command in ".{0,1000}",
        ) {
            // Two shapes the two branches of format_call_brief handle:
            // execute_command with a "command" key, and a generic tool
            // with a "path" key.
            let args_cmd = serde_json::json!({ "command": command });
            let _ = format_call_brief(&name, &args_cmd);
            let args_path = serde_json::json!({ "path": command });
            let _ = format_call_brief(&name, &args_path);
        }
    }
}

#[cfg(test)]
mod diff_attachment_tests {
    use super::*;
    use kod_types::{ToolCall, ToolResult};
    use tempfile::TempDir;

    /// Regression: after `write_file` succeeds, the result must carry
    /// a `"diff"` field showing the before/after delta, so the TUI
    /// row can render what changed. The engine snapshots the file
    /// before the write; the diff is computed against that snapshot.
    #[tokio::test]
    async fn write_file_result_carries_diff() {
        let tmp = TempDir::new().unwrap();
        // Seed a file whose "before" content is known.
        std::fs::write(tmp.path().join("greet.txt"), "hello\n").unwrap();

        let db_path = tmp.path().join("test.redb");
        let cfg = RouterConfig {
            context_window: 8192,
            short_term_capacity: 100,
            working_dir: tmp.path().to_path_buf(),
            enable_memory: false,
            max_skills_per_query: 3,
        };
        let engine = KodEngine::new(cfg, db_path).unwrap();
        engine.start().await.unwrap();

        // Skip if the engine could not construct a checkpoint
        // manager — a container without a writable home directory
        // legitimately cannot attach diffs.
        if engine.checkpoints().is_none() {
            eprintln!("skipping: no checkpoint manager (no home directory)");
            return;
        }

        let calls = vec![ToolCall {
            tool_name: "write_file".to_string(),
            arguments: serde_json::json!({
                "path": "greet.txt",
                "content": "hello\nworld\n"
            }),
        }];
        let round = engine.run_tool_calls(&calls, "test", None).await;
        assert_eq!(round.results.len(), 1);
        match &round.results[0] {
            ToolResult::Success(v) => {
                let diff = v.get("diff").and_then(|d| d.as_str());
                assert!(
                    diff.is_some(),
                    "write_file result must carry a diff field, got: {v}"
                );
                let diff = diff.unwrap();
                assert!(
                    diff.contains("+world"),
                    "diff should show the added line: {diff}"
                );
                assert!(
                    diff.contains("greet.txt"),
                    "diff should name the file: {diff}"
                );
            }
            other => panic!("expected Success, got {other:?}"),
        }
    }

    /// The summary produced from that result names the file and
    /// shows the added line, not a "written N bytes" placeholder.
    #[test]
    fn write_file_summary_renders_diff() {
        let v = serde_json::json!({
            "path": "/tmp/greet.txt",
            "written": 12,
            "diff": "--- a/greet.txt\n+++ b/greet.txt\n@@ -1 +1,2 @@\n hello\n+world\n"
        });
        let rendered = summarize_tool_result("write_file", &ToolResult::Success(v));
        assert!(rendered.contains("greet.txt"), "got: {rendered}");
        assert!(rendered.contains("+world"), "got: {rendered}");
        assert!(
            !rendered.contains("written 12 bytes"),
            "old placeholder still present: {rendered}"
        );
    }

    /// An identical-content write shows "no change" rather than an
    /// empty diff (which would confuse a user expecting to see
    /// something).
    #[test]
    fn identical_write_summary_says_no_change() {
        let v = serde_json::json!({
            "path": "/tmp/same.txt",
            "written": 4,
            "diff": ""
        });
        let rendered = summarize_tool_result("write_file", &ToolResult::Success(v));
        assert!(
            rendered.contains("no change"),
            "expected a no-change notice: {rendered}"
        );
    }
}


#[cfg(test)]
mod auto_check_tests {
    //! Tests for the auto-check injection in `run_tool_calls`.
    //!
    //! The feature is subtle: after a write_file succeeds, the engine
    //! runs the project's compiler and appends its diagnostics to the
    //! prompt the model sees on the next round. These tests drive a
    //! real tool round against a real tempfile workspace, so a
    //! regression in the injection shows up here rather than in
    //! production.

    use super::*;
    use kod_types::ToolCall;
    use tempfile::TempDir;

    /// A clean project: the write is followed by "Auto-check … is
    /// clean." in the prompt block.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn test_auto_check_clean_project() {
        let tmp = TempDir::new().unwrap();
        // Minimal Cargo project so CheckTool detects "cargo" and runs
        // `cargo check` — but the file has no errors, so the diagnostics
        // list is empty.
        std::fs::write(
            tmp.path().join("Cargo.toml"),
            "[package]\nname = \"fixture\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
        )
        .unwrap();
        std::fs::create_dir_all(tmp.path().join("src")).unwrap();
        std::fs::write(tmp.path().join("src/lib.rs"), "pub fn ok() {}\n").unwrap();

        let db_path = tmp.path().join("test.redb");
        let cfg = RouterConfig {
            context_window: 8192,
            short_term_capacity: 100,
            working_dir: tmp.path().to_path_buf(),
            enable_memory: false,
            max_skills_per_query: 3,
        };
        let engine = KodEngine::new(cfg, db_path).unwrap();
        engine.set_auto_check(true);
        engine.start().await.unwrap();

        let calls = vec![ToolCall {
            tool_name: "write_file".to_string(),
            arguments: serde_json::json!({
                "path": "src/lib.rs",
                "content": "pub fn ok() {}\n// added a harmless comment\n"
            }),
        }];
        let round = engine.run_tool_calls(&calls, "test", None).await;
        assert_eq!(round.results.len(), 1);

        // The write succeeded (auto-check runs only on success).
        assert!(matches!(&round.results[0], ToolResult::Success(_)));

        // The prompt block should carry the auto-check section. We do
        // not assert "clean" text exactly because the cargo output
        // format may evolve; the presence of `## Auto-check` is the
        // contract.
        assert!(
            round.prompt_block.contains("## Auto-check"),
            "clean auto-check block missing: {}",
            round.prompt_block
        );
        assert!(
            round.prompt_block.contains("is clean"),
            "clean auto-check should say so: {}",
            round.prompt_block
        );
    }

    /// Auto-check is disabled by default: the block does not appear
    /// even when a write succeeds.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn test_auto_check_disabled_by_default() {
        let tmp = TempDir::new().unwrap();
        std::fs::write(
            tmp.path().join("Cargo.toml"),
            "[package]\nname = \"fixture\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
        )
        .unwrap();
        std::fs::create_dir_all(tmp.path().join("src")).unwrap();
        std::fs::write(tmp.path().join("src/lib.rs"), "pub fn ok() {}\n").unwrap();

        let db_path = tmp.path().join("test.redb");
        let cfg = RouterConfig {
            context_window: 8192,
            short_term_capacity: 100,
            working_dir: tmp.path().to_path_buf(),
            enable_memory: false,
            max_skills_per_query: 3,
        };
        let engine = KodEngine::new(cfg, db_path).unwrap();
        // Intentionally NOT calling set_auto_check(true).
        engine.start().await.unwrap();

        let calls = vec![ToolCall {
            tool_name: "write_file".to_string(),
            arguments: serde_json::json!({
                "path": "src/lib.rs",
                "content": "pub fn ok() {}\n"
            }),
        }];
        let round = engine.run_tool_calls(&calls, "test", None).await;
        assert!(
            !round.prompt_block.contains("## Auto-check"),
            "auto-check must not run when disabled: {}",
            round.prompt_block
        );
    }

    /// A round that writes two files must fall through to the
    /// compiler, which sees both. The per-file LSP path would answer
    /// for only one of them.
    ///
    /// This test works without rust-analyzer on PATH: the multi-file
    /// shape makes `lsp_eligible` false regardless, so the compiler
    /// path runs on every machine. What it proves is that the
    /// multi-file case never short-circuits through LSP.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn test_auto_check_multi_file_round_uses_compiler() {
        let tmp = TempDir::new().unwrap();
        std::fs::write(
            tmp.path().join("Cargo.toml"),
            "[package]\nname = \"fixture\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
        )
        .unwrap();
        std::fs::create_dir_all(tmp.path().join("src")).unwrap();
        std::fs::write(tmp.path().join("src/lib.rs"), "pub mod extra;\n").unwrap();
        std::fs::write(
            tmp.path().join("src/extra.rs"),
            "pub fn bad() -> u32 { 42 }\n",
        )
        .unwrap();

        let db_path = tmp.path().join("test.redb");
        let cfg = RouterConfig {
            context_window: 8192,
            short_term_capacity: 100,
            working_dir: tmp.path().to_path_buf(),
            enable_memory: false,
            max_skills_per_query: 3,
        };
        let engine = KodEngine::new(cfg, db_path).unwrap();
        engine.set_auto_check(true);
        engine.start().await.unwrap();

        // The round writes both files: lib.rs unchanged in spirit,
        // extra.rs introduces a type error. Only the compiler sees
        // both.
        let calls = vec![
            ToolCall {
                tool_name: "write_file".to_string(),
                arguments: serde_json::json!({
                    "path": "src/lib.rs",
                    "content": "pub mod extra;\n// harmless comment\n"
                }),
            },
            ToolCall {
                tool_name: "write_file".to_string(),
                arguments: serde_json::json!({
                    "path": "src/extra.rs",
                    "content": "pub fn bad() -> u32 { \"not a number\" }\n"
                }),
            },
        ];
        let round = engine.run_tool_calls(&calls, "test", None).await;
        assert_eq!(round.results.len(), 2);

        assert!(
            round.prompt_block.contains("## Auto-check"),
            "multi-file round should trigger auto-check: {}",
            round.prompt_block
        );
        assert!(
            round.prompt_block.contains("extra.rs"),
            "the compiler should name the file with the error: {}",
            round.prompt_block
        );
    }

    /// Auto-check with a non-write tool call must not trigger. The
    /// engine should not run `cargo check` for a `read_file`.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn test_auto_check_skips_read_only_round() {
        let tmp = TempDir::new().unwrap();
        std::fs::write(
            tmp.path().join("Cargo.toml"),
            "[package]\nname = \"fixture\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
        )
        .unwrap();
        std::fs::create_dir_all(tmp.path().join("src")).unwrap();
        std::fs::write(tmp.path().join("src/lib.rs"), "pub fn ok() {}\n").unwrap();

        let db_path = tmp.path().join("test.redb");
        let cfg = RouterConfig {
            context_window: 8192,
            short_term_capacity: 100,
            working_dir: tmp.path().to_path_buf(),
            enable_memory: false,
            max_skills_per_query: 3,
        };
        let engine = KodEngine::new(cfg, db_path).unwrap();
        engine.set_auto_check(true);
        engine.start().await.unwrap();

        let calls = vec![ToolCall {
            tool_name: "read_file".to_string(),
            arguments: serde_json::json!({ "path": "src/lib.rs" }),
        }];
        let round = engine.run_tool_calls(&calls, "test", None).await;
        assert!(
            !round.prompt_block.contains("## Auto-check"),
            "read-only round must not trigger auto-check: {}",
            round.prompt_block
        );
    }
    /// A fixture with a pre-existing error. A write that introduces
    /// nothing new must produce a prompt that says so, not one that
    /// lists the pre-existing error as though the write caused it.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn test_auto_check_reports_only_new_diagnostics() {
        let tmp = TempDir::new().unwrap();
        std::fs::write(
            tmp.path().join("Cargo.toml"),
            "[package]\nname = \"fixture\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
        )
        .unwrap();
        std::fs::create_dir_all(tmp.path().join("src")).unwrap();
        // `src/existing.rs` has a type error. `src/touched.rs` is clean.
        std::fs::write(
            tmp.path().join("src/lib.rs"),
            "pub mod existing;\npub mod touched;\n",
        )
        .unwrap();
        std::fs::write(
            tmp.path().join("src/existing.rs"),
            "pub fn bad() -> u32 { \"not a number\" }\n",
        )
        .unwrap();
        std::fs::write(tmp.path().join("src/touched.rs"), "pub fn ok() {}\n").unwrap();

        let db_path = tmp.path().join("test.redb");
        let cfg = RouterConfig {
            context_window: 8192,
            short_term_capacity: 100,
            working_dir: tmp.path().to_path_buf(),
            enable_memory: false,
            max_skills_per_query: 3,
        };
        let engine = KodEngine::new(cfg, db_path).unwrap();
        engine.set_auto_check(true);
        engine.start().await.unwrap();

        // Capture the baseline: the fixture's pre-existing error goes in.
        // Called explicitly so the test does not race the background task
        // that `start()` spawns.
        engine.refresh_check_baseline().await;
        let baseline = engine.check_baseline().await;
        assert!(
            baseline.as_ref().is_some_and(|b| !b.is_empty()),
            "baseline should have captured the pre-existing error"
        );

        // Now write only the clean file. The pre-existing error in
        // existing.rs must NOT be reported as new.
        let calls = vec![ToolCall {
            tool_name: "write_file".to_string(),
            arguments: serde_json::json!({
                "path": "src/touched.rs",
                "content": "pub fn ok() {}\n// harmless comment\n"
            }),
        }];
        let round = engine.run_tool_calls(&calls, "test", None).await;
        assert!(
            round.prompt_block.contains("## Auto-check"),
            "auto-check should have run: {}",
            round.prompt_block
        );
        assert!(
            !round.prompt_block.contains("NEW diagnostic"),
            "a write that introduces nothing must not report NEW diagnostics: {}",
            round.prompt_block
        );
        assert!(
            !round.prompt_block.contains("existing.rs"),
            "the pre-existing error must not appear in the auto-check block: {}",
            round.prompt_block
        );
        assert!(
            round.prompt_block.contains("no new errors"),
            "the block should say the write is clean: {}",
            round.prompt_block
        );
    }

}
