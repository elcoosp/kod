//! Stream control-marker protocol for the engine.
//!
//! Pure string encode/decode for the `\0kod-*` markers the engine emits
//! on the chunk channel and the TUI/CLI parse. Extracted from
//! `engine/mod.rs` (which had grown past 19k lines) — nothing here
//! touches engine state, so it is a self-contained protocol module.
//!
//! Re-exported from `engine` so existing `engine::parse_*` call sites
//! are unchanged.

/// Emitted by the engine just before it sleeps out a provider
/// rate-limit window and re-drives the turn on the same endpoint
/// (H-RL1). The TUI renders it as a system row and the CLI prints one
/// line; neither appends it to the transcript.
pub const RATE_LIMIT_WAIT_MARKER: &str = "\0kod-rate-limit:";

/// Marker announcing an automatic server-busy wait:
/// `\0kod-server-busy:<secs>\0<attempt>\0<max>`. Same contract as
/// [`RATE_LIMIT_WAIT_MARKER`] but for provider overload (HTTP 503
/// `server_busy`, ~10 min cooldown) instead of send-frequency rate
/// limits. Rendered distinctly ("Server busy…") by the TUI/CLI.
pub const SERVER_BUSY_WAIT_MARKER: &str = "\0kod-server-busy:";

/// Build a rate-limit-wait marker: the window in seconds plus the
/// 1-based attempt and the attempt cap, for display.
pub fn rate_limit_wait_marker(secs: u64, attempt: u32, max_attempts: u32) -> String {
    format!("{RATE_LIMIT_WAIT_MARKER}{secs}\0{attempt}\0{max_attempts}")
}

/// Build a server-busy-wait marker (see [`SERVER_BUSY_WAIT_MARKER`]).
pub fn server_busy_wait_marker(secs: u64, attempt: u32, max_attempts: u32) -> String {
    format!("{SERVER_BUSY_WAIT_MARKER}{secs}\0{attempt}\0{max_attempts}")
}

/// Parse a wait marker of either kind into `(is_server_busy, secs,
/// attempt, max_attempts)`. A malformed tail degrades to defaults
/// instead of dropping the notice.
pub fn parse_wait_marker(chunk: &str) -> Option<(bool, u64, u32, u32)> {
    if let Some(marker) = parse_rate_limit_wait(chunk) {
        return Some((false, marker.0, marker.1, marker.2));
    }
    parse_server_busy_wait(chunk).map(|(secs, attempt, max)| (true, secs, attempt, max))
}

/// If `chunk` is a rate-limit-wait marker, return
/// `(secs, attempt, max_attempts)`. A malformed tail degrades to
/// defaults instead of dropping the notice, the way [`parse_tool_done`]
/// degrades its duration.
pub fn parse_rate_limit_wait(chunk: &str) -> Option<(u64, u32, u32)> {
    parse_wait_marker_secs(chunk, RATE_LIMIT_WAIT_MARKER)
}

/// If `chunk` is a server-busy-wait marker, return
/// `(secs, attempt, max_attempts)` (see [`parse_rate_limit_wait`]).
pub fn parse_server_busy_wait(chunk: &str) -> Option<(u64, u32, u32)> {
    parse_wait_marker_secs(chunk, SERVER_BUSY_WAIT_MARKER)
}

fn parse_wait_marker_secs(chunk: &str, marker: &str) -> Option<(u64, u32, u32)> {
    let rest = chunk.strip_prefix(marker)?;
    let mut parts = rest.splitn(3, '\0');
    let secs = parts.next()?.parse::<u64>().unwrap_or(0);
    let attempt = parts
        .next()
        .unwrap_or("1")
        .parse::<u32>()
        .unwrap_or(1)
        .max(1);
    let max = parts
        .next()
        .unwrap_or("1")
        .parse::<u32>()
        .unwrap_or(1)
        .max(1);
    Some((secs, attempt, max))
}

/// Marker prefix for tool-start notices inside the `process_streaming`
/// chunk channel: `\0kod-tool:<cid>:<name>\0`. `<cid>` is the provider's
/// tool-call id (or `""` when the provider sends none) — the TUI keys its
/// live tool-row registry on it so parallel calls update their own rows
/// (see `parse_tool_start`).
pub const TOOL_START_MARKER: &str = "\0kod-tool:";

/// Sanitize a call id for the marker wire format: no `:` (field separator)
/// and no `\0` (marker terminator) may survive.
fn clean_call_id(cid: &str) -> String {
    cid.replace([':', '\0'], "_")
}

/// Build a tool-start marker chunk for call `cid` carrying `name`.
pub fn tool_start_marker(cid: &str, name: &str) -> String {
    format!("{TOOL_START_MARKER}{}:{name}\0", clean_call_id(cid))
}

/// If `chunk` is a tool-start marker, return `(call_id, tool_name)`.
/// A legacy v1 chunk (no `cid:` prefix) parses as `("", name)`.
pub fn parse_tool_start(chunk: &str) -> Option<(&str, &str)> {
    let rest = chunk.strip_prefix(TOOL_START_MARKER)?.strip_suffix('\0')?;
    Some(rest.split_once(':').unwrap_or(("", rest)))
}

/// Marker prefix for tool-argument excerpts inside the
/// `process_streaming` chunk channel: `\0kod-args:<cid>:<one-line display>\0`.
/// Sent after a streamed call's arguments are assembled but before the
/// tool executes, so the TUI's "running …" line can show the actual
/// command/file instead of just the tool name.
pub const TOOL_ARGS_MARKER: &str = "\0kod-args:";

