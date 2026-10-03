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
