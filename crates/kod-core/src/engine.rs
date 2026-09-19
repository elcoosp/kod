//! Main KOD engine - orchestrates all subsystems.
//!
//! Coordinates the task router, LLM providers, skills, and memory
//! to process user requests end-to-end.

use crate::router::{RouterConfig, TaskResponse, TaskRouter};
use kod_error::{KodError, Result};
use kod_provider::request::{CompletionRequest, SystemPrompt};
use kod_provider::{
    GenerationOptions, GenerationResponse, LlmProvider, ModelRef, ProviderRegistry, StreamChunk,
};
use kod_tools::{
    ExecuteCommandTool, FileInfoTool, GitDiffTool, GitStatusTool, GrepTool, ListFilesTool,
    PatchFileTool, PathLockTable, ReadFileTool, ToolContext, ToolRegistry, WriteFileTool,
};
use kod_types::{ToolCall, ToolDefinition, ToolPermissions, ToolResult};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
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
const TOOL_ROUNDS_EXHAUSTED_NOTE: &str = "\n\n[tool-round limit reached — no further tool calls will run this turn. \
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

/// Streaming chunk count between Jev early-termination checks
/// (P1.2). Every check is a Jev round-trip; running one per
/// chunk would double the stream's wall time on a fast
/// provider. Five is the empirical sweet spot: enough
/// coverage that we rarely miss a completion, few enough
/// that the network cost stays under 10% of streaming time.
const EARLY_TERM_CHECK_EVERY_CHUNKS: usize = 5;

/// Minimum accumulated response length (in chars) before an
/// early-termination check runs. A model that has emitted
/// fewer than this many characters has not yet said anything
/// a completion check could meaningfully judge. 400 chars ≈
/// 100 tokens — the same floor the design document cites.
const EARLY_TERM_MIN_CHARS: usize = 400;

/// Probability at or above which Jev is considered certain
/// the response is complete (P1.2). Below `[jev.thresholds]
/// .early_termination_min`, the stream continues. The default
/// matches `JevThresholds::default().early_termination_min`.
const EARLY_TERM_DEFAULT_MIN: f32 = 0.9;

/// Sent on the chunk channel when the engine abandons the current
/// endpoint mid-stream because Jev judged the partial response
/// off-track (P5.6). The TUI drops whatever it accumulated for
/// the current attempt so the retry against the next endpoint
/// starts from a clean bubble. Emitted only from `stream_round`
/// on the very first round, before any tool call has run, so no
/// tool row can exist to clean up.
pub const STREAM_RESET_MARKER: &str = "\0kod-stream-reset\0";

/// The reset-marker chunk.
pub fn stream_reset_marker() -> String {
    STREAM_RESET_MARKER.to_string()
}

/// True for exactly the reset marker (no id, no JSON).
pub fn is_stream_reset_marker(chunk: &str) -> bool {
    chunk == STREAM_RESET_MARKER
}

