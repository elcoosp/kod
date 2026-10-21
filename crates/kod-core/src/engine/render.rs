//! Pure render/summarize/format helpers for the engine.
//!
//! Extracted from `engine/mod.rs`. Nothing here touches `KodEngine`
//! state; every item is a free function. Visibility reflects callers:
//! `pub` for the four items `kod-tui`/tests consume, `pub(crate)` for
//! those a sibling engine module calls, private for the rest.

#![allow(clippy::too_many_arguments)]

use kod_types::ToolResult;

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
/// Render a tool result for the prompt block, capped to `cap` chars.
///
/// When `redactor` is `Some`, the rendered JSON is passed through the
/// secret redactor (Tier 1.3). This is where the highest-volume
/// untrusted content lives — file contents, grep hits, shell output —
/// so the tool-result path is the more important half of the
/// in-prompt redaction story.
pub(crate) fn cap_rendered_result(
    result: &ToolResult,
    cap: usize,
    redactor: Option<&kod_types::redact::Redactor>,
) -> String {
    let ToolResult::Success(v) = result else {
        // Callers route only Success through this helper; the fallback
        // is defensive.
        return format!("{result:?}");
    };
    let raw = match redactor {
        Some(r) => {
            let (sanitized, _events) = r.redact(&v.to_string());
            sanitized
        }
        None => v.to_string(),
    };
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
                    truncate_bytes(s, per_field),
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
                truncate_bytes(&reserialized, cap),
                reserialized.len().saturating_sub(cap),
            );
        }
    }
    format!(
        "{}… [truncated {} bytes]",
        truncate_bytes(&raw, cap),
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
/// Anchor on the first or last non-empty line instead. Trim and strip the same
/// decorations a model often adds (`**GOAL MET**`, `GOAL MET.`,
/// `— GOAL MET`, `> GOAL MET`), then compare case-insensitively to
/// `GOAL MET`. A reply that only mentions the phrase mid-paragraph is
/// not a completion signal.
pub(crate) fn reply_declares_goal_met(text: &str) -> bool {
    // Collect non-empty trimmed lines. The contract says "end your reply
    // with GOAL MET", but DeepSeek-web via tab-bridge emits it as the
    // FIRST line (`GOAL MET\n## Summary…` — the bridge's shape log
    // collapses the newline, showing `GOAL MET ## Summary…`). Accept
    // either end so both forms stop the loop. Middle lines are ignored
    // so a mere mention mid-paragraph is not a completion signal.
    let lines: Vec<&str> = text
        .lines()
        .map(|l| l.trim())
        .filter(|l| !l.is_empty())
        .collect();
    if lines.is_empty() {
        return false;
    }
    let candidates = [lines[0], lines[lines.len() - 1]];
    candidates.iter().any(|l| line_is_goal_marker(l))
}

/// Single-line GOAL MET check shared by the first/last-line scan.
fn line_is_goal_marker(line: &str) -> bool {
    // Strip surrounding emphasis and leading quote / list markers.
    let stripped: String = line
        .trim_matches(|c: char| c.is_whitespace() || c == '*' || c == '`' || c == '>' || c == '-')
        .trim_start_matches(|c: char| c.is_whitespace() || c == '—' || c == ':')
        .trim_end_matches(|c: char| c.is_whitespace() || c == '.' || c == '!' || c == ':')
        .to_string();
    if stripped.eq_ignore_ascii_case("GOAL MET") {
        return true;
    }
    // Same-line summary form: `GOAL MET ## Summary…`, `GOAL MET - done`,
    // `GOAL MET: summary`. Require a structural separator after the
    // marker so `GOAL MET is what I'd say if done` still returns false.
    // Char-safe: `get(..8)` returns None (never panics) when byte 8
    // falls inside a multibyte codepoint, e.g. 5 CJK chars.
    if let Some(head) = stripped.get(..8)
        && head.eq_ignore_ascii_case("GOAL MET")
    {
        let tail = &stripped[8..];
        let sep = tail.trim_start().chars().next().unwrap_or(' ');
        if matches!(sep, '#' | '-' | '—' | ':' | '.' | '!' | '(') {
            return true;
        }
    }
    false
}

/// Delta §12.7: render a mental model's entries into a stable bullet
/// block. The block is what gets frozen into the prompt for the
/// session; the bytes must be deterministic given a set of entries,
/// so a re-render at the same generation produces the same string.
///
/// `max_tokens` is a soft cap: the render stops before adding a line
/// that would push `used + line.len()` past `max_tokens * 4` **bytes**
/// (the 4-bytes-per-token approximation the rest of the codebase uses
/// for prompt budget). Bytes are the right unit here, not chars: CJK
/// is roughly 3 bytes per token and English roughly 4, so a byte count
/// tracks a token budget more closely than a char count does. A
/// char-budgeted check would let 3x more CJK text through than the
/// token budget supports — a prompt-budget regression.
/// A single entry longer than the cap is dropped, not truncated, so
/// a half-sentence never enters the frozen block.
pub(crate) fn render_mental_model_block(
    entries: &[kod_types::MemoryEntry],
    max_tokens: usize,
) -> String {
    let budget_bytes = max_tokens.saturating_mul(4);
    let mut out = String::new();
    let mut used = 0usize;
    for e in entries {
        let content = e.content.trim();
        if content.is_empty() {
            continue;
        }
        let line = format!("- {content}\n");
        if used + line.len() > budget_bytes {
            continue;
        }
        used += line.len();
        out.push_str(&line);
    }
    out.trim_end().to_string()
}

/// Truncate a UTF-8 string to at most `max` bytes, rounding down to the
/// nearest char boundary. Returns the input unchanged when it already
/// fits. Use this instead of `&s[..max]` — the raw slice panics when
/// `max` lands mid-codepoint, which any non-ASCII tool output can hit
/// (a file containing "café", an error message with an em-dash, any
/// emoji in a directory listing).
pub(crate) fn truncate_chars(s: &str, max: usize) -> &str {
    kod_types::strutil::truncate_chars(s, max)
}

/// Truncate `s` to `max` chars, appending `ellipsis` only when the
/// truncation actually shortened the string.
///
/// `truncate_chars` returns the whole string when it fits; a
/// byte-length guard on the caller side (`s.len() > N`) can fire on
/// CJK input whose char count is at the cap (3 bytes per char), and
/// then a bare `format!("{shown}{ellipsis}")` appends an ellipsis to
/// an untruncated string. This helper makes the decision exact by
/// comparing the returned prefix to the input in the same units.
fn truncate_with_ellipsis(s: &str, max: usize, ellipsis: &str) -> String {
    let shown = truncate_chars(s, max);
    if shown.len() < s.len() {
        format!("{shown}{ellipsis}")
    } else {
        s.to_string()
    }
}

/// Byte-cap counterpart to [`truncate_chars`]. Use at sites whose
/// budget is expressed in bytes; same char-boundary behavior.
pub(crate) fn truncate_bytes(s: &str, max: usize) -> &str {
    kod_types::strutil::truncate_bytes(s, max)
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
pub(crate) fn append_round_text(buf: &mut String, text: &str) {
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
/// The model-declared reason for a tool call, from the top-level
/// `intent` arg the registry injects into every tool schema
/// (`kod_tools::registry::inject_intent_field`). Surfaced in the
/// running line and the completed row so the UI shows *why* the
/// model called, not just what with. Collapsed to one line and
/// capped so headers and the spinner stay one-liners. `None` when
/// the model sent no usable intent.
pub(crate) fn tool_intent(args: &serde_json::Value) -> Option<String> {
    let raw = args.get("intent")?.as_str()?;
    let one_line: String = raw.split_whitespace().collect::<Vec<_>>().join(" ");
    if one_line.is_empty() {
        return None;
    }
    const CAP: usize = 80;
    Some(truncate_with_ellipsis(&one_line, CAP, "…"))
}

/// Suffix a formatted tool display with the model's intent, when it
/// sent one. Appended — never prepended: row matching
/// (`tool_row_matches_header`, the execution ledger) keys on the
/// header *starting* with the tool name.
fn append_intent(base: &str, args: &serde_json::Value) -> String {
    match tool_intent(args) {
        Some(intent) => format!("{base} — {intent}"),
        None => base.to_string(),
    }
}

fn format_call_brief_base(name: &str, args: &serde_json::Value) -> String {
    if name == "execute_command" {
        if let Some(cmd) = args.get("command").and_then(|v| v.as_str()) {
            // Multi-line shell snippets read as their first line only.
            let first = cmd.lines().next().unwrap_or(cmd).trim();
            let short = truncate_with_ellipsis(first, 100, "…");
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
                let short = truncate_with_ellipsis(&s, 80, "…");
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
    let short = truncate_with_ellipsis(&raw, 80, "...");
    format!("{name} {short}")
}

/// One-line "what + why" for the running indicator: the argument
/// excerpt plus the model's declared intent, when it sent one
/// (`execute_command cargo test — verify the fix`).
pub fn format_call_brief(name: &str, args: &serde_json::Value) -> String {
    append_intent(&format_call_brief_base(name, args), args)
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
                let short = truncate_with_ellipsis(&s, 60, "…");
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
    append_intent(&parts.join(" "), args)
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

/// A cheap non-cryptographic hash for the image-render cache key.
pub(crate) fn simple_hash(s: &str) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in s.as_bytes() {
        h ^= *b as u64;
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    h
}

pub(crate) fn build_summary_prompt(dropped: &[kod_types::ChatMessage]) -> String {
    const CAP: usize = 32_000;
    let mut body = String::new();
    for m in dropped {
        let line = m.render_text();
        if body.len() + line.len() + 1 > CAP {
            body.push_str("\n[...earlier messages truncated for the summary call...]\n");
            break;
        }
        body.push_str(&line);
        body.push('\n');
    }
    format!(
        "Summarize the conversation excerpt below into four sections: \
         Context (what was being worked on), What we did (the changes \
         made), Current state (what works and what does not), and User \
         preferences (anything the user asked for that should persist). \
         Be specific; name files and functions. Do not restate the \
         excerpt, extract from it.\n\n## Excerpt\n\n{body}",
    )
}

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

    // git_diff: the patch itself, not the JSON envelope around it. The TUI
    // colours the row as a diff, and an escaped JSON string is not one.
    if name == "git_diff"
        && let Some(diff) = v.get("diff").and_then(|d| d.as_str())
    {
        let mut scope = vec![
            if v.get("staged").and_then(|s| s.as_bool()).unwrap_or(false) {
                "staged"
            } else {
                "unstaged"
            },
        ];
        if v.get("stat").and_then(|s| s.as_bool()).unwrap_or(false) {
            scope.push("--stat");
        }
        let path = v.get("path").and_then(|p| p.as_str()).map(shorten_path);
        if let Some(p) = path.as_deref() {
            scope.push(p);
        }
        if diff.trim().is_empty() {
            return format!("git diff ({}): no changes", scope.join(" "));
        }
        let body = cap_lines(diff.trim_end(), TOOL_DIFF_LINES);
        let trunc = if v
            .get("truncated")
            .and_then(|t| t.as_bool())
            .unwrap_or(false)
        {
            "\n… [truncated at the tool's 64 KB cap — narrow with `path` or `stat`]"
        } else {
            ""
        };
        return format!("git diff ({}):\n{body}{trunc}", scope.join(" "));
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

/// H-E7: pinned-aware turn cap. Drop the oldest unpinned turns until
/// the vector has at most `max` entries. A `turns.drain(..excess)` on
/// the same vector ignores `metadata.pinned`, so a user who pinned a
/// turn could silently lose it as soon as the round loop persisted a
/// few tool messages. Three near-identical call sites used to do this
/// three different ways; one helper is the fix.
pub(crate) fn cap_transcript(turns: &mut Vec<kod_types::ChatMessage>, max: usize) {
    if turns.len() <= max {
        return;
    }
    // Nominal policy: drop the oldest non-pinned messages.
    let mut to_drop = turns.len() - max;
    let mut keep = vec![true; turns.len()];
    for (i, t) in turns.iter().enumerate() {
        if to_drop == 0 {
            break;
        }
        if !t.metadata.pinned {
            keep[i] = false;
            to_drop -= 1;
        }
    }
    // Pair repair: a surviving `Role::Tool` whose owning assistant was
    // dropped is rejected by providers ("tool message without preceding
    // tool_calls"). Tool results always follow their assistant, so
    // every orphan sits at the front of the survivor region — extend
    // the drop set until the survivors start on a non-tool message.
    let mut start = 0;
    while start < turns.len() && !keep[start] {
        start += 1;
    }
    while start < turns.len() && turns[start].role == kod_types::MessageRole::Tool {
        keep[start] = false;
        start += 1;
    }
    let mut it = keep.into_iter();
    turns.retain(|_| it.next().unwrap_or(true));
}

/// Identity of a diagnostic for diffing between two check runs.
/// Ignores line and column: an edit that shifts a later error down by
/// a line did not create a new error.
pub(crate) fn diag_key(d: &kod_tools::check::Diagnostic) -> (String, Option<String>, String) {
    (d.file.clone(), d.code.clone(), d.message.clone())
}

/// Append up to `max` diagnostics to `block`, one per line, in the
/// format `severity [code] file:line:col — message`.
pub(crate) fn render_diagnostics(
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

/// Render the compiler auto-check block from already-computed inputs.
///
/// Pure: the caller runs `cargo check`, diffs against the baseline,
/// and resolves the Jev "genuinely new" filter; this decides what the
/// model is told. Extracted so the decision table can be tested
/// without a nested `cargo check` — the pre-fix integration test drove
/// the whole path, which meant it asserted on a subprocess succeeding
/// under CI contention rather than on the logic.
pub(crate) fn render_compiler_auto_check(
    block: &mut String,
    source: &str,
    diags: &[kod_tools::check::Diagnostic],
    baseline: Option<&[kod_tools::check::Diagnostic]>,
    new_diags: &[&kod_tools::check::Diagnostic],
) {
    use std::collections::HashSet;
    let baseline_keys: HashSet<(String, Option<String>, String)> = baseline
        .map(|v| v.iter().map(diag_key).collect())
        .unwrap_or_default();
    let current_keys: HashSet<(String, Option<String>, String)> =
        diags.iter().map(diag_key).collect();
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
                block.push_str(&format!("`{source}` is clean.\n"));
            } else {
                block.push_str(&format!(
                    "`{source}` reported {} diagnostic(s). \
                     (No baseline was captured, so all are shown — \
                     some may pre-date this write.)\n\n",
                    diags.len(),
                ));
                render_diagnostics(block, diags, 20);
            }
        }
        Some(_) if !new_diags.is_empty() => {
            block.push_str(&format!(
                "`{source}` reported {} NEW diagnostic(s) from this write:\n\n",
                new_diags.len(),
            ));
            let owned: Vec<kod_tools::check::Diagnostic> =
                new_diags.iter().map(|d| (*d).clone()).collect();
            render_diagnostics(block, &owned, 20);
            if resolved_count > 0 {
                block.push_str(&format!(
                    "\n({resolved_count} pre-existing diagnostic(s) resolved.)\n",
                ));
            }
            block.push_str("\nFix the new errors before continuing.\n");
        }
        Some(_) if resolved_count > 0 => {
            block.push_str(&format!(
                "`{source}`: your write introduced no new errors and \
                 resolved {resolved_count} pre-existing diagnostic(s).\n",
            ));
        }
        Some(_) if diags.is_empty() => {
            block.push_str(&format!("`{source}` is clean.\n"));
        }
        Some(_) => {
            block.push_str(&format!(
                "`{source}`: your write introduced no new errors. \
                 {} pre-existing diagnostic(s) remain, \
                 unrelated to this change.\n",
                diags.len(),
            ));
        }
    }
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
pub(crate) fn strip_conversation_tail(system_text: &str) -> String {
    const MARKER: &str = "## Conversation so far";
    match system_text.find(MARKER) {
        Some(i) => system_text[..i].trim_end().to_string(),
        None => system_text.to_string(),
    }
}

#[cfg(test)]
mod render_proptests {
    //! Properties for the private pure helpers in this module.
    //!
    //! `render.rs` has **zero tests**. The four helpers below are
    //! pure (`&str -> String` and `&str -> bool`), and their
    //! contracts are exactly what the module doc claims: "nothing
    //! here touches `KodEngine` state; every item is a free
    //! function."
    //!
    //! **No bug expected.** The one place that *looks* like a
    //! byte-vs-char bug — `truncate_with_ellipsis`'s
    //! `if shown.len() < s.len()` — is correct by construction,
    //! because `truncate_chars(s, max)` returns a strict prefix of
    //! `s`. `shown.len() < s.len()` iff `shown != s` iff the char
    //! count was truncated. The properties below pin that
    //! assumption so a future refactor of `truncate_chars` that
    //! returns a normalized or padded string is a visible
    //! decision, not a silent ellipsis bug.
    //!
    //! `line_is_goal_marker`'s `stripped.get(..8)` is byte-safe on
    //! CJK because `str::get(range)` returns `None` when the byte
    //! index is not a char boundary; the code handles the `None`
    //! case by skipping the block. A CJK property pins that.

    use super::*;
    use proptest::prelude::*;

    fn cjk_char() -> impl Strategy<Value = char> {
        prop::sample::select(vec![
            '中', '国', '建', '設', '計', '師', '文', '字', '力', '心',
        ])
    }

    fn cjk_string(max_len: usize) -> impl Strategy<Value = String> {
        prop::collection::vec(cjk_char(), 0..=max_len).prop_map(|chars| chars.into_iter().collect())
    }

    // ---- truncate_with_ellipsis ---------------------------------------

    proptest! {
        /// Under the char budget, the input is returned verbatim.
        #[test]
        fn prop_truncate_under_budget_is_identity(
            s in "[a-zA-Z]{0,20}",
            max in 20usize..=30usize,
        ) {
            prop_assert_eq!(truncate_with_ellipsis(&s, max, "…"), s);
        }

        /// Over the char budget, the result ends with the ellipsis.
        #[test]
        fn prop_truncate_over_budget_ends_with_ellipsis(
            s in "[a-zA-Z]{5,20}",
            max in 0usize..=4usize,
        ) {
            let out = truncate_with_ellipsis(&s, max, "…");
            prop_assert!(
                out.ends_with('…'),
                "output {:?} should end with ellipsis (s={:?}, max={})", out, s, max,
            );
        }

        /// The result's char count is bounded by `max + ellipsis chars`
        /// for both ASCII and CJK inputs. This is the char-boundary
        /// property that `truncate_with_ellipsis` relies on
        /// `truncate_chars` for.
        #[test]
        fn prop_truncate_bound_ascii(
            s in "[a-zA-Z]{0,30}",
            max in 0usize..=20usize,
        ) {
            let out = truncate_with_ellipsis(&s, max, "…");
            prop_assert!(
                out.chars().count() <= max + 1,
                "output {:?} has {} chars, budget {} + ellipsis 1",
                out, out.chars().count(), max,
            );
        }

        /// Same bound for CJK: a refactor of `truncate_chars` that
        /// swapped chars for bytes would blow the char budget here.
        #[test]
        fn prop_truncate_bound_cjk(
            s in cjk_string(20),
            max in 0usize..=10usize,
        ) {
            let out = truncate_with_ellipsis(&s, max, "…");
            prop_assert!(
                out.chars().count() <= max + 1,
                "output {:?} has {} chars, budget {} + ellipsis 1",
                out, out.chars().count(), max,
            );
        }

        /// ASCII input: result is always either the original or the
        /// original prefix plus ellipsis. No byte-level mangling.
        #[test]
        fn prop_truncate_result_is_a_prefix_plus_ellipsis(
            s in "[a-zA-Z]{0,30}",
            max in 0usize..=20usize,
        ) {
            let out = truncate_with_ellipsis(&s, max, "…");
            let prefix: String = s.chars().take(max).collect();
            let expected = if prefix.chars().count() < s.chars().count() {
                format!("{prefix}…")
            } else {
                s.clone()
            };
            prop_assert_eq!(out, expected);
        }
    }

    // ---- shorten_path --------------------------------------------------

    proptest! {
        /// A path with at most `KEEP + 1` non-empty segments is
        /// returned verbatim.
        #[test]
        fn prop_shorten_short_path_is_identity(seg in "[a-z]{1,8}") {
            prop_assert_eq!(shorten_path(&seg), seg.clone());
            let two = format!("{seg}/{seg}");
            prop_assert_eq!(shorten_path(&two), two);
            let three = format!("{seg}/{seg}/{seg}");
            prop_assert_eq!(shorten_path(&three), three);
        }

        /// A longer path loses its leading segments but keeps the
        /// last two.
        #[test]
        fn prop_shorten_keeps_last_two(
            segs in prop::collection::vec("[a-z]{1,6}", 4..=8),
        ) {
            let path = segs.join("/");
            let short = shorten_path(&path);
            prop_assert!(short.starts_with("…/"), "shortened {:?} should start with …/", short);
            let last_two = segs[segs.len() - 2..].join("/");
            prop_assert!(
                short.ends_with(&last_two),
                "shortened {:?} should end with {:?}", short, last_two,
            );
        }

        /// Idempotence: shortening a shortened path is a no-op.
        #[test]
        fn prop_shorten_idempotent(segs in prop::collection::vec("[a-z]{1,6}", 1..=8)) {
            let path = segs.join("/");
            let once = shorten_path(&path);
            let twice = shorten_path(&once);
            prop_assert_eq!(once, twice);
        }
    }

    // ---- cap_lines -----------------------------------------------------

    proptest! {
        /// Under the line limit, input is returned verbatim.
        #[test]
        fn prop_cap_lines_under_limit_is_identity(
            n_lines in 0usize..=4usize,
            max in 4usize..=10usize,
        ) {
            let text: String = (0..n_lines)
                .map(|i| format!("line {i}"))
                .collect::<Vec<_>>()
                .join("\n");
            prop_assert_eq!(cap_lines(&text, max), text);
        }

        /// Over the line limit, the result has at most `max + 1` lines
        /// (the extra line is the "… and N more" marker).
        #[test]
        fn prop_cap_lines_bound(
            n_lines in 5usize..=20usize,
            max in 1usize..=4usize,
        ) {
            let text: String = (0..n_lines)
                .map(|i| format!("line {i}"))
                .collect::<Vec<_>>()
                .join("\n");
            let out = cap_lines(&text, max);
            prop_assert!(
                out.lines().count() <= max + 1,
                "capped output has {} lines (max {} + 1)", out.lines().count(), max,
            );
        }
    }

    // ---- line_is_goal_marker -------------------------------------------

    #[test]
    fn prop_goal_met_exact_is_true() {
        assert!(line_is_goal_marker("GOAL MET"));
        assert!(line_is_goal_marker("goal met"));
        assert!(line_is_goal_marker("Goal Met"));
    }

    #[test]
    fn prop_goal_met_with_separator_is_true() {
        for tail in [
            " - done",
            ": summary",
            "# heading",
            ". done",
            "! done",
            "(note)",
        ] {
            let line = format!("GOAL MET{tail}");
            assert!(
                line_is_goal_marker(&line),
                "{:?} should match (structural separator after marker)",
                line,
            );
        }
    }

    #[test]
    fn prop_goal_met_without_separator_is_false() {
        for tail in [" is done", "X", "abc", ""] {
            let line = format!("GOAL MET{tail}");
            if tail.is_empty() {
                // Bare "GOAL MET" is caught by the first check.
                assert!(line_is_goal_marker(&line));
                continue;
            }
            assert!(
                !line_is_goal_marker(&line),
                "{:?} should NOT match (no structural separator after marker)",
                line,
            );
        }
    }

    /// CJK input never panics. A 5-CJK-char string is 15 bytes, so
    /// `stripped.get(..8)` returns `None` (byte 8 falls inside the
    /// 3rd character); the code handles that case by skipping the
    /// suffix check.
    #[test]
    fn prop_cjk_never_panics() {
        for s in ["中文中文中", "GOAL MET中文", "中文GOAL MET", "中"] {
            let _ = line_is_goal_marker(s);
        }
    }

    /// CJK text after the marker is not in the separator set, so the
    /// marker check returns false. Pins the ASCII-only separator
    /// contract.
    #[test]
    fn prop_cjk_after_marker_is_not_a_separator() {
        assert!(!line_is_goal_marker("GOAL MET中文"));
    }

    /// A CJK line with no ASCII marker is never a goal marker.
    #[test]
    fn prop_pure_cjk_is_not_goal_marker() {
        for s in ["中文", "中文中文中", "中文中文中文中文"] {
            assert!(
                !line_is_goal_marker(s),
                "{:?} should not be a goal marker",
                s
            );
        }
    }

    // ---- format_tool_header --------------------------------------------

    proptest! {
        /// `format_tool_header` is deterministic.
        #[test]
        fn prop_format_tool_header_deterministic(
            name in "[a-z_]{1,20}",
            path in "[a-zA-Z0-9/._-]{0,40}",
        ) {
            let args = serde_json::json!({ "path": path });
            prop_assert_eq!(
                format_tool_header(&name, &args),
                format_tool_header(&name, &args),
            );
        }

        /// The tool name is always the first token.
        #[test]
        fn prop_format_tool_header_starts_with_name(
            name in "[a-z_]{1,20}",
            path in "[a-zA-Z0-9/._-]{0,40}",
        ) {
            let args = serde_json::json!({ "path": path });
            let out = format_tool_header(&name, &args);
            prop_assert!(
                out == name || out.starts_with(&format!("{name} ")),
                "output {:?} should be the name or start with 'name '", out,
            );
        }
    }
}