/// Build a tool-args marker chunk carrying a one-line display string.
pub fn tool_args_marker(cid: &str, display: &str) -> String {
    format!("{TOOL_ARGS_MARKER}{}:{display}\0", clean_call_id(cid))
}

/// If `chunk` is a tool-args marker, return `(call_id, display)`.
pub fn parse_tool_args(chunk: &str) -> Option<(&str, &str)> {
    let rest = chunk.strip_prefix(TOOL_ARGS_MARKER)?.strip_suffix('\0')?;
    Some(rest.split_once(':').unwrap_or(("", rest)))
}

/// Marker prefix for per-tool completion notices inside the
/// `process_streaming` chunk channel:
/// `\0kod-done:<cid>\0<header>\0<summary>\0<duration_ms>`.
/// Sent by [`KodEngine::run_streaming_loop`] the moment each tool call
/// finishes — long before the whole agentic loop returns — so the TUI
/// can fill the live "running …" row in immediately instead of batching
/// every `ToolCompleted` at task end. Headers/summaries are sanitized
/// (no `\0`) when built by [`tool_done_marker`].
pub const TOOL_DONE_MARKER: &str = "\0kod-done:";

/// Build a tool-done marker chunk for one finished call.
pub fn tool_done_marker(cid: &str, header: &str, summary: &str, duration_ms: u64) -> String {
    let clean = |s: &str| s.replace('\0', " ");
    format!(
        "{TOOL_DONE_MARKER}{}\0{}\0{}\0{duration_ms}",
        clean_call_id(cid),
        clean(header),
        clean(summary)
    )
}

/// If `chunk` is a tool-done marker, return `(call_id, header, summary,
/// duration_ms)`. A missing or malformed duration degrades to `0` rather
/// than dropping the completion.
pub fn parse_tool_done(chunk: &str) -> Option<(&str, &str, &str, u64)> {
    let rest = chunk.strip_prefix(TOOL_DONE_MARKER)?;
    let mut parts = rest.splitn(4, '\0');
    let cid = parts.next()?;
    let header = parts.next()?;
    let summary = parts.next()?;
    let duration = parts.next().unwrap_or("0");
    Some((cid, header, summary, duration.parse::<u64>().unwrap_or(0)))
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

/// Marker for goal-loop turn boundaries on the streaming chunk
/// channel: `\0kod-turn:<n>\0`. Sent by `process_goal_streaming_for`
/// before each turn after the first so consumers can close the previous
/// turn's bubble and open a new one. A `\0`-prefixed control chunk —
/// never model prose — so consumers that only render plain text
/// (ACP, CLI pumps) must translate it to `—— turn N ——` or skip it,
/// never print it raw.
pub const TURN_MARKER: &str = "\0kod-turn:";

/// Build a turn-boundary marker chunk for goal-loop turn `n`.
pub fn turn_marker(turn: u32) -> String {
    format!("{TURN_MARKER}{turn}\0")
}

/// If `chunk` is a turn-boundary marker, return the turn number.
pub fn parse_turn_marker(chunk: &str) -> Option<u32> {
    let rest = chunk.strip_prefix(TURN_MARKER)?.strip_suffix('\0')?;
    rest.parse::<u32>().ok()
}

/// Marker for auxiliary-work notices on the streaming chunk channel:
/// `\0kod-activity:<label>\0`. Sent around synchronous helper work
/// that stalls the visible turn — Jev verdicts, memory fact
/// extraction, decision mining — so consumers can show what the turn
/// is actually doing instead of a stale "thinking…". A `\0`-prefixed
/// control chunk: text-only consumers skip it, the TUI renders the
/// label as its spinner phase, and any later text / tool / thinking /
/// turn chunk supersedes it.
pub const ACTIVITY_MARKER: &str = "\0kod-activity:";

/// Build an activity marker chunk carrying a short lowercase label
/// (`saving memories…`). The label is sanitized (no `\0`).
pub fn activity_marker(label: &str) -> String {
    format!("{ACTIVITY_MARKER}{}\0", label.replace('\0', " "))
}

/// If `chunk` is an activity marker, return its label.
pub fn parse_activity_marker(chunk: &str) -> Option<&str> {
    chunk.strip_prefix(ACTIVITY_MARKER)?.strip_suffix('\0')
}

/// Marker for per-round usage on the streaming chunk channel:
/// `\0kod-usage:<prompt>,<completion>\0`. Sent after every round
/// that reports provider usage, so consumers can track the live
/// context-window snapshot. A `\0`-prefixed control chunk: skipped
/// by text-only consumers, never printed raw.
///
/// Why per-round, not the merged turn total: the turn's merged
/// `TokenUsage` sums prompt tokens across rounds, and every round
/// re-sends the full history — the sum is a multiple of the actual
/// window contents. The LAST round's prompt + completion is the
/// true snapshot, and later markers supersede earlier ones.
pub const USAGE_MARKER: &str = "\0kod-usage:";

/// Build a usage marker chunk for one round's provider numbers.
pub fn usage_marker(prompt_tokens: usize, completion_tokens: usize) -> String {
    format!("{USAGE_MARKER}{prompt_tokens},{completion_tokens}\0")
}

/// If `chunk` is a usage marker, return `(prompt, completion)`.
/// Malformed numbers degrade to `None` rather than a wrong meter.
pub fn parse_usage_marker(chunk: &str) -> Option<(usize, usize)> {
    let rest = chunk.strip_prefix(USAGE_MARKER)?.strip_suffix('\0')?;
    let (p, c) = rest.split_once(',')?;
    Some((p.parse::<usize>().ok()?, c.parse::<usize>().ok()?))
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