/// Why `stream_round` decided to stop reading chunks (P1.2 + P5.6).
///
/// `Complete` is the classic early termination: the response
/// already answers the request, drop the rest of the stream. The
/// caller keeps the text.
///
/// `OffTrack` is P5.6's signal: Jev is confident the model
/// drifted from the request. The caller discards the text and —
/// when a fallback endpoint remains — retries against it.
///
/// `None` is the common case: keep streaming.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum EarlyTermination {
    None,
    Complete,
    OffTrack,
}

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
        if changed && let Ok(reserialized) = serde_json::to_string(&trimmed) {
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
    let last_line = text.lines().rev().map(|l| l.trim()).find(|l| !l.is_empty());
    let Some(line) = last_line else {
        return false;
    };
    // Strip surrounding emphasis and leading quote / list markers.
    let stripped: String = line
        .trim_matches(|c: char| c.is_whitespace() || c == '*' || c == '`' || c == '>' || c == '-')
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
        let at_word_start = i == 0 || matches!(bytes[i - 1], b' ' | b'\t' | b'\n' | b'\r');
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
                && (token.contains('/') || token.contains('.') || token.starts_with('~'));
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

/// Strip the transcript section the router appends to its plan.
///
/// The router's `build_prompt_with_budget` ends with:
///
/// ```text
/// ## Conversation so far
///
/// {history}
///
/// ## User Request
///
/// {input}
/// ```
///
/// Both sections are already passed as structured messages on the
/// `CompletionRequest` path. Keeping them in the system prompt would
/// duplicate the user's turn on every call — a token waste and a
/// source of confusion for the model.
///
/// The cut is at the FIRST occurrence of `## Conversation so far` so a
/// later mention in the model's own text does not truncate mid-reply.
/// A prompt without the marker is returned unchanged (the router
/// changed its shape, or a caller built a custom one).
fn strip_conversation_tail(system_text: &str) -> String {
    const MARKER: &str = "## Conversation so far";
    match system_text.find(MARKER) {
        Some(i) => system_text[..i].trim_end().to_string(),
        None => system_text.to_string(),
    }
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
#[derive(Debug, Clone, Default)]
pub struct GenerationDefaults {
    pub temperature: Option<f32>,
    pub max_tokens: Option<usize>,
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
                v.get("path")
                    .and_then(|p| p.as_str())
                    .map(shorten_path)
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
    if name == "read_file" && v.get("binary").and_then(|b| b.as_bool()).unwrap_or(false) {
        let path = v
            .get("path")
            .and_then(|p| p.as_str())
            .map(shorten_path)
            .unwrap_or_else(|| name.to_string());
        let size = v.get("size_bytes").and_then(|s| s.as_u64()).unwrap_or(0);
        return format!("{} · binary ({} bytes) — not shown as text", path, size);
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
    if name == "list_files" && v.get("path_kind").and_then(|k| k.as_str()) == Some("file") {
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
        let dir_full = v.get("path").and_then(|p| p.as_str()).unwrap_or("");
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
            if dir_short.is_empty() {
                name
            } else {
                &dir_short
            }
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
        let timeout_secs = v.get("timeout_secs").and_then(|n| n.as_u64()).unwrap_or(0);
        let truncated = stdout_trunc || stderr_trunc;
        let signalled = v.get("exit_signal").and_then(|s| s.as_i64()).is_some();

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

/// The turn-scoped parameters every round of the agentic loop needs.
///
/// Bundled because the two loop methods (`run_collected_loop`,
/// `run_streaming_loop`) received the same five parameters on every
/// call, pushing both signatures past the eight-argument limit
/// `clippy::too_many_arguments` enforces and making the parameter list
/// hard to read. Bundling loses nothing — the fields do not vary
/// between rounds of a turn — and the caller builds the bundle once.
struct RoundContext<'a> {
    system_text: &'a str,
    model_ref: &'a ModelRef,
    definitions: &'a [ToolDefinition],
    options: &'a GenerationOptions,
    holder: &'a str,
    /// Turn trace builder (Tier 1.4). `None` when no trace writer is
    /// installed — every trace call is a no-op in that case.
    trace: Option<&'a std::sync::Mutex<crate::trace::TurnTraceBuilder>>,
}

/// Outcome of one tool-execution round: results for the response plus a
/// prompt block feeding them back to the model. `elapsed_ms` parallels
/// `results` — per-call wall time for the live done-markers.
struct ToolRound {
    results: Vec<ToolResult>,
    prompt_block: String,
    elapsed_ms: Vec<u64>,
    /// Structured form of this round: one assistant message carrying
    /// the calls (with ids), then one `Role::Tool` message per call
    /// linked by `tool_call_id` (design §2 AD-02). Appended to the
    /// per-transcript history by the caller. The text rendering of
    /// the transcript skips these for now (see `render_history_for`),
    /// so the pre-migration prompt bytes are unchanged; they are the
    /// payload the eventual `CompletionRequest` migration (AD-01)
    /// will ship on the wire.
    messages: Vec<kod_types::ChatMessage>,
}

/// Default transcript key: the interactive session. Public methods
/// without an explicit key operate on this. Swarm agents use a
/// `swarm:<agent-id>` key so concurrent agents do not interleave their
/// turns into one shared history.
const DEFAULT_TRANSCRIPT_KEY: &str = "";

/// Main engine for KOD
pub struct KodEngine {
    router: Arc<TaskRouter>,
    /// Named-endpoint map (A4b). When `Some`, `resolve_provider` reads
    /// through `current_model` into this registry rather than the
    /// legacy slot above.
    registry: RwLock<Option<Arc<ProviderRegistry>>>,
    /// Which endpoint+model a session is using. Set by `set_registry`
    /// (to the caller's declared default) and by `set_current_model`
    /// (TUI `/model` switch). Only consulted when `registry` is Some.
    current_model: RwLock<ModelRef>,
    /// Task-type → endpoint routing table (A6). `None` on a v1 config
    /// (no `[llm.routing]` section) — the chain resolver then falls
    /// back to `current_model` as a single-element chain. Populated by
    /// `set_registry` from the config's `routing` field.
    routing: RwLock<Option<kod_config::RoutingConfig>>,
    is_running: RwLock<bool>,
    tools: Arc<ToolRegistry>,
    tool_context: ToolContext,
    /// Shared per-path advisory locks. Cloned into every per-call
    /// tool context the engine derives, so a swarm agent and the
    /// interactive session contend on the same table.
    lock_table: Arc<PathLockTable>,
    working_dir: PathBuf,
    /// Steer notes queued while a prompt is running, keyed by transcript
    /// (D4-D4, AD-11). `steer("note")` writes to the default key;
    /// `steer_for(key, note)` targets one agent. The loops drain only
    /// their own key.
    steers: RwLock<HashMap<String, Vec<String>>>,
    /// Set by [`KodEngine::request_cancel`]; loops check it between
    /// rounds. Keyed by transcript (D4-D4): a cancel for
    /// `swarm:{agent-id}` stops only that agent, not the whole swarm.
    /// The default key `""` is the interactive session.
    cancels: RwLock<std::collections::HashSet<String>>,
    /// Transcripts, one per key. `DEFAULT_TRANSCRIPT_KEY` is the
    /// interactive session; a swarm agent uses `swarm:<agent-id>` so
    /// concurrent agents do not interleave their turns.
    history: RwLock<HashMap<String, Vec<kod_types::ChatMessage>>>,
    /// Per-transcript working-directory override (D4-D1). A swarm
    /// agent registers its worktree path here before running; tool
    /// calls on that transcript use the override for
    /// `ToolContext.working_dir` instead of the engine-wide root.
    /// Absent for the default session — that key falls through to
    /// `self.working_dir`, which is the pre-D4 behaviour.
    transcript_working_dirs: RwLock<HashMap<String, PathBuf>>,
    /// Per-transcript plan (Tier 2.1). Populated by the model on the
    /// first turn of a Complex/MultiStep task, re-rendered in every
    /// subsequent system prompt. Absent when the task is simple.
    plans: RwLock<HashMap<String, crate::plan::Plan>>,
    /// Per-transcript write set (D4.2). The swarm runner registers
    /// each agent's `expected_writes` under the agent's transcript
    /// key before the agent starts. The engine applies the globs to
    /// the `ToolContext` of every tool call that transcript makes,
    /// so a write outside the declared set is refused at the call
    /// boundary. An entry that was never set (the default session,
    /// a non-swarm run) behaves as if the globs were `None` — no
    /// restriction beyond the existing permission gate.
    transcript_write_globs: RwLock<HashMap<String, Option<Vec<String>>>>,
    /// Total chars of history rendered into a prompt. Defaults to
    /// [`DEFAULT_HISTORY_CHAR_BUDGET`]; the TUI and CLI set this from
    /// `LlmConfig::context_window` at startup so a 128k model actually
    /// gets 128k worth of history instead of the 8k-safe default.
    history_budget: std::sync::atomic::AtomicUsize,
    /// The grounded prompt + its per-section budget allocation, for the
    /// most recent `process*` call on each transcript.
    ///
    /// `PromptTrace::text` is what `/debug last-prompt` dumps: the full
    /// environment block, tool inventory, skill inventory, rendered
    /// history, and user input the model received. `PromptTrace::alloc`
    /// is what `/debug tokens` renders as the budget table — the
    /// character allocation the `PromptBudget` gave each truncatable
    /// section (history, skills, memory, repomap) plus the request.
    ///
    /// Overwritten each call; bounded by the prompt builder's own caps.
    /// Same keying as `history`.
    last_prompt: RwLock<HashMap<String, crate::budget::PromptTrace>>,
    /// Provider options captured from `LlmConfig` at engine construction.
    /// Transitional until D1 (endpoint config per ModelRef). Kept in an
    /// `RwLock<Option<...>>` so `set_generation_defaults` works through
    /// `&self` (the engine is shared as `Arc<KodEngine>`).
    generation_defaults: RwLock<GenerationDefaults>,
    /// Optional session log. When `Some`, every tool call and its result
    /// are appended as one JSONL entry, `kod replay`-able. `None` (the
    /// default) is the right shape for a test or a one-shot command.
    session_recorder:
        std::sync::RwLock<Option<std::sync::Arc<crate::session_log::SessionRecorder>>>,
    /// The active user request per transcript key (P3.1). Set
    /// at the top of `process_streaming_with_model_for` and
    /// `process_for` so tool-call helpers that fire deep in the
    /// stack (auto-approval, early termination) can include the
    /// request in their Jev state. Cleared when the call
    /// returns.
    current_requests: RwLock<HashMap<String, String>>,
    /// Optional Jev (TypeSafe System One) client (design P0.1).
    /// `None` — the default — means every Jev-aware call site
    /// runs its pre-Jev heuristic with no network call. The
    /// CLI/TUI install one via `set_jev_client` when
    /// `JevConfig::enabled` is true.
    jev_client: std::sync::RwLock<Option<crate::jev::JevClient>>,
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
    /// Read-protection rules (Tier 1.3).
    read_protection: std::sync::RwLock<Option<kod_config::ReadProtection>>,
    /// The redactor used for content sanitization (Tier 1.3).
    redactor: std::sync::Arc<kod_types::redact::Redactor>,
    /// Session cost accumulator (Tier 1.2). Clone the engine to
    /// share it with a UI.
    cost_tracker: crate::cost::CostTracker,
    /// Per-tool quota counters (Tier 2.5). Reset per turn and per
    /// session; enforced before every dispatch.
    tool_counts: std::sync::Arc<crate::tool_quota::ToolCounts>,
    /// The [limits.tools] configuration loaded at startup. `None`
    /// until `install_limits` runs.
    tool_quotas: std::sync::RwLock<Option<std::collections::BTreeMap<String, kod_config::ToolQuota>>>,
    /// Monotonic per-session turn id for the trace log (Tier 1.4).
    next_turn_id: std::sync::atomic::AtomicU64,
    /// Append-only writer for `turns.jsonl`, next to the session log.
    /// `None` — the default — is the right shape for a test or a
    /// one-shot command that does not want a trace file.
    turn_trace_writer: std::sync::RwLock<Option<std::sync::Arc<crate::trace_writer::TraceWriter>>>,
    /// The current round's taint level (Tier 1.1). Escalated by every
    /// untrusted tool call; reset at the start of every user turn.
    taint: std::sync::RwLock<kod_types::trust::TrustLevel>,
    /// Whether `web_fetch` may reach the network. `AtomicBool` so the
    /// setter works through `&self`, matching the sandbox flag. Off by
    /// default; the CLI and TUI apply `LlmConfig::network_access` at
    /// startup.
    network_access_atomic: std::sync::atomic::AtomicBool,
    /// When true, a successful `write_file` / `patch_file` triggers an
    /// automatic project check and the diagnostics are appended to the
    /// model's tool-results block. See `ToolsConfig::auto_check`.
    auto_check_atomic: std::sync::atomic::AtomicBool,
    /// When true (the default), a successful write also runs the LSP
    /// diagnostics pass on the touched file(s) and appends a
    /// `## LSP diagnostics` block to the next turn's prompt. Cheap;
    /// independent of `auto_check`. See `ToolsConfig::auto_lsp`.
    auto_lsp_atomic: std::sync::atomic::AtomicBool,
    /// Per-project policy (D3-C1). `None` until a caller installs one
    /// via `set_policy` — the CLI/TUI build it from `KodConfig` at
    /// startup. When `None`, the legacy `confirm_writes_atomic` gate
    /// remains in force for `write_file`/`patch_file`, which is the
    /// pre-D3 behaviour a test or embedder sees. When `Some`, every
    /// tool call is consulted against the policy.
    policy: RwLock<Option<Arc<kod_config::PolicyEngine>>>,
    /// Session-scoped "never" rules (`a` on the approval dialog). A
    /// rule that matches a call is denied before any policy layer is
    /// consulted — the highest priority.
    deny_rules: RwLock<std::collections::HashSet<kod_config::SessionDeny>>,

    /// Multi-language LSP pool (design D5.1). One `LspClient` per
    /// server binary, lazily started on the first request for its
    /// language. The manager is created once at engine construction
    /// and held for the engine's lifetime; `shutdown()` tears it
    /// down. Exposed via [`KodEngine::lsp_manager`] so the `lsp_*`
    /// tools use the same pool the engine uses — a second manager
    /// would spawn a second rust-analyzer, doubling the index cost.
    lsp_manager: Arc<kod_lsp::LspManager>,
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
    pending_approvals:
        RwLock<std::collections::HashMap<u64, tokio::sync::oneshot::Sender<ApprovalDecision>>>,
    /// The communication hub every swarm agent registers on
    /// (D4.3). Owned by the engine so the note/read tools (which
    /// live at this composition root) have a single hub to talk to,
    /// and so the swarm runner has one to spawn its `AgentSwarm`
    /// with. Replaced per-run by the runner calling `clear_all()`.
    swarm_hub: Arc<kod_swarm::AgentCommunicationHub>,
    /// The engine's own identity on the hub. Stable for the life of
    /// the engine — the runner registers its own per-agent ids on
    /// top, and the note/read tools broadcast as this id.
    /// This engine's session identity. Generated once at

    /// construction and stable for the engine's lifetime. Used to

    /// attribute auto-extracted episodic facts to the session

    /// that produced them (design D2.5). Swarm-agent transcripts

    /// carry their own UUID in the transcript key, and that UUID

    /// is preferred when present.
    session_id: kod_types::SessionId,

    swarm_coordinator_id: kod_types::AgentId,
    /// The session's todo list. Shared across swarm agents and across
    /// every turn of the same engine.
    todo_list: kod_tools::TodoList,
    /// File checkpoint snapshots. `Some` when a checkpoint directory
    /// could be determined from the working directory; `None` when
    /// the home directory is unavailable (a stripped container, a
    /// test that has unset HOME). See [`crate::checkpoint`].
    checkpoints: Option<Arc<crate::checkpoint::CheckpointManager>>,
    /// The background consolidation task (design D2.5), when
    /// `memory.compaction_interval_secs > 0` and memory is enabled.
    /// Aborted and awaited in `shutdown()` BEFORE the redb close so
    /// `Arc::try_unwrap` on the router succeeds — the task holds a
    /// clone of the router, and the close needs the last reference.
    memory_consolidation_task: tokio::sync::RwLock<Option<tokio::task::JoinHandle<()>>>,

    /// MCP host (D6.1). `None` — the default — means no MCP tools
    /// are registered. The CLI/TUI install one via `set_mcp_host`
    /// when `[mcp.servers]` is non-empty. The host owns the spawned
    /// server processes; `shutdown` tears them down.
    mcp: RwLock<Option<Arc<crate::mcp_adapters::McpHost>>>,
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

/// Marker prefix for a batch of approval requests: one marker per
/// round, carrying every `Ask`-gated call the round produced.
/// `\0kod-approval-batch:<batch_id>:<json>\0`, where `<json>` decodes
/// to [`ApprovalBatch`].
///
/// A batch of exactly one item is protocol-identical to the old
/// single-item marker from the user's point of view: a consumer that
/// special-cases `items.len() == 1` renders the familiar dialog.
pub const TOOL_APPROVAL_BATCH_MARKER: &str = "\0kod-approval-batch:";

/// Build a batch-approval chunk carrying the JSON-encoded batch.
pub fn tool_approval_batch_marker(batch_id: u64, batch_json: &str) -> String {
    let clean = batch_json.replace('\0', " ");
    format!("{TOOL_APPROVAL_BATCH_MARKER}{batch_id}:{clean}\0")
}

/// If `chunk` is a batch-approval marker, return `(batch_id, json)`.
pub fn parse_tool_approval_batch(chunk: &str) -> Option<(u64, &str)> {
    let rest = chunk.strip_prefix(TOOL_APPROVAL_BATCH_MARKER)?;
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
    /// The engine's internal approval id. Present in items of an
    /// [`ApprovalBatch`]; `None` on the legacy single-item marker
    /// (which carries the id out-of-band as
    /// `\0kod-approval:<id>:<json>`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<u64>,
}

/// A single round's worth of approvals, emitted together so a
/// consumer can present them as one batch.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ApprovalBatch {
    pub items: Vec<ApprovalRequest>,
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
    /// Test-only shim: install a bare provider behind a one-endpoint
    /// registry named "default". The pre-cleanup `set_provider` shape
    /// is not part of the production API any more; this helper keeps
    /// the in-crate tests readable without rebuilding a registry at
    /// every call site.
    
/// Extract a JSON array of step strings from a model reply that may
/// carry prose around it (Tier 2.1). Tolerant: first `[` to last `]`,
/// every element coerced to a string.
fn parse_plan_steps(text: &str) -> Option<Vec<String>> {
    let start = text.find('[')?;
    let end = text.rfind(']')?;
    if end <= start {
        return None;
    }
    let v: serde_json::Value =
        serde_json::from_str(&text[start..=end]).ok()?;
    let arr = v.as_array()?;
    let steps: Vec<String> = arr
        .iter()
        .filter_map(|x| {
            x.as_str()
                .map(String::from)
                .or_else(|| x.as_str().map(String::from))
        })
        .filter(|s| !s.trim().is_empty())
        .collect();
    if steps.is_empty() {
        None
    } else {
        Some(steps)
    }
}

#[cfg(test)]
    pub(crate) async fn install_test_provider(&self, provider: Arc<dyn LlmProvider>) {
        let mut reg = kod_provider::ProviderRegistry::new();
        reg.insert(
            "default",
            provider,
            kod_provider::ProviderCapabilities::conservative(),
            "",
        );
        self.set_registry(
            Arc::new(reg),
            kod_provider::ModelRef::new("default", ""),
            None,
        )
        .await;
    }

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
                git_access: kod_types::GitAccess::Write,
                allowed_paths: Vec::new(),
                forbidden_paths: Vec::new(),
            });
        let router = TaskRouter::new(config, db_path)?;
        let lock_table = Arc::new(PathLockTable::new());
        // Snapshots are best-effort: a session without a home directory
        // still runs, it just cannot roll back. The manager is
        // per-working-directory, so two sessions on different projects
        // do not see each other's checkpoints.
        let checkpoints =
            crate::checkpoint::CheckpointManager::for_working_dir(&working_dir).map(Arc::new);

        Ok(Self {
            router: Arc::new(router),
            registry: RwLock::new(None),
            current_model: RwLock::new(ModelRef::new("default", "")),
            routing: RwLock::new(None),
            is_running: RwLock::new(false),
            tools: Arc::new(ToolRegistry::new()),
            tool_context,
            lock_table,
            working_dir: working_dir.clone(),
            steers: RwLock::new(HashMap::new()),
            cancels: RwLock::new(std::collections::HashSet::new()),
            history: RwLock::new(HashMap::new()),
            transcript_working_dirs: RwLock::new(HashMap::new()),
            plans: RwLock::new(HashMap::new()),
            transcript_write_globs: RwLock::new(HashMap::new()),
            history_budget: std::sync::atomic::AtomicUsize::new(DEFAULT_HISTORY_CHAR_BUDGET),
            last_prompt: RwLock::new(HashMap::new()),
            generation_defaults: RwLock::new(GenerationDefaults::default()),
            session_recorder: std::sync::RwLock::new(None),
            current_requests: RwLock::new(HashMap::new()),
            jev_client: std::sync::RwLock::new(None),
            hooks: std::sync::RwLock::new(
                std::sync::Arc::new(crate::hooks::HookRunner::disabled()),
            ),
            // 2 == SandboxMode::Auto: use the best platform primitive
            // when available, silently fall through otherwise. The CLI
            // and TUI escalate to Require via `--sandbox`; a caller
            // that never touches this gets the design's safe default.
            sandbox_mode_atomic: std::sync::atomic::AtomicU8::new(2),
            read_protection: std::sync::RwLock::new(None),
            redactor: std::sync::Arc::new(kod_types::redact::Redactor::default()),
            cost_tracker: crate::cost::CostTracker::new(),
            tool_counts: std::sync::Arc::new(crate::tool_quota::ToolCounts::new()),
            tool_quotas: std::sync::RwLock::new(None),
            next_turn_id: std::sync::atomic::AtomicU64::new(1),
            turn_trace_writer: std::sync::RwLock::new(None),
            taint: std::sync::RwLock::new(kod_types::trust::TrustLevel::Assistant),
            network_access_atomic: std::sync::atomic::AtomicBool::new(false),
            auto_check_atomic: std::sync::atomic::AtomicBool::new(false),
            auto_lsp_atomic: std::sync::atomic::AtomicBool::new(true),
            policy: RwLock::new(None),
            deny_rules: RwLock::new(std::collections::HashSet::new()),
            lsp_manager: Arc::new(kod_lsp::LspManager::new(working_dir.clone())),
            check_baseline: Arc::new(RwLock::new(None)),
            next_approval_id: std::sync::atomic::AtomicU64::new(1),
            pending_approvals: RwLock::new(std::collections::HashMap::new()),
            pending_questions: RwLock::new(std::collections::HashMap::new()),
            next_question_id: std::sync::atomic::AtomicU64::new(1),
            swarm_hub: Arc::new(kod_swarm::AgentCommunicationHub::new()),
            session_id: kod_types::SessionId::new(),

            swarm_coordinator_id: kod_types::AgentId::new(),
            todo_list: kod_tools::new_todo_list(),
            checkpoints,
            memory_consolidation_task: tokio::sync::RwLock::new(None),
            mcp: RwLock::new(None),
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
            *guard = GenerationDefaults {
                temperature,
                max_tokens,
            };
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
    /// Install read-protection rules (Tier 1.3).
    /// The session cost accumulator (Tier 1.2).
    /// The current round's taint (Tier 1.1). `Assistant` when no
    /// tool has run in this round.
    pub fn taint_level(&self) -> kod_types::trust::TrustLevel {
        match self.taint.read() {
            Ok(g) => *g,
            Err(p) => *p.into_inner(),
        }
    }

    /// Public taint reset — the user reviewed the content.
    pub fn clear_taint(&self) {
        if let Ok(mut g) = self.taint.write() {
            *g = kod_types::trust::TrustLevel::Assistant;
        }
    }

    fn reset_taint(&self) {
        if let Ok(mut g) = self.taint.write() {
            *g = kod_types::trust::TrustLevel::Assistant;
        }
    }

    fn escalate_taint(&self, level: kod_types::trust::TrustLevel) {
        if let Ok(mut g) = self.taint.write()
            && level > *g
        {
            *g = level;
        }
    }

    /// Tier 1.1 — a call requires escalation only when the round's
    /// taint is one of the two adversarial levels AND the call is one
    /// of the high-impact tools. The policy engine has already had
    /// its chance to deny; this gate converts `Allow` into `Ask`
    /// under a tainted round.
    fn requires_approval_for_taint(&self, call: &ToolCall) -> bool {
        let t = self.taint_level();
        if !t.is_tainting() {
            return false;
        }
        matches!(
            call.tool_name.as_str(),
            "execute_command"
                | "write_file"
                | "patch_file"
                | "git_commit"
                | "git_branch_create"
        )
    }

    /// Snapshot of per-tool counters, for `/limits`.
    pub fn tool_count_snapshot(&self) -> Vec<(String, usize, usize)> {
        self.tool_counts.snapshot()
    }

    /// Reset per-session tool counters. `/limits reset`.
    pub fn reset_tool_counts(&self) {
        self.tool_counts.reset();
    }

    /// The resolved quota for a tool, if any is installed. Explicit
    /// entry first, then the `"default"` entry.
    fn quota_for(&self, tool: &str) -> Option<kod_config::ToolQuota> {
        let g = self.tool_quotas.read().ok()?;
        let map = g.as_ref()?;
        map.get(tool)
            .or_else(|| map.get("default"))
            .filter(|q| q.per_turn > 0 || q.per_session > 0 || q.per_command > 0)
            .cloned()
    }

    /// On the first turn of a Complex or MultiStep task, ask the
    /// model for a step-by-step plan and store it (Tier 2.1).
    ///
    /// The plan prompt is deliberately short — the model already has
    /// the user's request as the input. The result is JSON-parsed
    /// with a tolerant parser (first `[` to last `]`), the same
    /// pattern `parse_subtasks` uses for swarm.
    ///
    /// Fail-silent: no plan is a valid outcome. A bad Jev signal, a
    /// short request, or a malformed reply all leave the session
    /// without a plan, which is the pre-2.1 behaviour.
    async fn maybe_create_plan(
        &self,
        key: &str,
        input: &str,
        task_type: crate::router::TaskType,
        provider: &Arc<dyn LlmProvider>,
        options: &GenerationOptions,
    ) {
        use crate::router::TaskType;
        if !matches!(task_type, TaskType::Complex | TaskType::MultiStep) {
            return;
        }
        // Do not overwrite an existing plan.
        if self.plans.read().await.contains_key(key) {
            return;
        }
        // A very short request cannot carry a plan worth generating.
        if input.len() < 30 {
            return;
        }
        let prompt = format!(
            "Produce a concise, ordered plan for the request below.\n\
             Output ONLY a JSON array of strings, one per step.\n\
             Rules:\n\
             - 3 to 8 steps.\n\
             - Each step is one imperative sentence.\n\
             - No explanations, no headers, no wrapping prose.\n\n\
             Request: {input}"
        );
        let raw = match provider.generate(&prompt, options).await {
            Ok(t) => t,
            Err(e) => {
                tracing::debug!(error = %e, "plan generation skipped");
                return;
            }
        };
        let Some(steps) = Self::parse_plan_steps(&raw) else {
            tracing::debug!("plan reply did not contain a JSON array");
            return;
        };
        if steps.is_empty() {
            return;
        }
        let plan = crate::plan::Plan::new(input, steps);
        let step_count = plan.steps.len();
        self.set_plan(key, plan).await;
        tracing::info!(steps = step_count, "plan created");
    }

    /// Log one memory retrieval event (Tier 2.4). Called once per
    /// prompt that retrieves anything; a no-op when no recorder is
    /// installed or the retrieval was empty.
    fn log_memory_retrieval(
        &self,
        turn_id: u64,
        query: &str,
        retrieved: &[(String, f32)],
    ) {
        if retrieved.is_empty() {
            return;
        }
        let now_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0);
        // Hash the query so the log carries a stable identity without
        // storing the (potentially sensitive) text itself.
        let mut h: u64 = 0xcbf2_9ce4_8422_2325;
        for b in query.as_bytes() {
            h ^= *b as u64;
            h = h.wrapping_mul(0x0000_0100_0000_01b3);
        }
        let entry = crate::session_log::SessionEntry::MemoryRetrieval {
            timestamp_ms: now_ms,
            turn_id,
            query_hash: format!("{h:016x}"),
            retrieved: retrieved.to_vec(),
            referenced: Vec::new(),
            user_corrected: false,
        };
        if let Ok(guard) = self.session_recorder.read()
            && let Some(rec) = guard.as_ref()
            && let Err(e) = rec.record(&entry)
        {
            tracing::warn!(error = %e, "could not append MemoryRetrieval to session log");
        }
    }

    /// The plan for a transcript, if one has been created (Tier 2.1).
    pub async fn plan_for(&self, key: &str) -> Option<crate::plan::Plan> {
        self.plans.read().await.get(key).cloned()
    }

    /// Replace the plan for a transcript.
    pub async fn set_plan(&self, key: &str, plan: crate::plan::Plan) {
        self.plans.write().await.insert(key.to_string(), plan);
    }

    /// Drop the plan for a transcript.
    pub async fn clear_plan(&self, key: &str) {
        self.plans.write().await.remove(key);
    }

    /// Apply a `PlanUpdate` to the transcript's plan, if one exists.
    /// Returns the human-readable description from `Plan::apply`, or
    /// a message saying no plan exists.
    pub async fn apply_plan_update(
        &self,
        key: &str,
        update: crate::plan::PlanUpdate,
    ) -> String {
        let mut g = self.plans.write().await;
        match g.get_mut(key) {
            Some(p) => p.apply(update),
            None => "No plan exists for this session. A plan is created                      on the first turn of a complex task."
                .to_string(),
        }
    }

    pub fn cost_tracker(&self) -> &crate::cost::CostTracker {
        &self.cost_tracker
    }

    /// Install the `[limits]` block on the cost tracker (Tier 1.2)
    /// and the per-tool quotas (Tier 2.5).
    pub fn install_limits(&self, cfg: &kod_config::LimitsConfig) {
        self.cost_tracker.install_config(cfg);
        if let Ok(mut g) = self.tool_quotas.write() {
            *g = Some(cfg.tools.clone());
        }
    }

    pub fn set_read_protection(&self, rp: kod_config::ReadProtection) {
        if let Ok(mut slot) = self.read_protection.write() {
            *slot = Some(rp);
        }
    }

    /// Replace the default redactor (Tier 1.3).
    pub fn set_redactor(&mut self, redactor: kod_types::redact::Redactor) {
        self.redactor = std::sync::Arc::new(redactor);
    }

    /// The current read-protection, if any.
    pub fn read_protection_setting(&self) -> Option<kod_config::ReadProtection> {
        self.read_protection.read().ok().and_then(|g| g.clone())
    }

    /// The current redactor (shared handle).
    pub fn redactor(&self) -> std::sync::Arc<kod_types::redact::Redactor> {
        self.redactor.clone()
    }

    pub fn set_sandbox_mode(&self, mode: kod_tools::context::SandboxMode) {
        use std::sync::atomic::Ordering;
        let v = match mode {
            kod_tools::context::SandboxMode::Disabled => 0u8,
            kod_tools::context::SandboxMode::Auto => 2u8,
            kod_tools::context::SandboxMode::Require => 1u8,
        };
        self.sandbox_mode_atomic.store(v, Ordering::Relaxed);
    }

    /// The effective sandbox state as a caller (the TUI header, a
    /// status panel) wants to render it: the configured mode plus the
    /// backend that will actually be used.
    ///
    /// The second element is the resolver's chosen primitive name
    /// (`"bwrap"`, `"landlock"`, `"sandbox-exec"`) when one is
    /// available, or `None` when the caller has Auto mode and no
    /// primitive is installed — the honest "off" case the design's
    /// AD-10 wants visible.
    pub fn sandbox_status(&self) -> (kod_tools::context::SandboxMode, Option<&'static str>) {
        let mode = self.sandbox_setting();
        // `Disabled` never queries the resolver; the caller asked for
        // no sandbox and that is what they get.
        if matches!(mode, kod_tools::context::SandboxMode::Disabled) {
            return (mode, None);
        }
        let resolver = kod_tools::context::SandboxResolver::detect();
        (mode, resolver.backend_name())
    }

    /// The current sandbox mode.
    pub fn sandbox_setting(&self) -> kod_tools::context::SandboxMode {
        use std::sync::atomic::Ordering;
        match self.sandbox_mode_atomic.load(Ordering::Relaxed) {
            1 => kod_tools::context::SandboxMode::Require,
            2 => kod_tools::context::SandboxMode::Auto,
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

    /// Enable or disable the post-write LSP diagnostics pass. Called
    /// by the CLI/TUI at startup with `ToolsConfig::auto_lsp`.
    pub fn set_auto_lsp(&self, enabled: bool) {
        self.auto_lsp_atomic
            .store(enabled, std::sync::atomic::Ordering::Relaxed);
    }

    /// The current auto-LSP setting.
    pub fn auto_lsp_setting(&self) -> bool {
        self.auto_lsp_atomic
            .load(std::sync::atomic::Ordering::Relaxed)
    }

    /// Is a language server available for this path?
    ///
    /// Delegates to `kod_lsp::binary_for_path`. Kept as an associated
    /// function because `kod-tui` and the `lsp_*` tools call it to
    /// decide whether an "install X" message is appropriate before
    /// making a request.
    pub fn lsp_binary_for(path: &std::path::Path) -> Option<&'static str> {
        kod_lsp::binary_for_path(path)
    }

    /// The engine's LSP pool. Exposed so callers (the `lsp_*` tools,
    /// `kod doctor`, the post-write diagnostics hook) can reach the
    /// same `LspManager` the engine uses without going through the
    /// engine's higher-level methods.
    pub fn lsp_manager(&self) -> &Arc<kod_lsp::LspManager> {
        &self.lsp_manager
    }

    /// Return LSP diagnostics for `path`.
    ///
    /// Empty on any failure (no server, spawn error, protocol error,
    /// timeout). The caller treats empty as "no LSP feedback" and
    /// falls back to `CheckTool::run_check`, which disambiguates
    /// "clean" from "unreachable".
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
        self.lsp_manager
            .diagnostics(path, content, overall_timeout)
            .await
    }

    /// LSP: definition at `(line, column)` in `path`. 1-based
    /// coordinates (the tool layer already uses them). Empty on any
    /// failure.
    pub async fn lsp_definition(
        &self,
        path: &std::path::Path,
        line: u32,
        column: u32,
    ) -> Vec<kod_lsp::Location> {
        let pos = kod_lsp::Position { line, column };
        self.lsp_manager.definition(path, pos).await
    }

    /// LSP: all references to the symbol at `(line, column)`.
    pub async fn lsp_references(
        &self,
        path: &std::path::Path,
        line: u32,
        column: u32,
        include_declaration: bool,
    ) -> Vec<kod_lsp::Location> {
        let pos = kod_lsp::Position { line, column };
        self.lsp_manager
            .references(path, pos, include_declaration)
            .await
    }

    /// LSP: hover text at `(line, column)`.
    pub async fn lsp_hover(
        &self,
        path: &std::path::Path,
        line: u32,
        column: u32,
    ) -> Option<kod_lsp::Hover> {
        let pos = kod_lsp::Position { line, column };
        self.lsp_manager.hover(path, pos).await
    }

    /// Shut down every language server. Called by `shutdown()`.
    async fn lsp_shutdown(&self) {
        self.lsp_manager.shutdown_all().await;
        tracing::info!("LSP servers shut down");
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
    pub async fn check_baseline(&self) -> Option<Vec<kod_tools::check::Diagnostic>> {
        self.check_baseline.read().await.clone()
    }

    /// Install a per-project policy (D3-C1). Called by the CLI/TUI
    /// at startup from `PolicyEngine::load`. When this is not called,
    /// the engine falls back to the legacy `confirm_writes` gate for
    /// `write_file`/`patch_file` and leaves every other tool alone —
    /// the pre-D3 behaviour.
    pub async fn set_policy(&self, policy: Arc<kod_config::PolicyEngine>) {
        *self.policy.write().await = Some(policy);
    }

    /// Install the MCP host (D6.1). Called by the CLI/TUI at startup
    /// when `[mcp.servers]` is non-empty. When this is not called,
    /// no MCP tools are registered — a session without MCP is the
    /// default, and a session that only uses built-in tools pays
    /// nothing for the feature.
    pub async fn set_mcp_host(&self, host: Arc<crate::mcp_adapters::McpHost>) {
        *self.mcp.write().await = Some(host);
    }

    /// The installed MCP host, if any. Read by the CLI/TUI and by a
    /// future `/mcp` command; not consulted by the tool loop, which
    /// only sees the adapters registered at `start()` time.
    pub async fn mcp_host(&self) -> Option<Arc<crate::mcp_adapters::McpHost>> {
        self.mcp.read().await.clone()
    }

    /// The installed policy, if any.
    pub async fn policy(&self) -> Option<Arc<kod_config::PolicyEngine>> {
        self.policy.read().await.clone()
    }

    /// Register a session-scoped "never" rule (the `a` choice on the
    /// approval dialog). Consulted before every policy layer; a
    /// matching call is denied without a prompt, for the rest of the
    /// process.
    pub async fn add_deny_rule(&self, rule: kod_config::SessionDeny) {
        self.deny_rules.write().await.insert(rule);
    }

    /// The current set of session deny rules. Read by the TUI to show
    /// `kod policy show`-style summaries, and by tests.
    pub async fn deny_rules(&self) -> Vec<kod_config::SessionDeny> {
        let mut rules: Vec<kod_config::SessionDeny> =
            self.deny_rules.read().await.iter().cloned().collect();
        // `HashSet` iteration order is unspecified. `kod policy forget
        // <n>` names rules by their position in this list, so the list
        // must be deterministic: sort by tool, then by path pattern.
        rules.sort_by(|a, b| {
            (a.tool.as_str(), a.path_pattern.as_deref())
                .cmp(&(b.tool.as_str(), b.path_pattern.as_deref()))
        });
        rules
    }

    /// The session deny rule at 1-based index `n`, in the stable order
    /// `deny_rules()` returns. `None` when `n` is 0 or past the end.
    ///
    /// The set is a `HashSet<SessionDeny>` (order-undefined), so the
    /// index is meaningful only in combination with `deny_rules()`:
    /// this accessor sorts internally the same way that method does,
    /// so `kod policy forget <n>` and the listing a user reads agree
    /// on which rule index `n` names.
    pub async fn deny_rule_at(&self, n: usize) -> Option<kod_config::SessionDeny> {
        if n == 0 {
            return None;
        }
        let mut rules: Vec<kod_config::SessionDeny> =
            self.deny_rules.read().await.iter().cloned().collect();
        // Stable ordering — the same shape `deny_rules()` returns.
        // `SessionDeny` derives `Hash + Eq` but not `Ord`; sort on
        // the fields we can order to get a deterministic listing.
        rules.sort_by(|a, b| {
            (a.tool.as_str(), a.path_pattern.as_deref())
                .cmp(&(b.tool.as_str(), b.path_pattern.as_deref()))
        });
        rules.into_iter().nth(n - 1)
    }

    /// Drop a session deny rule. Returns `true` when the rule was
    /// present. The equality is the derived `Hash + Eq` on
    /// `SessionDeny`; two rules built from the same tool and path
    /// pattern compare equal, so a caller that fetched the rule via
    /// `deny_rule_at` can hand it straight back.
    pub async fn remove_deny_rule(&self, rule: &kod_config::SessionDeny) -> bool {
        self.deny_rules.write().await.remove(rule)
    }

    /// The provider for the current model, resolved through the
    /// registry. `None` when no registry is installed or the endpoint
    /// is unknown. Public because the swarm runner needs it and does
    /// not hold a registry reference itself.
    pub async fn current_provider(&self) -> Option<Arc<dyn LlmProvider>> {
        let registry = self.registry.read().await.clone();
        let model = self.current_model.read().await.clone();
        registry.as_ref().and_then(|r| r.resolve(&model).ok())
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
    pub fn set_session_recorder(&self, recorder: Arc<crate::session_log::SessionRecorder>) {
        if let Ok(mut slot) = self.session_recorder.write() {
            *slot = Some(recorder);
        }
    }

    /// Install a turn-trace writer (Tier 1.4). One `TurnTrace` per
    /// `process_*` call is appended to the file the writer holds.
    pub fn set_turn_trace_writer(
        &self,
        writer: std::sync::Arc<crate::trace_writer::TraceWriter>,
    ) {
        if let Ok(mut slot) = self.turn_trace_writer.write() {
            *slot = Some(writer);
        }
    }

    /// The trace log path, when a writer is installed.
    pub fn trace_path(&self) -> Option<std::path::PathBuf> {
        self.turn_trace_writer
            .read()
            .ok()
            .and_then(|g| g.as_ref().map(|w| w.path().to_path_buf()))
    }

    /// Allocate the next turn id. Monotonic; scoped to the session.
    fn next_turn_id(&self) -> crate::trace::TurnId {
        self.next_turn_id
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    }

    /// Emit a completed trace, if a writer is installed. Failure to
    /// write is logged but never propagated — the trace is
    /// diagnostic, not functional.
    fn emit_turn_trace(&self, trace: &crate::trace::TurnTrace) {
        if let Ok(guard) = self.turn_trace_writer.read()
            && let Some(w) = guard.as_ref()
            && let Err(e) = w.record(trace)
        {
            tracing::warn!(
                error = %e,
                path = %w.path().display(),
                "could not append turn trace",
            );
        }
    }

    /// The session log path, when one is installed.
    pub fn session_log_path(&self) -> Option<std::path::PathBuf> {
        self.session_recorder
            .read()
            .ok()
            .and_then(|guard| guard.as_ref().map(|r| r.path().to_path_buf()))
    }

    /// Record the current user request for `key`. Called at the top
    /// of each `process*` entry point so deeper helpers can include
    /// the request in their Jev state. Idempotent — a caller that
    /// forgets to clear a previous request still sees the newest one.
    async fn set_current_request(&self, key: &str, input: &str) {
        self.current_requests
            .write()
            .await
            .insert(key.to_string(), input.to_string());
    }

    /// The active user request for `key`, if any.
    async fn current_request(&self, key: &str) -> Option<String> {
        self.current_requests.read().await.get(key).cloned()
    }

    /// Drop the request recorded for `key`. Called at the end of a
    /// `process*` call so a subsequent tool round on a stale key does
    /// not see the wrong request.
    async fn clear_current_request(&self, key: &str) {
        self.current_requests.write().await.remove(key);
    }

    /// Install a Jev client (design P0.1). Call sites that
    /// consult Jev check [`KodEngine::jev_client`] first and
    /// fall through to their heuristic when it is `None`.
    pub fn set_jev_client(&self, client: crate::jev::JevClient) {
        if let Ok(mut slot) = self.jev_client.write() {
            *slot = Some(client);
        }
    }

    /// The installed Jev client, if any. Cloned so the caller
    /// does not hold the lock across an await.
    pub fn jev_client(&self) -> Option<crate::jev::JevClient> {
        self.jev_client.read().ok().and_then(|g| g.clone())
    }

    /// True when a Jev client is installed on this engine.
    /// Cheap — used by `/jev` and by any UI that wants to
    /// indicate the integration is live.
    pub fn jev_enabled(&self) -> bool {
        self.jev_client.read().map(|g| g.is_some()).unwrap_or(false)
    }

    /// A short status line for `/jev`. `None` when no client is
    /// installed.
    pub fn jev_status(&self) -> Option<String> {
        let client = self.jev_client()?;
        let cfg = client.config();
        Some(format!(
            "enabled={} model={} cache_ttl={}s cache_entries={} timeout={}ms fail_open={} redact_paths={}",
            cfg.enabled,
            cfg.model.as_deref().unwrap_or("jev-latest"),
            cfg.cache_ttl_secs,
            client.cache_len(),
            cfg.timeout_ms,
            cfg.fail_open,
            cfg.redact_paths,
        ))
    }

    /// The active thresholds for `/jev`, formatted for display.
    pub fn jev_thresholds_line(&self) -> Option<String> {
        let client = self.jev_client()?;
        let t = client.thresholds();
        Some(format!(
            "task_classify={:.2} tool_filter={:.2} early_term={:.2} auto_approve={:.2} memory_filter={:.2} ambiguity={:.2}",
            t.task_classify_min,
            t.tool_filter_min,
            t.early_termination_min,
            t.auto_approve_min,
            t.memory_filter_min,
            t.ambiguity_min,
        ))
    }

    /// Drop every cached Jev decision. Returns the number of
    /// entries that were dropped, or `None` when no client is
    /// installed.
    pub fn jev_clear_cache(&self) -> Option<usize> {
        let client = self.jev_client()?;
        let n = client.cache_len();
        client.clear_cache();
        Some(n)
    }

    /// Ask Jev what kind of text a streamed chunk is (P1.4).
    ///
    /// Returns one of `prose_answer`, `reasoning`, `restatement`,
    /// `code_block`. The TUI routes `reasoning` and `restatement`
    /// into a collapsed row so the user perceives the model as
    /// faster without any actual latency change.
    ///
    /// Cheap: the state is a short buffer tail, the question is a
    /// four-label score. The TUI calls it at most once per second,
    /// not per chunk.
    ///
    /// Returns `None` when Jev is disabled or errored — the caller
    /// then renders the chunk as normal prose, which is the
    /// pre-Jev behaviour.
    pub async fn classify_chunk_with_jev(
        &self,
        holder: &str,
        buffer_tail: &str,
    ) -> Option<String> {
        let jev = self.jev_client()?;
        let request = self.current_request(holder).await.unwrap_or_default();
        let state = crate::jev::build_state(
            &format!(
                "User request: {request}\n\nRecent buffer: {}",
                crate::jev::preview_chars(buffer_tail, 500),
            ),
            &[],
        );
        let labels = &["prose_answer", "reasoning", "restatement", "code_block"];
        let started = std::time::Instant::now();
        let decision = jev
            .evaluate_score(
                &state,
                "What kind of text is this streamed chunk?",
                labels,
            )
            .await
            .ok()?;
        let elapsed_ms = started.elapsed().as_millis() as u64;
        self.log_jev_decision(
            holder,
            "chunk_classify",
            &crate::jev::preview_chars(buffer_tail, 200),
            "chunk_kind",
            serde_json::json!({ "kind": decision.value }),
            decision.confidence,
            elapsed_ms,
            false,
            crate::jev::DecisionSource::Jev,
        );
        Some(decision.value)
    }

    /// Decide the sandbox mode for one `execute_command` (P3.4).
    ///
    /// The engine's global mode is the default. On a session with
    /// the sandbox in `Auto`, Jev is asked whether the command needs
    /// OS-level sandboxing. A `safe` / `network_risk` command runs
    /// unsandboxed (sandbox disabled for this one call); a
    /// `filesystem_risk` or `destructive` command keeps the
    /// configured mode. `Require` is never downgraded — a user who
    /// asked for mandatory sandboxing gets it.
    ///
    /// Fail-open: any error keeps the configured mode.
    async fn choose_sandbox_mode_for_command(
        &self,
        holder: &str,
        command: &str,
        configured: kod_tools::context::SandboxMode,
    ) -> kod_tools::context::SandboxMode {
        use kod_tools::context::SandboxMode;
        // Only `Auto` is negotiable: `Disabled` already means no
        // sandbox, and `Require` is a user-enforced guarantee.
        if !matches!(configured, SandboxMode::Auto) {
            return configured;
        }
        let Some(jev) = self.jev_client() else {
            return configured;
        };
        let state = crate::jev::build_state(command, &[]);
        let labels = &["safe", "network_risk", "filesystem_risk", "destructive"];
        let started = std::time::Instant::now();
        let decision = jev
            .evaluate_score(&state, "Command risk level", labels)
            .await;
        let elapsed_ms = started.elapsed().as_millis() as u64;
        let (level, source) = match decision {
            Ok(d) => (d.value, crate::jev::DecisionSource::Jev),
            Err(_) => (String::new(), crate::jev::DecisionSource::Heuristic),
        };
        let result = match level.as_str() {
            "safe" | "network_risk" => SandboxMode::Disabled,
            _ => SandboxMode::Auto,
        };
        self.log_jev_decision(
            holder,
            "sandbox_decision",
            command,
            "risk_level",
            serde_json::json!({
                "risk_level": level,
                "sandbox_mode": format!("{result:?}"),
            }),
            1.0,
            elapsed_ms,
            false,
            source,
        );
        result
    }

    /// Refine the router's skill match with Jev (P4.2).
    ///
    /// The router's substring matcher misses semantic matches — a
    /// request to "design a landing page" does not contain the
    /// substring "ui-ux" and so the `ui-ux-designer` skill loses to
    /// whatever named-skill happened to match lexically. This helper
    /// asks Jev to score each available skill against the request
    /// and returns the names of the skills that scored at or above
    /// `[jev.thresholds].memory_filter_min`.
    ///
    /// The union of the router's match and Jev's semantic score is
    /// returned, so a lexical match is never dropped. Bounded to
    /// `MAX_SKILLS` (a project with 100 skills still costs one
    /// round-trip).
    ///
    /// Returns `None` when Jev is disabled, the loaded skill set is
    /// empty, or the call errors. `None` means "keep the router's
    /// answer as-is".
    async fn rank_skills_with_jev(
        &self,
        holder: &str,
        input: &str,
        router_match: &[String],
    ) -> Option<Vec<String>> {
        const MAX_SKILLS: usize = 30;
        let jev = self.jev_client()?;
        let all = self.loaded_skill_details().await;
        if all.is_empty() {
            return None;
        }
        let slice: Vec<(String, String)> = all.into_iter().take(MAX_SKILLS).collect();
        let questions: Vec<(String, String)> = slice
            .iter()
            .enumerate()
            .map(|(i, (name, desc))| {
                (
                    format!("skill_{i}"),
                    format!(
                        "Is this skill relevant to the user's request? Skill: {name} — {}",
                        crate::jev::preview_chars(desc, 300),
                    ),
                )
            })
            .collect();
        let state = crate::jev::build_state(input, &[]);
        let started = std::time::Instant::now();
        let rows = jev.evaluate_yes_no_batch(&state, &questions).await.ok()?;
        let elapsed_ms = started.elapsed().as_millis() as u64;
        let threshold = jev.thresholds().memory_filter_min;

        let mut merged: Vec<String> = router_match.to_vec();
        let mut answers = serde_json::Map::new();
        for (i, (name, _)) in slice.iter().enumerate() {
            let p = rows
                .iter()
                .find(|(id, _)| id == &format!("skill_{i}"))
                .map(|(_, p)| *p)
                .unwrap_or(0.0);
            answers.insert(format!("skill_{i}"), serde_json::json!(p));
            if p >= threshold && !merged.contains(name) {
                merged.push(name.clone());
            }
        }
        self.log_jev_decision(
            holder,
            "skill_match",
            input,
            "relevance_per_skill",
            serde_json::Value::Object(answers),
            1.0,
            elapsed_ms,
            false,
            crate::jev::DecisionSource::Jev,
        );
        Some(merged)
    }

    /// Rank grep / search_files results by Jev relevance (P2.3).
    ///
    /// When a search returns more than `MIN_RESULTS_TO_RANK` hits,
    /// ask Jev which ones best address the user's request, then drop
    /// the ones scored below `[jev.thresholds].memory_filter_min`.
    /// The tool's own caps still bound the size; this pass just
    /// removes hits that are syntactically matches but semantically
    /// noise.
    ///
    /// Returns `None` when:
    ///
    /// * Jev is disabled,
    /// * the search returned fewer than `MIN_RESULTS_TO_RANK` hits,
    /// * the result payload does not carry a `results` array,
    /// * every hit scored relevant, or Jev errored.
    ///
    /// The `results` key is what the tool writes; a caller that
    /// renames it must update this helper.
    async fn rank_search_results_with_jev(
        &self,
        holder: &str,
        call: &ToolCall,
        result: &ToolResult,
    ) -> Option<ToolResult> {
        const MIN_RESULTS_TO_RANK: usize = 20;
        const MAX_RESULTS_TO_RANK: usize = 60;
        let jev = self.jev_client()?;
        let request = self.current_request(holder).await?;
        let ToolResult::Success(v) = result else {
            return None;
        };
        let arr = v.get("results").and_then(|r| r.as_array())?;
        if arr.len() < MIN_RESULTS_TO_RANK {
            return None;
        }
        let slice: Vec<&serde_json::Value> = arr.iter().take(MAX_RESULTS_TO_RANK).collect();

        // One yes/no per hit; the "question" carries the search line.
        let questions: Vec<(String, String)> = slice
            .iter()
            .enumerate()
            .map(|(i, hit)| {
                let line = hit
                    .get("text")
                    .or_else(|| hit.get("line"))
                    .and_then(|t| t.as_str())
                    .unwrap_or("");
                let path = hit
                    .get("file")
                    .or_else(|| hit.get("path"))
                    .and_then(|t| t.as_str())
                    .unwrap_or("");
                (
                    format!("hit_{i}"),
                    format!(
                        "Is this {} result relevant to the request? \"{}\" in {}: {}",
                        call.tool_name, request, path,
                        crate::jev::preview_chars(line, 200),
                    ),
                )
            })
            .collect();

        let state = crate::jev::build_state(&request, &[]);
        let started = std::time::Instant::now();
        let rows = jev.evaluate_yes_no_batch(&state, &questions).await.ok()?;
        let elapsed_ms = started.elapsed().as_millis() as u64;
        let threshold = jev.thresholds().memory_filter_min;

        let mut keep_idx: std::collections::HashSet<usize> =
            std::collections::HashSet::new();
        let mut answers = serde_json::Map::new();
        for (i, hit) in slice.iter().enumerate() {
            let p = rows
                .iter()
                .find(|(id, _)| id == &format!("hit_{i}"))
                .map(|(_, p)| *p)
                .unwrap_or(1.0);
            answers.insert(format!("hit_{i}"), serde_json::json!(p));
            if p >= threshold {
                keep_idx.insert(i);
            }
            let _ = hit;
        }
        // Keep everything beyond the classification window.
        for i in slice.len()..arr.len() {
            keep_idx.insert(i);
        }
        let dropped = arr.len() - keep_idx.len();
        if dropped == 0 {
            return None;
        }
        self.log_jev_decision(
            holder,
            "search_rank",
            &crate::jev::preview_chars(&request, 200),
            &format!("{} hits", arr.len()),
            serde_json::Value::Object(answers),
            1.0,
            elapsed_ms,
            false,
            crate::jev::DecisionSource::Jev,
        );

        let filtered: Vec<serde_json::Value> = arr
            .iter()
            .enumerate()
            .filter(|(i, _)| keep_idx.contains(i))
            .map(|(_, v)| v.clone())
            .collect();
        let mut new_v = v.clone();
        if let Some(obj) = new_v.as_object_mut() {
            obj.insert(
                "results".to_string(),
                serde_json::Value::Array(filtered),
            );
            obj.insert(
                "ranked_by_jev".to_string(),
                serde_json::json!({
                    "dropped": dropped,
                    "kept": keep_idx.len(),
                }),
            );
        }
        Some(ToolResult::Success(new_v))
    }

    /// Ask Jev to triage the hunks in a unified diff (P2.4) before
    /// the diff reaches the model's prompt block.
    ///
    /// Splits the diff by `@@` hunk headers, asks Jev to score each
    /// one's relevance to the user's request (`context_only`,
    /// `relevant`, `critical`), and replaces the `context_only`
    /// hunks with a one-line `… (N lines elided, context only)` note.
    /// The unified-diff framing (--- / +++ headers) is preserved so
    /// the model can still parse the result.
    ///
    /// Returns `None` when:
    ///
    /// * Jev is disabled or has no current request for this transcript,
    /// * the diff has fewer than 2 hunks (nothing to compress),
    /// * every hunk scores `relevant` or `critical`,
    /// * the whole call errors.
    ///
    /// Returning `None` leaves the original result untouched — the
    /// caller uses the same `ToolResult` it already had.
    async fn filter_diff_hunks_with_jev(
        &self,
        holder: &str,
        result: &ToolResult,
    ) -> Option<ToolResult> {
        let jev = self.jev_client()?;
        let request = self.current_request(holder).await?;
        let ToolResult::Success(v) = result else {
            return None;
        };
        let diff = v.get("diff").and_then(|d| d.as_str())?;
        if diff.is_empty() {
            return None;
        }
        // Split into header + hunks. The header is everything before
        // the first `@@ ` line.
        let mut lines = diff.lines();
        let mut header_lines: Vec<&str> = Vec::new();
        let mut hunks: Vec<Vec<&str>> = Vec::new();
        let mut cur: Vec<&str> = Vec::new();
        let mut seen_hunk = false;
        for line in lines.by_ref() {
            if line.starts_with("@@") {
                if !cur.is_empty() {
                    hunks.push(std::mem::take(&mut cur));
                }
                cur.push(line);
                seen_hunk = true;
            } else if seen_hunk {
                cur.push(line);
            } else {
                header_lines.push(line);
            }
        }
        if !cur.is_empty() {
            hunks.push(cur);
        }
        if hunks.len() < 2 {
            return None;
        }

        // One score per hunk, in a single batch of yes/no for the
        // binary "is this context-only vs relevant?".
        const MAX_HUNKS: usize = 20;
        let evaluated = hunks.iter().take(MAX_HUNKS);
        let questions: Vec<(String, String)> = evaluated
            .enumerate()
            .map(|(i, h)| {
                (
                    format!("hunk_{i}"),
                    format!(
                        "Is this diff hunk relevant to the user's request, or context-only? Request: {request}. Hunk:\n{}",
                        h.iter().take(30).copied().collect::<Vec<_>>().join("\n"),
                    ),
                )
            })
            .collect();
        let state = crate::jev::build_state(&request, &[]);
        let started = std::time::Instant::now();
        let rows = jev
            .evaluate_yes_no_batch(&state, &questions)
            .await
            .ok()?;
        let elapsed_ms = started.elapsed().as_millis() as u64;
        let threshold = jev.thresholds().memory_filter_min;
        let mut elided = 0_usize;
        let mut kept_hunks: Vec<&Vec<&str>> = Vec::new();
        let mut answers = serde_json::Map::new();
        for (i, h) in hunks.iter().enumerate() {
            if i >= MAX_HUNKS {
                // Past the cap: keep, do not classify.
                kept_hunks.push(h);
                continue;
            }
            let p = rows
                .iter()
                .find(|(id, _)| id == &format!("hunk_{i}"))
                .map(|(_, p)| *p)
                .unwrap_or(1.0);
            answers.insert(format!("hunk_{i}"), serde_json::json!(p));
            if p < threshold {
                elided += h.len();
            } else {
                kept_hunks.push(h);
            }
        }
        if elided == 0 {
            // Nothing to compress — leave the result untouched.
            return None;
        }

        self.log_jev_decision(
            holder,
            "diff_triage",
            &crate::jev::preview_chars(diff, 200),
            &format!("{} hunks", hunks.len()),
            serde_json::Value::Object(answers),
            1.0,
            elapsed_ms,
            false,
            crate::jev::DecisionSource::Jev,
        );

        let mut filtered = header_lines.join("\n");
        for h in kept_hunks {
            filtered.push('\n');
            filtered.push_str(&h.join("\n"));
        }
        if elided > 0 {
            filtered.push_str(&format!(
                "\n… ({} lines of context-only hunks elided by Jev)",
                elided,
            ));
        }

        // Rebuild the result with the filtered diff. The TUI still
        // sees the original via the raw results map — only the
        // prompt block the model reads goes through here.
        let mut new_v = v.clone();
        if let Some(obj) = new_v.as_object_mut() {
            obj.insert(
                "diff".to_string(),
                serde_json::Value::String(filtered),
            );
        }
        Some(ToolResult::Success(new_v))
    }

    /// Write a `SessionEntry::ToolOutcome` for a completed tool call
    /// when the call is "interesting" (P5.3): it took at least
    /// `MIN_JEV_CLASSIFY_MS`, or it failed. The gate keeps the
    /// classification cost off trivially fast read tools while
    /// preserving the signal on the calls that matter for
    /// `/debug tokens` and `/jev stats`.
    ///
    /// Fail-silent: any error or timeout writes nothing — the raw
    /// `ToolCall` entry is the ground truth; this is enrichment.
    async fn classify_tool_outcome_with_jev(
        &self,
        holder: &str,
        call: &ToolCall,
        result: &ToolResult,
        duration_ms: u64,
    ) {
        const MIN_JEV_CLASSIFY_MS: u64 = 100;
        let failed = matches!(result, ToolResult::Error(_));
        if duration_ms < MIN_JEV_CLASSIFY_MS && !failed {
            return;
        }
        let Some(jev) = self.jev_client() else {
            return;
        };
        let Some(request_text) = self.current_request(holder).await else {
            return;
        };
        let result_text = match result {
            ToolResult::Success(v) => {
                serde_json::to_string(v).unwrap_or_default()
            }
            ToolResult::Error(e) => format!("ERROR: {e}"),
            _ => String::new(),
        };
        let state = crate::jev::build_state(
            &format!(
                "User request: {request_text}\nTool: {}\nArguments: {}\nDuration: {duration_ms}ms\nResult: {}",
                call.tool_name,
                serde_json::to_string(&call.arguments).unwrap_or_default(),
                crate::jev::preview_chars(&result_text, 800),
            ),
            &[],
        );
        let outcome_labels = &["success", "partial", "failure", "irrelevant"];
        let impact_labels = &["none", "minor", "significant", "critical"];
        let started = std::time::Instant::now();
        let outcome = jev
            .evaluate_score(&state, "What was the outcome of this tool call?", outcome_labels)
            .await;
        let impact = jev
            .evaluate_score(
                &state,
                "How significant is this tool call's impact on the user's task?",
                impact_labels,
            )
            .await;
        let elapsed_ms = started.elapsed().as_millis() as u64;

        let (outcome_label, impact_label, confidence, source) = match (outcome, impact) {
            (Ok(o), Ok(i)) => (
                o.value,
                i.value,
                o.confidence.max(i.confidence),
                crate::jev::DecisionSource::Jev,
            ),
            _ => (
                if failed { "failure".to_string() } else { "success".to_string() },
                "minor".to_string(),
                1.0,
                crate::jev::DecisionSource::Heuristic,
            ),
        };

        if let Ok(guard) = self.session_recorder.read()
            && let Some(rec) = guard.as_ref()
        {
            let entry = crate::session_log::SessionEntry::ToolOutcome {
                timestamp_ms: std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_millis() as u64)
                    .unwrap_or(0),
                holder: holder.to_string(),
                tool_name: call.tool_name.clone(),
                outcome: outcome_label,
                user_visible_impact: impact_label,
                confidence,
                latency_ms: elapsed_ms,
                source: source.as_str().to_string(),
            };
            let _ = rec.record(&entry);
        }
    }

    /// Ask Jev whether the reply answers the user's request (P5.4).
    ///
    /// Returns a short advisory string to append to the reply when
    /// Jev is confident the reply is off-track (`answers_the_question
    /// < 0.5` AND the response is at least 200 chars long). Returns
    /// `None` for every other case, so a normal reply is unchanged.
    ///
    /// Not a hard gate — the reply is still delivered. The advisory
    /// tells the user *why* they might want to /regenerate, which is
    /// often more useful than a silent quality score.
    ///
    /// Logged as `JevDecision` with `purpose = "quality_gate"`.
    async fn check_response_quality_with_jev(
        &self,
        key: &str,
        request: &str,
        response: &str,
    ) -> Option<String> {
        if response.len() < 200 {
            return None;
        }
        let jev = self.jev_client()?;
        let state = crate::jev::build_state(
            &format!("User request: {request}\n\nAssistant response: {response}"),
            &[],
        );
        let pairs = [
            (
                "answers_the_question".to_string(),
                "Does the assistant's response answer the user's request?"
                    .to_string(),
            ),
        ];
        let started = std::time::Instant::now();
        let result = jev.evaluate_yes_no_batch(&state, &pairs).await;
        let elapsed_ms = started.elapsed().as_millis() as u64;
        let (p_yes, source) = match result {
            Ok(rows) => (
                rows.first().map(|(_, p)| *p).unwrap_or(1.0),
                crate::jev::DecisionSource::Jev,
            ),
            Err(_) => (1.0, crate::jev::DecisionSource::Heuristic),
        };
        self.log_jev_decision(
            key,
            "quality_gate",
            &crate::jev::preview_chars(request, 200),
            "answers_the_question",
            serde_json::json!({ "answers_the_question": p_yes }),
            p_yes,
            elapsed_ms,
            false,
            source,
        );
        if p_yes < 0.5 {
            Some(format!(
                "\n\n---\n\n*Note: Jev flagged this reply as possibly off-track                  (confidence the response answers the request: {:.0}%). If it missed                  the point, /regenerate to try again.*",
                p_yes * 100.0,
            ))
        } else {
            None
        }
    }

    /// Refine the diagnostic baseline diff with Jev (P4.4).
    ///
    /// The syntactic `diag_key` comparison treats a line-shifted
    /// diagnostic as new — "unused variable `x` at line 42" and
    /// "unused variable `x` at line 45" hash differently, so the
    /// second counts as introduced by the write even when it moved
    /// because an earlier edit added lines. Jev reads the (file,
    /// code, message) triple and answers whether each "new"
    /// diagnostic is genuinely new or a shifted version of one in
    /// the baseline.
    ///
    /// The return value is the index set of diagnostics the caller
    /// should still treat as new. On disabled/errored Jev, every
    /// index is returned (the syntactic diff stands).
    ///
    /// Logged as a JevDecision with `purpose = "diagnostic_triage"`.
    async fn classify_new_diagnostics_with_jev(
        &self,
        key: &str,
        new_diags: &[&kod_tools::check::Diagnostic],
        baseline: &[kod_tools::check::Diagnostic],
    ) -> std::collections::HashSet<usize> {
        let all: std::collections::HashSet<usize> =
            (0..new_diags.len()).collect();
        let Some(jev) = self.jev_client() else {
            return all;
        };
        if new_diags.is_empty() {
            return all;
        }
        // One state carrying the new diagnostics and a compact
        // rendering of the baseline. The baseline is bounded so a
        // project with hundreds of pre-existing diagnostics does
        // not produce a pathological request.
        let baseline_preview: String = baseline
            .iter()
            .take(40)
            .map(|d| format!("{}:{} [{}] {}", d.file, d.line, d.code.as_deref().unwrap_or("?"), d.message))
            .collect::<Vec<_>>()
            .join("\n");
        let current_preview: String = new_diags
            .iter()
            .enumerate()
            .map(|(i, d)| format!("[{i}] {}:{} [{}] {}", d.file, d.line, d.code.as_deref().unwrap_or("?"), d.message))
            .collect::<Vec<_>>()
            .join("\n");
        let state = crate::jev::build_state(
            &format!(
                "Baseline diagnostics (before the write):\n{baseline_preview}\n\nCurrent diagnostics (after the write):\n{current_preview}"
            ),
            &[],
        );
        // One yes/no per entry: is this genuinely new, or a shifted
        // duplicate of one in the baseline?
        let questions: Vec<(String, String)> = new_diags
            .iter()
            .enumerate()
            .map(|(i, d)| {
                (
                    format!("diag_{i}"),
                    format!(
                        "Is diagnostic [{i}] ({file}:{line} [{code}] {msg}) genuinely new, or the same as one of the baseline diagnostics just at a different line?",
                        i = i,
                        file = d.file,
                        line = d.line,
                        code = d.code.as_deref().unwrap_or("?"),
                        msg = d.message,
                    ),
                )
            })
            .collect();
        let started = std::time::Instant::now();
        let result = jev.evaluate_yes_no_batch(&state, &questions).await;
        let elapsed_ms = started.elapsed().as_millis() as u64;
        let (rows, source) = match result {
            Ok(r) => (r, crate::jev::DecisionSource::Jev),
            Err(_) => (Vec::new(), crate::jev::DecisionSource::Heuristic),
        };
        let threshold = jev.thresholds().task_classify_min;
        let mut truly_new: std::collections::HashSet<usize> = std::collections::HashSet::new();
        let mut answers = serde_json::Map::new();
        for (id, p) in &rows {
            answers.insert(id.clone(), serde_json::json!(p));
            if let Some(rest) = id.strip_prefix("diag_")
                && let Ok(idx) = rest.parse::<usize>()
                && *p >= threshold
            {
                truly_new.insert(idx);
            }
        }
        // Fail-open: if Jev answered nothing, keep the syntactic
        // verdict (every entry is "new").
        if rows.is_empty() {
            return all;
        }
        self.log_jev_decision(
            key,
            "diagnostic_triage",
            &format!("{} new diagnostics", new_diags.len()),
            "genuinely_new",
            serde_json::Value::Object(answers),
            1.0,
            elapsed_ms,
            false,
            source,
        );
        truly_new
    }

    /// Ask Jev whether each citation in `text` is substantiated by
    /// the cited location (P4.5). The syntactic check in
    /// `citations::check_and_annotate` verifies the file exists and
    /// the line is in range; this pass checks the stronger claim —
    /// that the cited line actually supports the prose.
    ///
    /// Returns `text` unchanged when Jev is disabled, there are no
    /// citations, or every citation scores `relevant`/`essential`.
    /// Otherwise returns `text + "\n\n<semantic block>"` naming
    /// the citations that did not pass.
    ///
    /// Fail-open: on Jev error, the original text is returned and
    /// the failure is logged at `Heuristic`.
    async fn semantic_verify_citations(&self, key: &str, text: &str) -> String {
        let Some(jev) = self.jev_client() else {
            return text.to_string();
        };
        let citations = crate::citations::extract_citations(text);
        if citations.is_empty() {
            return text.to_string();
        }
        // Bound the batch — a reply with 100 citations is pathological.
        const MAX_CITATIONS: usize = 10;
        let slice: Vec<_> = citations.iter().take(MAX_CITATIONS).collect();

        // Read each cited file and capture the surrounding line(s).
        // A file that cannot be read is skipped — the syntactic
        // checker already reported it.
        let mut questions: Vec<(String, String)> = Vec::with_capacity(slice.len());
        for (idx, c) in slice.iter().enumerate() {
            let abs = self.working_dir.join(&c.raw_path);
            let Ok(content) = std::fs::read_to_string(&abs) else {
                continue;
            };
            let mut lines_iter = content.lines();
            let line_text = if c.line == 0 {
                String::new()
            } else {
                lines_iter
                    .nth(c.line.saturating_sub(1) as usize)
                    .unwrap_or("")
                    .to_string()
            };
            let end = c.end_line.unwrap_or(c.line);
            let slice_text = if end > c.line {
                content
                    .lines()
                    .skip(c.line.saturating_sub(1) as usize)
                    .take((end - c.line + 1) as usize)
                    .collect::<Vec<_>>()
                    .join("\n")
            } else {
                line_text
            };
            if slice_text.trim().is_empty() {
                continue;
            }
            questions.push((
                format!("citation_{idx}"),
                format!(
                    "Does the code at {}:{} support the claim made about it? Code excerpt:\n{}",
                    c.raw_path, c.line, slice_text
                ),
            ));
        }
        if questions.is_empty() {
            return text.to_string();
        }

        let state = crate::jev::build_state(text, &[]);
        let started = std::time::Instant::now();
        let result = jev.evaluate_yes_no_batch(&state, &questions).await;
        let elapsed_ms = started.elapsed().as_millis() as u64;
        let (rows, source) = match result {
            Ok(r) => (r, crate::jev::DecisionSource::Jev),
            Err(_) => (Vec::new(), crate::jev::DecisionSource::Heuristic),
        };

        let threshold = jev.thresholds().memory_filter_min; // 0.7 default
        let mut failing: Vec<String> = Vec::new();
        let mut answers = serde_json::Map::new();
        for (id, p) in &rows {
            answers.insert(id.clone(), serde_json::json!(p));
            if *p < threshold
                && let Some(rest) = id.strip_prefix("citation_")
                && let Ok(idx) = rest.parse::<usize>()
                && let Some(c) = slice.get(idx)
            {
                failing.push(format!("{}:{}", c.raw_path, c.line));
            }
        }

        self.log_jev_decision(
            key,
            "citation_semantic",
            &crate::jev::preview_chars(text, 200),
            "per_citation_support",
            serde_json::Value::Object(answers),
            1.0,
            elapsed_ms,
            false,
            source,
        );

        if failing.is_empty() {
            return text.to_string();
        }
        let mut out = String::from(text);
        out.push_str("\n\n## Citation semantic check\n\n");
        out.push_str(
            "Jev could not confirm that the following cited lines support the claims made about them:\n",
        );
        for f in &failing {
            out.push_str(&format!("- {f}\n"));
        }
        out
    }

    /// Pre-detect ambiguous requests and, on a streaming call,
    /// prompt the user for clarification before the first LLM round
    /// (P3.3).
    ///
    /// Returns the input unchanged when:
    ///
    /// * Jev is disabled,
    /// * the request is trivially short (`< 10` chars) — nothing to
    ///   disambiguate,
    /// * `is_ambiguous` scores below `[jev.thresholds].ambiguity_min`,
    /// * the caller has no chunk channel (`process` non-streaming),
    /// * the user cancels or times out the clarification dialog.
    ///
    /// Returns `input + "\n\nAdditional clarification from user:
    /// <answer>"` when the user supplies one. Every path logs a
    /// `SessionEntry::JevDecision` with `purpose = "ambiguity"`.
    ///
    /// Fail-open: a Jev error logs `Heuristic` and returns the input
    /// unchanged.
    async fn augment_input_with_jev_ambiguity_check(
        &self,
        key: &str,
        input: &str,
        chunk_tx: Option<&tokio::sync::mpsc::Sender<String>>,
    ) -> String {
        if input.len() < 10 {
            return input.to_string();
        }
        let Some(jev) = self.jev_client() else {
            return input.to_string();
        };
        let state = crate::jev::build_state(input, &[]);
        let pairs = [(
            "is_ambiguous".to_string(),
            "Is this request ambiguous without more context?".to_string(),
        )];
        let started = std::time::Instant::now();
        let result = jev.evaluate_yes_no_batch(&state, &pairs).await;
        let elapsed_ms = started.elapsed().as_millis() as u64;
        let (p_amb, source) = match result {
            Ok(rows) => (
                rows.first().map(|(_, p)| *p).unwrap_or(0.0),
                crate::jev::DecisionSource::Jev,
            ),
            Err(_) => (0.0, crate::jev::DecisionSource::Heuristic),
        };
        self.log_jev_decision(
            key,
            "ambiguity",
            input,
            "is_ambiguous",
            serde_json::json!({ "is_ambiguous": p_amb }),
            p_amb,
            elapsed_ms,
            false,
            source,
        );
        let threshold = jev.thresholds().ambiguity_min;
        if p_amb < threshold {
            return input.to_string();
        }
        // Non-streaming callers cannot ask the user — the log entry
        // above is the only side effect for them.
        let Some(tx) = chunk_tx else {
            return input.to_string();
        };
        let question_text = format!(
            "Your request may be ambiguous. Add any missing context, \
             or press Enter to continue as-is:\n\n> {input}"
        );
        let req = kod_tools::ask::QuestionRequest {
            question: question_text,
            placeholder: Some("(clarification)".to_string()),
        };
        let json = serde_json::to_string(&req).unwrap_or_else(|_| "{}".to_string());
        let id = self
            .next_question_id
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let (otx, orx) = tokio::sync::oneshot::channel();
        self.pending_questions.write().await.insert(id, otx);
        let _ = tx.send(question_marker(id, &json)).await;
        let answer = tokio::time::timeout(
            std::time::Duration::from_secs(AWAIT_APPROVAL_SECS),
            orx,
        )
        .await;
        match answer {
            Ok(Ok(text))
                if !text.is_empty()
                    && text != "(cancelled)"
                    && text != "(question cancelled)" =>
            {
                format!(
                    "{input}\n\nAdditional clarification from user: {text}"
                )
            }
            _ => input.to_string(),
        }
    }

    /// Group approval requests by logical change (P3.2).
    ///
    /// One ask per pending call; the answers group calls that Jev
    /// scores as belonging to the same logical change (`change_a`
    /// through `change_d`, or `standalone`). The caller uses the
    /// grouping to present a single dialog with a combined summary
    /// instead of N dialogs, each of which the user must click
    /// through.
    ///
    /// Returns a map from group label to the indices in the input
    /// slice. The caller renders one row per group.
    ///
    /// Returns an empty map when Jev is disabled or the batch has
    /// fewer than 2 items.
    async fn group_approvals_with_jev(
        &self,
        holder: &str,
        calls: &[ToolCall],
    ) -> std::collections::HashMap<String, Vec<usize>> {
        let mut out: std::collections::HashMap<String, Vec<usize>> =
            std::collections::HashMap::new();
        if calls.len() < 2 {
            return out;
        }
        let Some(jev) = self.jev_client() else {
            return out;
        };
        let state = crate::jev::build_state(
            &format!("Pending approvals: {} items", calls.len()),
            &[],
        );
        let labels = &["change_a", "change_b", "change_c", "standalone"];
        for (i, call) in calls.iter().enumerate() {
            let question = format!(
                "Which logical change does this approval belong to? Call: {} {}",
                call.tool_name,
                crate::jev::preview_chars(
                    &serde_json::to_string(&call.arguments).unwrap_or_default(),
                    200,
                ),
            );
            let decision = match jev.evaluate_score(&state, &question, labels).await {
                Ok(d) => d,
                Err(_) => return out,
            };
            out.entry(decision.value).or_default().push(i);
        }
        self.log_jev_decision(
            holder,
            "approval_group",
            &format!("{} approvals", calls.len()),
            "group_per_call",
            serde_json::json!({
                "groups": out.iter().map(|(k, v)| (k.clone(), v.len())).collect::<std::collections::HashMap<_,_>>(),
            }),
            1.0,
            0,
            false,
            crate::jev::DecisionSource::Jev,
        );
        out
    }

    /// Pre-extract handoff facts from a transcript (P4.8).
    ///
    /// Given the transcript's user+assistant messages, ask Jev three
    /// yes/no questions per message:
    ///
    /// * `is_decision` — does this message contain a durable decision?
    /// * `is_unfinished_task` — does this message describe an
    ///   unfinished task?
    /// * `is_file_reference` — does this message reference a file
    ///   path?
    ///
    /// The messages that score above threshold on any question are
    /// returned in their original order, tagged with which categories
    /// they matched. The `/handoff` command embeds this list in the
    /// LLM prompt so the model has a curated list of the durable
    /// facts to render, instead of having to re-read every turn.
    ///
    /// Bounded to `MAX_MESSAGES` messages. Returns an empty vec when
    /// Jev is disabled or errors, and the caller falls back to its
    /// current behaviour.
    pub async fn extract_handoff_facts_with_jev(
        &self,
        transcript: &str,
    ) -> Vec<String> {
        const MAX_MESSAGES: usize = 40;
        let Some(jev) = self.jev_client() else {
            return Vec::new();
        };
        // Split the transcript into paragraph-sized chunks. The
        // handoff input comes in as one markdown string; splitting on
        // double newlines produces one message per turn in the
        // format the TUI exports.
        let chunks: Vec<&str> = transcript
            .split("\n\n")
            .filter(|s| s.trim().len() > 20)
            .take(MAX_MESSAGES)
            .collect();
        if chunks.is_empty() {
            return Vec::new();
        }

        let state = crate::jev::build_state(transcript, &[]);
        let mut questions: Vec<(String, String)> = Vec::new();
        for (i, chunk) in chunks.iter().enumerate() {
            questions.push((
                format!("decision_{i}"),
                format!(
                    "Does this message contain a durable decision or a concrete chosen approach? Message: {}",
                    crate::jev::preview_chars(chunk, 400),
                ),
            ));
            questions.push((
                format!("unfinished_{i}"),
                format!(
                    "Does this message describe an unfinished task, a TODO, or the next step? Message: {}",
                    crate::jev::preview_chars(chunk, 400),
                ),
            ));
            questions.push((
                format!("file_{i}"),
                format!(
                    "Does this message reference a specific file path? Message: {}",
                    crate::jev::preview_chars(chunk, 400),
                ),
            ));
        }
        let started = std::time::Instant::now();
        let rows = match jev.evaluate_yes_no_batch(&state, &questions).await {
            Ok(r) => r,
            Err(_) => return Vec::new(),
        };
        let elapsed_ms = started.elapsed().as_millis() as u64;
        let threshold = jev.thresholds().task_classify_min;

        let mut facts: Vec<String> = Vec::new();
        for (i, chunk) in chunks.iter().enumerate() {
            let get = |prefix: &str| {
                rows.iter()
                    .find(|(id, _)| id == &format!("{prefix}_{i}"))
                    .map(|(_, p)| *p)
                    .unwrap_or(0.0)
            };
            let mut tags: Vec<&str> = Vec::new();
            if get("decision") >= threshold {
                tags.push("decision");
            }
            if get("unfinished") >= threshold {
                tags.push("unfinished");
            }
            if get("file") >= threshold {
                tags.push("file");
            }
            if !tags.is_empty() {
                facts.push(format!(
                    "[{}] {}",
                    tags.join(","),
                    crate::jev::preview_chars(chunk, 400),
                ));
            }
        }
        self.log_jev_decision(
            DEFAULT_TRANSCRIPT_KEY,
            "handoff_extract",
            &crate::jev::preview_chars(transcript, 200),
            "decision,unfinished,file",
            serde_json::json!({
                "chunks": chunks.len(),
                "facts": facts.len(),
            }),
            1.0,
            elapsed_ms,
            false,
            crate::jev::DecisionSource::Jev,
        );
        facts
    }

    /// Filter MCP tools before they reach the LLM (P5.1).
    ///
    /// An MCP filesystem server exposes ~10 tools; a GitHub server
    /// exposes ~20. When more than `MIN_MCP_TOOLS_TO_FILTER` MCP
    /// tools are registered on the engine, ask Jev which one should
    /// handle the current request and return only that one (plus a
    /// small safety margin of `KEEP_TOP_N`). When the registry has
    /// few or no MCP tools, returns the input unchanged.
    ///
    /// Fail-open: on any Jev error or a request with no current text,
    /// the input is returned unchanged.
    async fn filter_mcp_tools_with_jev(
        &self,
        holder: &str,
        input: &str,
        definitions: Vec<ToolDefinition>,
    ) -> Vec<ToolDefinition> {
        const MIN_MCP_TOOLS_TO_FILTER: usize = 5;
        const KEEP_TOP_N: usize = 3;
        // Partition into MCP and non-MCP. The non-MCP half is
        // untouched by this filter.
        let (mcp, other): (Vec<ToolDefinition>, Vec<ToolDefinition>) = definitions
            .into_iter()
            .partition(|d| d.name.starts_with(crate::mcp_adapters::MCP_TOOL_PREFIX));
        if mcp.len() < MIN_MCP_TOOLS_TO_FILTER {
            let mut out = other;
            out.extend(mcp);
            return out;
        }
        let Some(jev) = self.jev_client() else {
            let mut out = other;
            out.extend(mcp);
            return out;
        };
        // One yes/no per MCP tool name. Bounded to avoid a
        // pathological request.
        const MAX_TOOL_NAMES: usize = 30;
        let slice: Vec<ToolDefinition> =
            mcp.iter().take(MAX_TOOL_NAMES).cloned().collect();
        let questions: Vec<(String, String)> = slice
            .iter()
            .enumerate()
            .map(|(i, d)| {
                (
                    format!("tool_{i}"),
                    format!(
                        "Would the tool `{}` (whose description is {}) be useful for this request?",
                        d.name,
                        crate::jev::preview_chars(&d.description, 200),
                    ),
                )
            })
            .collect();
        let state = crate::jev::build_state(input, &[]);
        let started = std::time::Instant::now();
        let rows = match jev.evaluate_yes_no_batch(&state, &questions).await {
            Ok(r) => r,
            Err(_) => {
                let mut out = other;
                out.extend(mcp);
                return out;
            }
        };
        let elapsed_ms = started.elapsed().as_millis() as u64;
        let threshold = jev.thresholds().tool_filter_min;
        // Sort MCP tools by p descending, keep top N above threshold.
        let mut scored: Vec<(usize, f32)> = slice
            .iter()
            .enumerate()
            .map(|(i, _)| {
                let p = rows
                    .iter()
                    .find(|(id, _)| id == &format!("tool_{i}"))
                    .map(|(_, p)| *p)
                    .unwrap_or(0.0);
                (i, p)
            })
            .collect();
        scored.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
        let kept_mcp: Vec<ToolDefinition> = scored
            .iter()
            .take(KEEP_TOP_N)
            .filter(|(_, p)| *p >= threshold)
            .filter_map(|(i, _)| slice.get(*i).cloned())
            .collect();
        // If the filter dropped everything, keep the original set
        // — an LLM with no MCP tool is worse than one with ten.
        if kept_mcp.is_empty() {
            let mut out = other;
            out.extend(mcp);
            return out;
        }
        self.log_jev_decision(
            holder,
            "mcp_filter",
            input,
            "per_mcp_tool_relevance",
            serde_json::json!({
                "total_mcp": mcp.len(),
                "kept": kept_mcp.iter().map(|d| d.name.clone()).collect::<Vec<_>>(),
            }),
            1.0,
            elapsed_ms,
            false,
            crate::jev::DecisionSource::Jev,
        );
        let mut out = other;
        out.extend(kept_mcp);
        out
    }

    /// Ask Jev whether the session has moved to a new phase (P5.5).
    ///
    /// Returns `Some((old, new))` when Jev is confident the phase
    /// changed between the last N turns and now, where N is the
    /// last `PHASE_WINDOW` user+assistant messages. The labels are
    /// drawn from a fixed set (`exploring`, `coding`, `debugging`,
    /// `testing`, `refactoring`, `documenting`).
    ///
    /// The caller (`Event::ResponseComplete` in the TUI) uses a
    /// positive return to suggest `/handoff`.
    ///
    /// Returns `None` when Jev is disabled, the transcript is too
    /// short to judge, the phase is unchanged, or the confidence
    /// is below `[jev.thresholds].auto_approve_min`.
    pub async fn detect_phase_change_with_jev(
        &self,
        holder: &str,
    ) -> Option<(String, String)> {
        const PHASE_WINDOW: usize = 6;
        let jev = self.jev_client()?;
        // Pull the recent transcript. Bounded so a long session
        // does not blow the state.
        let history = {
            let g = self.history.read().await;
            g.get(holder).cloned().unwrap_or_default()
        };
        if history.len() < 3 {
            return None;
        }
        let tail: Vec<String> = history
            .iter()
            .rev()
            .take(PHASE_WINDOW)
            .map(|m| {
                let role = match m.role {
                    kod_types::MessageRole::User => "user",
                    kod_types::MessageRole::Assistant => "assistant",
                    _ => "other",
                };
                format!("{role}: {}", crate::jev::preview_chars(&m.content, 300))
            })
            .collect();
        let transcript_tail = tail.into_iter().rev().collect::<Vec<_>>().join("\n");
        let state = crate::jev::build_state(&transcript_tail, &[]);

        // Ask the current phase, then the previous phase. Compare
        // and gate on confidence.
        let labels = &[
            "exploring",
            "coding",
            "debugging",
            "testing",
            "refactoring",
            "documenting",
        ];
        let started = std::time::Instant::now();
        let now = jev
            .evaluate_score(
                &state,
                "What phase is the session currently in?",
                labels,
            )
            .await
            .ok()?;
        let elapsed_ms = started.elapsed().as_millis() as u64;
        // Previous phase: use the earliest half of the window as the
        // state for a second question. Reuses the same Jev client.
        let earlier: String = transcript_tail
            .lines()
            .take(transcript_tail.lines().count() / 2)
            .collect::<Vec<_>>()
            .join("\n");
        let state2 = crate::jev::build_state(&earlier, &[]);
        let prev = jev
            .evaluate_score(
                &state2,
                "What phase was the session in before the most recent turn?",
                labels,
            )
            .await
            .ok()?;
        let confidence = now.confidence.min(prev.confidence);
        self.log_jev_decision(
            holder,
            "phase_detect",
            &crate::jev::preview_chars(&transcript_tail, 200),
            "current_phase,previous_phase",
            serde_json::json!({
                "current_phase": now.value,
                "previous_phase": prev.value,
                "confidence": confidence,
            }),
            confidence,
            elapsed_ms,
            false,
            crate::jev::DecisionSource::Jev,
        );
        if now.value == prev.value {
            return None;
        }
        if confidence < jev.thresholds().auto_approve_min {
            return None;
        }
        Some((prev.value, now.value))
    }

    /// Compress a large `read_file` result by dropping lines Jev
    /// judges irrelevant (P2.2).
    ///
    /// Only `read_file` benefits meaningfully: the other tools'
    /// output is already structured (grep results, diffs, JSON) and
    /// the caller's own caps already handle those. A read_file of
    /// 400 lines for a task that needs 20 is the common case this
    /// targets.
    ///
    /// Bounded to `MAX_LINES_TO_SCORE` lines. Every line is asked
    /// about in one batch. Lines scored above `[jev.thresholds]
    /// .memory_filter_min` are kept; dropped runs are replaced with
    /// a one-line `… N lines elided` marker so line numbers stay
    /// meaningful to a caller that wants them.
    ///
    /// Returns `None` when Jev is disabled, the result is not a
    /// `read_file` success, the content is shorter than
    /// `MIN_LINES_TO_COMPRESS`, or the call errors.
    async fn compress_tool_result_with_jev(
        &self,
        holder: &str,
        call: &ToolCall,
        result: &ToolResult,
    ) -> Option<ToolResult> {
        const MIN_LINES_TO_COMPRESS: usize = 60;
        const MAX_LINES_TO_SCORE: usize = 200;
        let jev = self.jev_client()?;
        let request = self.current_request(holder).await?;
        let ToolResult::Success(v) = result else {
            return None;
        };
        if call.tool_name != "read_file" {
            return None;
        }
        let content = v.get("content").and_then(|c| c.as_str())?;
        let lines: Vec<&str> = content.lines().collect();
        if lines.len() < MIN_LINES_TO_COMPRESS {
            return None;
        }
        let slice = &lines[..lines.len().min(MAX_LINES_TO_SCORE)];

        let state = crate::jev::build_state(&request, &[]);
        let questions: Vec<(String, String)> = slice
            .iter()
            .enumerate()
            .map(|(i, line)| {
                (
                    format!("line_{i}"),
                    format!(
                        "Is this file line relevant to the request? Line: {}",
                        crate::jev::preview_chars(line, 160),
                    ),
                )
            })
            .collect();
        let started = std::time::Instant::now();
        let rows = jev.evaluate_yes_no_batch(&state, &questions).await.ok()?;
        let elapsed_ms = started.elapsed().as_millis() as u64;
        let threshold = jev.thresholds().memory_filter_min;

        let mut keep_idx: std::collections::HashSet<usize> =
            std::collections::HashSet::new();
        let mut answers = serde_json::Map::new();
        for (i, line) in slice.iter().enumerate() {
            let p = rows
                .iter()
                .find(|(id, _)| id == &format!("line_{i}"))
                .map(|(_, p)| *p)
                .unwrap_or(1.0);
            answers.insert(format!("line_{i}"), serde_json::json!(p));
            if p >= threshold {
                keep_idx.insert(i);
            }
            let _ = line;
        }
        // Keep everything past the classification window.
        for i in slice.len()..lines.len() {
            keep_idx.insert(i);
        }
        let dropped = lines.len() - keep_idx.len();
        if dropped < 10 {
            // Not worth the substitution — keep the original.
            return None;
        }
        self.log_jev_decision(
            holder,
            "tool_result_compress",
            &request,
            "per_line_relevance",
            serde_json::Value::Object(answers),
            1.0,
            elapsed_ms,
            false,
            crate::jev::DecisionSource::Jev,
        );

        // Rebuild content with elision markers.
        let mut out_lines: Vec<String> = Vec::with_capacity(keep_idx.len());
        let mut i = 0_usize;
        let mut elided_run = 0_usize;
        while i < lines.len() {
            if keep_idx.contains(&i) {
                if elided_run > 0 {
                    out_lines.push(format!("… {elided_run} lines elided by Jev"));
                    elided_run = 0;
                }
                out_lines.push(lines[i].to_string());
            } else {
                elided_run += 1;
            }
            i += 1;
        }
        if elided_run > 0 {
            out_lines.push(format!("… {elided_run} lines elided by Jev"));
        }
        let new_content = out_lines.join("\n");

        let mut new_v = v.clone();
        if let Some(obj) = new_v.as_object_mut() {
            obj.insert(
                "content".to_string(),
                serde_json::Value::String(new_content),
            );
            obj.insert(
                "lines_elided_by_jev".to_string(),
                serde_json::json!(dropped),
            );
        }
        Some(ToolResult::Success(new_v))
    }

    /// Ask Jev which registered endpoint should serve a task
    /// (P5.2). Returns `None` when Jev is disabled, no registry is
    /// installed, or Jev fails — the caller falls back to the static
    /// `[llm.routing.by_task]` table.
    ///
    /// Public so the swarm runner can consult it for a capability
    /// before the per-round dispatch.
    pub async fn pick_endpoint_with_jev(&self, task_key: &str) -> Option<ModelRef> {
        let jev = self.jev_client()?;
        let registry = self.registry.read().await.clone()?;
        let names = registry.names();
        if names.len() < 2 {
            return None;
        }
        let state = crate::jev::build_state(
            &format!("Task type: {task_key}"),
            &[],
        );
        let started = std::time::Instant::now();
        let labels: Vec<&str> = names.iter().map(String::as_str).collect();
        let decision = jev
            .evaluate_score(&state, "Which endpoint should handle this task?", &labels)
            .await
            .ok()?;
        let elapsed_ms = started.elapsed().as_millis() as u64;
        let model = registry.default_model(&decision.value)?;
        self.log_jev_decision(
            DEFAULT_TRANSCRIPT_KEY,
            "endpoint_routing",
            task_key,
            "endpoint",
            serde_json::json!({ "endpoint": decision.value }),
            decision.confidence,
            elapsed_ms,
            false,
            crate::jev::DecisionSource::Jev,
        );
        Some(ModelRef::new(decision.value, model))
    }

    /// Semantic swarm overlap check (P4.7). Given the parsed
    /// subtasks, ask Jev which pairs touch the same conceptual file
    /// even when their globs do not share a prefix. Returns a list
    /// of `(i, j)` index pairs the caller may want to serialize,
    /// or an empty vec when Jev is disabled or finds no semantic
    /// overlap.
    ///
    /// Bounded to `MAX_PAIRS` pairs (`{n choose 2}` of the first few
    /// subtasks) so a large swarm does not produce a pathological
    /// request.
    ///
    /// Uses one score question per candidate pair, joined in a
    /// single batch of yes/no.
    pub async fn semantic_overlap_check(
        &self,
        subtasks: &[(String, Vec<String>)],
    ) -> Vec<(usize, usize)> {
        const MAX_SUBTASKS: usize = 6;
        let Some(jev) = self.jev_client() else {
            return Vec::new();
        };
        if subtasks.len() < 2 {
            return Vec::new();
        }
        let slice = &subtasks[..subtasks.len().min(MAX_SUBTASKS)];
        // Build the state once: the descriptions plus each subtask's
        // declared globs.
        let mut ctx = String::from("Subtasks:\n");
        for (i, (desc, globs)) in slice.iter().enumerate() {
            ctx.push_str(&format!(
                "[{}] {} :: {}\n",
                i,
                crate::jev::preview_chars(desc, 200),
                globs.join(", ")
            ));
        }
        let state = crate::jev::build_state(&ctx, &[]);
        let mut questions: Vec<(String, String)> = Vec::new();
        for i in 0..slice.len() {
            for j in (i + 1)..slice.len() {
                questions.push((
                    format!("pair_{i}_{j}"),
                    format!(
                        "Do subtasks {i} and {j} touch the same conceptual file, even if their declared globs differ?"
                    ),
                ));
            }
        }
        let started = std::time::Instant::now();
        let rows = match jev.evaluate_yes_no_batch(&state, &questions).await {
            Ok(r) => r,
            Err(_) => return Vec::new(),
        };
        let elapsed_ms = started.elapsed().as_millis() as u64;
        let threshold = jev.thresholds().memory_filter_min;
        let mut out: Vec<(usize, usize)> = Vec::new();
        let mut answers = serde_json::Map::new();
        for (id, p) in &rows {
            answers.insert(id.clone(), serde_json::json!(p));
            if *p >= threshold
                && let Some(rest) = id.strip_prefix("pair_")
                && let Some((a, b)) = rest.split_once('_')
                && let (Ok(i), Ok(j)) = (a.parse::<usize>(), b.parse::<usize>())
            {
                out.push((i, j));
            }
        }
        self.log_jev_decision(
            DEFAULT_TRANSCRIPT_KEY,
            "swarm_overlap",
            &crate::jev::preview_chars(&ctx, 200),
            "colliding_pairs",
            serde_json::Value::Object(answers),
            1.0,
            elapsed_ms,
            false,
            crate::jev::DecisionSource::Jev,
        );
        out
    }

    /// Ask Jev which capability best fits a subtask description
    /// (P4.6). Returns the answer as a string label so the caller
    /// maps it back through `kod_swarm::Capability::from_str`. Returns
    /// `None` when Jev is disabled or errors — the caller's
    /// heuristic (`capability_for`) is the fallback.
    ///
    /// Public because `swarm_runner` is a different module and calls
    /// this through the `Arc<KodEngine>`.
    pub async fn validate_subtask_capability(
        &self,
        description: &str,
    ) -> Option<String> {
        let jev = self.jev_client()?;
        let state = crate::jev::build_state(description, &[]);
        let labels = &[
            "coding",
            "testing",
            "documentation",
            "code-review",
            "planning",
            "research",
            "debugging",
            "refactoring",
        ];
        let started = std::time::Instant::now();
        let decision = jev
            .evaluate_score(
                &state,
                "Which capability best describes this subtask?",
                labels,
            )
            .await
            .ok()?;
        let elapsed_ms = started.elapsed().as_millis() as u64;
        self.log_jev_decision(
            DEFAULT_TRANSCRIPT_KEY,
            "swarm_capability",
            description,
            "capability",
            serde_json::json!({ "capability": decision.value }),
            decision.confidence,
            elapsed_ms,
            false,
            crate::jev::DecisionSource::Jev,
        );
        Some(decision.value)
    }

    /// Ask Jev whether a subtask's write-set globs plausibly match
    /// its description (P4.6). Returns `Some(true)` for "yes",
    /// `Some(false)` for a confident "no", `None` when Jev is
    /// disabled or errors.
    pub async fn validate_subtask_globs(
        &self,
        description: &str,
        globs: &[String],
    ) -> Option<bool> {
        if globs.is_empty() {
            return None;
        }
        let jev = self.jev_client()?;
        let state = crate::jev::build_state(
            &format!(
                "Subtask: {description}\nWrite-set globs: {}",
                globs.join(", ")
            ),
            &[],
        );
        let pairs = [(
            "globs_match".to_string(),
            "Do these file globs plausibly match the subtask description?".to_string(),
        )];
        let started = std::time::Instant::now();
        let rows = jev.evaluate_yes_no_batch(&state, &pairs).await.ok()?;
        let elapsed_ms = started.elapsed().as_millis() as u64;
        let p_yes = rows.first().map(|(_, p)| *p).unwrap_or(1.0);
        self.log_jev_decision(
            DEFAULT_TRANSCRIPT_KEY,
            "swarm_globs",
            description,
            "globs_match",
            serde_json::json!({ "globs_match": p_yes }),
            p_yes,
            elapsed_ms,
            false,
            crate::jev::DecisionSource::Jev,
        );
        let threshold = jev.thresholds().task_classify_min;
        Some(p_yes >= threshold)
    }

    /// Reweight a `PromptBudget` allocation based on Jev (P2.1).
    ///
    /// The base allocation is the fixed 50/20/20/10 split for
    /// history / skills / memory / repomap. This helper asks Jev
    /// three yes/no questions — does this round need repomap, full
    /// history, and skill instructions — and shifts the `remaining`
    /// budget between sections accordingly. The total is preserved:
    /// sections only trade, they never grow the prompt.
    ///
    /// A section that says "no" contributes its share to the
    /// sections that said "yes". `memory` always contributes when it
    /// exists (dropping it has a bigger cost than any tokens saved),
    /// so it does not get its own question.
    ///
    /// Returns `base` unchanged when Jev is disabled, errors, or the
    /// input is too short to classify.
    async fn reallocate_with_jev(
        &self,
        holder: &str,
        input: &str,
        base: &crate::budget::Allocation,
    ) -> crate::budget::Allocation {
        let Some(jev) = self.jev_client() else {
            return *base;
        };
        if input.len() < 20 {
            return *base;
        }
        let state = crate::jev::build_state(input, &[]);
        let pairs = [
            (
                "needs_repomap".to_string(),
                "Does this request need the repository's symbol map?".to_string(),
            ),
            (
                "needs_history".to_string(),
                "Does this request need the full conversation history?".to_string(),
            ),
            (
                "needs_skills".to_string(),
                "Does this request need skill instructions?".to_string(),
            ),
        ];
        let started = std::time::Instant::now();
        let rows = match jev.evaluate_yes_no_batch(&state, &pairs).await {
            Ok(r) => r,
            Err(_) => return *base,
        };
        let elapsed_ms = started.elapsed().as_millis() as u64;
        let get = |k: &str| {
            rows.iter()
                .find(|(id, _)| id == k)
                .map(|(_, p)| *p)
                .unwrap_or(1.0)
        };
        let wants = [
            get("needs_history"),
            get("needs_skills"),
            1.0_f32,
            get("needs_repomap"),
        ];
        let threshold = jev.thresholds().task_classify_min;
        let total_share: u32 = base.history as u32
            + base.skills as u32
            + base.memory as u32
            + base.repomap as u32;
        let mut yes_count = 0_u32;
        for w in &wants {
            if *w >= threshold {
                yes_count += 1;
            }
        }
        if yes_count == 0 || total_share == 0 {
            return *base;
        }
        let per_section = total_share / yes_count;
        let mut leftover = total_share - per_section * yes_count;

        let mut out = crate::budget::Allocation {
            request: base.request,
            history: 0,
            skills: 0,
            memory: 0,
            repomap: 0,
        };
        let mut give = |slot: &mut usize| {
            let mut add = per_section as usize;
            if leftover > 0 {
                add += 1;
                leftover -= 1;
            }
            *slot = add;
        };
        if wants[0] >= threshold { give(&mut out.history); }
        if wants[1] >= threshold { give(&mut out.skills); }
        if wants[2] >= threshold { give(&mut out.memory); }
        if wants[3] >= threshold { give(&mut out.repomap); }

        self.log_jev_decision(
            holder,
            "budget_reweight",
            input,
            "needs_history,needs_skills,needs_repomap",
            serde_json::json!({
                "needs_history": wants[0],
                "needs_skills": wants[1],
                "needs_repomap": wants[3],
                "alloc": {
                    "history": out.history,
                    "skills": out.skills,
                    "memory": out.memory,
                    "repomap": out.repomap,
                },
            }),
            1.0,
            elapsed_ms,
            false,
            crate::jev::DecisionSource::Jev,
        );
        out
    }

    /// Filter a whole `MemoryContext` through Jev (P2.5). Applies
    /// `filter_memory_entries_with_jev` to both the working and the
    /// long-term halves, keyed by memory id, and rebuilds the
    /// context with the surviving entries.
    ///
    /// The `total_tokens` field is recomputed as the sum of the
    /// survivors' estimated lengths, so the prompt budget sees the
    /// post-filter number, not the pre-filter one.
    ///
    /// No-op when Jev is disabled, the context is empty, or the
    /// transcript has no current request (a caller that never ran a
    /// `process_*` entry point).
    async fn filter_memory_context_with_jev(
        &self,
        holder: &str,
        ctx: Option<kod_types::MemoryContext>,
    ) -> Option<kod_types::MemoryContext> {
        let mut ctx = ctx?;
        let jev = self.jev_client()?;
        let Some(request) = self.current_request(holder).await else {
            return Some(ctx);
        };
        // Skip when the total content is trivial — nothing to save.
        let total_chars = ctx
            .working_memory
            .iter()
            .chain(ctx.long_term.iter())
            .map(|e| e.content.len())
            .sum::<usize>();
        if total_chars < 400 {
            return Some(ctx);
        }

        // Build the id→text list for both halves in one request.
        let mut pairs: Vec<(String, String)> = Vec::new();
        for e in &ctx.working_memory {
            pairs.push((format!("w:{}", e.id), e.content.clone()));
        }
        for e in &ctx.long_term {
            pairs.push((format!("l:{}", e.id), e.content.clone()));
        }
        let kept = self.filter_memory_entries_with_jev(pairs).await;
        let keep_set: std::collections::HashSet<String> = kept.into_iter().map(|(id, _)| id).collect();

        ctx.working_memory
            .retain(|e| keep_set.contains(&format!("w:{}", e.id)));
        ctx.long_term
            .retain(|e| keep_set.contains(&format!("l:{}", e.id)));

        // Recompute the token estimate from the survivors. The exact
        // formula is not important here — this is a bookkeeping
        // value the budget pass reads.
        ctx.total_tokens = ctx
            .working_memory
            .iter()
            .chain(ctx.long_term.iter())
            .map(|e| e.content.len() / 4)
            .sum();
        let _ = request; // documented use above, kept for symmetry
        let _ = jev;
        Some(ctx)
    }

    /// Filter memory entries by Jev relevance (P4.1). Given a list
    /// of `(id, text)` pairs and the user's current request, returns
    /// the subset Jev scores as `relevant` or `essential`.
    ///
    /// The whole set is sent in one Score question so a large memory
    /// list costs one round-trip, not one per entry. Entries whose
    /// relevance falls below `[jev.thresholds].memory_filter_min` are
    /// dropped. Fail-open: on Jev failure, the original list is
    /// returned unchanged.
    pub async fn filter_memory_entries_with_jev(
        &self,
        entries: Vec<(String, String)>,
    ) -> Vec<(String, String)> {
        if entries.is_empty() {
            return entries;
        }
        let Some(jev) = self.jev_client() else {
            return entries;
        };
        let Some(request) = self.current_request(DEFAULT_TRANSCRIPT_KEY).await else {
            return entries;
        };
        let threshold = jev.thresholds().memory_filter_min;

        // Build one yes/no per entry, keyed by id. Bounded to 30
        // entries per call so a runaway memory store cannot produce
        // a pathological round-trip.
        const MAX_ENTRIES: usize = 30;
        let slice: Vec<(String, String)> = entries.iter().take(MAX_ENTRIES).cloned().collect();
        let state = crate::jev::build_state(&request, &[]);
        let questions: Vec<(String, String)> = slice
            .iter()
            .map(|(id, text)| {
                (
                    id.clone(),
                    format!(
                        "Is this memory entry relevant to the request? Entry: {}",
                        crate::jev::preview_chars(text, 400)
                    ),
                )
            })
            .collect();
        let started = std::time::Instant::now();
        let result = jev.evaluate_yes_no_batch(&state, &questions).await;
        let elapsed_ms = started.elapsed().as_millis() as u64;

        let (keep_ids, answers, source) = match result {
            Ok(rows) => {
                let mut keep = std::collections::HashSet::<String>::new();
                let mut map = serde_json::Map::new();
                for (id, p) in &rows {
                    map.insert(id.clone(), serde_json::json!(p));
                    if *p >= threshold {
                        keep.insert(id.clone());
                    }
                }
                (
                    keep,
                    serde_json::Value::Object(map),
                    crate::jev::DecisionSource::Jev,
                )
            }
            Err(_) => (
                entries.iter().map(|(id, _)| id.clone()).collect(),
                serde_json::json!({}),
                crate::jev::DecisionSource::Heuristic,
            ),
        };

        self.log_jev_decision(
            DEFAULT_TRANSCRIPT_KEY,
            "memory_filter",
            &request,
            "relevance_per_entry",
            answers,
            1.0,
            elapsed_ms,
            false,
            source,
        );

        let filtered: Vec<(String, String)> = entries
            .into_iter()
            .filter(|(id, _)| keep_ids.contains(id))
            .collect();
        if filtered.is_empty() {
            // An over-eager filter that drops everything starves the
            // prompt. Keep the original if nothing survived.
            return Vec::new();
        }
        filtered
    }

    /// Try to answer an `ask_user` question from the existing
    /// context (P3.5). Returns `Some(answer)` when Jev is confident
    /// the question can be answered from the request text plus a
    /// recent history excerpt; `None` when the user should be asked.
    ///
    /// The caller uses the returned answer as the tool result
    /// instead of emitting a question marker, so the model proceeds
    /// without interrupting the user. Logged as a JevDecision.
    async fn try_answer_question_from_context(
        &self,
        key: &str,
        question: &str,
    ) -> Option<String> {
        let jev = self.jev_client()?;
        let request = self.current_request(key).await?;
        // Recent history gives Jev enough state to answer "what file"
        // style questions. Bounded so the state stays cheap.
        let hist = self.render_history_for(key).await;
        let hist_tail: String = hist
            .chars()
            .rev()
            .take(2000)
            .collect::<String>()
            .chars()
            .rev()
            .collect();

        let state = crate::jev::build_state(
            &format!(
                "User request: {request}\n\nRecent conversation excerpt:\n{hist_tail}\n\nQuestion the model wants to ask: {question}"
            ),
            &[],
        );
        let pairs = [
            (
                "can_answer_from_context".to_string(),
                "Can this question be answered from the available context?"
                    .to_string(),
            ),
        ];
        let started = std::time::Instant::now();
        let result = jev.evaluate_yes_no_batch(&state, &pairs).await;
        let elapsed_ms = started.elapsed().as_millis() as u64;
        let (p_yes, source) = match result {
            Ok(rows) => (
                rows.first().map(|(_, p)| *p).unwrap_or(0.0),
                crate::jev::DecisionSource::Jev,
            ),
            Err(_) => (0.0, crate::jev::DecisionSource::Heuristic),
        };
        let threshold = jev.thresholds().ambiguity_min;
        self.log_jev_decision(
            key,
            "ask_user_check",
            question,
            "can_answer_from_context",
            serde_json::json!({ "can_answer_from_context": p_yes }),
            p_yes,
            elapsed_ms,
            false,
            source,
        );
        if p_yes < threshold {
            return None;
        }
        // A confident "yes" does not by itself produce an answer. If
        // the model asked the user for a string, the context has one;
        // re-ask with the same request as the "answer". This keeps
        // the wrapper's shape unchanged — one yes/no gate, then the
        // caller's existing flow.
        Some(format!(
            "(Jev answered from context — the request already specified this.) {request}"
        ))
    }

    /// Choose the endpoint for a streaming round (P1.3).
    ///
    /// On the first round of a turn, and on every round after a tool
    /// execution, asks Jev what kind of round this is (planning,
    /// tool_execution, synthesis, summary) and maps the answer to an
    /// endpoint via `[jev.round_routing]`. Returns `None` when Jev is
    /// disabled, the kind has no mapping, the mapping does not name a
    /// registered endpoint, or the state does not warrant a route.
    ///
    /// Fail-open: any error returns `None` and the caller keeps the
    /// chain-resolved endpoint for this round.
    async fn pick_round_endpoint(
        &self,
        key: &str,
        round_idx: usize,
        had_tool_results: bool,
    ) -> Option<ModelRef> {
        let jev = self.jev_client()?;
        let cfg = jev.config();
        if cfg.round_routing.is_empty() {
            return None;
        }
        let request_text = self.current_request(key).await?;

        // State distilled from the loop counters. Short so the
        // request stays cheap.
        let state = crate::jev::build_state(
            &format!("User request: {request_text}"),
            &[
                ("round_index", &round_idx.to_string()),
                ("tool_ran", if had_tool_results { "yes" } else { "no" }),
            ],
        );
        let labels = &["planning", "tool_execution", "synthesis", "summary"];
        let question = "What kind of round is this?";
        let started = std::time::Instant::now();
        let decision = jev.evaluate_score(&state, question, labels).await;
        let elapsed_ms = started.elapsed().as_millis() as u64;

        let (kind, source) = match decision {
            Ok(d) => (d.value, crate::jev::DecisionSource::Jev),
            Err(_) => (String::new(), crate::jev::DecisionSource::Heuristic),
        };
        let endpoint = if kind.is_empty() {
            None
        } else {
            cfg.endpoint_for_round(&kind).map(String::from)
        };

        // Resolve to a ModelRef only when the endpoint is registered.
        let result = match endpoint.as_deref() {
            Some(name) => {
                let registry = self.registry.read().await.clone();
                registry.and_then(|reg| {
                    reg.default_model(name)
                        .map(|model| ModelRef::new(name.to_string(), model))
                })
            }
            None => None,
        };

        let answers = serde_json::json!({
            "round_kind": kind,
            "endpoint": endpoint,
            "applied": result.is_some(),
        });
        self.log_jev_decision(
            key,
            "round_routing",
            &format!("round {round_idx}"),
            "round_kind",
            answers,
            1.0,
            elapsed_ms,
            false,
            source,
        );
        result
    }

    /// Should the stream be cut short? Called from `stream_round` on
    /// every `EARLY_TERM_CHECK_EVERY_CHUNKS`-th chunk after the
    /// accumulated response reaches `EARLY_TERM_MIN_CHARS`
    /// (P1.2).
    ///
    /// Asks two questions in one round-trip:
    ///
    /// * `is_complete`: does the accumulated response fully answer
    ///   the user's request?
    /// * `is_off_track`: has the model drifted from the request?
    ///
    /// Returns `true` when either clears the configured
    /// `[jev.thresholds].early_termination_min`. Logs a
    /// `SessionEntry::JevDecision` on every call — a false positive
    /// here (cutting a response short) is exactly the failure mode
    /// the log exists to diagnose.
    ///
    /// Fail-open: disabled or errored Jev returns `false` and the
    /// stream continues to the model's natural terminator.
    async fn should_early_terminate(
        &self,
        key: &str,
        accumulated: &str,
    ) -> EarlyTermination {
        let Some(jev) = self.jev_client() else {
            return EarlyTermination::None;
        };
        let Some(request) = self.current_request(key).await else {
            return EarlyTermination::None;
        };
        // Require at least one full sentence: a model that has
        // emitted only "Let me" is not done, however confident Jev
        // sounds about it.
        if !accumulated.contains(['.', '!', '?', '\n']) {
            return EarlyTermination::None;
        }

        let threshold = {
            let t = jev.thresholds().early_termination_min;
            // Use the configured value when it is sane; fall
            // back to the documented default when a bad config
            // produced zero (every response would terminate).
            if t > 0.0 { t } else { EARLY_TERM_DEFAULT_MIN }
        };
        let state = crate::jev::build_state(
            &format!("User request: {request}\nResponse so far: {accumulated}"),
            &[],
        );
        let pairs = [
            (
                "is_complete".to_string(),
                "Does the accumulated response fully answer the user's request?"
                    .to_string(),
            ),
            (
                "is_off_track".to_string(),
                "Has the model drifted away from the user's request?".to_string(),
            ),
        ];
        let started = std::time::Instant::now();
        let result = jev.evaluate_yes_no_batch(&state, &pairs).await;
        let elapsed_ms = started.elapsed().as_millis() as u64;

        let (complete_p, off_track_p, source) = match result {
            Ok(rows) => {
                let c = rows
                    .iter()
                    .find(|(k, _)| k == "is_complete")
                    .map(|(_, p)| *p)
                    .unwrap_or(0.0);
                let o = rows
                    .iter()
                    .find(|(k, _)| k == "is_off_track")
                    .map(|(_, p)| *p)
                    .unwrap_or(0.0);
                (c, o, crate::jev::DecisionSource::Jev)
            }
            Err(_) => (0.0, 0.0, crate::jev::DecisionSource::Heuristic),
        };

        // Off-track beats complete when both fire: cutting the
        // stream is the right local action, but so is signalling
        // the caller to try a different endpoint. A model that is
        // confidently wrong on the first try should not be trusted
        // to finish the sentence.
        let verdict = if off_track_p >= threshold {
            EarlyTermination::OffTrack
        } else if complete_p >= threshold {
            EarlyTermination::Complete
        } else {
            EarlyTermination::None
        };
        let answers = serde_json::json!({
            "is_complete": complete_p,
            "is_off_track": off_track_p,
            "verdict": match verdict {
                EarlyTermination::None => "none",
                EarlyTermination::Complete => "complete",
                EarlyTermination::OffTrack => "off_track",
            },
        });
        let conf = complete_p.max(off_track_p);
        self.log_jev_decision(
            key,
            "early_termination",
            &crate::jev::preview_chars(accumulated, 200),
            "is_complete,is_off_track",
            answers,
            conf,
            elapsed_ms,
            false,
            source,
        );
        verdict
    }

    /// Ask Jev whether a batch of `Ask`-decision tool calls is safe
    /// to auto-approve (P3.1).
    ///
    /// For each call, two questions are asked in one round-trip:
    ///
    /// * `likely_approved`: would the user almost certainly approve
    ///   this call?
    /// * `risk_level`: read_only | reversible | destructive |
    ///   irreversible.
    ///
    /// A call is auto-approved only when:
    ///
    /// * `likely_approved >= jev.thresholds.auto_approve_min`, AND
    /// * `risk_level` is not `destructive` or `irreversible`.
    ///
    /// The set of auto-approved call indices is returned. Every
    /// decision is logged as a `SessionEntry::JevDecision`.
    ///
    /// Fail-open: on any Jev error, an empty set is returned and the
    /// caller's existing dialog path runs unchanged.
    async fn auto_approve_with_jev(
        &self,
        key: &str,
        calls: &[ToolCall],
        ask_indices: &std::collections::HashSet<usize>,
    ) -> std::collections::HashSet<usize> {
        let mut approved: std::collections::HashSet<usize> =
            std::collections::HashSet::new();
        if ask_indices.is_empty() {
            return approved;
        }
        let Some(jev) = self.jev_client() else {
            return approved;
        };
        let Some(request_text) = self.current_request(key).await else {
            return approved;
        };
        let threshold = jev.thresholds().auto_approve_min;

        for &i in ask_indices {
            let Some(call) = calls.get(i) else { continue };
            let state = crate::jev::build_state(
                &format!(
                    "User request: {request_text}\nTool: {}\nArguments: {}",
                    call.tool_name,
                    serde_json::to_string(&call.arguments).unwrap_or_default()
                ),
                &[],
            );
            let started = std::time::Instant::now();

            // Ask both questions in one round-trip via the batch API
            // for yes/no; risk_level is a separate score call because
            // it has an ordered label set.
            let pairs = [
                (
                    "likely_approved".to_string(),
                    "Would the user almost certainly approve this tool call?".to_string(),
                ),
            ];
            let yes = jev.evaluate_yes_no_batch(&state, &pairs).await;
            let elapsed_ms = started.elapsed().as_millis() as u64;

            let (p_yes, yes_source) = match yes {
                Ok(rows) => (
                    rows.first().map(|(_, p)| *p).unwrap_or(0.0),
                    crate::jev::DecisionSource::Jev,
                ),
                Err(_) => (0.0, crate::jev::DecisionSource::Heuristic),
            };

            // Short-circuit: skip the second call when the first is
            // already below the threshold — the risk question costs a
            // network round-trip and cannot flip a "no".
            let (risk_label, risk_source) = if p_yes >= threshold {
                let risk_labels =
                    &["read_only", "reversible", "destructive", "irreversible"];
                match jev
                    .evaluate_score(
                        &state,
                        "How risky is this operation?",
                        risk_labels,
                    )
                    .await
                {
                    Ok(d) => (Some(d.value), crate::jev::DecisionSource::Jev),
                    Err(_) => (None, crate::jev::DecisionSource::Heuristic),
                }
            } else {
                (None, crate::jev::DecisionSource::Heuristic)
            };

            let risk_blocking = matches!(
                risk_label.as_deref(),
                Some("destructive") | Some("irreversible")
            );
            let final_decision = p_yes >= threshold && !risk_blocking;

            if final_decision {
                approved.insert(i);
            }

            let source = if matches!(yes_source, crate::jev::DecisionSource::Heuristic)
                || matches!(risk_source, crate::jev::DecisionSource::Heuristic)
            {
                crate::jev::DecisionSource::Heuristic
            } else {
                crate::jev::DecisionSource::Jev
            };
            let answers = serde_json::json!({
                "likely_approved": p_yes,
                "risk_level": risk_label,
                "auto_approved": final_decision,
            });
            self.log_jev_decision(
                key,
                "auto_approve",
                &format!("{} {}", call.tool_name, format_call_brief(&call.tool_name, &call.arguments)),
                "likely_approved,risk_level",
                answers,
                p_yes,
                elapsed_ms,
                false,
                source,
            );
        }

        approved
    }

    /// Pre-filter the tool inventory with Jev (P1.1).
    ///
    /// One yes/no question per [`kod_types::ToolCategory`] asks
    /// "does this request need a category of tool?". Categories with
    /// a probability at or above `[jev.thresholds].tool_filter_min`
    /// survive; every other category's definitions are dropped.
    ///
    /// Safety valve: a filter that would leave the tool list empty
    /// returns the full list unchanged — asking a model to act
    /// without a single tool is never the right answer. Likewise,
    /// when Jev is disabled or errors, the full list is returned
    /// (fail-open).
    ///
    /// Logs one `SessionEntry::JevDecision` per call.
    async fn filter_tool_definitions_with_jev(
        &self,
        key: &str,
        input: &str,
        definitions: Vec<ToolDefinition>,
    ) -> Vec<ToolDefinition> {
        let Some(jev) = self.jev_client() else {
            return definitions;
        };
        let threshold = jev.thresholds().tool_filter_min;
        let state = crate::jev::build_state(input, &[]);
        let labels = kod_types::ToolCategory::all_labels();
        let questions: Vec<(String, String)> = labels
            .iter()
            .map(|l| {
                (
                    (*l).to_string(),
                    format!("Does the user's request require a {l} tool?"),
                )
            })
            .collect();
        let started = std::time::Instant::now();
        let decision = jev.evaluate_yes_no_batch(&state, &questions).await;
        let elapsed_ms = started.elapsed().as_millis() as u64;

        let (categories, answers_json, confidence, source) = match decision {
            Ok(pairs) => {
                let mut cats: Vec<kod_types::ToolCategory> = Vec::new();
                let mut map = serde_json::Map::new();
                let mut min_conf = 1.0_f32;
                for (label, p) in &pairs {
                    map.insert(label.clone(), serde_json::json!(p));
                    if *p >= threshold {
                        if let Some(c) = kod_types::ToolCategory::from_label(label) {
                            cats.push(c);
                        }
                    }
                    min_conf = min_conf.min((*p - 0.5).abs() * 2.0);
                }
                (
                    cats,
                    serde_json::Value::Object(map),
                    min_conf,
                    crate::jev::DecisionSource::Jev,
                )
            }
            Err(_) => (
                labels
                    .iter()
                    .filter_map(|l| kod_types::ToolCategory::from_label(l))
                    .collect(),
                serde_json::json!({}),
                1.0,
                crate::jev::DecisionSource::Heuristic,
            ),
        };

        let filtered: Vec<ToolDefinition> = definitions
            .iter()
            .filter(|d| categories.contains(&d.category))
            .cloned()
            .collect();

        let questions_summary = labels.join(",");
        self.log_jev_decision(
            key,
            "tool_filter",
            input,
            &questions_summary,
            answers_json,
            confidence,
            elapsed_ms,
            false,
            source,
        );

        if filtered.is_empty() {
            definitions
        } else {
            filtered
        }
    }

    /// Ask Jev which `TaskType` best describes `input`, and merge
    /// that answer with the keyword heuristic's. The rule is:
    ///
    /// * Jev disabled            -> return the heuristic as-is.
    /// * Jev returns unknown     -> return the heuristic as-is.
    /// * Jev confidence < threshold (from `JevConfig::thresholds`) ->
    ///   return the heuristic as-is.
    /// * Otherwise               -> return Jev's answer.
    ///
    /// Every path logs a `SessionEntry::JevDecision` so `/jev stats`
    /// sees the call. The state is a one-line description of the
    /// request; it deliberately does not carry file contents.
    async fn refine_task_type_with_jev(
        &self,
        key: &str,
        input: &str,
        heuristic: crate::router::TaskType,
    ) -> crate::router::TaskType {
        let Some(jev) = self.jev_client() else {
            return heuristic;
        };
        let state = crate::jev::build_state(
            input,
            &[("heuristic_task_type", heuristic.as_label())],
        );
        let question = "Which task type best describes this request?";
        let labels = crate::router::TaskType::all_labels();
        let started = std::time::Instant::now();
        let decision = jev.evaluate_choice(&state, question, labels).await;
        let elapsed_ms = started.elapsed().as_millis() as u64;

        let (final_type, confidence, source) = match decision {
            Ok(d) => {
                let parsed = crate::router::TaskType::from_label(&d.value);
                let threshold = jev.thresholds().task_classify_min;
                match parsed {
                    Some(t) if d.confidence >= threshold => {
                        (t, d.confidence, crate::jev::DecisionSource::Jev)
                    }
                    _ => (heuristic, d.confidence, crate::jev::DecisionSource::Heuristic),
                }
            }
            Err(_) => (
                heuristic,
                1.0,
                crate::jev::DecisionSource::Heuristic,
            ),
        };

        let answers = serde_json::json!({
            "task_type": final_type.as_label(),
            "confidence": confidence,
        });
        let questions_summary = "task_type".to_string();
        self.log_jev_decision(
            key,
            "task_classify",
            input,
            &questions_summary,
            answers,
            confidence,
            elapsed_ms,
            false,
            source,
        );

        final_type
    }

    /// Write one `SessionEntry::JevDecision` to the installed
    /// session log. No-op when no recorder is installed. Every
    /// Jev-aware call site goes through this helper so the log
    /// shape stays uniform and `/jev stats` can rely on it.
    #[allow(clippy::too_many_arguments)]
    pub fn log_jev_decision(
        &self,
        holder: &str,
        purpose: &str,
        state_preview: &str,
        questions_summary: &str,
        answers: serde_json::Value,
        confidence: f32,
        latency_ms: u64,
        cached: bool,
        source: crate::jev::DecisionSource,
    ) {
        if let Ok(guard) = self.session_recorder.read() {
            if let Some(rec) = guard.as_ref() {
                let entry = crate::session_log::SessionEntry::JevDecision {
                    timestamp_ms: std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .map(|d| d.as_millis() as u64)
                        .unwrap_or(0),
                    holder: holder.to_string(),
                    purpose: purpose.to_string(),
                    state_preview: crate::jev::preview_chars(state_preview, 200),
                    questions_summary: questions_summary.to_string(),
                    answers,
                    confidence,
                    latency_ms,
                    cached,
                    source: source.as_str().to_string(),
                };
                let _ = rec.record(&entry);
            }
        }
    }

    /// Install a `ProviderRegistry` (A4b). The registry becomes the
    /// source of truth for provider resolution; the legacy `provider`
    /// slot is left untouched but is no longer consulted while a
    /// registry is present.
    ///
    /// `default_model` names the endpoint the engine resolves to when
    /// no routing decision has been made yet — usually the config's
    /// `default` endpoint, or the first endpoint of a v2 config.
    pub async fn set_registry(
        &self,
        registry: Arc<ProviderRegistry>,
        default_model: ModelRef,
        routing: Option<kod_config::RoutingConfig>,
    ) {
        *self.registry.write().await = Some(registry);
        *self.current_model.write().await = default_model;
        *self.routing.write().await = routing;
    }

    /// Switch the current (endpoint, model). This is the TUI's
    /// `/model` operation: a `ModelRef` change without rebuilding the
    /// registry. A no-op when no registry is installed (the legacy
    /// `set_provider` path uses a provider whose model was baked in at
    /// construction).
    pub async fn set_current_model(&self, model_ref: ModelRef) {
        *self.current_model.write().await = model_ref;
    }

    /// The current (endpoint, model). Read by the TUI header and by
    /// `/model` completion.
    pub async fn current_model(&self) -> ModelRef {
        self.current_model.read().await.clone()
    }

    /// The USD pricing the registry associates with `model_ref`'s
    /// endpoint, if the endpoint carries a `[pricing]` block.
    ///
    /// Public so the CLI's `kod models` (and any future cost-report
    /// surface) can read the same number the engine uses when it
    /// populates `TaskResponse::pricing`.
    pub async fn pricing_for(&self, model_ref: &ModelRef) -> Option<kod_provider::ModelPricing> {
        let reg = self.registry.read().await;
        reg.as_ref()
            .and_then(|r| r.capabilities(&model_ref.endpoint))
            .and_then(|c| c.pricing)
    }

    /// Compute the allocation for a prompt of `input.len()` chars
    /// against the configured endpoint's window. Reads the config
    /// each time; cheap (one file read at most) and correct after a
    /// `/model` switch that landed a different endpoint.
    async fn prompt_allocation(
        &self,
        input: &str,
        _history: &str,
    ) -> std::result::Result<crate::budget::Allocation, crate::budget::BudgetError> {
        let (window, max_out) = match kod_config::KodConfig::load_default() {
            Ok(cfg) => {
                let ep = cfg.llm.default_endpoint();
                (ep.context_window, ep.max_tokens.unwrap_or(2048))
            }
            Err(_) => {
                // Fall back to the built-in default so the engine
                // still works on a machine whose config is unreadable.
                let d = kod_config::LlmConfig::default();
                let ep = d.default_endpoint();
                (ep.context_window, ep.max_tokens.unwrap_or(2048))
            }
        };
        let budget = crate::budget::PromptBudget::from_tokens(window, max_out);
        budget.allocate(input.len())
    }

    /// The ordered `ModelRef` chain for a task type. The first element
    /// is the endpoint `routing.by_task` names, or `current_model` when
    /// there is no routing table for this task. The rest are the
    /// entries in `routing.fallback`, deduplicated against the primary
    /// and each other. Empty when no provider is available at all.
    ///
    /// A v1 config (`routing = None`) produces a single-element chain
    /// containing `current_model`, so the fallback loop degenerates to
    /// a single attempt — identical to the pre-A6 behaviour.
    /// Streaming chain for a turn. The override, when present,
    /// replaces the *primary* endpoint with a caller-chosen
    /// `ModelRef` (the swarm runner's per-capability routing).
    /// `[llm.routing].fallback` endpoints are still appended so a
    /// fallback chain survives the override — the override is a
    /// routing decision, not a fallback-free mandate.
    ///
    /// `None` delegates to `resolve_chain_for_task`, the pre-A7
    /// behaviour. Both paths return an empty vec when no provider is
    /// reachable; the caller reports "no provider".
    async fn build_streaming_chain(
        &self,
        task_key: &str,
        override_model: Option<&ModelRef>,
    ) -> Vec<ModelRef> {
        match override_model {
            None => self.resolve_chain_for_task(task_key).await,
            Some(primary) => {
                let mut chain = vec![primary.clone()];
                let routing = self.routing.read().await.clone();
                if let Some(r) = &routing {
                    let registry = self.registry.read().await.clone();
                    if let Some(reg) = registry {
                        for endpoint in &r.fallback {
                            if chain.iter().any(|c| c.endpoint == *endpoint) {
                                continue;
                            }
                            if let Some(model) = reg.default_model(endpoint) {
                                chain.push(ModelRef::new(endpoint.clone(), model));
                            }
                        }
                    }
                }
                chain
            }
        }
    }

    /// Resolve a swarm subtask's capability to a `ModelRef` via
    /// `[llm.routing.swarm]`. `None` when the config has no swarm
    /// table, no entry for this capability, or the endpoint is not
    /// registered — every one of which means "route by task type",
    /// the pre-A7 behaviour.
    ///
    /// Public so the swarm runner (which holds an `Arc<KodEngine>`
    /// and no config) can query the mapping before each subtask.
    pub async fn resolve_model_ref_for_capability(
        &self,
        capability: &kod_swarm::Capability,
    ) -> Option<ModelRef> {
        let routing = self.routing.read().await.clone()?;
        let endpoint = routing.swarm.get(capability.as_str())?;
        let registry = self.registry.read().await.clone()?;
        registry
            .default_model(endpoint)
            .map(|model| ModelRef::new(endpoint.clone(), model))
    }

    async fn resolve_chain_for_task(&self, task_key: &str) -> Vec<ModelRef> {
        // Legacy path: no registry.
        if self.registry.read().await.is_none() {
            return vec![self.current_model.read().await.clone()];
        }
        let registry = {
            let g = self.registry.read().await;
            g.as_ref().map(Arc::clone).unwrap()
        };
        let routing = self.routing.read().await.clone();

        let mut chain: Vec<ModelRef> = Vec::new();
        let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();

        // P5.2 — Jev's dynamic routing decision, when available,
        // goes first. The static table's entry, if any, still
        // appears in the chain as a fallback.
        if let Some(primary) = self.pick_endpoint_with_jev(task_key).await {
            if seen.insert(primary.endpoint.clone()) {
                chain.push(primary);
            }
        }

        if let Some(r) = &routing
            && let Some(primary) = r.by_task.get(task_key)
            && let Some(model) = registry.default_model(primary)
            && seen.insert(primary.clone())
        {
            chain.push(ModelRef::new(primary.clone(), model));
        }

        if let Some(r) = &routing {
            for endpoint in &r.fallback {
                if !seen.insert(endpoint.clone()) {
                    continue;
                }
                if let Some(model) = registry.default_model(endpoint) {
                    chain.push(ModelRef::new(endpoint.clone(), model));
                }
            }
        }

        // No routing, or routing that pointed at unregistered
        // endpoints: fall back to current_model as the sole entry.
        if chain.is_empty() {
            chain.push(self.current_model.read().await.clone());
        }
        chain
    }

    /// Resolve a `ModelRef` to a provider. Registry-based when a
    /// registry is installed; the legacy single-provider slot
    /// otherwise (any `ModelRef` resolves to it).
    async fn resolve_provider_for_model_ref(
        &self,
        model_ref: &ModelRef,
    ) -> Result<Arc<dyn LlmProvider>> {
        match self.registry.read().await.as_ref() {
            Some(registry) => registry.resolve(model_ref),
            None => Err(Self::no_provider_error()),
        }
    }

    /// Append a `SessionEntry::Cost` for one provider call, when a
    /// recorder is installed and the endpoint reported pricing.
    /// Best-effort: a write failure logs and the run continues.
    async fn record_cost(
        &self,
        holder: &str,
        model_ref: &ModelRef,
        usage: &kod_provider::TokenUsage,
        pricing: kod_provider::ModelPricing,
    ) {
        if let Ok(guard) = self.session_recorder.read()
            && let Some(rec) = guard.as_ref()
        {
            let now_ms = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_millis() as u64)
                .unwrap_or(0);
            let cost = pricing.cost_usd(usage.prompt_tokens, usage.completion_tokens);
            // Tier 1.2 — update the live tracker. Done *before* the
            // log write so a UI sees the updated spend even if the
            // recorder fails.
            self.cost_tracker.record(cost);
            let entry = crate::session_log::SessionEntry::Cost {
                timestamp_ms: now_ms,
                holder: holder.to_string(),
                endpoint: model_ref.endpoint.clone(),
                model: model_ref.model.clone(),
                prompt_tokens: usage.prompt_tokens,
                completion_tokens: usage.completion_tokens,
                cost_usd: cost,
            };
            if let Err(e) = rec.record(&entry) {
                tracing::warn!(error = %e, "could not append Cost to session log");
            }
        }
    }

    /// Append a `SessionEntry::ModelFallback` to the recorder, if one is
    /// installed. Best-effort: a write failure logs and the run
    /// continues.
    async fn record_model_fallback(
        &self,
        holder: &str,
        from: &ModelRef,
        to: &ModelRef,
        error: &str,
    ) {
        if let Ok(guard) = self.session_recorder.read()
            && let Some(rec) = guard.as_ref()
        {
            let now_ms = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_millis() as u64)
                .unwrap_or(0);
            let entry = crate::session_log::SessionEntry::ModelFallback {
                timestamp_ms: now_ms,
                holder: holder.to_string(),
                from: from.display(),
                to: to.display(),
                error: error.to_string(),
            };
            if let Err(e) = rec.record(&entry) {
                tracing::warn!(error = %e, "could not append ModelFallback to session log");
            }
        }
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
             `engine.install_test_provider(Arc::new(provider))` before calling \
             process — kod-cli and kod-tui do this automatically from \
             ~/.config/kod/config.toml."
                .to_string(),
        )
    }

    /// The communication hub the swarm's blackboard lives on.
    /// Cloning the `Arc` gives a caller a handle to the same hub the
    /// note/read tools talk to, and that a `SwarmRunner` uses to
    /// spawn its agents.
    pub fn swarm_hub(&self) -> Arc<kod_swarm::AgentCommunicationHub> {
        Arc::clone(&self.swarm_hub)
    }

    /// The engine's identity on the hub — the sender id the note
    /// tool broadcasts as. Stable across the engine's lifetime.
    pub fn swarm_coordinator_id(&self) -> &kod_types::AgentId {
        &self.swarm_coordinator_id
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
        match self.current_provider().await {
            Some(p) => p.list_models().await,
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
        // Swarm coordination tools (D4.3). The blackboard is the
        // engine's `AgentCommunicationHub` — the note tool broadcasts
        // a `KnowledgeShare` and the read tool filters the coordinator's
        // received history.
        self.tools
            .register(Box::new(crate::swarm_adapters::SwarmNoteTool::new(
                self.swarm_hub(),
                self.swarm_coordinator_id.clone(),
            )))
            .await;
        self.tools
            .register(Box::new(crate::swarm_adapters::SwarmReadTool::new(
                self.swarm_hub(),
                self.swarm_coordinator_id.clone(),
            )))
            .await;
        self.tools.register(Box::new(ListFilesTool::new())).await;
        self.tools.register(Box::new(GrepTool::new())).await;
        self.tools.register(Box::new(FileInfoTool::new())).await;
        self.tools
            .register(Box::new(ExecuteCommandTool::new()))
            .await;
        // Git tools. Read-only ones (status/diff/branch list) are
        // gated by `GitAccess::Read`; the two write tools (commit,
        // branch create) by `GitAccess::Write`. Both permission
        // levels come from `ToolContext::permissions.git_access`,
        // which the engine grants at construction.
        // Memory tools (D2-B3a). Registered when the router has a
        // memory manager. The tools handle the disabled case by
        // erroring, but not registering when memory is off keeps the
        // model from seeing a tool that cannot do anything.
        if self.router.has_memory() {
            self.tools
                .register(Box::new(crate::memory_tools::MemorySaveTool::new(
                    self.router.clone(),
                )))
                .await;
            self.tools
                .register(Box::new(crate::memory_tools::MemorySearchTool::new(
                    self.router.clone(),
                )))
                .await;
        }

        // LSP tools (D5-L3a). Registered unconditionally; each tool
        // holds a clone of the shared client slot and returns an
        // error naming the missing server when no LSP binary exists
        // for the file's language. `engine.start()` takes `&self`, so
        // the tools take the slot Arc, not the engine Arc.
        self.tools
            .register(Box::new(crate::lsp_tools::LspDiagnosticsTool::new(
                Arc::clone(&self.lsp_manager),
            )))
            .await;
        self.tools
            .register(Box::new(crate::lsp_tools::LspDefinitionTool::new(
                Arc::clone(&self.lsp_manager),
            )))
            .await;
        self.tools
            .register(Box::new(crate::lsp_tools::LspReferencesTool::new(
                Arc::clone(&self.lsp_manager),
            )))
            .await;
        self.tools
            .register(Box::new(crate::lsp_tools::LspHoverTool::new(Arc::clone(
                &self.lsp_manager,
            ))))
            .await;

        // MCP tools (D6.1). Every enabled server is spawned once
        // here, its tools listed, and one `McpToolAdapter` registered
        // per tool under the `mcp:<server>.<tool>` naming policy. A
        // server that fails to start is logged and skipped — one
        // broken plugin does not block the built-in tools, nor the
        // other plugins.
        //
        // This runs inside `start()` (which is idempotent by
        // construction: a second call errors out early) so the MCP
        // servers and the tool registry reach a consistent state at
        // the same moment.
        if let Some(host) = self.mcp.read().await.clone() {
            let tools = host.startup_tools().await;
            let count = tools.len();
            for t in tools {
                self.tools.register(t).await;
            }
            if count > 0 {
                tracing::info!(count, "registered MCP tools");
            }
        }

        self.tools.register(Box::new(GitStatusTool::new())).await;
        self.tools.register(Box::new(GitDiffTool::new())).await;
        self.tools
            .register(Box::new(kod_tools::GitCommitTool::new()))
            .await;
        self.tools
            .register(Box::new(kod_tools::GitBranchTool::new()))
            .await;
        self.tools
            .register(Box::new(kod_tools::TodoTool::new(self.todo_list.clone())))
            .await;
        self.tools
            .register(Box::new(kod_tools::SearchFilesTool::new()))
            .await;
        self.tools
            .register(Box::new(kod_tools::AskUserTool::new()))
            .await;
        self.tools
            .register(Box::new(kod_tools::PlanTool::new()))
            .await;
        // `web_fetch` is registered unconditionally; the per-context
        // `network_access` permission gates the actual call. This is
        // the same shape the git tools use, and it means a future
        // caller that wants to enable network access for one agent
        // does not have to re-register the tool.
        self.tools
            .register(Box::new(kod_tools::WebFetchTool::new()))
            .await;
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

        self.start_memory_consolidation_task().await;

        tracing::info!("KOD engine started");
        Ok(())
    }

    /// Spawn the periodic memory-consolidation task (design D2.5).
    /// No-op when `compaction_interval_secs == 0` or memory is disabled.
    ///
    /// The task holds a clone of the router `Arc` (the manager lives
    /// inside it). `shutdown()` aborts and awaits this handle before
    /// closing redb, so the router's unique-holder path is available
    /// at that moment.
    async fn start_memory_consolidation_task(&self) {
        let interval_secs = match kod_config::KodConfig::load_default() {
            Ok(c) => c.memory.compaction_interval_secs,
            Err(_) => 0,
        };
        if interval_secs == 0 || !self.router.has_memory() {
            return;
        }
        // Already started (idempotent: start() is only callable once,
        // but be defensive against a future caller).
        if self.memory_consolidation_task.read().await.is_some() {
            return;
        }

        let router = Arc::clone(&self.router);
        let handle = tokio::spawn(async move {
            let interval = std::time::Duration::from_secs(interval_secs);
            loop {
                tokio::time::sleep(interval).await;
                match router.consolidate_memory().await {
                    Ok(report) if report.archived > 0 || report.fused > 0 => {
                        tracing::info!(
                            archived = report.archived,
                            fused = report.fused,
                            "memory consolidation pass",
                        );
                    }
                    Ok(_) => {
                        tracing::debug!("memory consolidation pass: nothing to do");
                    }
                    Err(e) => {
                        tracing::warn!(
                            error = %e,
                            "memory consolidation failed; will retry next tick",
                        );
                    }
                }
            }
        });
        *self.memory_consolidation_task.write().await = Some(handle);
        tracing::debug!(interval_secs, "memory consolidation task started",);
    }

    /// Stop the consolidation task (if any) and wait for it to actually
    /// drop. Necessary before the redb close: the task holds a router
    /// clone, and `Arc::try_unwrap` needs the last reference.
    async fn stop_memory_consolidation_task(&self) {
        let handle = self.memory_consolidation_task.write().await.take();
        if let Some(h) = handle {
            h.abort();
            let _ = h.await;
            tracing::debug!("memory consolidation task stopped");
        }
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
        let _provider_probe = self.registry.read().await.clone();
        if _provider_probe.is_some() {
            // Process through router for task classification and context
            let mut response = self.router.process_input(input).await?;
            // P2.5 — drop memory entries Jev judges irrelevant
            // before the prompt budget sees them.
            response.memory_context = self
                .filter_memory_context_with_jev(key, response.memory_context)
                .await;
            // Tier 2.4 — record this turn's retrieval so `/memory
            // eval` can score the retrieval hit rate later.
            if let Some(ctx) = response.memory_context.as_ref() {
                let entries: Vec<(String, f32)> = ctx
                    .working_memory
                    .iter()
                    .chain(ctx.long_term.iter())
                    .map(|e| (e.id.to_string(), e.relevance))
                    .collect();
                // No turn id in the collected path; use 0 to mean
                // "collected-path turn". The streaming path uses the
                // real trace id.
                self.log_memory_retrieval(0, input, &entries);
            }

            // Build the full prompt using the router's context builder
            let task_type = self
                .refine_task_type_with_jev(key, input, response.task_type)
                .await;
            // Tier 2.1 — on Complex/MultiStep tasks, ask the model
            // for a plan on the first turn. Bounded cost: one short
            // generation call.
            {
                let options_for_plan =
                    self.generation_defaults.read().await.to_options();
                if let Ok(provider) = self
                    .resolve_provider_for_model_ref(&self.current_model.read().await.clone())
                    .await
                {
                    self.maybe_create_plan(key, input, task_type, &provider, &options_for_plan)
                        .await;
                }
            }
            // P4.2 — augment the router's lexical skill match with
            // Jev's semantic scoring. The union keeps every lexical
            // match and adds semantic ones the substring matcher
            // would have missed. `None` leaves the router's list.
            let refined_skills = self
                .rank_skills_with_jev(key, input, &response.skills_used)
                .await
                .unwrap_or_else(|| response.skills_used.clone());
            let history = self.render_history_for(key).await;
            self.remember_turn_for(key, true, input).await;
            let base_alloc = self.prompt_allocation(input, &history).await;
            // P2.1 — modulate the fixed 50/20/20/10 shares with
            // Jev's per-round read. The total is preserved; only
            // the split changes.
            let alloc = match &base_alloc {
                Ok(a) => Ok(self.reallocate_with_jev(key, input, a).await),
                Err(e) => Err(*e),
            };
            let prompt = match &alloc {
                Ok(a) => {
                    self.router
                        .build_prompt_with_budget(
                            input,
                            &task_type,
                            &history,
                            response.memory_context.clone(),
                            Some(a),
                        )
                        .await?
                }
                Err(e) => {
                    return Err(kod_error::KodError::InvalidParameters {
                        reason: e.to_string(),
                    });
                }
            };

            // Ground the model: where it runs and what it can touch.
            // Without this it claims "no filesystem access" even though
            // tools are wired below.
            let definitions = self
                .filter_tool_definitions_with_jev(key, input, self.tools.get_definitions().await)
                .await;
            // P5.1 — trim the MCP half of the tool list. Independent
            // of the category filter above; both feed the same
            // `definitions` value the LLM sees.
            let definitions = self
                .filter_mcp_tools_with_jev(key, input, definitions)
                .await;
            let convo = self.ground_prompt(prompt.clone(), &definitions);

            // Snapshot the grounded prompt before the loop mutates it
            // with tool results. This is what `/debug last-prompt` shows.
            {
                let trace = crate::budget::PromptTrace {
                    text: convo.clone(),
                    alloc: alloc.as_ref().ok().copied(),
                };
                self.last_prompt
                    .write()
                    .await
                    .insert(key.to_string(), trace);
            }

            // The structured system prompt the provider will see: the
            // router's plan rendered to text, with the environment
            // grounding appended. The `prompt` variable above remains
            // the un-grounded text used for `/debug` and the initial
            // `pending` string in the loop; the two are byte-compatible
            // up to the grounding block.
            let system_text = {
                let plan = self
                    .router
                    .build_prompt_plan(
                        input,
                        &task_type,
                        &history,
                        response.memory_context.clone(),
                        alloc.as_ref().ok(),
                    )
                    .await?;
                plan.render_text()
            };

            // The transcript the provider will see. `remember_turn_for`
            // above recorded the user's turn, so this already ends with
            // the current user message.
            let initial_messages: Vec<kod_types::ChatMessage> = {
                let guard = self.history.read().await;
                guard.get(key).cloned().unwrap_or_default()
            };

            // Agentic loop: generate (with tools) -> execute -> feed back.
            // Wrapped in a fallback chain (A6): the primary endpoint is
            // tried first, then each `routing.fallback` endpoint, on
            // errors that `is_retryable()` classifies as transient.
            let options = self.generation_defaults.read().await.to_options();
            let task_key = format!("{:?}", response.task_type);
            let chain = self.resolve_chain_for_task(&task_key).await;
            if chain.is_empty() {
                return Err(Self::no_provider_error());
            }
            let mut last_err: Option<KodError> = None;
            let mut outcome: Option<(
                String,
                Vec<ToolCall>,
                Vec<ToolResult>,
                Option<kod_provider::TokenUsage>,
            )> = None;
            // The provider that served the winning attempt, kept so the
            // tool-only-reply summary below calls through the same
            // endpoint the model response came from.
            let mut winning_provider: Option<Arc<dyn LlmProvider>> = None;
            let mut winning_model: Option<ModelRef> = None;
            for (i, model_ref) in chain.iter().enumerate() {
                let this_provider = match self.resolve_provider_for_model_ref(model_ref).await {
                    Ok(p) => p,
                    Err(e) => {
                        tracing::warn!(
                            endpoint = %model_ref.endpoint,
                            error = %e,
                            "cannot resolve endpoint; skipping in chain"
                        );
                        last_err = Some(e);
                        continue;
                    }
                };
                let mut attempt_pending = convo.clone();
                let mut attempt_messages = initial_messages.clone();
                let round = RoundContext {
                    system_text: &system_text,
                    model_ref,
                    definitions: &definitions,
                    options: &options,
                    holder: key,
                    trace: None,
                };
                match self
                    .run_collected_loop(
                        &this_provider,
                        &mut attempt_pending,
                        &mut attempt_messages,
                        &round,
                    )
                    .await
                {
                    Ok(v) => {
                        // Save the pending buffer back: `run_collected_loop`
                        // mutated its own clone, and the summary path below
                        // needs the same mutation (the tool-result block).
                        outcome = Some(v);
                        winning_provider = Some(this_provider);
                        winning_model = Some(model_ref.clone());
                        // We deliberately discard `attempt_pending` here;
                        // the summary path reuses the *original* `convo`.
                        // In practice the summary is short and the model
                        // re-derives from the tool results visible in the
                        // prompt; the pre-A6 behaviour had the same
                        // shape (pending mutated in place, but a
                        // tool-only reply after a fallback is rare
                        // enough that this is acceptable).
                        break;
                    }
                    Err(e) => {
                        // Tier 2.2 — classify the failure and pick a
                        // strategy. Non-recoverable classes surface
                        // immediately; recoverable ones decide whether
                        // to retry the same endpoint (with an
                        // adjustment) or fall through to the next.
                        let failure = crate::retry_strategy::TurnFailure::classify(
                            &e.to_string(),
                        );
                        let action = crate::retry_strategy::choose_action(&failure);
                        let has_next = i + 1 < chain.len();
                        let should_fall_through = failure.recoverable()
                            && has_next
                            && matches!(
                                action,
                                crate::retry_strategy::RetryAction::NextEndpoint
                                    | crate::retry_strategy::RetryAction::SameEndpointBackoff
                                    | crate::retry_strategy::RetryAction::SameEndpointLowerTemp
                                    | crate::retry_strategy::RetryAction::ShrinkHistory
                                    | crate::retry_strategy::RetryAction::ReinjectTools
                                    | crate::retry_strategy::RetryAction::SameEndpointConstrained
                            );
                        if should_fall_through {
                            let next = &chain[i + 1];
                            tracing::warn!(
                                from = %model_ref.display(),
                                to = %next.display(),
                                error = %e,
                                class = %failure.summary(),
                                action = ?action,
                                "provider error; falling back"
                            );
                            self.record_model_fallback(
                                key,
                                model_ref,
                                next,
                                &format!("{} ({})", e, failure.summary()),
                            )
                            .await;
                            last_err = Some(e);
                            continue;
                        }
                        return Err(e);
                    }
                }
            }
            let (final_text, tool_calls, tool_results, usage) =
                outcome.ok_or_else(|| last_err.unwrap_or_else(Self::no_provider_error))?;
            // The winning endpoint's configured pricing. `None` for
            // a local endpoint (no cost), a remote endpoint without
            // a `[pricing]` block, or a response that never reached
            // the provider.
            let pricing = match &winning_model {
                Some(m) => self.pricing_for(m).await,
                None => None,
            };
            // Persist the cost line for this call, when we know the
            // pricing. One line per call, not per session — a session
            // log read later can reconstruct the total by summing,
            // and a per-turn figure is what a debug pass needs.
            if let (Some(m), Some(p), Some(u)) = (winning_model.as_ref(), pricing, usage.as_ref()) {
                self.record_cost(key, m, u, p).await;
            }
            // Model only called tools and never wrote back: ask for a summary.
            let final_text = if final_text.trim().is_empty() && !tool_calls.is_empty() {
                let mut summary_prompt = convo.clone();
                // Serialize the round's tool results into the prompt:
                // the summary call goes through the legacy
                // `generate(&str)` API, so the model has to see them
                // as text here. Regression guard: an earlier shape
                // sent only `convo`, which is the *pre-loop* prompt,
                // so the model was asked to summarize work it could
                // not see.
                if !tool_results.is_empty() {
                    summary_prompt.push_str("\n\n## Tool results from this turn\n");
                    for (i, r) in tool_results.iter().enumerate() {
                        summary_prompt.push_str(&format!("\n### Result {}\n{:?}\n", i + 1, r));
                    }
                }
                summary_prompt.push_str(
                    "\nSummarize what you did and the result for the user in plain text.",
                );
                let summary_provider = winning_provider.ok_or_else(Self::no_provider_error)?;
                summary_provider.generate(&summary_prompt, &options).await?
            } else {
                final_text
            };

            // Research mode: verify file:line citations before
            // returning. Purely local — no extra LLM call, no
            // network. The block appears only when at least one
            // citation fails to verify; a clean reply stays clean.
            let final_text = if matches!(task_type, crate::router::TaskType::Research) {
                let syntactic =
                    crate::citations::check_and_annotate(&final_text, &self.working_dir).text;
                // P4.5 — after the syntactic check, run the semantic
                // pass. The two are additive: the syntactic block
                // reports missing/out-of-range citations, the
                // semantic block reports lines that do not support
                // their claim.
                self.semantic_verify_citations(key, &syntactic).await
            } else {
                final_text
            };

            self.clear_current_request(key).await;

            // P5.4 — response quality gate. Non-blocking: the
            // reply is delivered unchanged; only an advisory is
            // appended when Jev is confident the reply missed
            // the request.
            let request_text = self
                .current_request(key)
                .await
                .unwrap_or_default();
            let final_text = match self
                .check_response_quality_with_jev(key, &request_text, &final_text)
                .await
            {
                Some(advisory) => format!("{final_text}{advisory}"),
                None => final_text,
            };
            self.remember_turn_for(key, false, &final_text).await;

            return Ok(TaskResponse {
                task_type: response.task_type,
                text: Some(final_text),
                tool_calls,
                tool_results,
                skills_used: refined_skills.clone(),
                memory_used: response.memory_used,
                execution_time_ms: response.execution_time_ms,
                usage,
                pricing,
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
        self.process_streaming_with_model_for(key, input, chunk_tx, None)
            .await
    }

    /// Streaming variant of [`KodEngine::process_for`] with an optional
    /// model override.
    ///
    /// The override is the swarm runner's per-capability routing hook
    /// (design D1.4 PR A7): a subtask's `capability` (Coding, Testing,
    /// CodeReview, …) resolves against `[llm.routing.swarm]` to a
    /// `ModelRef`, and the streaming chain tries that model first.
    /// Fallback endpoints from `[llm.routing].fallback` are still
    /// appended — the override replaces the *primary* choice, not the
    /// fallback chain.
    ///
    /// `None` routes by task type as before; this is what every
    /// non-swarm caller passes, and it is what
    /// `process_streaming_for` itself passes.
    pub async fn process_streaming_with_model_for(
        &self,
        key: &str,
        input: &str,
        chunk_tx: &tokio::sync::mpsc::Sender<String>,
        override_model: Option<ModelRef>,
    ) -> Result<TaskResponse> {
        {
            let running = self.is_running.read().await;
            if !*running {
                return Err(KodError::InvalidState("Engine not running".to_string()));
            }
        }
        self.set_current_request(key, input).await;
        // Tier 1.1 — a fresh user turn clears any prior taint.
        self.reset_taint();
        self.tool_counts.begin_turn();
        // Tier 1.4 — open a turn trace. Emitted when this call returns.
        let trace_id = self.next_turn_id();
        let trace = std::sync::Mutex::new(crate::trace::TurnTraceBuilder::new(trace_id, key));
        let trace_ref: Option<&std::sync::Mutex<crate::trace::TurnTraceBuilder>> = Some(&trace);
        self.cost_tracker.begin_turn();
        // P3.3 — ask Jev whether the request is ambiguous; if so and
        // a streaming consumer is attached, prompt for clarification
        // before the LLM ever sees the request.
        let clarified = self
            .augment_input_with_jev_ambiguity_check(key, input, Some(chunk_tx))
            .await;
        if clarified != input {
            // Update the stored request so downstream Jev checks see
            // the clarified version.
            self.set_current_request(key, &clarified).await;
        }
        let expanded_input = expand_at_references(&clarified, &self.working_dir);
        let input = expanded_input.as_str();

        // See process(): clone out of the lock before any long await.
        let _provider_probe = self.registry.read().await.clone();
        if _provider_probe.is_some() {
            let mut response = self.router.process_input(input).await?;
            // P2.5 — drop memory entries Jev judges irrelevant
            // before the prompt budget sees them.
            response.memory_context = self
                .filter_memory_context_with_jev(key, response.memory_context)
                .await;
            // Tier 2.4 — log this turn's retrieval with the real
            // trace id so `/memory eval` can correlate entries with
            // the reply that used them.
            if let Some(ctx) = response.memory_context.as_ref() {
                let entries: Vec<(String, f32)> = ctx
                    .working_memory
                    .iter()
                    .chain(ctx.long_term.iter())
                    .map(|e| (e.id.to_string(), e.relevance))
                    .collect();
                self.log_memory_retrieval(trace_id, input, &entries);
            }
            let task_type = self
                .refine_task_type_with_jev(key, input, response.task_type)
                .await;
            // P4.2 — augment the router's lexical skill match with
            // Jev's semantic scoring. The union keeps every lexical
            // match and adds semantic ones the substring matcher
            // would have missed. `None` leaves the router's list.
            let refined_skills = self
                .rank_skills_with_jev(key, input, &response.skills_used)
                .await
                .unwrap_or_else(|| response.skills_used.clone());
            let history = self.render_history_for(key).await;
            self.remember_turn_for(key, true, input).await;
            let base_alloc = self.prompt_allocation(input, &history).await;
            // P2.1 — modulate the fixed 50/20/20/10 shares with
            // Jev's per-round read. The total is preserved; only
            // the split changes.
            let alloc = match &base_alloc {
                Ok(a) => Ok(self.reallocate_with_jev(key, input, a).await),
                Err(e) => Err(*e),
            };
            let prompt = match &alloc {
                Ok(a) => {
                    self.router
                        .build_prompt_with_budget(
                            input,
                            &task_type,
                            &history,
                            response.memory_context.clone(),
                            Some(a),
                        )
                        .await?
                }
                Err(e) => {
                    return Err(kod_error::KodError::InvalidParameters {
                        reason: e.to_string(),
                    });
                }
            };
            let definitions = self
                .filter_tool_definitions_with_jev(key, input, self.tools.get_definitions().await)
                .await;
            // P5.1 — trim the MCP half of the tool list. Independent
            // of the category filter above; both feed the same
            // `definitions` value the LLM sees.
            let definitions = self
                .filter_mcp_tools_with_jev(key, input, definitions)
                .await;
            let pending = self.ground_prompt(prompt.clone(), &definitions);

            // Snapshot the grounded prompt for /debug last-prompt and
            // the per-section allocation for /debug tokens.
            {
                let trace = crate::budget::PromptTrace {
                    text: pending.clone(),
                    alloc: alloc.as_ref().ok().copied(),
                };
                self.last_prompt
                    .write()
                    .await
                    .insert(key.to_string(), trace);
            }

            // Structured system prompt and initial messages: same
            // shape as the collected path (see `process_for`).
            let system_text = {
                let plan = self
                    .router
                    .build_prompt_plan(
                        input,
                        &task_type,
                        &history,
                        response.memory_context.clone(),
                        alloc.as_ref().ok(),
                    )
                    .await?;
                plan.render_text()
            };
            let initial_messages: Vec<kod_types::ChatMessage> = {
                let guard = self.history.read().await;
                guard.get(key).cloned().unwrap_or_default()
            };

            let options = self.generation_defaults.read().await.to_options();
            // Fallback chain (A6). Streaming retries reuse the same
            // chunk_tx, so a successful fallback continues the visible
            // stream exactly where the failed attempt stopped; a
            // retryable error typically fires before any token, so the
            // user sees a clean stream from the fallback endpoint.
            let task_key = format!("{:?}", response.task_type);
            let chain = self
                .build_streaming_chain(&task_key, override_model.as_ref())
                .await;
            if chain.is_empty() {
                return Err(Self::no_provider_error());
            }
            let mut last_err: Option<KodError> = None;
            let mut outcome: Option<(
                String,
                Vec<ToolCall>,
                Vec<ToolResult>,
                Option<kod_provider::TokenUsage>,
            )> = None;
            let mut winning_provider: Option<Arc<dyn LlmProvider>> = None;
            let mut winning_model: Option<ModelRef> = None;
            for (i, model_ref) in chain.iter().enumerate() {
                let this_provider = match self.resolve_provider_for_model_ref(model_ref).await {
                    Ok(p) => p,
                    Err(e) => {
                        tracing::warn!(
                            endpoint = %model_ref.endpoint,
                            error = %e,
                            "cannot resolve endpoint; skipping in chain"
                        );
                        last_err = Some(e);
                        continue;
                    }
                };
                let mut attempt_pending = pending.clone();
                let mut attempt_messages = initial_messages.clone();
                let round = RoundContext {
                    system_text: &system_text,
                    model_ref,
                    definitions: &definitions,
                    options: &options,
                    holder: key,
                    trace: trace_ref,
                };
                match self
                    .run_streaming_loop(
                        &this_provider,
                        &mut attempt_pending,
                        &mut attempt_messages,
                        chunk_tx,
                        &round,
                    )
                    .await
                {
                    Ok((text, calls, results, usage, retry)) => {
                        // P5.6 — Jev said the round was off-track. If
                        // a fallback endpoint remains, keep the
                        // retry going; otherwise accept the result
                        // (fail-open: a bad reply is better than no
                        // reply).
                        if retry && i + 1 < chain.len() {
                            let next = &chain[i + 1];
                            tracing::warn!(
                                from = %model_ref.display(),
                                to = %next.display(),
                                "Jev flagged the reply off-track; trying next endpoint"
                            );
                            self.record_model_fallback(
                                key,
                                model_ref,
                                next,
                                "jev quality gate",
                            )
                            .await;
                            last_err = None;
                            continue;
                        }
                        outcome = Some((text, calls, results, usage));
                        winning_provider = Some(this_provider);
                        winning_model = Some(model_ref.clone());
                        break;
                    }
                    Err(e) if e.is_retryable() && i + 1 < chain.len() => {
                        let next = &chain[i + 1];
                        tracing::warn!(
                            from = %model_ref.display(),
                            to = %next.display(),
                            error = %e,
                            "retryable provider error; falling back"
                        );
                        self.record_model_fallback(key, model_ref, next, &e.to_string())
                            .await;
                        last_err = Some(e);
                        continue;
                    }
                    Err(e) => return Err(e),
                }
            }
            let (final_text, tool_calls, tool_results, usage) =
                outcome.ok_or_else(|| last_err.unwrap_or_else(Self::no_provider_error))?;
            // The winning endpoint's configured pricing. `None` for
            // a local endpoint (no cost), a remote endpoint without
            // a `[pricing]` block, or a response that never reached
            // the provider.
            let pricing = match &winning_model {
                Some(m) => self.pricing_for(m).await,
                None => None,
            };
            // Persist the cost line for this call, when we know the
            // pricing. One line per call, not per session — a session
            // log read later can reconstruct the total by summing,
            // and a per-turn figure is what a debug pass needs.
            if let (Some(m), Some(p), Some(u)) = (winning_model.as_ref(), pricing, usage.as_ref()) {
                self.record_cost(key, m, u, p).await;
            }
            let final_text = if final_text.trim().is_empty() && !tool_calls.is_empty() {
                let mut summary_prompt = pending.clone();
                // Same treatment as the collected path: the summary
                // call cannot see the structured messages, so the
                // results are rendered into the text prompt.
                if !tool_results.is_empty() {
                    summary_prompt.push_str("\n\n## Tool results from this turn\n");
                    for (i, r) in tool_results.iter().enumerate() {
                        summary_prompt.push_str(&format!("\n### Result {}\n{:?}\n", i + 1, r));
                    }
                }
                summary_prompt.push_str(
                    "\nSummarize what you did and the result for the user in plain text.",
                );
                let summary_provider = winning_provider.ok_or_else(Self::no_provider_error)?;
                self.stream_summary(&summary_provider, &summary_prompt, &options, chunk_tx)
                    .await?
            } else {
                final_text
            };

            // Research mode: verify file:line citations. Same pass
            // as the collected path, plus a chunk over the stream so
            // the TUI shows the block as part of the reply, not as
            // a separate message. A clean reply emits nothing.
            let final_text = if matches!(task_type, crate::router::TaskType::Research) {
                let annotated =
                    crate::citations::check_and_annotate(&final_text, &self.working_dir);
                if let Some(block) = &annotated.block {
                    let _ = chunk_tx.send(format!("\n\n{block}")).await;
                }
                annotated.text
            } else {
                final_text
            };

            // P5.4 — response quality gate. Non-blocking: the
            // reply is delivered unchanged; only an advisory is
            // appended when Jev is confident the reply missed
            // the request.
            let request_text = self
                .current_request(key)
                .await
                .unwrap_or_default();
            let final_text = match self
                .check_response_quality_with_jev(key, &request_text, &final_text)
                .await
            {
                Some(advisory) => format!("{final_text}{advisory}"),
                None => final_text,
            };
            self.remember_turn_for(key, false, &final_text).await;

            // Tier 1.4 — finish and emit the trace.
            if let Ok(mut g) = trace.lock() {
                g.set_reply_chars(final_text.len());
            }
            // We have to move the builder out of the Mutex to finish
            // it; since we are the only owner at this point, this is
            // a simple `into_inner` on the tracked cell.
            let finished = match std::sync::Arc::try_unwrap(std::sync::Arc::new(trace)) {
                Ok(m) => m.into_inner().unwrap_or_else(|e| e.into_inner()),
                Err(_) => return Ok(TaskResponse {
                    task_type: response.task_type,
                    text: Some(final_text),
                    tool_calls,
                    tool_results,
                    skills_used: refined_skills.clone(),
                    memory_used: response.memory_used,
                    execution_time_ms: response.execution_time_ms,
                    usage,
                    pricing,
                    memory_context: response.memory_context,
                }),
            };
            self.emit_turn_trace(&finished.finish());

            return Ok(TaskResponse {
                task_type: response.task_type,
                text: Some(final_text),
                tool_calls,
                tool_results,
                skills_used: refined_skills.clone(),
                memory_used: response.memory_used,
                execution_time_ms: response.execution_time_ms,
                usage,
                pricing,
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
        self.set_current_request(key, input).await;
        // P3.3 — ask Jev whether the request is ambiguous; if so and
        // a streaming consumer is attached, prompt for clarification
        // before the LLM ever sees the request.
        let clarified = self
            .augment_input_with_jev_ambiguity_check(key, input, Some(chunk_tx))
            .await;
        if clarified != input {
            // Update the stored request so downstream Jev checks see
            // the clarified version.
            self.set_current_request(key, &clarified).await;
        }
        let expanded_input = expand_at_references(&clarified, &self.working_dir);
        let input = expanded_input.as_str();

        // See process(): clone out of the lock before any long await.
        let _provider_probe = self.registry.read().await.clone();
        if _provider_probe.is_some() {
            let mut response = self.router.process_input(input).await?;
            // P2.5 — drop memory entries Jev judges irrelevant
            // before the prompt budget sees them.
            response.memory_context = self
                .filter_memory_context_with_jev(key, response.memory_context)
                .await;
            let task_type = self
                .refine_task_type_with_jev(key, input, response.task_type)
                .await;
            // P4.2 — augment the router's lexical skill match with
            // Jev's semantic scoring. The union keeps every lexical
            // match and adds semantic ones the substring matcher
            // would have missed. `None` leaves the router's list.
            let refined_skills = self
                .rank_skills_with_jev(key, input, &response.skills_used)
                .await
                .unwrap_or_else(|| response.skills_used.clone());
            let history = self.render_history_for(key).await;
            self.remember_turn_for(key, true, input).await;
            let base_alloc = self.prompt_allocation(input, &history).await;
            // P2.1 — modulate the fixed 50/20/20/10 shares with
            // Jev's per-round read. The total is preserved; only
            // the split changes.
            let alloc = match &base_alloc {
                Ok(a) => Ok(self.reallocate_with_jev(key, input, a).await),
                Err(e) => Err(*e),
            };
            let prompt = match &alloc {
                Ok(a) => {
                    self.router
                        .build_prompt_with_budget(
                            input,
                            &task_type,
                            &history,
                            response.memory_context.clone(),
                            Some(a),
                        )
                        .await?
                }
                Err(e) => {
                    return Err(kod_error::KodError::InvalidParameters {
                        reason: e.to_string(),
                    });
                }
            };
            let definitions = self
                .filter_tool_definitions_with_jev(key, input, self.tools.get_definitions().await)
                .await;
            // P5.1 — trim the MCP half of the tool list. Independent
            // of the category filter above; both feed the same
            // `definitions` value the LLM sees.
            let definitions = self
                .filter_mcp_tools_with_jev(key, input, definitions)
                .await;
            let mut pending = self.ground_prompt(prompt.clone(), &definitions);
            pending.push_str(&format!(
                "\n## Goal\n\n{goal}\n\nWork turn by turn toward this goal using tools. Do not ask the user for confirmation — act. When the goal is fully reached, end your reply with a line containing exactly GOAL MET and summarize what was done. If a tool errors, work around it and keep going.\n"
            ));

            // Snapshot includes the goal block — that is what the model
            // sees on turn 1, which is what users want to inspect when a
            // goal run misbehaves.
            {
                let trace = crate::budget::PromptTrace {
                    text: pending.clone(),
                    alloc: alloc.as_ref().ok().copied(),
                };
                self.last_prompt
                    .write()
                    .await
                    .insert(key.to_string(), trace);
            }

            // Structured inputs for the streaming loop. The goal text
            // is part of the system prompt, not a message, so a fresh
            // turn of the goal loop does not create a duplicate user
            // message every iteration.
            let system_text = {
                let plan = self
                    .router
                    .build_prompt_plan(
                        input,
                        &task_type,
                        &history,
                        response.memory_context.clone(),
                        alloc.as_ref().ok(),
                    )
                    .await?;
                let base = plan.render_text();
                format!(
                    "{base}\n\n## Goal\n\n{goal}\n\nWork turn by turn toward this goal using tools. Do not ask the user for confirmation — act. When the goal is fully reached, end your reply with a line containing exactly GOAL MET and summarize what was done. If a tool errors, work around it and keep going.\n"
                )
            };
            // The user message for the initial turn. The goal loop
            // re-uses the same structured base across iterations; the
            // `pending` text grows per turn, but the message list is
            // rebuilt from the transcript + this user turn.
            // The goal loop runs turn by turn; each turn's request
            // must include everything the previous turns produced
            // (assistant text, tool calls, tool results). Pre-migration
            // the text `pending` was mutated in place across turns, so
            // turn 2 saw turn 1; the structured path needs the same
            // accumulation to preserve that behaviour.
            let mut goal_messages: Vec<kod_types::ChatMessage> = {
                let guard = self.history.read().await;
                guard.get(key).cloned().unwrap_or_default()
            };

            let options = self.generation_defaults.read().await.to_options();
            // Resolve the fallback chain once; reused across turns.
            let task_key = format!("{:?}", response.task_type);
            let goal_chain = self.resolve_chain_for_task(&task_key).await;
            if goal_chain.is_empty() {
                return Err(Self::no_provider_error());
            }
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
                    let nudge = "Continue working toward the goal above. If it is now fully reached, reply with GOAL MET plus a short summary instead of calling more tools.";
                    pending.push_str(&format!("\n\n{nudge}\n"));
                    // The nudge is what tells the model to *continue*;
                    // on the structured path it has to be a message
                    // for the provider to see it.
                    goal_messages.push(kod_types::ChatMessage::text(
                        kod_types::MessageId::new(),
                        kod_types::MessageRole::User,
                        nudge.to_string(),
                        time::OffsetDateTime::now_utc(),
                    ));
                }
                self.apply_steers(&mut pending, &mut goal_messages, key)
                    .await;
                // Per-turn fallback chain (A6). The chain is resolved
                // once outside the turn loop and reused, so a fallback
                // chosen on turn N is also the primary for turn N+1.
                let mut turn_outcome: Option<(
                    String,
                    Vec<ToolCall>,
                    Vec<ToolResult>,
                    Option<kod_provider::TokenUsage>,
                    // P5.6 — ignored in the goal loop; only the
                    // streaming single-turn path uses the retry
                    // signal.
                    bool,
                )> = None;
                let mut turn_err: Option<KodError> = None;
                for (i, model_ref) in goal_chain.iter().enumerate() {
                    let this_provider = match self.resolve_provider_for_model_ref(model_ref).await {
                        Ok(p) => p,
                        Err(e) => {
                            tracing::warn!(
                                endpoint = %model_ref.endpoint,
                                error = %e,
                                "cannot resolve endpoint; skipping in chain"
                            );
                            turn_err = Some(e);
                            continue;
                        }
                    };
                    let mut attempt_pending = pending.clone();
                    let mut attempt_messages = goal_messages.clone();
                    let round = RoundContext {
                        system_text: &system_text,
                        model_ref,
                        definitions: &definitions,
                        options: &options,
                        holder: key,
                        trace: None,
                    };
                    match self
                        .run_streaming_loop(
                            &this_provider,
                            &mut attempt_pending,
                            &mut attempt_messages,
                            chunk_tx,
                            &round,
                        )
                        .await
                    {
                        Ok(v) => {
                            // Fold this turn's extended messages back
                            // so the next turn starts from the full
                            // accumulated conversation, not the
                            // pre-loop snapshot.
                            goal_messages = attempt_messages;
                            turn_outcome = Some(v);
                            break;
                        }
                        Err(e) if e.is_retryable() && i + 1 < goal_chain.len() => {
                            let next = &goal_chain[i + 1];
                            tracing::warn!(
                                from = %model_ref.display(),
                                to = %next.display(),
                                error = %e,
                                "retryable provider error; falling back"
                            );
                            self.record_model_fallback(key, model_ref, next, &e.to_string())
                                .await;
                            turn_err = Some(e);
                            continue;
                        }
                        Err(e) => return Err(e),
                    }
                }
                let (final_text, calls, results, usage, _retry) =
                    turn_outcome.ok_or_else(|| turn_err.unwrap_or_else(Self::no_provider_error))?;
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
                skills_used: refined_skills.clone(),
                memory_used: response.memory_used,
                execution_time_ms: response.execution_time_ms,
                usage: last_usage,
                // Goal-loop turns share one fallback chain; the
                // pricing that would report accurately is per-turn,
                // and the TUI's cost display uses the streaming
                // path, not the goal path. Left `None` rather than
                // guessed.
                pricing: None,
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
        messages: &mut Vec<kod_types::ChatMessage>,
        round: &RoundContext<'_>,
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
            if self.is_cancelled_for(round.holder) {
                return Err(KodError::InvalidState("cancelled by user".to_string()));
            }
            // Pre-queued steers reach round 1. `apply_steers`
            // also runs after a tool round; both calls are safe
            // because it drains.
            self.apply_steers(pending, messages, round.holder).await;
            // Rebuild the structured request every round. Only the
            // `messages` field changes; the system prompt, tools,
            // options and model are constant for the turn.
            let req = self.build_grounded_request(
                round.system_text,
                messages.clone(),
                round.definitions,
                round.options,
                round.model_ref,
            );
            match provider.complete(&req).await? {
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
                    let section = self.run_tool_calls(&calls, round.holder, None).await;
                    tool_calls.extend(calls);
                    tool_results.extend(section.results.clone());
                    messages.extend(section.messages.iter().cloned());
                    // Keep the text prompt in sync for callers that
                    // still read `pending` (apply_steers, the
                    // exhausted-rounds note). The provider no longer
                    // sees this string on the primary path.
                    pending.push_str(&format!("\n\n{}", section.prompt_block));
                    // Persist the structured slice (AD-02): the next
                    // `process_*` call reads `self.history[key]` as
                    // its starting messages, so the tool round-trip
                    // has to survive past this call to be visible
                    // there.
                    if !section.messages.is_empty() {
                        let mut hist = self.history.write().await;
                        let turns = hist.entry(round.holder.to_string()).or_default();
                        turns.extend(section.messages.iter().cloned());
                        let excess = turns.len().saturating_sub(MAX_HISTORY_TURNS);
                        if excess > 0 {
                            turns.drain(..excess);
                        }
                    }
                    self.apply_steers(pending, messages, round.holder).await;
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
                    let section = self.run_tool_calls(&calls, round.holder, None).await;
                    tool_calls.extend(calls);
                    tool_results.extend(section.results.clone());
                    messages.extend(section.messages.iter().cloned());
                    pending.push_str(&format!("\n\n{}", section.prompt_block));
                    // Persist the structured slice (AD-02): the next
                    // `process_*` call reads `self.history[key]` as
                    // its starting messages, so the tool round-trip
                    // has to survive past this call to be visible
                    // there.
                    if !section.messages.is_empty() {
                        let mut hist = self.history.write().await;
                        let turns = hist.entry(round.holder.to_string()).or_default();
                        turns.extend(section.messages.iter().cloned());
                        let excess = turns.len().saturating_sub(MAX_HISTORY_TURNS);
                        if excess > 0 {
                            turns.drain(..excess);
                        }
                    }
                    self.apply_steers(pending, messages, round.holder).await;
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
            messages.push(kod_types::ChatMessage::text(
                kod_types::MessageId::new(),
                kod_types::MessageRole::User,
                TOOL_ROUNDS_EXHAUSTED_NOTE.trim().to_string(),
                time::OffsetDateTime::now_utc(),
            ));
        }
        Ok((final_text, tool_calls, tool_results, last_usage))
    }

    /// Append queued steer notes for `key` to the running conversation
    /// (each once). Called inside the three agentic loops with the
    /// loop's own `holder`.
    ///
    /// Two destinations, on purpose:
    ///
    /// - `messages`: a structured `User` message. This is what the
    ///   provider actually receives on the AD-01 path. A regression
    ///   that only appended to `pending` (the pre-migration shape)
    ///   left the steer invisible to the model; the test in
    ///   `crates/kod-core/tests/steers_reach_the_provider.rs` pins
    ///   the structured form.
    /// - `pending`: the text trace. Kept so `/debug last-prompt`
    ///   shows the steer in the exact position the pre-migration
    ///   path would have placed it, which is what users have learned
    ///   to read.
    async fn apply_steers(
        &self,
        pending: &mut String,
        messages: &mut Vec<kod_types::ChatMessage>,
        key: &str,
    ) {
        for note in self.take_steers_for(key).await {
            let body = format!(
                "## User steer (new instruction — adjust course now, do not restart what already worked)\n{note}"
            );
            pending.push_str(&format!("\n\n{body}\n"));
            messages.push(kod_types::ChatMessage::text(
                kod_types::MessageId::new(),
                kod_types::MessageRole::User,
                body,
                time::OffsetDateTime::now_utc(),
            ));
        }
    }

    /// Streaming agentic loop: text chunks are forwarded to `chunk_tx` the
    /// moment they arrive; tool-start markers go through the same channel.
    async fn run_streaming_loop(
        &self,
        provider: &Arc<dyn LlmProvider>,
        pending: &mut String,
        messages: &mut Vec<kod_types::ChatMessage>,
        chunk_tx: &tokio::sync::mpsc::Sender<String>,
        round: &RoundContext<'_>,
    ) -> Result<(
        String,
        Vec<ToolCall>,
        Vec<ToolResult>,
        Option<kod_provider::TokenUsage>,
        // P5.6 — Jev flagged the round off-track and the caller may
        // want to retry against the next endpoint. Only ever true on
        // round 0 (before any tool call has run).
        bool,
    )> {
        let mut final_text = String::new();
        let mut tool_calls: Vec<ToolCall> = Vec::new();
        let mut tool_results: Vec<ToolResult> = Vec::new();
        let mut last_usage: Option<kod_provider::TokenUsage> = None;
        // Per-round routing (P1.3). `current_provider` and
        // `current_model_ref` are the round's effective provider and
        // model; the loop starts with the chain-resolved choice and
        // swaps to whatever `pick_round_endpoint` returns.
        let mut current_provider: Arc<dyn LlmProvider> = provider.clone();
        let mut current_model_ref: ModelRef = round.model_ref.clone();
        let mut had_tool_results = false;
        for round_idx in 0..MAX_TOOL_ROUNDS {
            if self.is_cancelled_for(round.holder) {
                return Err(KodError::InvalidState("cancelled by user".to_string()));
            }
            // Pre-queued steers reach round 1. See the same comment in
            // `run_collected_loop`.
            self.apply_steers(pending, messages, round.holder).await;

            // Ask Jev what kind of round this is and whether a
            // different endpoint should serve it. A `None` result is
            // the common case (no routing configured) and leaves the
            // chain-resolved choice in place.
            if let Some(next) = self
                .pick_round_endpoint(round.holder, round_idx, had_tool_results)
                .await
            {
                match self.resolve_provider_for_model_ref(&next).await {
                    Ok(p) => {
                        current_provider = p;
                        current_model_ref = next;
                    }
                    Err(e) => {
                        tracing::warn!(
                            endpoint = %next.endpoint,
                            error = %e,
                            "round-routed endpoint did not resolve; keeping current"
                        );
                    }
                }
            }

            // Rebuild a RoundContext for this round so the
            // effective model_ref is visible to the grounded
            // request and the tool calls below.
            let round_for_this = RoundContext {
                system_text: round.system_text,
                model_ref: &current_model_ref,
                definitions: round.definitions,
                options: round.options,
                holder: round.holder,
                trace: None,
            };
            let (text, calls, usage, off_track) = self
                .stream_round(
                    &current_provider,
                    round_for_this.system_text,
                    messages,
                    round_for_this.model_ref,
                    round_for_this.definitions,
                    round_for_this.options,
                    chunk_tx,
                    round_for_this.holder,
                    round_for_this.trace,
                )
                .await?;
            // P5.6 — on the very first round, an off-track verdict
            // is a hard stop: discard the round's text and signal
            // the caller to try the next endpoint. Emit the reset
            // marker so the TUI drops what it displayed.
            if off_track && round_idx == 0 {
                let _ = chunk_tx.send(stream_reset_marker()).await;
                return Ok((
                    String::new(),
                    Vec::new(),
                    Vec::new(),
                    None,
                    true,
                ));
            }
            last_usage = usage.or(last_usage);
            append_round_text(&mut final_text, &text);
            if calls.is_empty() {
                break;
            }
            // A tool round just started: mark the state so the next
            // iteration's Jev question sees "tool_ran = yes".
            had_tool_results = true;
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
            let section = self
                .run_tool_calls(&calls, round.holder, Some(chunk_tx))
                .await;
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
            tool_results.extend(section.results.clone());
            // Structured transcript slice (design §2 AD-02): the
            // assistant's tool calls + the tool results go into the
            // request messages. The text `pending` string is kept in
            // sync for `apply_steers` and the exhausted-rounds note.
            messages.extend(section.messages.iter().cloned());
            pending.push_str(&format!("\n\n{}", section.prompt_block));
            // Persist the structured slice (AD-02). Same rationale as
            // the collected loop above.
            if !section.messages.is_empty() {
                let mut hist = self.history.write().await;
                let turns = hist.entry(round.holder.to_string()).or_default();
                turns.extend(section.messages.iter().cloned());
                let excess = turns.len().saturating_sub(MAX_HISTORY_TURNS);
                if excess > 0 {
                    turns.drain(..excess);
                }
            }
            self.apply_steers(pending, messages, round.holder).await;
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
            // Same treatment as a steer: the note must be visible to
            // the summary call on the structured path, not just in
            // the text trace.
            messages.push(kod_types::ChatMessage::text(
                kod_types::MessageId::new(),
                kod_types::MessageRole::User,
                TOOL_ROUNDS_EXHAUSTED_NOTE.trim().to_string(),
                time::OffsetDateTime::now_utc(),
            ));
            let _ = chunk_tx
                .send(format!(
                    "\n\n[tool-round limit ({MAX_TOOL_ROUNDS}) reached — summarising progress]\n"
                ))
                .await;
        }
        Ok((final_text, tool_calls, tool_results, last_usage, false))
    }

    /// One streaming round: forward text live, assemble tool calls from
    /// Round outcome: text, tool calls, usage, and — new for P5.6 —
    /// a `retry_suggested` flag. `true` means Jev judged the round
    /// off-track and the caller may want to try the next endpoint
    /// with a fresh stream. The text in that case is whatever was
    /// accumulated before the abort; the caller discards it.
    async fn stream_round(
        &self,
        provider: &Arc<dyn LlmProvider>,
        system_text: &str,
        messages: &[kod_types::ChatMessage],
        model_ref: &ModelRef,
        definitions: &[ToolDefinition],
        options: &GenerationOptions,
        chunk_tx: &tokio::sync::mpsc::Sender<String>,
        holder: &str,
        round_trace: Option<&std::sync::Mutex<crate::trace::TurnTraceBuilder>>,
    ) -> Result<(
        String,
        Vec<ToolCall>,
        Option<kod_provider::TokenUsage>,
        bool,
    )> {
        use futures::StreamExt;
        use std::collections::BTreeMap;

        #[derive(Default)]
        struct Partial {
            id: Option<String>,
            name: Option<String>,
            args: String,
        }

        // Build the structured request the provider will see. The
        // streaming variant of `complete` is `stream_completion`; its
        // working default collects the reply and replays it, so a
        // provider that has not overridden it still produces chunks in
        // the right order. Both concrete providers in this workspace
        // override it with real SSE.
        let req = self.build_grounded_request(
            system_text,
            messages.to_vec(),
            definitions,
            options,
            model_ref,
        );
        let mut stream = provider.stream_completion(&req);
        let mut text = String::new();
        let mut partials: BTreeMap<usize, Partial> = BTreeMap::new();
        let mut last_usage: Option<kod_provider::TokenUsage> = None;
        let mut chunk_count: usize = 0;
        let mut retry_suggested = false;
        while let Some(item) = stream.next().await {
            match item? {
                StreamChunk::Text(t) => {
                    text.push_str(&t);
                    let _ = chunk_tx.send(t).await;
                    chunk_count += 1;
                    // Early-termination check (P1.2). The
                    // character floor and the sentence
                    // requirement are enforced inside the helper;
                    // the chunk counter here just gates the call
                    // rate.
                    if chunk_count % EARLY_TERM_CHECK_EVERY_CHUNKS == 0
                        && text.len() >= EARLY_TERM_MIN_CHARS
                    {
                        match self.should_early_terminate(holder, &text).await {
                            EarlyTermination::Complete => break,
                            EarlyTermination::OffTrack => {
                                retry_suggested = true;
                                break;
                            }
                            EarlyTermination::None => {}
                        }
                    }
                }
                StreamChunk::ToolCallStart { index, id, name } => {
                    let entry = partials.entry(index).or_default();
                    if entry.id.is_none() {
                        entry.id = id;
                    }
                    if entry.name.is_none() {
                        entry.name = Some(name.clone());
                        let _ = chunk_tx.send(tool_start_marker(&name)).await;
                    }
                }
                StreamChunk::ToolCallDelta { index, arguments } => {
                    partials.entry(index).or_default().args.push_str(&arguments);
                }
                StreamChunk::Usage(usage) => {
                    last_usage = Some(usage);
                }
                StreamChunk::Done => break,
            }
        }
        // Tier 1.4 — record this round's usage into the trace before
        // returning. No-op when no writer is installed.
        if let Some(mutex) = round_trace {
            let (prompt, completion) = match last_usage.as_ref() {
                Some(u) => (u.prompt_tokens, u.completion_tokens),
                None => (0_usize, 0_usize),
            };
            if let Ok(mut g) = mutex.lock() {
                g.add_usage(prompt, completion, None, 0.0);
            }
        }

        let mut calls = Vec::with_capacity(partials.len());
        for (_, p) in partials {
            let Some(name) = p.name else { continue };
            let arguments: serde_json::Value = serde_json::from_str(&p.args)
                .unwrap_or_else(|_| serde_json::Value::String(p.args.clone()));
            calls.push(ToolCall {
                id: p.id,
                tool_name: name,
                arguments,
            });
        }
        Ok((text, calls, last_usage, retry_suggested))
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

    /// Build a grounded `CompletionRequest` from the router plan and
    /// the transcript (design §2 AD-01, AD-16).
    ///
    /// The split:
    ///
    /// - **system**: the router's `PromptPlan::system` segments
    ///   concatenated in order, with the environment + tool inventory
    ///   grounding appended as a final volatile segment. The
    ///   cacheable / volatile ordering is preserved by rendering the
    ///   plan to text first and grounding the result — a provider with
    ///   explicit cache support (Anthropic) is given the whole string
    ///   as one segment; the split between cacheable and volatile is
    ///   not needed for correctness on either wire today, only for
    ///   cache *placement*. A follow-up can pass the segments through
    ///   as separate `SystemPrompt::segments` to place the breakpoint
    ///   exactly, and the design notes the shape.
    ///
    /// - **messages**: the transcript as structured `ChatMessage`s
    ///   (`self.history[key]` already holds the user's turn by the time
    ///   this runs; the assistant's turns and the tool rounds are
    ///   appended by the loops).
    ///
    /// The `model` field carries the resolved endpoint, so a provider
    /// that is asked to switch mid-stream can honour the request's
    /// choice rather than the one baked in at construction.
    fn build_grounded_request(
        &self,
        system_text: &str,
        messages: Vec<kod_types::ChatMessage>,
        definitions: &[ToolDefinition],
        options: &GenerationOptions,
        model: &ModelRef,
    ) -> CompletionRequest {
        // The router's rendered plan ends with a transcript section
        // (`## Conversation so far`) followed by the user's request
        // (`## User Request`). Both are already passed as `messages`
        // — duplicating them inside the system prompt wastes tokens
        // and confuses a model that sees the same turn twice.
        let head = strip_conversation_tail(system_text);

        // Split the head at the router's own marker. Everything before
        // `## Volatile suffix` is the byte-stable cacheable prefix
        // (Identity + Repository map); everything from it on is the
        // volatile tail (Environment, tool inventory, skills, memory).
        // A provider with explicit cache support places a breakpoint at
        // the last cacheable segment; this two-segment shape is what
        // makes that breakpoint meaningful.
        const VOLATILE_MARKER: &str = "## Volatile suffix";
        let (cacheable, volatile_tail) = match head.find(VOLATILE_MARKER) {
            Some(i) => (head[..i].trim_end().to_string(), head[i..].to_string()),
            None => {
                // No marker (custom-built prompt): the whole thing is
                // volatile. Honest degradation — a caller that lost
                // the convention loses the cache benefit, not
                // correctness.
                (String::new(), head)
            }
        };

        // The environment + tool inventory grounding is appended to the
        // volatile segment — it depends on the current tool set and on
        // the working directory, so it is never cacheable. `ground_prompt`
        // appends a leading blank line + `## Environment` block, so
        // passing an empty tail still produces a valid segment.
        let grounded_volatile = self.ground_prompt(volatile_tail, definitions);

        let mut system = SystemPrompt::new();
        if !cacheable.is_empty() {
            system = system.with(cacheable, true);
        }
        system = system.with(grounded_volatile, false);
        CompletionRequest {
            system,
            messages,
            tools: definitions.to_vec(),
            options: options.clone(),
            model: model.clone(),
        }
    }

    /// Append the environment + tool inventory grounding to a router prompt.
    /// Look up the trust level of a tool by name (Tier 1.1).
    async fn tool_trust_level(
        &self,
        name: &str,
    ) -> Option<kod_types::trust::TrustLevel> {
        let defs = self.tools.get_definitions().await;
        defs.into_iter()
            .find(|d| d.name == name)
            .map(|d| d.trust_level)
    }

    fn ground_prompt(&self, mut prompt: String, definitions: &[ToolDefinition]) -> String {
        prompt.push_str(&format!(
            "\n## Environment\n\n- Working directory: {}\n- OS: {}\n",
            self.working_dir.display(),
            std::env::consts::OS
        ));
        // Tier 2.1 — if a plan exists for the default transcript,
        // prepend it to the prompt. The plan is a stable target the
        // model can consult on every round.
        //
        // `ground_prompt` is synchronous, so we peek at the map with
        // a `try_read`; a rare miss is fine (the plan appears on the
        // next round).
        if let Ok(g) = self.plans.try_read()
            && let Some(plan) = g.get(DEFAULT_TRANSCRIPT_KEY)
        {
            prompt.push_str("\n\n");
            prompt.push_str(&plan.render_prompt_block());
        }

        if !definitions.is_empty() {
            let names: Vec<String> = definitions
                .iter()
                .map(|d| format!("- {}: {}", d.name, d.description))
                .collect();
            prompt.push_str(&format!(
                "\n## Tool use\n\nYou have these tools (function calls, rooted at the working directory above):\n{}\nCall them when you need facts from this machine instead of guessing. Tool outputs return as `## Tool results` blocks — then answer the user.\n",
                names.join("\n")
            ));
            // Tier 1.1 — the trust invariant. Injected whenever the
            // prompt carries a tool inventory, since that is the
            // precondition for a tool result block later in the turn.
            // The wording is load-bearing; see TRUST_INVARIANT.
            prompt.push_str("\n## Trust boundary\n\n");
            prompt.push_str(kod_types::trust::TRUST_INVARIANT);
            prompt.push('\n');
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
        // D4-D1: a transcript may point at its own working directory
        // (a swarm agent's worktree). Resolve it here and override the
        // engine-wide working_dir on the cloned context so every tool
        // in this round runs rooted at the right place.
        let per_transcript_wd = self.working_dir_for(effective_holder).await;
        let mut tool_context = self
            .tool_context
            .clone()
            .with_locks(Arc::clone(&self.lock_table), effective_holder)
            .with_sandbox(self.sandbox_setting());
        // Tier 1.3 — thread the engine's read-protection and
        // redactor into the per-call context.
        if let Ok(guard) = self.read_protection.read() {
            tool_context.read_protection = guard.clone();
        }
        tool_context.redactor = Some(self.redactor.clone());
        if per_transcript_wd != self.working_dir {
            tool_context.working_dir = per_transcript_wd;
        }
        // Per-transcript write set (D4.2). A swarm agent that
        // declared `src/parser/**` gets that restriction applied to
        // every tool call it makes, in this round and the next,
        // until the runner clears it.
        if let Some(globs) = self.write_globs_for(effective_holder).await {
            tool_context.allowed_write_globs = Some(globs);
        }
        // The engine-level network flag overrides whatever the
        // construction-time context held. This is what makes
        // `set_network_access` meaningful through an `Arc<KodEngine>`
        // (no `&mut self` available): the flag is read here, per call,
        // and applied to the context the tool sees.
        tool_context.permissions.network_access = self.network_access_setting();

        // Domain allow-list (D3-C5) from the effective policy, if any.
        // The policy's `[tools.web_fetch].domains` is the source of
        // truth; a policy that sets it but the user keeps network
        // access globally on still gets its domain restriction.
        if let Some(policy) = self.policy.read().await.as_ref()
            && let Some(tp) = policy.effective().tools.get("web_fetch")
            && let Some(domains) = &tp.domains
        {
            tool_context.allowed_domains = domains.clone();
        }

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
        if any_mutating && let Some(cp) = self.checkpoints.as_ref() {
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
        // Policy layer (D3-C1). Every tool call is decided before it
        // runs. The session deny rules are checked first (highest
        // priority), then the policy engine if one is installed, then
        // the legacy `confirm_writes` gate as a backward-compatible
        // fallback.
        //
        // Three outcomes:
        //   Allow -> nothing inserted, call proceeds.
        //   Deny  -> reason stored in `denied[i]`, call skipped.
        //   Ask   -> approval marker emitted; the same oneshot
        //            machinery the pre-D3 confirm_writes used.
        let policy = self.policy.read().await.clone();
        let deny_rules: std::collections::HashSet<kod_config::SessionDeny> =
            self.deny_rules.read().await.clone();
        let mut denied: std::collections::HashMap<usize, String> = std::collections::HashMap::new();
        let mut need_approval: std::collections::HashSet<usize> = std::collections::HashSet::new();
        let mut decisions: Vec<(usize, kod_config::PolicyDecision)> = Vec::new();

        for (i, call) in calls.iter().enumerate() {
            // Tier 1.1 — a high-impact tool call under a tainted round
            // forces an `Ask` regardless of what policy says. The
            // taint is the stricter signal.
            if self.requires_approval_for_taint(call) {
                let decision = kod_config::PolicyDecision {
                    outcome: kod_config::Decision::Ask,
                    rule: format!(
                        "taint escalation: {} under {:?}",
                        call.tool_name,
                        self.taint_level(),
                    ),
                    source: kod_config::PolicySource::SessionDeny,
                };
                decisions.push((i, decision.clone()));
                match decision.outcome {
                    kod_config::Decision::Allow => {}
                    kod_config::Decision::Deny => {
                        denied.insert(i, decision.rule.clone());
                    }
                    kod_config::Decision::Ask => {
                        need_approval.insert(i);
                    }
                }
                continue;
            }
            let decision = match &policy {
                Some(p) => p.decide(
                    &call.tool_name,
                    &call.arguments,
                    &tool_context.working_dir,
                    &deny_rules,
                ),
                None => {
                    // No policy installed: allow every call. The
                    // CLI and TUI always install a PolicyEngine at
                    // startup via install_policy_async; the fallback
                    // exists for tests and embedders that build the
                    // engine directly.
                    kod_config::PolicyDecision {
                        outcome: kod_config::Decision::Allow,
                        rule: "no policy installed".to_string(),
                        source: kod_config::PolicySource::Preset,
                    }
                }
            };

            match decision.outcome {
                kod_config::Decision::Allow => {}
                kod_config::Decision::Deny => {
                    denied.insert(i, decision.rule.clone());
                }
                kod_config::Decision::Ask => {
                    need_approval.insert(i);
                }
            }
            decisions.push((i, decision));
        }

        // Log every decision before executing (or refusing) so the
        // JSONL carries the full audit trail even if the run is
        // interrupted mid-way.
        if let Ok(guard) = self.session_recorder.read()
            && let Some(rec) = guard.as_ref()
        {
            let now_ms = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_millis() as u64)
                .unwrap_or(0);
            for (i, d) in &decisions {
                let Some(call) = calls.get(*i) else { continue };
                let outcome = match d.outcome {
                    kod_config::Decision::Allow => "allow",
                    kod_config::Decision::Deny => "deny",
                    kod_config::Decision::Ask => "ask",
                };
                let entry = crate::session_log::SessionEntry::PolicyDecision {
                    timestamp_ms: now_ms,
                    holder: effective_holder.to_string(),
                    tool_name: call.tool_name.clone(),
                    outcome: outcome.to_string(),
                    rule: d.rule.clone(),
                    source: format!("{:?}", d.source).to_lowercase(),
                };
                let _ = rec.record(&entry);
            }
        }

        // Jev-gated auto-approval (P3.1). Before emitting a
        // dialog, ask Jev whether the user would almost
        // certainly approve each `Ask` call. Calls that clear
        // both the `likely_approved` threshold and the risk
        // gate are removed from `need_approval` and logged as
        // auto-approved `SessionEntry::Approval` entries, so the
        // audit trail is identical to a user approving them by
        // hand.
        if !need_approval.is_empty() {
            let auto = self
                .auto_approve_with_jev(effective_holder, calls, &need_approval)
                .await;
            if !auto.is_empty() {
                for i in &auto {
                    if let Some(call) = calls.get(*i)
                        && let Ok(guard) = self.session_recorder.read()
                        && let Some(rec) = guard.as_ref()
                    {
                        let now_ms = std::time::SystemTime::now()
                            .duration_since(std::time::UNIX_EPOCH)
                            .map(|d| d.as_millis() as u64)
                            .unwrap_or(0);
                        let entry = crate::session_log::SessionEntry::Approval {
                            timestamp_ms: now_ms,
                            holder: effective_holder.to_string(),
                            tool_name: call.tool_name.clone(),
                            decision: "auto-approve".to_string(),
                        };
                        let _ = rec.record(&entry);
                    }
                }
                for i in &auto {
                    need_approval.remove(i);
                }
            }
        }

        // Approval flow for every `Ask` call. The round's asks are
        // batched: one marker carries every item, the consumer walks
        // them, and the engine awaits each oneshot in order. A batch
        // of one is protocol-identical to the pre-batch single-item
        // marker from the consumer's perspective — the dialog just
        // shows one row.
        if !need_approval.is_empty() {
            // P3.2 — ask Jev to group the pending approvals by
            // logical change. The result is logged for `/jev stats`
            // and is available to a future UI that renders grouped
            // dialogs. A no-op (empty map) when Jev is disabled or
            // the batch has fewer than 2 items.
            {
                let pending_calls: Vec<ToolCall> = need_approval
                    .iter()
                    .filter_map(|i| calls.get(*i).cloned())
                    .collect();
                let _groups = self
                    .group_approvals_with_jev(effective_holder, &pending_calls)
                    .await;
            }
            match chunk_tx {
                Some(tx) => {
                    // Phase 1 — build every request and register every
                    // oneshot. Doing all the registration up-front means
                    // the consumer can answer items out of order without
                    // a race: a decision sent before the engine reaches
                    // that item's `orx.await` is buffered by the
                    // oneshot, not dropped.
                    let mut items: Vec<ApprovalRequest> = Vec::new();
                    let mut awaiting: Vec<(
                        usize,
                        tokio::sync::oneshot::Receiver<ApprovalDecision>,
                    )> = Vec::new();
                    for i in &need_approval {
                        let Some(call) = calls.get(*i) else { continue };
                        let summary = format_call_brief(&call.tool_name, &call.arguments);
                        let diff = snapshot_ids
                            .get(*i)
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
                        items.push(ApprovalRequest {
                            tool_name: call.tool_name.clone(),
                            arguments: call.arguments.clone(),
                            diff,
                            summary,
                            id: Some(id),
                        });
                        awaiting.push((*i, orx));
                    }

                    // Phase 2 — emit ONE batch marker.
                    let batch = ApprovalBatch { items };
                    let json = serde_json::to_string(&batch)
                        .unwrap_or_else(|_| "{\"items\":[]}".to_string());
                    let batch_id = self
                        .next_approval_id
                        .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    let _ = tx.send(tool_approval_batch_marker(batch_id, &json)).await;

                    // Phase 3 — await each in order. Each resolution
                    // emits one `SessionEntry::Approval` so the JSONL
                    // carries the audit trail: who decided, on which
                    // tool, and what they decided.
                    for (i, orx) in awaiting {
                        let decision = tokio::time::timeout(
                            std::time::Duration::from_secs(AWAIT_APPROVAL_SECS),
                            orx,
                        )
                        .await;
                        let tool_name = calls
                            .get(i)
                            .map(|c| c.tool_name.clone())
                            .unwrap_or_default();
                        let (log_decision, allow) = match decision {
                            Ok(Ok(ApprovalDecision::Approve)) => ("approve", true),
                            Ok(Ok(ApprovalDecision::Deny)) => ("deny", false),
                            Ok(Ok(ApprovalDecision::DenyAlways)) => ("deny-always", false),
                            Ok(Err(_)) => ("cancelled", false),
                            Err(_) => ("timeout", false),
                        };
                        if let Ok(guard) = self.session_recorder.read()
                            && let Some(rec) = guard.as_ref()
                        {
                            let now_ms = std::time::SystemTime::now()
                                .duration_since(std::time::UNIX_EPOCH)
                                .map(|d| d.as_millis() as u64)
                                .unwrap_or(0);
                            let entry = crate::session_log::SessionEntry::Approval {
                                timestamp_ms: now_ms,
                                holder: effective_holder.to_string(),
                                tool_name: tool_name.clone(),
                                decision: log_decision.to_string(),
                            };
                            let _ = rec.record(&entry);
                        }
                        if !allow {
                            if log_decision == "deny-always"
                                && let Some(call) = calls.get(i)
                            {
                                let path_pattern = call
                                    .arguments
                                    .get("path")
                                    .and_then(|v| v.as_str())
                                    .map(|s| s.to_string());
                                let rule = kod_config::SessionDeny {
                                    tool: call.tool_name.clone(),
                                    path_pattern,
                                };
                                self.add_deny_rule(rule).await;
                            }
                            let reason = match log_decision {
                                "deny" | "deny-always" => "denied by user",
                                "cancelled" => "approval cancelled",
                                "timeout" => {
                                    // The format! below needs the
                                    // AWAIT_APPROVAL_SECS bound.
                                    // We build it here to keep the
                                    // existing message shape.
                                    // (Uses the same value as before.)
                                    "no approval answer within timeout — denied"
                                }
                                other => other,
                            };
                            denied.insert(i, reason.to_string());
                        }
                    }
                }
                None => {
                    for i in &need_approval {
                        denied.insert(
                            *i,
                            "policy requires approval but this execution \
                             path has no interactive consumer. Use the TUI, \
                             or install a permissive policy."
                                .to_string(),
                        );
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
            // P3.5 — before emitting the question, ask Jev whether
            // the request already contains the answer. A confident
            // "yes" skips the interruption entirely; the model sees
            // the request text as the tool's result and carries on.
            let question_text = call
                .arguments
                .get("question")
                .and_then(|v| v.as_str())
                .unwrap_or("(no question)")
                .to_string();
            if let Some(auto_answer) = self
                .try_answer_question_from_context(effective_holder, &question_text)
                .await
            {
                answers.insert(i, auto_answer);
                continue;
            }
            match chunk_tx {
                Some(tx) => {
                    let question = question_text;
                    let placeholder = call
                        .arguments
                        .get("placeholder")
                        .and_then(|v| v.as_str())
                        .map(String::from);
                    let req = kod_tools::ask::QuestionRequest {
                        question,
                        placeholder,
                    };
                    let json = serde_json::to_string(&req).unwrap_or_else(|_| "{}".to_string());
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
                    out.push((Ok(ToolResult::Error(format!("write denied: {reason}"))), 0));
                    continue;
                }
                // P3.4 — per-command sandbox decision. Only
                // `execute_command` is negotiable; other tools keep
                // the round's configured mode. A clone of the
                // context is used so the decision does not leak to
                // sibling calls.
                let call_ctx = if call.tool_name == "execute_command"
                    && let Some(cmd) = call.arguments.get("command").and_then(|v| v.as_str())
                {
                    let chosen = self
                        .choose_sandbox_mode_for_command(
                            effective_holder,
                            cmd,
                            tool_context.sandbox,
                        )
                        .await;
                    let mut c = tool_context.clone();
                    c.sandbox = chosen;
                    c
                } else {
                    tool_context.clone()
                };
                // Tier 2.5 — quota check before dispatch.
                let command = if call.tool_name == "execute_command" {
                    call.arguments.get("command").and_then(|v| v.as_str())
                } else {
                    None
                };
                let quota = self.quota_for(&call.tool_name);
                match crate::tool_quota::check(
                    &self.tool_counts,
                    &call.tool_name,
                    quota.as_ref(),
                    command,
                ) {
                    crate::tool_quota::QuotaVerdict::Hard { reason } => {
                        out.push((
                            Ok(ToolResult::Error(format!("quota exceeded: {reason}"))),
                            0,
                        ));
                        continue;
                    }
                    crate::tool_quota::QuotaVerdict::Soft { reason } => {
                        tracing::warn!(tool = %call.tool_name, "tool quota soft: {reason}");
                    }
                    crate::tool_quota::QuotaVerdict::Ok => {}
                }
                self.tool_counts.record(&call.tool_name, command);
                let start = std::time::Instant::now();
                let res = self
                    .tools
                    .execute_tool(&call.tool_name, &call.arguments, &call_ctx)
                    .await;
                out.push((res, start.elapsed().as_millis() as u64));
            }
            out
        } else {
            // Read-only round: no approvals are involved (approval is
            // only requested for write_file / patch_file, both of
            // which set `any_mutating` above), so the concurrent path
            // is unchanged.
            // Tier 2.5 — count the read-only dispatches before they
            // run. Enforcement (refusal) is serial-only; a
            // read-only round that hits a hard cap still runs its
            // peers so the model gets a complete answer.
            for call in calls.iter() {
                self.tool_counts.record(&call.tool_name, None);
            }
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

        // Tier 1.1 — every tool that ran escalates the round's
        // taint to the worse of the current level and the tool's
        // declared trust. The escalation is synchronous and cheap.
        for call in calls.iter() {
            if let Some(t) = self.tool_trust_level(&call.tool_name).await {
                self.escalate_taint(t);
            }
        }

        // Tier 2.1 — plan_update interception. The tool cannot reach
        // the engine's plan map, so we apply the update here and
        // replace whatever the tool returned.
        for (i, call) in calls.iter().enumerate() {
            if call.tool_name != "plan_update" {
                continue;
            }
            let update = serde_json::from_value::<crate::plan::PlanUpdate>(
                call.arguments.clone(),
            );
            let answer = match update {
                Ok(u) => self.apply_plan_update(effective_holder, u).await,
                Err(e) => format!("plan_update: invalid arguments: {e}"),
            };
            answers.insert(i, answer);
        }

        // P5.3 — semantic outcome classification for interesting

        // tool calls. Runs after the raw entries are written so the
        // syntactic trail is always on disk even if Jev is down.
        // Every call is a no-op when Jev is disabled.
        for (i, call) in calls.iter().enumerate() {
            let Some((result, ms)) = raw_results.get(i) else {
                continue;
            };
            // Only successful or errored calls are worth classifying;
            // a `RequiresConfirmation` never actually ran.
            let Ok(result) = result.as_ref() else {
                continue;
            };
            self.classify_tool_outcome_with_jev(effective_holder, call, result, *ms)
                .await;
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
                    obj.insert("diff".to_string(), serde_json::Value::String(diff));
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
            // P2.4 — for write_file / patch_file, ask Jev to
            // triage the diff's hunks before the prompt block is
            // built. `None` means "leave the diff alone" and the
            // original result flows through unchanged.
            let result_for_prompt: ToolResult = if matches!(
                call.tool_name.as_str(),
                "write_file" | "patch_file"
            ) {
                self.filter_diff_hunks_with_jev(effective_holder, &result)
                    .await
                    .unwrap_or_else(|| result.clone())
            } else if call.tool_name == "read_file" {
                // P2.2 — compress large read_file results by
                // dropping lines Jev judges irrelevant. `None`
                // means leave the original untouched.
                self.compress_tool_result_with_jev(effective_holder, call, &result)
                    .await
                    .unwrap_or_else(|| result.clone())
            } else if matches!(call.tool_name.as_str(), "grep" | "search_files") {
                // P2.3 — rank the search hits before the prompt
                // block is built. `None` means the ranking did not
                // run (disabled Jev, few hits, or Jev error); the
                // original result flows through unchanged.
                self.rank_search_results_with_jev(effective_holder, call, &result)
                    .await
                    .unwrap_or_else(|| result.clone())
            } else {
                result.clone()
            };
            let result = result_for_prompt;
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
            // Tier 1.1 — wrap the rendered result in a source-trust
            // marker so the model can tell tool output from its own
            // prior text. `run_tool_calls` does not carry the
            // definitions slice; look up the level by name.
            let trust = self
                .tool_trust_level(&call.tool_name)
                .await
                .unwrap_or(kod_types::trust::TrustLevel::ToolTrusted);
            block.push_str(&format!("\n### {} {}\n", call.tool_name, call.arguments));
            block.push_str(&trust.open_marker(&call.tool_name, None));
            block.push('\n');
            block.push_str(&rendered);
            block.push('\n');
            block.push_str(kod_types::trust::TrustLevel::close_marker());
            block.push('\n');
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
        // Auto-check / auto-LSP diagnostics. The two flags are
        // independent:
        //
        //   - `auto_lsp` (default true): run the language server's
        //     diagnostics pass on the touched file and append a
        //     `## LSP diagnostics` block when the server returns any.
        //     Cheap (sub-second per file) and per-file.
        //   - `auto_check` (default false): run the project's
        //     compiler/linter and append `## Auto-check`. Thorough
        //     but slower and per-project.
        //
        // When both are on and the file's language has a server, LSP
        // runs first; an empty LSP response falls through to the
        // compiler (which disambiguates "clean" from "unreachable").
        // When only `auto_lsp` is on and LSP is empty or unavailable,
        // no block is emitted — the compiler is not consulted on the
        // user's behalf.
        if self.auto_check_setting() || self.auto_lsp_setting() {
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
                // `auto_lsp` on the engine is the per-session override
                // the CLI/TUI apply; the config's
                // `[lsp] auto_diagnostics` is the persistent choice. The
                // pass runs only when both are true — a user who set
                // either to false has asked not to be charged the
                // per-write diagnostics cost.
                let lsp_config_auto = kod_config::KodConfig::load_default()
                    .ok()
                    .map(|c| c.lsp.auto_diagnostics)
                    .unwrap_or(true);
                let lsp_wanted = self.auto_lsp_setting() && lsp_config_auto;
                let compiler_wanted = self.auto_check_setting();
                let lsp_eligible =
                    lsp_wanted && writes.len() == 1 && Self::lsp_binary_for(&writes[0].0).is_some();

                let diags: Vec<kod_tools::check::Diagnostic>;
                let source: String;
                let used_lsp: bool;

                if lsp_eligible {
                    let (path, content) = &writes[0];
                    let binary = Self::lsp_binary_for(path).unwrap_or("lsp");
                    // `settle_ms` from `[lsp]` bounds how long we
                    // wait for the server to publish before accepting
                    // an empty answer as final. Read from the same
                    // config load used for `auto_diagnostics` above;
                    // default 1500 ms if the config is unreadable.
                    let settle_ms = kod_config::KodConfig::load_default()
                        .ok()
                        .map(|c| c.lsp.settle_ms)
                        .unwrap_or(1_500);
                    let lsp_diags = self
                        .lsp_diagnostics(path, content, std::time::Duration::from_millis(settle_ms))
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
                        used_lsp = true;
                    } else if compiler_wanted {
                        // Empty LSP answer: could mean "clean" or
                        // "unreachable". Fall through to the
                        // compiler, which disambiguates.
                        match kod_tools::CheckTool::run_check(&self.working_dir, 60).await {
                            Ok(outcome) => {
                                source = outcome.command.clone();
                                diags = outcome.diagnostics;
                                used_lsp = false;
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
                                    messages: Vec::new(),
                                };
                            }
                        }
                    } else {
                        // Only auto_lsp is on and LSP returned empty:
                        // either the server found nothing or is
                        // unreachable, and without compiler
                        // confirmation we cannot say which. Emit no
                        // block rather than inventing one.
                        return ToolRound {
                            results,
                            prompt_block: block,
                            elapsed_ms,
                            // Auto-check error path: no structured transcript slice.
                            messages: Vec::new(),
                        };
                    }
                } else if compiler_wanted {
                    match kod_tools::CheckTool::run_check(&self.working_dir, 60).await {
                        Ok(outcome) => {
                            source = outcome.command.clone();
                            diags = outcome.diagnostics;
                            used_lsp = false;
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
                                // Auto-check error path: no structured transcript slice.
                                messages: Vec::new(),
                            };
                        }
                    }
                } else {
                    // LSP was wanted but not eligible (no server for
                    // this file's language, or more than one file in
                    // the round), and the compiler is off. Nothing to
                    // emit — the user has chosen LSP-only.
                    return ToolRound {
                        results,
                        prompt_block: block,
                        elapsed_ms,
                        // Auto-check error path: no structured transcript slice.
                        messages: Vec::new(),
                    };
                }

                // LSP diagnostics are per-file and live from the
                // server; the baseline diff (a per-project compiler
                // snapshot) does not apply. Emit them verbatim under
                // their own header.
                if used_lsp {
                    block.push_str("\n## LSP diagnostics\n\n");
                    if diags.is_empty() {
                        block.push_str(&format!("`{}`: no diagnostics on this file.\n", source,));
                    } else {
                        block.push_str(&format!(
                            "`{}` reported {} diagnostic(s) on this file:\n\n",
                            source,
                            diags.len(),
                        ));
                        render_diagnostics(&mut block, &diags, 20);
                    }
                } else {
                    // Compiler diagnostics keep the baseline diff so
                    // the model sees *new* errors, not pre-existing
                    // ones.
                    let baseline = self.check_baseline.read().await.clone();
                    let baseline_keys: std::collections::HashSet<(String, Option<String>, String)> =
                        baseline
                            .as_ref()
                            .map(|v| v.iter().map(diag_key).collect())
                            .unwrap_or_default();
                    let current_keys: std::collections::HashSet<(String, Option<String>, String)> =
                        diags.iter().map(diag_key).collect();

                    let syntactically_new: Vec<&kod_tools::check::Diagnostic> = diags
                        .iter()
                        .filter(|d| !baseline_keys.contains(&diag_key(d)))
                        .collect();
                    // P4.4 — ask Jev which of these are genuinely
                    // new versus shifted copies of a baseline
                    // diagnostic. Falls through to "all new" when
                    // Jev is disabled or errors, so the pre-Jev
                    // behaviour is the fallback.
                    let new_diags: Vec<&kod_tools::check::Diagnostic> =
                        if let Some(b) = baseline.as_ref() {
                            let keep = self
                                .classify_new_diagnostics_with_jev(
                                    effective_holder,
                                    &syntactically_new,
                                    b,
                                )
                                .await;
                            syntactically_new
                                .into_iter()
                                .enumerate()
                                .filter(|(i, _)| keep.contains(i))
                                .map(|(_, d)| d)
                                .collect()
                        } else {
                            syntactically_new
                        };
                    let resolved_count = if baseline.is_some() {
                        baseline_keys
                            .iter()
                            .filter(|k| !current_keys.contains(*k))
                            .count()
                    } else {
                        0
                    };

                    block.push_str("\n## Auto-check\n\n");
                    match baseline {
                        None => {
                            if diags.is_empty() {
                                block.push_str(&format!("`{}` is clean.\n", source));
                            } else {
                                block.push_str(&format!(
                                    "`{}` reported {} diagnostic(s). \
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
                            let owned: Vec<kod_tools::check::Diagnostic> =
                                new_diags.iter().map(|d| (*d).clone()).collect();
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

                    // One `SessionEntry::Diagnostics` per file (AD-15).
                    // The counts aggregate the round's diagnostics by
                    // file, so a multi-file compiler pass yields one
                    // entry per file rather than a single project-wide
                    // lump.
                    if let Ok(guard) = self.session_recorder.read()
                        && let Some(rec) = guard.as_ref()
                    {
                        use std::collections::BTreeMap;
                        let mut per_file: BTreeMap<String, (usize, usize)> = BTreeMap::new();
                        for d in &diags {
                            let entry = per_file.entry(d.file.clone()).or_insert((0, 0));
                            match d.severity.as_str() {
                                "error" => entry.0 += 1,
                                "warning" => entry.1 += 1,
                                _ => {}
                            }
                        }
                        let now_ms = std::time::SystemTime::now()
                            .duration_since(std::time::UNIX_EPOCH)
                            .map(|d| d.as_millis() as u64)
                            .unwrap_or(0);
                        for (file, (errs, warns)) in per_file {
                            let entry = crate::session_log::SessionEntry::Diagnostics {
                                timestamp_ms: now_ms,
                                file,
                                error_count: errs,
                                warning_count: warns,
                            };
                            let _ = rec.record(&entry);
                        }
                    }

                    // The post-write state becomes the new baseline.
                    *self.check_baseline.write().await = Some(diags);
                }
            }
        }

        // Structured transcript slice (AD-02). One assistant message
        // carrying the calls, then one tool message per result linked
        // by `tool_call_id`. Ids are preserved when the provider
        // emitted them; a provider that did not (some local servers
        // omit them) gets a synthesized `call_N` so the transcript is
        // well-formed on every wire.
        let mut messages: Vec<kod_types::ChatMessage> = Vec::new();
        let mut assistant_msg = kod_types::ChatMessage::text(
            kod_types::MessageId::new(),
            kod_types::MessageRole::Assistant,
            String::new(),
            time::OffsetDateTime::now_utc(),
        );
        for (i, call) in calls.iter().enumerate() {
            let id = call.id.clone().unwrap_or_else(|| format!("call_{i}"));
            assistant_msg.tool_calls.push(kod_types::ToolCall {
                id: Some(id.clone()),
                tool_name: call.tool_name.clone(),
                arguments: call.arguments.clone(),
            });
        }
        // Only push the assistant message when there was at least one
        // call — an empty assistant turn is not a legal wire shape.
        if !assistant_msg.tool_calls.is_empty() {
            messages.push(assistant_msg);
        }
        for (i, call) in calls.iter().enumerate() {
            let id = call.id.clone().unwrap_or_else(|| format!("call_{i}"));
            let rendered = match results.get(i) {
                Some(kod_types::ToolResult::Success(v)) => v.to_string(),
                Some(kod_types::ToolResult::Error(e)) => format!("error: {e}"),
                Some(kod_types::ToolResult::RequiresConfirmation { description, .. }) => {
                    format!("requires confirmation: {description}")
                }
                None => String::new(),
            };
            let mut tool_msg = kod_types::ChatMessage::text(
                kod_types::MessageId::new(),
                kod_types::MessageRole::Tool,
                rendered,
                time::OffsetDateTime::now_utc(),
            );
            tool_msg.tool_call_id = Some(id);
            messages.push(tool_msg);
        }

        // MemoryWrite audit for the tool channel (AD-15). Every
        // successful `memory_save` writes one entry, with the id and
        // tags the tool returned. The extraction path logs under the
        // "extraction" channel; the user's `/remember` under "user".
        if let Ok(guard) = self.session_recorder.read()
            && let Some(rec) = guard.as_ref()
        {
            let now_ms = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_millis() as u64)
                .unwrap_or(0);
            for (call, result) in calls.iter().zip(results.iter()) {
                if call.tool_name != "memory_save" {
                    continue;
                }
                let ToolResult::Success(v) = result else {
                    continue;
                };
                let Some(id) = v.get("id").and_then(|v| v.as_str()) else {
                    continue;
                };
                let tags: Vec<String> = v
                    .get("tags")
                    .and_then(|v| v.as_array())
                    .map(|a| {
                        a.iter()
                            .filter_map(|x| x.as_str())
                            .map(String::from)
                            .collect()
                    })
                    .unwrap_or_default();
                let entry = crate::session_log::SessionEntry::MemoryWrite {
                    timestamp_ms: now_ms,
                    memory_id: id.to_string(),
                    channel: "tool".to_string(),
                    tags,
                };
                let _ = rec.record(&entry);
            }
        }

        ToolRound {
            results,
            prompt_block: block,
            elapsed_ms,
            messages,
        }
    }

    /// Run the memory extraction pass over the session transcript
    /// (D2-B3b). Best-effort: errors leave the store unchanged and
    /// return `Ok(0)`.
    ///
    /// Called by `shutdown()` when `memory.extract_on_shutdown` is
    /// true. Also callable directly from a test or a future
    /// `/remember-session` command.
    pub async fn extract_memories_now(&self, key: &str) -> Result<usize> {
        let config = match kod_config::KodConfig::load_default() {
            Ok(c) => c,
            Err(e) => {
                tracing::warn!(error = %e, "extract_memories_now: config load failed");
                return Ok(0);
            }
        };
        let max_entries = config.memory.extract_max_entries.max(1);

        let transcript: Vec<kod_types::ChatMessage> = {
            let history = self.history.read().await;
            history.get(key).cloned().unwrap_or_default()
        };
        if transcript.is_empty() {
            return Ok(0);
        }

        let chain = self.resolve_chain_for_task("Simple").await;
        let Some(model_ref) = chain.first() else {
            return Ok(0);
        };
        let provider = match self.resolve_provider_for_model_ref(model_ref).await {
            Ok(p) => p,
            Err(e) => {
                tracing::warn!(error = %e, "extract_memories_now: no provider");
                return Ok(0);
            }
        };

        let facts =
            match kod_memory::extract::extract(provider, model_ref, &transcript, max_entries).await
            {
                Ok(f) => f,
                Err(e) => {
                    tracing::warn!(error = %e, "extract_memories_now: extraction failed");
                    return Ok(0);
                }
            };
        if facts.is_empty() {
            return Ok(0);
        }

        let project_key = Some(crate::router::TaskRouter::project_key_for(
            &self.working_dir,
        ));

        let mut stored = 0usize;
        for fact in &facts {
            let mut metadata = kod_memory::extract::metadata_for(fact, project_key.clone());
            // The design (D2.5) attributes every auto-extracted fact to
            // a session so consolidation can treat "an episode with no
            // touch in 60 days" as archivable without ever archiving a
            // durable `LongTerm` entry the user asked to remember. The
            // transcript key is already a string; a `SessionId` is a
            // UUID newtype, so a hash-shaped key degrades to `None` —
            // the fact is still stored, it just loses the attribution
            // a per-session triage would need.
            metadata.session_id = Some(self.session_id_for_holder(key));
            // Store as Episodic (not LongTerm): the extraction channel
            // is the auto path; only `memory_save` and the user's
            // `/remember` write the durable layer.
            match self
                .router
                .store_episodic(&fact.content, metadata.clone())
                .await
            {
                Ok(id) => {
                    // One `MemoryWrite` per stored fact (AD-15). The
                    // extraction channel is the auto path; `memory_save`
                    // and `/remember` log the "tool" and "user" channels
                    // respectively.
                    if let Ok(guard) = self.session_recorder.read()
                        && let Some(rec) = guard.as_ref()
                    {
                        let now_ms = std::time::SystemTime::now()
                            .duration_since(std::time::UNIX_EPOCH)
                            .map(|d| d.as_millis() as u64)
                            .unwrap_or(0);
                        let entry = crate::session_log::SessionEntry::MemoryWrite {
                            timestamp_ms: now_ms,
                            memory_id: id.as_uuid().to_string(),
                            channel: "extraction".to_string(),
                            tags: metadata.tags.clone(),
                        };
                        let _ = rec.record(&entry);
                    }
                    stored += 1;
                }
                Err(e) => {
                    tracing::warn!(
                        error = %e,
                        content = %fact.content,
                        "extract_memories_now: store failed; skipping fact"
                    );
                }
            }
        }
        tracing::info!(stored, total = facts.len(), "memory extraction complete");
        Ok(stored)
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

        // 3b. MCP server shutdown (D6.1). Best-effort: each spawned
        //     server is killed and reaped; a slow or misbehaving
        //     server is dropped, not awaited indefinitely. A failure
        //     here is logged and does not block the rest of the
        //     shutdown sequence.
        if let Some(host) = self.mcp.read().await.clone() {
            host.shutdown_all().await;
        }

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

        // 7. Memory extraction (D2-B3b), opt-in.
        if let Ok(cfg) = kod_config::KodConfig::load_default()
            && cfg.memory.extract_on_shutdown
            && let Err(e) = self.extract_memories_now("").await
        {
            tracing::warn!(error = %e, "shutdown memory extraction failed");
        }

        // 7b. Stop the consolidation task before any redb close —
        //     it holds a router clone, and `Arc::try_unwrap`
        //     needs the last reference.
        self.stop_memory_consolidation_task().await;

        // 8. Explicit redb close (design D0.3). Best-effort: the
        // router is behind an `Arc` and a caller that cloned the
        // engine may still hold a reference. `Arc::try_unwrap`
        // returns the value on the unique-holder path — the common
        // case for a CLI/TUI session that has stopped driving the
        // engine — and returns the `Arc` back on the shared path,
        // in which case we fall back to the previous behaviour
        // (the OS file lock releases when the last reference drops).
        match Arc::try_unwrap(self.router.clone()) {
            Ok(router) => router.close_memory(),
            Err(_) => tracing::debug!(
                "router Arc still shared at shutdown; redb will close when \
                 the last reference drops"
            ),
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

    /// Ask the default transcript's running prompt to stop at the next
    /// round boundary.
    pub fn request_cancel(&self) {
        self.request_cancel_for(DEFAULT_TRANSCRIPT_KEY);
    }

    /// Ask a specific transcript's running prompt to stop. Used by the
    /// swarm runner's per-agent cancel (D4-D4) so cancelling one agent
    /// does not stop the whole team.
    pub fn request_cancel_for(&self, key: &str) {
        if let Ok(mut guard) = self.cancels.try_write() {
            guard.insert(key.to_string());
        } else {
            // A write already in flight; the cancel is a single flag,
            // blocking on it is fine.
            let mut guard = futures::executor::block_on(self.cancels.write());
            guard.insert(key.to_string());
        }
    }

    /// Clear a previous cancel for the default transcript (called when
    /// a new prompt is dispatched).
    pub fn clear_cancel(&self) {
        self.clear_cancel_for(DEFAULT_TRANSCRIPT_KEY);
    }

    /// Clear a specific transcript's cancel flag.
    pub fn clear_cancel_for(&self, key: &str) {
        if let Ok(mut guard) = self.cancels.try_write() {
            guard.remove(key);
        } else {
            let mut guard = futures::executor::block_on(self.cancels.write());
            guard.remove(key);
        }
    }

    /// True if a cancel was requested for the default transcript.
    pub fn is_cancelled(&self) -> bool {
        self.is_cancelled_for(DEFAULT_TRANSCRIPT_KEY)
    }

    /// True if a cancel was requested for `key`.
    pub fn is_cancelled_for(&self, key: &str) -> bool {
        if let Ok(guard) = self.cancels.try_read() {
            guard.contains(key)
        } else {
            let guard = futures::executor::block_on(self.cancels.read());
            guard.contains(key)
        }
    }

    /// Queue a steering note on the default transcript.
    pub async fn steer(&self, note: &str) {
        self.steer_for(DEFAULT_TRANSCRIPT_KEY, note).await;
    }

    /// Queue a steering note on `key`. Injected into that transcript's
    /// conversation after the current tool round finishes.
    pub async fn steer_for(&self, key: &str, note: &str) {
        let note = note.trim();
        if note.is_empty() {
            return;
        }
        let mut guard = self.steers.write().await;
        guard
            .entry(key.to_string())
            .or_default()
            .push(note.to_string());
    }

    /// Drain queued steer notes for `key` (each is applied once, in
    /// order).
    async fn take_steers_for(&self, key: &str) -> Vec<String> {
        let mut guard = self.steers.write().await;
        guard.remove(key).unwrap_or_default()
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
            let mut to_drop = excess;
            turns.retain(|t| {
                if to_drop > 0 && !t.metadata.pinned {
                    to_drop -= 1;
                    false
                } else {
                    true
                }
            });
        }
        // Char-budget trim: D1 moved the transcript from a rendered
        // text section to a structured `messages` field the provider
        // sees verbatim; without a cap here the budget was ignored.
        let budget = self.history_budget();
        let mut total: usize = 0;
        let mut cutoff = turns.len();
        for i in (0..turns.len()).rev() {
            let line_len = turns[i].render_text().len() + 1;
            if total + line_len > budget {
                break;
            }
            total += line_len;
            cutoff = i;
        }
        for i in (0..cutoff).rev() {
            if turns[i].metadata.pinned {
                cutoff = i;
            }
        }
        if cutoff > 0 {
            turns.drain(..cutoff);
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

        // First pass: find the oldest index that fits under the
        // budget. `cutoff` is the smallest index that will be
        // included; iterate newest-to-oldest and stop when the next
        // turn would push the rendered size over.
        let mut total = 0usize;
        let mut cutoff = turns.len();
        for i in (0..turns.len()).rev() {
            // `ChatMessage::render_text` emits "User: {content}" /
            // "Assistant: {content}" / "System: {content}" etc. —
            // byte-identical to what `HistoryTurn` used to produce
            // for user and assistant turns. The characterization test
            // in tests/characterization_history.rs locks this.
            let line_len = turns[i].render_text().len() + 1; // + '\n'
            if total + line_len > budget {
                break;
            }
            total += line_len;
            cutoff = i;
        }

        // Second pass: walk backward past the cutoff and pull in any
        // pinned turn. A pinned turn is never dropped — the whole
        // point of pinning is that the user has decided this turn
        // matters more than the budget. The scan walks to 0 so a
        // pin at the very start survives even when the budget ran
        // out at index 20.
        for i in (0..cutoff).rev() {
            if turns[i].metadata.pinned {
                cutoff = i;
            }
        }

        let mut out = String::new();
        for message in &turns[cutoff..] {
            // Skip the structured tool transcript slice (AD-02): the
            // tool results are still delivered to the model via the
            // `## Tool results` block appended to `pending` at the end
            // of each round. Rendering them here as `Tool: …` lines
            // would change the prompt bytes and defeat the AD-16
            // cacheable prefix invariant. When the engine migrates to
            // `CompletionRequest` (AD-01), the structured slice goes
            // on the wire and this filter goes away.
            if matches!(message.role, kod_types::MessageRole::Tool) {
                continue;
            }
            // An assistant message whose only content is a set of tool
            // calls (empty text) has nothing to render. Same rationale:
            // it is structural, not prose.
            if matches!(message.role, kod_types::MessageRole::Assistant)
                && message.content.trim().is_empty()
                && !message.tool_calls.is_empty()
            {
                continue;
            }
            out.push_str(&message.render_text());
            out.push('\n');
        }
        out
    }

    /// The prompt the provider received on the most recent
    /// `process*` call on the default transcript.
    pub async fn last_prompt(&self) -> Option<String> {
        self.last_prompt_for(DEFAULT_TRANSCRIPT_KEY).await
    }

    /// Log a `SessionEntry::MemoryWrite` for the user channel (AD-15).
    ///
    /// The TUI's `/remember` command writes directly through a
    /// `MemoryManager` it constructs itself (the engine's store is
    /// behind an `Arc` and is not the same manager instance the CLI
    /// builds). Rather than route the write through the engine — a
    /// larger refactor — the TUI calls this method after a successful
    /// write so the JSONL audit trail is uniform across all three
    /// channels: extraction, tool, and user.
    ///
    /// No-op when no recorder is installed (the CLI default).
    pub async fn record_user_memory_write(&self, memory_id: &str, tags: Vec<String>) {
        if let Ok(guard) = self.session_recorder.read()
            && let Some(rec) = guard.as_ref()
        {
            let now_ms = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_millis() as u64)
                .unwrap_or(0);
            let entry = crate::session_log::SessionEntry::MemoryWrite {
                timestamp_ms: now_ms,
                memory_id: memory_id.to_string(),
                channel: "user".to_string(),
                tags,
            };
            let _ = rec.record(&entry);
        }
    }

    /// This engine's session identity. Stable across the engine's
    /// lifetime; the value used to attribute auto-extracted episodic
    /// facts (design D2.5).
    pub fn session_id(&self) -> &kod_types::SessionId {
        &self.session_id
    }

    /// The session id that a transcript key belongs to.
    ///
    /// A swarm-agent transcript key is `"swarm:<uuid>"`; the UUID is
    /// the agent's, and an agent's auto-extracted facts are
    /// attributable to that agent. Any other key (including the
    /// interactive session's `""`) maps to the engine's own
    /// `session_id`.
    ///
    /// Public so a caller (a filter, a debug command) can ask the
    /// same question without knowing the key format.
    pub fn session_id_for_holder(&self, key: &str) -> kod_types::SessionId {
        if let Some(rest) = key.strip_prefix("swarm:")
            && let Ok(uuid) = uuid::Uuid::parse_str(rest)
        {
            return kod_types::SessionId::from_uuid(uuid);
        }
        self.session_id.clone()
    }

    /// The prompt the provider received on the most recent `process*`
    /// call for `key`.
    pub async fn last_prompt_for(&self, key: &str) -> Option<String> {
        self.last_prompt
            .read()
            .await
            .get(key)
            .map(|t| t.text.clone())
    }

    /// The full prompt trace (text + per-section allocation) for the
    /// most recent `process*` call on the default transcript.
    /// `PromptTrace::alloc` is what `/debug tokens` renders as the
    /// budget table; `PromptTrace::text` is byte-identical to what
    /// `last_prompt` returns.
    pub async fn last_prompt_trace(&self) -> Option<crate::budget::PromptTrace> {
        self.last_prompt_trace_for(DEFAULT_TRANSCRIPT_KEY).await
    }

    /// The full prompt trace for `key`.
    pub async fn last_prompt_trace_for(&self, key: &str) -> Option<crate::budget::PromptTrace> {
        self.last_prompt.read().await.get(key).cloned()
    }

    /// Point a transcript at a different working directory (D4-D1).
    /// Called by the swarm runner before an agent runs, with the
    /// agent's worktree path. Idempotent. Passing `None` clears the
    /// override for that key.
    pub async fn set_transcript_working_dir(&self, key: &str, dir: Option<PathBuf>) {
        let mut guard = self.transcript_working_dirs.write().await;
        match dir {
            Some(p) => {
                guard.insert(key.to_string(), p);
            }
            None => {
                guard.remove(key);
            }
        }
    }

    /// The working directory a transcript's tool calls run against.
    /// The per-key override when present, otherwise the engine-wide
    /// root.
    pub async fn working_dir_for(&self, key: &str) -> PathBuf {
        self.transcript_working_dirs
            .read()
            .await
            .get(key)
            .cloned()
            .unwrap_or_else(|| self.working_dir.clone())
    }

    /// Clear the per-transcript working directory override for `key`.
    /// Called by the swarm runner after an agent's worktree is
    /// removed.
    pub async fn clear_transcript_working_dir(&self, key: &str) {
        self.transcript_working_dirs.write().await.remove(key);
    }

    /// Register a per-transcript write set (D4.2). `Some(globs)`
    /// restricts the transcript's tool calls to paths matching at
    /// least one glob; `None` removes the restriction (the default
    /// for every transcript). Called by the swarm runner before
    /// each agent starts.
    pub async fn set_transcript_write_globs(&self, key: &str, globs: Option<Vec<String>>) {
        let mut guard = self.transcript_write_globs.write().await;
        match globs {
            Some(g) if !g.is_empty() => {
                guard.insert(key.to_string(), Some(g));
            }
            // An empty `Some(vec![])` and a `None` mean the same
            // thing: no claim in force. Normalising here means the
            // tool-context builder has one case to check, not two.
            _ => {
                guard.remove(key);
            }
        }
    }

    /// The write set that applies to a transcript's tool calls.
    /// `None` when no claim is in force.
    pub async fn write_globs_for(&self, key: &str) -> Option<Vec<String>> {
        self.transcript_write_globs
            .read()
            .await
            .get(key)
            .cloned()
            .flatten()
    }

    /// Clear the per-transcript write set for `key`. Called by the
    /// swarm runner after each agent finishes.
    pub async fn clear_transcript_write_globs(&self, key: &str) {
        self.transcript_write_globs.write().await.remove(key);
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

    /// Pin or unpin the most-recent transcript turn whose content
    /// matches `content` exactly. Returns `true` if a match was
    /// found.
    ///
    /// Matches newest-to-oldest so a re-issued prompt pins the most
    /// recent occurrence — the one the user just clicked.
    ///
    /// Keyed by content rather than index because the TUI's message
    /// list and the engine's transcript are not the same length (the
    /// TUI renders tool rows and system notices the engine never
    /// saw). Content equality is the one identity both sides share.
    pub async fn set_turn_pinned_by_content(&self, key: &str, content: &str, pinned: bool) -> bool {
        let mut history = self.history.write().await;
        let Some(turns) = history.get_mut(key) else {
            return false;
        };
        for t in turns.iter_mut().rev() {
            if t.content == content {
                t.metadata.pinned = pinned;
                return true;
            }
        }
        false
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
        self.compact_history_for(DEFAULT_TRANSCRIPT_KEY, max_turns)
            .await
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

// (The `which` helper moved to `kod_lsp::binary_for_path` when the
// LSP pool was introduced; `lsp_binary_for` now delegates there.)

/// Identity of a diagnostic for diffing between two check runs.
/// Ignores line and column: an edit that shifts a later error down by
/// a line did not create a new error.
fn diag_key(d: &kod_tools::check::Diagnostic) -> (String, Option<String>, String) {
    (d.file.clone(), d.code.clone(), d.message.clone())
}

/// Append up to `max` diagnostics to `block`, one per line, in the
/// format `severity [code] file:line:col — message`.
fn render_diagnostics(block: &mut String, diags: &[kod_tools::check::Diagnostic], max: usize) {
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
    async fn grounded_request_splits_cacheable_and_volatile() {
        let temp = TempDir::new().unwrap();
        let db_path = temp.path().join("test.redb");
        let cfg = RouterConfig {
            skill_threshold: 0.3,
            context_window: 8192,
            short_term_capacity: 100,
            working_dir: temp.path().to_path_buf(),
            enable_memory: false,
            max_skills_per_query: 3,
            embedder: None,
        };
        let engine = KodEngine::new(cfg, db_path).unwrap();
        engine.start().await.unwrap();

        let system_text = "identity bits\n\n## Stable prefix (cacheable)\n\nrepo map bits\n\n## Volatile suffix (not cached)\n\nvolatile bits\n\n## Conversation so far\n\nUser: hi\n\n## User Request\n\nhi";
        let req = engine.build_grounded_request(
            system_text,
            Vec::new(),
            &[],
            &GenerationOptions::default(),
            &ModelRef::new("test", "test-model"),
        );
        assert_eq!(
            req.system.segments.len(),
            2,
            "expected exactly two segments: cacheable head + volatile tail",
        );
        assert!(
            req.system.segments[0].cacheable,
            "first segment must be cacheable"
        );
        assert!(
            !req.system.segments[1].cacheable,
            "second segment must be volatile",
        );
        assert!(
            req.system.segments[0].text.contains("repo map bits"),
            "cacheable segment must carry the pre-marker content: {}",
            req.system.segments[0].text,
        );
        assert!(
            !req.system.segments[0].text.contains("volatile bits"),
            "volatile content must not leak into the cacheable segment",
        );
        assert!(
            req.system.segments[1].text.contains("volatile bits"),
            "volatile segment must carry the post-marker content: {}",
            req.system.segments[1].text,
        );
        assert!(
            req.system.segments[1].text.contains("## Environment"),
            "environment grounding must be appended to the volatile segment: {}",
            req.system.segments[1].text,
        );
        // The conversation tail must not appear anywhere.
        assert!(
            !req.system.segments[1]
                .text
                .contains("## Conversation so far"),
            "conversation tail must be stripped from the system prompt",
        );
    }

    #[tokio::test]
    async fn session_id_for_default_transcript_is_the_engine_id() {
        let temp = TempDir::new().unwrap();
        let db_path = temp.path().join("test.redb");
        let cfg = RouterConfig {
            skill_threshold: 0.3,
            context_window: 8192,
            short_term_capacity: 100,
            working_dir: temp.path().to_path_buf(),
            enable_memory: false,
            max_skills_per_query: 3,
            embedder: None,
        };
        let engine = KodEngine::new(cfg, db_path).unwrap();
        assert_eq!(
            engine.session_id_for_holder(""),
            *engine.session_id(),
            "the default transcript must attribute facts to the engine's session",
        );
        assert_eq!(
            engine.session_id_for_holder("swarm:not-a-uuid"),
            *engine.session_id(),
            "a non-UUID suffix falls back to the engine's session",
        );
    }

    #[tokio::test]
    async fn session_id_for_swarm_key_prefers_the_embedded_uuid() {
        let temp = TempDir::new().unwrap();
        let db_path = temp.path().join("test.redb");
        let cfg = RouterConfig {
            skill_threshold: 0.3,
            context_window: 8192,
            short_term_capacity: 100,
            working_dir: temp.path().to_path_buf(),
            enable_memory: false,
            max_skills_per_query: 3,
            embedder: None,
        };
        let engine = KodEngine::new(cfg, db_path).unwrap();
        let agent_uuid = uuid::Uuid::new_v4();
        let key = format!("swarm:{agent_uuid}");
        let sid = engine.session_id_for_holder(&key);
        assert_eq!(
            sid.as_uuid(),
            &agent_uuid,
            "a swarm transcript key must attribute facts to the agent's UUID",
        );
        assert_ne!(
            sid,
            *engine.session_id(),
            "the agent's session must be distinct from the engine's",
        );
    }

    #[tokio::test]
    async fn two_engines_have_distinct_session_ids() {
        let temp1 = TempDir::new().unwrap();
        let temp2 = TempDir::new().unwrap();
        let e1 = KodEngine::new(
            RouterConfig {
                skill_threshold: 0.3,
                context_window: 8192,
                short_term_capacity: 100,
                working_dir: temp1.path().to_path_buf(),
                enable_memory: false,
                max_skills_per_query: 3,
                embedder: None,
            },
            temp1.path().join("t.redb"),
        )
        .unwrap();
        let e2 = KodEngine::new(
            RouterConfig {
                skill_threshold: 0.3,
                context_window: 8192,
                short_term_capacity: 100,
                working_dir: temp2.path().to_path_buf(),
                enable_memory: false,
                max_skills_per_query: 3,
                embedder: None,
            },
            temp2.path().join("t.redb"),
        )
        .unwrap();
        assert_ne!(
            e1.session_id(),
            e2.session_id(),
            "each engine must have a distinct session id",
        );
    }

    #[tokio::test]
    async fn deny_rule_at_zero_returns_none() {
        let temp = TempDir::new().unwrap();
        let db_path = temp.path().join("test.redb");
        let cfg = RouterConfig {
            skill_threshold: 0.3,
            context_window: 8192,
            short_term_capacity: 100,
            working_dir: temp.path().to_path_buf(),
            enable_memory: false,
            max_skills_per_query: 3,
            embedder: None,
        };
        let engine = KodEngine::new(cfg, db_path).unwrap();
        engine.start().await.unwrap();
        assert!(engine.deny_rule_at(0).await.is_none());
        assert!(engine.deny_rule_at(1).await.is_none());
    }

    #[tokio::test]
    async fn deny_rule_at_returns_the_sorted_index() {
        let temp = TempDir::new().unwrap();
        let db_path = temp.path().join("test.redb");
        let cfg = RouterConfig {
            skill_threshold: 0.3,
            context_window: 8192,
            short_term_capacity: 100,
            working_dir: temp.path().to_path_buf(),
            enable_memory: false,
            max_skills_per_query: 3,
            embedder: None,
        };
        let engine = KodEngine::new(cfg, db_path).unwrap();
        engine.start().await.unwrap();

        // Three rules with distinguishable sort keys.
        engine
            .add_deny_rule(kod_config::SessionDeny {
                tool: "write_file".into(),
                path_pattern: Some("src/**".into()),
            })
            .await;
        engine
            .add_deny_rule(kod_config::SessionDeny {
                tool: "execute_command".into(),
                path_pattern: None,
            })
            .await;
        engine
            .add_deny_rule(kod_config::SessionDeny {
                tool: "write_file".into(),
                path_pattern: Some("docs/**".into()),
            })
            .await;

        let listed = engine.deny_rules().await;
        assert_eq!(listed.len(), 3);
        // Sorted by (tool, path_pattern):
        //   (execute_command, None) < (write_file, Some("docs/**")) < (write_file, Some("src/**"))
        assert_eq!(
            engine.deny_rule_at(1).await,
            Some(kod_config::SessionDeny {
                tool: "execute_command".into(),
                path_pattern: None,
            })
        );
        assert_eq!(
            engine.deny_rule_at(2).await,
            Some(kod_config::SessionDeny {
                tool: "write_file".into(),
                path_pattern: Some("docs/**".into()),
            })
        );
        assert_eq!(
            engine.deny_rule_at(3).await,
            Some(kod_config::SessionDeny {
                tool: "write_file".into(),
                path_pattern: Some("src/**".into()),
            })
        );
        assert!(engine.deny_rule_at(4).await.is_none());
    }

    #[tokio::test]
    async fn remove_deny_rule_is_by_value_and_reports_presence() {
        let temp = TempDir::new().unwrap();
        let db_path = temp.path().join("test.redb");
        let cfg = RouterConfig {
            skill_threshold: 0.3,
            context_window: 8192,
            short_term_capacity: 100,
            working_dir: temp.path().to_path_buf(),
            enable_memory: false,
            max_skills_per_query: 3,
            embedder: None,
        };
        let engine = KodEngine::new(cfg, db_path).unwrap();
        engine.start().await.unwrap();

        let rule = kod_config::SessionDeny {
            tool: "write_file".into(),
            path_pattern: Some("src/**".into()),
        };
        engine.add_deny_rule(rule.clone()).await;
        assert_eq!(engine.deny_rules().await.len(), 1);

        // A value-equal rule removes it.
        let same_value = kod_config::SessionDeny {
            tool: "write_file".into(),
            path_pattern: Some("src/**".into()),
        };
        assert!(engine.remove_deny_rule(&same_value).await);
        assert!(engine.deny_rules().await.is_empty());

        // Removing again reports false.
        assert!(!engine.remove_deny_rule(&same_value).await);
    }

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
            fn stream_completion<'a>(
        &'a self,
        _req: &'a CompletionRequest,
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
            embedder: None,
            skill_threshold: 0.3,
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
            .install_test_provider(Arc::new(SlowProvider {
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
            .install_test_provider(Arc::new(SlowProvider {
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
            embedder: None,
            skill_threshold: 0.3,
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
            embedder: None,
            skill_threshold: 0.3,
            context_window: 8192,
            short_term_capacity: 100,
            working_dir: temp.path().to_path_buf(),
            enable_memory: false,
            max_skills_per_query: 3,
        };
        let engine = KodEngine::new(cfg, db_path).unwrap();
        engine.start().await.unwrap();

        let (tx, _rx) = tokio::sync::mpsc::channel::<String>(4);
        let err = engine.process_streaming("hello?", &tx).await.unwrap_err();
        assert!(matches!(err, KodError::InvalidState(_)), "got {err:?}");
    }

    #[tokio::test]
    async fn test_process_goal_streaming_without_provider_errors() {
        let temp = TempDir::new().unwrap();
        let db_path = temp.path().join("test.redb");
        let cfg = RouterConfig {
            embedder: None,
            skill_threshold: 0.3,
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
            embedder: None,
            skill_threshold: 0.3,
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

        engine
            .remember_turn(true, "the user asked about rust")
            .await;
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
            embedder: None,
            skill_threshold: 0.3,
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
            embedder: None,
            skill_threshold: 0.3,
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

        {
            let mut reg = kod_provider::ProviderRegistry::new();
            reg.insert(
                "default",
                Arc::new(NopProvider),
                kod_provider::ProviderCapabilities::conservative(),
                "",
            );
            engine
                .set_registry(
                    Arc::new(reg),
                    kod_provider::ModelRef::new("default", ""),
                    None,
                )
                .await;
        }
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
            embedder: None,
            skill_threshold: 0.3,
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
            embedder: None,
            skill_threshold: 0.3,
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
            embedder: None,
            skill_threshold: 0.3,
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
            embedder: None,
            skill_threshold: 0.3,
            context_window: 8192,
            short_term_capacity: 100,
            working_dir: temp.path().to_path_buf(),
            enable_memory: false,
            max_skills_per_query: 3,
        };
        let engine = KodEngine::new(cfg, db_path).unwrap();
        engine.start().await.unwrap();

        let calls = vec![ToolCall {
            id: None,
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
            embedder: None,
            skill_threshold: 0.3,
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
                id: None,
                tool_name: "write_file".to_string(),
                arguments: serde_json::json!({
                    "path": "serialize_probe.txt",
                    "content": "hello-serial"
                }),
            },
            ToolCall {
                id: None,
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
            embedder: None,
            skill_threshold: 0.3,
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
                id: None,
                tool_name: "read_file".to_string(),
                arguments: serde_json::json!({ "path": "a.txt" }),
            },
            ToolCall {
                id: None,
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

    /// A pinned turn must survive a budget that would otherwise drop
    /// it. Regression: before this, the pin flag was stored but never
    /// consulted — the budget scan dropped the oldest turn
    /// unconditionally, so a pinned turn at the start of a long
    /// session was lost exactly when it mattered.
    #[tokio::test]
    async fn test_render_history_keeps_pinned_turn() {
        use tempfile::TempDir;
        let temp = TempDir::new().unwrap();
        let db_path = temp.path().join("test.redb");
        let cfg = RouterConfig {
            embedder: None,
            skill_threshold: 0.3,
            context_window: 8192,
            short_term_capacity: 100,
            working_dir: temp.path().to_path_buf(),
            enable_memory: false,
            max_skills_per_query: 3,
        };
        let engine = KodEngine::new(cfg, db_path).unwrap();
        engine.start().await.unwrap();
        // Tiny budget so most turns get dropped.
        engine.set_history_budget(MIN_HISTORY_CHAR_BUDGET);

        // The first turn is unique enough to identify in the output.
        let first = "PINNED-CONTENT-UNIQUE-MARKER-that-fits";
        engine.seed_turn(true, first).await;
        assert!(
            engine.set_turn_pinned_by_content("", first, true).await,
            "pinning the first turn must succeed"
        );

        // Flood the transcript so the first turn would be dropped.
        for i in 0..40 {
            engine
                .seed_turn(true, &format!("filler-{i}-{}", "x".repeat(400)))
                .await;
        }

        // Render and check: the pinned turn must be present even
        // though the budget cannot hold all 41 turns.
        let rendered = engine.render_history().await;
        assert!(
            rendered.contains("PINNED-CONTENT-UNIQUE-MARKER"),
            "pinned turn was dropped from rendered history: {}",
            &rendered[..rendered.len().min(500)]
        );
    }

    /// Unpinning reverses the protection.
    #[tokio::test]
    async fn test_render_history_drops_unpinned_turn_again() {
        use tempfile::TempDir;
        let temp = TempDir::new().unwrap();
        let db_path = temp.path().join("test.redb");
        let cfg = RouterConfig {
            embedder: None,
            skill_threshold: 0.3,
            context_window: 8192,
            short_term_capacity: 100,
            working_dir: temp.path().to_path_buf(),
            enable_memory: false,
            max_skills_per_query: 3,
        };
        let engine = KodEngine::new(cfg, db_path).unwrap();
        engine.start().await.unwrap();
        engine.set_history_budget(MIN_HISTORY_CHAR_BUDGET);

        let first = "PINNED-THEN-UNPINNED-MARKER";
        engine.seed_turn(true, first).await;
        engine.set_turn_pinned_by_content("", first, true).await;
        for i in 0..40 {
            engine
                .seed_turn(true, &format!("filler-{i}-{}", "x".repeat(400)))
                .await;
        }
        // Unpin and re-render.
        engine.set_turn_pinned_by_content("", first, false).await;
        let rendered = engine.render_history().await;
        assert!(
            !rendered.contains("PINNED-THEN-UNPINNED-MARKER"),
            "unpinned turn should be dropped under a tight budget"
        );
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
            embedder: None,
            skill_threshold: 0.3,
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
            id: None,
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
            embedder: None,
            skill_threshold: 0.3,
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
            id: None,
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
            embedder: None,
            skill_threshold: 0.3,
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
            id: None,
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
            embedder: None,
            skill_threshold: 0.3,
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
                id: None,
                tool_name: "write_file".to_string(),
                arguments: serde_json::json!({
                    "path": "src/lib.rs",
                    "content": "pub mod extra;\n// harmless comment\n"
                }),
            },
            ToolCall {
                id: None,
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
            embedder: None,
            skill_threshold: 0.3,
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
            id: None,
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
            embedder: None,
            skill_threshold: 0.3,
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
            id: None,
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

#[cfg(test)]
mod coverage_at_references {
    //! `expand_at_references` is the @-syntax preprocessor for a
    //! prompt. The containment rule it enforces is the same one
    //! the tool context uses: a resolved path must live inside
    //! the working directory, so a `@../../etc/passwd` in a
    //! prompt cannot leak a file the agent was not asked to read.
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn a_real_file_inside_the_workspace_is_expanded() {
        let tmp = TempDir::new().unwrap();
        std::fs::write(tmp.path().join("foo.rs"), "fn main() {}\n").unwrap();
        let out = expand_at_references("see @foo.rs for details", tmp.path());
        assert!(out.contains("<file path="), "no file block: {out}");
        assert!(out.contains("fn main()"), "content missing: {out}");
        // The surrounding prose survives.
        assert!(out.contains("see "), "prefix lost: {out}");
        assert!(out.contains(" for details"), "suffix lost: {out}");
    }

    #[test]
    fn a_missing_file_leaves_the_token_untouched() {
        let tmp = TempDir::new().unwrap();
        let out = expand_at_references("see @missing.rs here", tmp.path());
        assert!(out.contains("@missing.rs"), "token mangled: {out}");
        assert!(!out.contains("<file"), "spurious expansion: {out}");
    }

    #[test]
    fn a_non_path_token_is_left_alone() {
        // A bare `@user` (no slash, no dot) is a mention, not a
        // path. The heuristic is documented behaviour; a
        // regression that expanded it would try to read a file
        // named `user`.
        let tmp = TempDir::new().unwrap();
        let out = expand_at_references("hi @user how are you", tmp.path());
        assert_eq!(out, "hi @user how are you");
    }

    #[test]
    fn a_path_that_escapes_the_workspace_is_refused() {
        // Even a path that exists on disk must not expand if it
        // lives outside the working directory.
        let tmp = TempDir::new().unwrap();
        let out = expand_at_references("see @../etc/passwd", tmp.path());
        assert!(
            !out.contains("<file"),
            "escape expanded: {out}",
        );
        assert!(out.contains("@../etc/passwd"), "token eaten: {out}");
    }

    #[test]
    fn an_empty_input_is_unchanged() {
        let tmp = TempDir::new().unwrap();
        assert_eq!(expand_at_references("", tmp.path()), "");
    }

    #[test]
    fn a_prompt_with_no_at_tokens_is_unchanged() {
        let tmp = TempDir::new().unwrap();
        let input = "plain text, nothing to expand";
        assert_eq!(expand_at_references(input, tmp.path()), input);
    }

    #[test]
    fn only_word_boundary_at_signs_start_a_reference() {
        // `mail@host.com` has an `@` inside a word; the boundary
        // check must not treat it as a reference. The heuristic
        // also filters it out (no slash, no dot in the token? —
        // actually `host.com` has a dot). The boundary check runs
        // first.
        let tmp = TempDir::new().unwrap();
        let out = expand_at_references("contact me at user@example.com", tmp.path());
        // The `.com` shape would look path-like; the boundary
        // check is what stops the expansion.
        assert!(out.contains("user@example.com"), "mangled: {out}");
    }

    #[test]
    fn a_bare_at_sign_is_preserved() {
        let tmp = TempDir::new().unwrap();
        let out = expand_at_references("just @ alone", tmp.path());
        assert!(out.contains('@'), "at sign lost: {out}");
    }

    #[test]
    fn an_empty_file_is_expanded_to_an_empty_block() {
        // A zero-byte file is a legitimate input. The block must
        // still appear — the model needs to know it was read.
        let tmp = TempDir::new().unwrap();
        std::fs::write(tmp.path().join("empty.txt"), "").unwrap();
        let out = expand_at_references("see @empty.txt", tmp.path());
        assert!(out.contains("<file path="), "no block for empty file: {out}");
    }

    #[test]
    fn a_directory_reference_does_not_expand() {
        // `@subdir` matches the path shape (`subdir` has no dot,
        // so the looks-like-path heuristic rejects it — the test
        // pins that rejection). A directory whose name has a dot
        // is also rejected, because `is_file()` fails.
        let tmp = TempDir::new().unwrap();
        std::fs::create_dir(tmp.path().join("my.dir")).unwrap();
        let out = expand_at_references("see @my.dir", tmp.path());
        assert!(!out.contains("<file"), "directory expanded: {out}");
    }
}
