//! Main KOD engine - orchestrates all subsystems.
//!
//! Coordinates the task router, LLM providers, skills, and memory
//! to process user requests end-to-end.

mod agent_loop;
mod compaction;
mod control;
mod jev;
mod jev_advisor;
mod lifecycle;
mod lsp;
mod markers;
pub mod render;
mod memory;
mod plans;
mod policy;
mod process;
mod routing;
mod settings;
mod state;
mod swarm;
mod tool_dispatch;
mod transcript;

pub use markers::*;
pub use render::*;

use crate::router::{RouterConfig, TaskResponse, TaskRouter};
use kod_error::{KodError, Result};
use kod_provider::request::{CompletionRequest, SystemPrompt, SystemSegment};
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

/// H-RL1: how many times the engine may sleep out a provider rate-limit
/// window and re-drive the same request within one turn (streaming and
/// collected paths share the cap). A second full window inside one turn
/// means the provider's quota is not coming back soon enough to be
/// worth parking the session again — the error surfaces instead.
const MAX_RATE_LIMIT_RETRIES: u32 = 2;

/// How many times the engine may sleep out a provider *overload*
/// (`server_busy`, ~10 min cooldown) and re-drive the same request
/// within one turn. Higher than [`MAX_RATE_LIMIT_RETRIES`]: overload
/// clears faster than a send-frequency quota, so two extra waits are
/// still worth parking the session for.
const MAX_SERVER_BUSY_RETRIES: u32 = 4;

/// Delta §4.5: the minimum token count a tool result must have
/// before the inline-imaging pass considers rasterizing it.
const MIN_INLINE_IMAGE_TOKENS: u64 = 3_000;

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
pub(crate) enum EarlyTermination {
    None,
    Complete,
    OffTrack,
}

/// Marker announcing an automatic rate-limit wait on the streaming
/// chunk channel: `\0kod-rate-limit:<secs>\0<attempt>\0<max>`.
#[cfg(test)]
mod rate_limit_marker_tests;

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
        let ch = input[i..].chars().next().unwrap_or('\0');
        out.push(ch);
        i += ch.len_utf8();
    }
    // Re-decode as UTF-8 — the byte-copy above yields a valid string
    // because we only skipped whole bytes when expanding.
    out
}

/// F2c-9: keep `~/.kod/background/` from growing without bound. The
/// spool files are per-job output captures; a session that runs many
/// background commands leaves one file each, never removed. Sweep to
/// the newest `MAX` by mtime when a new job starts (cheap: one
/// `read_dir` per job creation, not per frame).
///
/// Best-effort: a filesystem error is logged and ignored — a full
/// sweep is a hygiene nicety, not a correctness path.
fn sweep_background_spools(dir: &std::path::Path) {
    const MAX_SPOOLS: usize = 64;
    let Ok(rd) = std::fs::read_dir(dir) else {
        return;
    };
    let mut files: Vec<(std::path::PathBuf, std::time::SystemTime)> = rd
        .filter_map(|e| e.ok())
        .filter_map(|e| {
            let p = e.path();
            if p.extension().and_then(|x| x.to_str()) != Some("log") {
                return None;
            }
            let t = e.metadata().ok()?.modified().ok()?;
            Some((p, t))
        })
        .collect();
    if files.len() <= MAX_SPOOLS {
        return;
    }
    // Oldest first; remove everything past the newest MAX.
    files.sort_by_key(|(_, t)| *t);
    let remove = files.len() - MAX_SPOOLS;
    for (p, _) in files.into_iter().take(remove) {
        let _ = std::fs::remove_file(&p);
    }
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
            // No preference: the caller's own timeout applies
            // unchanged. A swarm worker's effort is set at its
            // dispatch, not here.
            effort: None,
            // No tool-choice directive: the model decides.
            tool_choice: None,
        }
    }
}

/// Render a tool result for chat: file lists become counts + names,
/// command output keeps its lines, everything caps at [`TOOL_RESULT_LINES`]
/// with an explicit "…and N more" instead of a mid-token cut.

/// Build the summarization prompt from the block about to be dropped.
///
/// Bounded: the dropped block can be arbitrarily large, and the
/// summary call should cost a fraction of the context it replaces. The
/// cap is generous (32k chars, roughly 8k tokens) because a summary of
/// a summary compounds error; the model needs enough of the original
/// to write something usable.

/// Push a background notice onto a transcript's steer queue.
async fn push_background_interrupt(
    steers: &std::sync::Arc<
        tokio::sync::RwLock<HashMap<String, Vec<kod_core_state::steer::SoftInterrupt>>>,
    >,
    holder: &str,
    content: String,
) {
    if content.trim().is_empty() {
        return;
    }
    steers
        .write()
        .await
        .entry(holder.to_string())
        .or_default()
        .push(kod_core_state::steer::SoftInterrupt::background(content));
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
/// A session-scoped learned approval (Tier 2.3). Populated by the
/// "always approve" action; two calls match when their tool name and
/// arguments' hash are identical.
#[derive(Debug, Clone, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub struct LearnedAllow {
    pub tool_name: String,
    /// FNV-1a hash of the call's arguments in their canonical JSON
    /// form. Whitespace and key order are not normalized — a
    /// differing call is a new request for approval.
    pub args_hash: String,
}

impl LearnedAllow {
    pub fn from_call(call: &ToolCall) -> Self {
        let bytes = serde_json::to_vec(&call.arguments).unwrap_or_default();
        // T3-C3: SHA-256-derived u64 instead of FNV-1a so collisions are
        // cryptographically hard rather than trivially brute-forceable.
        use sha2::{Digest, Sha256};
        let mut sha = Sha256::new();
        sha.update(&bytes);
        let digest = sha.finalize();
        let mut h: u64 = 0;
        for b in &digest[..8] {
            h = (h << 8) | (*b as u64);
        }
        Self {
            tool_name: call.tool_name.clone(),
            args_hash: format!("{h:016x}"),
        }
    }
}

pub(crate) struct RoundContext<'a> {
    system_text: &'a str,
    model_ref: &'a ModelRef,
    definitions: &'a [ToolDefinition],
    options: &'a GenerationOptions,
    holder: &'a str,
    /// Turn trace builder (Tier 1.4). `None` when no trace writer is
    /// installed — every trace call is a no-op in that case.
    trace: Option<&'a std::sync::Mutex<kod_core_state::trace::TurnTraceBuilder>>,
    /// Next endpoint in the chain after this one (P5.6). The
    /// streaming loop uses it for mid-stream switching: when Jev
    /// flags the reply off-track, `stream_round` swaps to this
    /// endpoint's stream in place. `None` for the collected path,
    /// the goal loop, and the last endpoint in a chain.
    fallback: Option<&'a ModelRef>,
}

/// Outcome of one tool-execution round: results for the response plus a
/// prompt block feeding them back to the model. `elapsed_ms` parallels
/// `results` — per-call wall time for the live done-markers.
/// S10: the return value of `KodEngine::gate_tool_calls`. Carries
/// every piece of state the caller needs to proceed: the per-call
/// decisions, the deny set, the ask set, and the two locks the gate
/// acquired (so the caller's later code can reuse them without
/// re-acquiring).
pub(crate) struct PolicyGateResult {
    denied: std::collections::HashMap<usize, String>,
    need_approval: std::collections::HashSet<usize>,
    decisions: Vec<(usize, kod_config::PolicyDecision)>,
    policy: Option<std::sync::Arc<kod_config::PolicyEngine>>,
}

/// What one streaming round produced. A struct rather than the tuple
/// the method used to return: the speculation vector is a fifth
/// element and five-element tuples are unreadable at the call site.
///
/// `speculations` is indexed parallel to `calls` — `speculations[i]`
/// is the pre-fetched read for `calls[i]`, or `None` when no
/// speculation was made (the call is not a read, the read failed, or
/// speculation is disabled).
pub(crate) struct StreamRoundOutcome {
    text: String,
    calls: Vec<ToolCall>,
    usage: Option<kod_provider::TokenUsage>,
    retry_suggested: bool,
    speculations: Vec<Option<kod_core_tools::speculation::SpeculativeRead>>,
    /// The provider's `stop_reason` for the round, when reported
    /// (`"end_turn"`, `"max_tokens"`, `"tool_use"`, …). Threaded out
    /// so the run collector's stop-reason histogram is populated.
    stop_reason: Option<String>,
    /// M-13: a mid-stream error, carried ALONGSIDE the partial text
    /// and calls the loop assembled. The caller decides whether to
    /// surface it. Pre-fix, the error `return Err(err)`ed the partials
    /// away even though the H-E11 comment promised they survive.
    partial_error: Option<kod_error::KodError>,
}

pub(crate) struct ToolRound {
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
/// §7.3: how long a memory entry stays out of the prompt after being
/// injected once. 45 minutes: long enough that a working session does
/// not repeat itself, short enough that a fact is refreshed before it
/// is forgotten.
const MEMORY_INJECTION_TTL_MS: u64 = 45 * 60 * 1000;

pub(crate) const DEFAULT_TRANSCRIPT_KEY: &str = "";

/// Main engine for KOD
/// H-S13: a conservative static allowlist for the Jev-driven sandbox
/// downgrade. A command that fails any of these tests keeps its
/// sandbox regardless of what Jev said, because Jev classified
/// model-authored text and a prompt injection can flip its own
/// verdict.
///
/// The check is structural, not textual:
///   - no redirection or pipe characters
///   - no shell chaining operators
///   - no command substitution
///   - the first token must be one of a small set of common
///     dev / query binaries
///   - no arguments that look like script injection (`eval`, `exec`,
///     `source`, `.`, `:`)
fn command_is_sandbox_downgrade_safe(command: &str) -> bool {
    let trimmed = command.trim();
    if trimmed.is_empty() {
        return false;
    }
    // Structural: any of these means we cannot cheaply reason about
    // what the command does.
    const UNSAFE_TOKENS: &[&str] = &[";", "&&", "||", "|", ">", "<", "`", "$(", "${", "\n", "\r"];
    if UNSAFE_TOKENS.iter().any(|t| trimmed.contains(t)) {
        return false;
    }
    // First whitespace-separated token, allowing a full path.
    let first = trimmed
        .split_whitespace()
        .next()
        .unwrap_or("")
        .rsplit('/')
        .next()
        .unwrap_or("");
    // A curated list of binaries that are read-only or trivially
    // auditable. `git` is on the list only with a read-only
    // subcommand (checked below).
    const ALLOW: &[&str] = &[
        "ls", "cat", "head", "tail", "grep", "find", "pwd", "which", "echo", "true", "false", "wc",
        "sort", "uniq", "diff", "file", "stat", "tree", "du", "df", "date", "env", "printenv",
        "id", "whoami", "cargo", "rustc", "rustup", "go", "gofmt", "python", "python3", "node",
        "npm", "npx", "tsc", "ruff", "pytest", "make", "cmake",
    ];
    if !ALLOW.iter().any(|b| b == &first) {
        // `git` needs the subcommand check.
        if first != "git" {
            return false;
        }
        let sub = trimmed
            .split_whitespace()
            .nth(1)
            .unwrap_or("")
            .rsplit('/')
            .next()
            .unwrap_or("");
        return matches!(
            sub,
            "status" | "diff" | "log" | "show" | "branch" | "remote" | "blame"
        );
    }
    // Reject a couple of argument shapes that are still unsafe even
    // when the first token is allowlisted.
    for tok in ["eval", "exec", "source"] {
        if trimmed.split_whitespace().any(|w| w == tok) {
            return false;
        }
    }
    true
}

/// The bus and registry shared by every agent in a swarm run.
///
/// Installed by [`KodEngine::install_swarm_file_bus`] before the
/// runner spawns agents and removed when the run ends. The engine's
/// per-call [`ToolContext`] derives a file-touch hook from it for
/// swarm transcripts; the interactive session (`""`) never gets a
/// hook, so a single-user turn pays zero observation cost.
#[derive(Clone)]
pub(crate) struct SwarmFileBus {
    pub bus: std::sync::Arc<kod_swarm::file_touch::FileTouchBus>,
    pub service: std::sync::Arc<kod_swarm::file_touch::FileTouchService>,
}

/// Delta §11.2: what a cold-revive surface check found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ColdReviveVerdict {
    /// The session log carries no `SessionInit` for the holder. A
    /// caller can fall back to a fresh-surface revive.
    NoInitEntry,
    /// The current tool surface matches the persisted fingerprint
    /// byte-for-byte. Safe to revive.
    SurfaceMatches,
    /// The surface drifted: the working dir or the tool set changed
    /// between the run that wrote the log and the current process.
    /// `missing` are tools the persisted run had that are gone now;
    /// `added` are tools the current process has that the log did
    /// not name. Both sorted for a stable log line.
    SurfaceDrifted {
        missing: Vec<String>,
        added: Vec<String>,
    },
}

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
    /// Delta section 9.7: per-failure-class fallback chains. Read in
    /// the fallback loops when a candidate chain falls through; a
    /// class entry names additional endpoints to try. Empty (the
    /// default) preserves the flat `routing.fallback` behavior.
    retry_config: RwLock<kod_config::RetryConfig>,
    /// `Arc`-shared so the advisor sink can observe run state
    /// without holding a reference to the engine (which would be a
    /// cycle: the engine owns the tool registry owns the tool owns
    /// the sink). Same pattern as `steers` and `lock_table`.
    is_running: Arc<RwLock<bool>>,
    tools: Arc<ToolRegistry>,
    tool_context: ToolContext,
    /// Delta §7.5: the in-memory artifact store the internal-URL
    /// router serves for `artifact://`. Exposed publicly via
    /// [`Self::store_artifact`] so shake, the minimizer, and any
    /// future large-blob offload path have one place to write. The
    /// router that dispatches to it is installed into the base
    /// `tool_context` at construction, so every per-call context
    /// the engine derives sees the same store.
    artifact_handler: Arc<kod_tools::ArtifactHandler>,
    /// Shared per-path advisory locks. Cloned into every per-call
    /// tool context the engine derives, so a swarm agent and the
    /// interactive session contend on the same table.
    lock_table: Arc<PathLockTable>,
    working_dir: PathBuf,
    /// Steer notes queued while a prompt is running, keyed by transcript
    /// (D4-D4, AD-11). `steer("note")` writes to the default key;
    /// `steer_for(key, note)` targets one agent. The loops drain only
    /// their own key.
    /// Arc-shared so a spawned background watcher can deliver an
    /// interrupt without holding a reference to the engine.
    steers: std::sync::Arc<RwLock<HashMap<String, Vec<kod_core_state::steer::SoftInterrupt>>>>,
    /// Set by [`KodEngine::request_cancel`]; loops check it between
    /// rounds. Keyed by transcript (D4-D4): a cancel for
    /// `swarm:{agent-id}` stops only that agent, not the whole swarm.
    /// The default key `""` is the interactive session.
    /// Per-transcript cancel state: `key → (fire_epoch, fired)`.
    ///
    /// The epoch exists because `clear_cancel_for` is called on a
    /// retry path (the swarm runner clears before reusing a
    /// transcript key), and a cancel that arrived *after* the runner
    /// decided to retry must not be erased by that clear. A clear
    /// carries the epoch it observed; if a newer fire has happened
    /// since, the clear is a no-op and the cancel stands. Without
    /// this, cancelling an agent mid-retry could be silently undone.
    cancels: parking_lot::RwLock<std::collections::HashMap<String, (u64, bool)>>,
    /// Delta §9.8: the process-wide pause gate. Checked at exactly
    /// two boundaries — before each model call and before each tool
    /// round — so an in-flight stream runs to completion rather than
    /// being torn down at the check. `Arc` because the TUI and CLI
    /// hold their own clone to call `pause` / `resume` from the
    /// keybinding layer without going through the engine.
    pause_gate: std::sync::Arc<crate::pause_gate::PauseGate>,
    /// Delta §11.6: the session's goal runtime. At most one active
    /// objective, with a token and wall-clock budget. The goal loop
    /// accounts each turn's usage against it and stops when the
    /// budget is spent.
    goal_runtime: RwLock<kod_core_state::goals::GoalRuntime>,
    /// Delta §11.4: owner-routed, batched delivery of finished-job
    /// results. A spawned background task enqueues here; the round
    /// boundary drains it into one steer per owner.
    async_delivery: std::sync::Arc<parking_lot::Mutex<crate::async_delivery::AsyncDelivery>>,
    /// Delta §9.11: per-run metadata. Updated once per turn and once
    /// per tool call; read by `/stats`.
    run_collector: std::sync::Arc<parking_lot::Mutex<crate::run_collector::RunCollector>>,
    /// Delta §13.2: per-request analytics aggregates.
    stats: std::sync::Arc<parking_lot::Mutex<kod_stats::request::Aggregates>>,
    /// Delta §14.5: the OTLP telemetry handle. A disabled handle
    /// (the default) makes every `record_*` a no-op; a caller
    /// installs an enabled one with `set_telemetry`.
    telemetry: RwLock<kod_telemetry::Telemetry>,
    /// Delta §12.7: session-frozen mental models. Rendered at a
    /// transcript boundary and injected as a cacheable system segment,
    /// so the bytes are stable for the session and the provider's
    /// prefix cache is not invalidated mid-turn.
    mental_models: RwLock<kod_memory::mental_models::MentalModels>,
    /// Delta §14.3: stream rules matched against the model's output as
    /// it streams. A rule that fires with `interrupt` aborts the
    /// stream so the caller can inject the correction and retry.
    ttsr: RwLock<kod_provider::ttsr::TtsrEngine>,
    /// Delta §12.6: per-transcript retention cursors. A rolling hash
    /// over the retained prefix tells the continuous-extraction path
    /// what is new since the last pass; a rewind or in-place edit
    /// resets it so the whole transcript is re-sent.
    retention_cursors: RwLock<HashMap<String, kod_memory::retention::RetentionCursor>>,
    /// Delta §7.2: late LSP diagnostics a background pass queued
    /// since the last turn. Drained at the start of `prepare_turn`
    /// and appended to the system prompt under
    /// `## LSP diagnostics (late)`. `Arc` so the spawned watcher can
    /// hold it independently of `self`.
    deferred_diagnostics: std::sync::Arc<kod_core_state::deferred_diagnostics::DeferredDiagnostics>,
    /// WS-B: per-transcript count of eligible user prompts seen since
    /// the last sharpshooter extraction. Mirrors the retention cursor:
    /// the decision extractor gets its own cadence counter so
    /// `decisions_every_n_turns` is enforceable per transcript.
    decisions_cursors: RwLock<HashMap<String, usize>>,
    /// Delta §13.2: behavioral signals folded over the session's user
    /// messages.
    behavioral: std::sync::Arc<parking_lot::Mutex<kod_stats::behavioral::BehavioralSignals>>,
    /// Transcripts, one per key. `DEFAULT_TRANSCRIPT_KEY` is the
    /// interactive session; a swarm agent uses `swarm:<agent-id>` so
    /// concurrent agents do not interleave their turns.
    history: RwLock<HashMap<String, Vec<kod_types::ChatMessage>>>,
    /// P2-a: summaries produced by the background compaction task,
    /// keyed by transcript. Written when the summary call completes,
    /// consumed by `maybe_compact_for` on the turn that crosses the
    /// hard threshold. An entry that is present replaces the
    /// emergency no-LLM text; an absent entry falls back to it.
    pending_summaries: std::sync::Arc<RwLock<HashMap<String, String>>>,

    /// P2-a: transcripts with a summary task currently running, so a
    /// soft threshold crossed on three consecutive turns starts one
    /// task, not three. Cleared when the task finishes (success or
    /// failure).
    summaries_in_flight: std::sync::Arc<RwLock<std::collections::HashSet<String>>>,

    /// Transcripts that have already been prewarmed for the turn
    /// currently being composed. Cleared when the turn begins, so the
    /// next fresh turn warms again.
    prewarmed: RwLock<std::collections::HashSet<String>>,

    /// §7.3: when each memory entry was last injected into a prompt,
    /// per transcript. A long session retrieves the same preference
    /// on every turn — the entry still matches the query, so nothing
    /// filters it — and the model reads it for the tenth time. The
    /// response that consumed an injection is already in the
    /// transcript; re-sending it is pure noise.
    ///
    /// Value is wall-clock ms of the injection. An entry younger than
    /// [`MEMORY_INJECTION_TTL_MS`] is skipped; older is re-injected,
    /// because the turn that saw it has scrolled out of the context
    /// by then.
    injected_memory_at: RwLock<HashMap<String, HashMap<kod_types::MemoryId, u64>>>,

    /// P2-a: the token count the provider reported for the most
    /// recent request on each transcript (prompt + completion). This
    /// is the *observed* number, not a char estimate — the two
    /// disagree by 20-50% and the compaction decision needs the real
    /// one. Absent until a transcript's first completed call.
    observed_usage: RwLock<HashMap<String, u64>>,

    /// Delta §4.1: the ordered compaction ladder. Called from
    /// `maybe_compact_for` before the summary path so a mechanical
    /// reduction (shake / prune) can avoid a model call entirely.
    /// The doc's rule: reduction before summarization.
    compaction_dispatcher: std::sync::Arc<crate::compaction_dispatcher::CompactionDispatcher>,

    /// Delta §2.4: a per-transcript anchor on the provider's own last
    /// settled usage, plus the message index that usage covered. The
    /// gauge turns per-turn context-size accounting from O(transcript)
    /// into O(new messages since the anchor). Distinct from
    /// `observed_usage` above (which stores only the raw u64) because
    /// the gauge also carries the anchor position, which is what makes
    /// tail-only estimation possible.
    ///
    /// Cleared on any event that invalidates the anchored prefix:
    /// compaction (messages removed), transcript forget, model switch.
    /// A stale anchor would make `estimate` lie about the size of the
    /// bytes the provider is charging for.
    context_gauges: RwLock<HashMap<String, kod_core_state::context_gauge::ContextGauge>>,
    /// Delta §4.4: per-transcript provider-native compaction blocks.
    /// Key is the transcript key (same key `history` uses); value is
    /// the opaque `encrypted_content` the provider returned. Attached
    /// to the next request built for that transcript so the server
    /// reuses its KV cache instead of re-reading.
    native_compaction_blocks: RwLock<HashMap<String, String>>,
    /// Delta §4.5: rasterized-frame cache keyed on
    /// `tool_call_id:content_hash`.
    image_render_cache: RwLock<HashMap<String, kod_types::RasterizedImage>>,
    /// Delta §10: whether speculative reads are admitted. On by
    /// default — the primitive's cost is one wasted read in the worst
    /// case, and the validation step (TOCTOU digest check) means the
    /// read is never *used* unless the file is provably unchanged.
    speculative_reads: RwLock<bool>,
    /// Delta §4.5: per-transcript rasterized frames. Key is the
    /// transcript key; value is the ordered list of frames to attach
    /// on the next request. Cleared on transcript reset for the same
    /// reason `native_compaction_blocks` is.
    image_frames: RwLock<HashMap<String, Vec<kod_provider::request::ImageFrame>>>,
    /// Delta §11.8: the advisor emission guard. Shared with the
    /// `advise` tool; the tool admits through it, the turn loop
    /// calls `begin_update` on it once per turn so the per-update
    /// budget resets exactly once per user prompt.
    advisor_guard: std::sync::Arc<parking_lot::Mutex<kod_swarm::advisor::EmissionGuard>>,

    /// Delta §9.4: per-transcript tool-call loop guards. When the
    /// model issues the same tool call (same name, same arguments)
    /// `DEFAULT_LOOP_THRESHOLD` rounds in a row, the guard emits a
    /// corrective that the round loop injects as a System message
    /// before the next model call. See `kod_core_tools::tool_loop_guard` for
    /// the fingerprint rules.
    tool_loop_guards: RwLock<HashMap<String, kod_core_tools::tool_loop_guard::ToolLoopGuard>>,
    /// Delta §11.7: per-transcript todo nudge trackers. Counts
    /// mutating tool calls since the last todo touch, caps mid-run
    /// nudges, and latches the completion reminder.
    todo_trackers: RwLock<HashMap<String, kod_tools::todo_tracker::TodoTracker>>,

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
    plans: RwLock<HashMap<String, kod_core_state::plan::Plan>>,
    /// Delta §12.3: set by the periodic memory-consolidation task
    /// when its tick produced a report worth acting on; drained by
    /// the next turn's `process_for` / streaming path so the
    /// consolidation runs on the engine side (the periodic task
    /// holds only a router clone).
    sharpshooter_due: std::sync::Arc<std::sync::atomic::AtomicBool>,
    /// Delta §11.12: per-transcript prewalk state. A prewalk arms
    /// when the user wants a one-way mid-session model handoff: it
    /// injects a "plan deliberately" nudge once, and the first
    /// mutating tool call fires the handoff (switch the model,
    /// scrub the nudge, push a checklist).
    prewalks: RwLock<HashMap<String, kod_core_tools::prewalk::Prewalk>>,
    /// Delta §11.10: transcripts currently in explicit plan mode. A
    /// transcript in plan mode restricts the tool set to read-only
    /// tools (read_file / grep / list_files / file_info / web_search)
    /// so the model cannot mutate the workspace while planning. The
    /// user enters and exits plan mode with the TUI/CLI toggle; the
    /// engine holds no implicit state machine — a plan that was
    /// auto-generated on a Complex task does not by itself put the
    /// transcript in plan mode.
    plan_mode: RwLock<std::collections::HashSet<String>>,
    /// Delta §11.10: per-transcript paths whose `read_file` results
    /// must survive shake and prune. Set by a caller (a future
    /// `plan_update` variant, a user command) when the plan reads a
    /// document the model needs to keep in context. Empty for a
    /// transcript with no plan or no declared reference paths.
    plan_reference_paths: RwLock<HashMap<String, std::collections::HashSet<std::path::PathBuf>>>,
    /// Per-transcript durable decisions (Tier 3.4). Populated on
    /// every turn from the classifier; rendered into the prompt after
    /// the plan.
    decision_logs: RwLock<HashMap<String, kod_core_state::decisions::DecisionLog>>,
    /// Delta §12.3: per-session friction-gated decision deltas. Each
    /// entry has been admitted through the grounding gate (evidence
    /// is an exact substring of the user prompt it was extracted
    /// from). The queue is drained by a consolidation pass — a
    /// follow-up — that ranks by friction and rewrites
    /// `architecture.md` / `product.md` / `style.md` under a line
    /// ceiling.
    sharpshooter_deltas: RwLock<Vec<kod_memory::sharpshooter::DecisionDelta>>,
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
    last_prompt: RwLock<HashMap<String, kod_core_state::budget::PromptTrace>>,
    /// Provider options captured from `LlmConfig` at engine construction.
    /// Transitional until D1 (endpoint config per ModelRef). Kept in an
    /// `RwLock<Option<...>>` so `set_generation_defaults` works through
    /// `&self` (the engine is shared as `Arc<KodEngine>`).
    generation_defaults: RwLock<GenerationDefaults>,
    /// Optional session log. When `Some`, every tool call and its result
    /// are appended as one JSONL entry, `kod replay`-able. `None` (the
    /// default) is the right shape for a test or a one-shot command.
    session_recorder:
        std::sync::RwLock<Option<std::sync::Arc<kod_core_state::session_log::SessionRecorder>>>,
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
    jev_client: std::sync::RwLock<Option<std::sync::Arc<dyn crate::jev::JevDecider>>>,
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
    /// F2b-10: from `[policy.git] history_protected`. Threaded into
    /// every per-call `ToolContext`; `true` (the default) refuses a
    /// raw write into `.git`.
    git_history_protected: std::sync::atomic::AtomicBool,
    /// The redactor used for content sanitization (Tier 1.3).
    redactor: std::sync::Arc<kod_types::redact::Redactor>,
    /// Delta §14.1: the reversible secret-placeholder vault. When
    /// `Some` and `[security.redact] in_prompt = true`, outgoing
    /// prompts have every registered secret replaced with a
    /// placeholder, and incoming tool arguments have placeholders
    /// replaced with the raw value. `None` disables the whole
    /// mechanism; `RwLock<Option<...>>` because a caller installs the
    /// vault after construction (same shape as `policy`).
    secret_vault: RwLock<Option<std::sync::Arc<kod_types::secret_placeholder::SecretVault>>>,
    /// Session cost accumulator (Tier 1.2). Clone the engine to
    /// share it with a UI.
    cost_tracker: kod_core_state::cost::CostTracker,
    /// On-disk persistence for plans and decision logs (Tier 3.4).
    /// `None` until `set_state_store` installs one — the default for
    /// a test or an embedder that does not want disk state.
    state_store: std::sync::RwLock<Option<kod_core_state::state::StateStore>>,
    /// Per-tool quota counters (Tier 2.5). Reset per turn and per
    /// session; enforced before every dispatch.
    tool_counts: std::sync::Arc<crate::tool_quota::ToolCounts>,
    /// The [limits.tools] configuration loaded at startup. `None`
    /// until `install_limits` runs.
    tool_quotas:
        std::sync::RwLock<Option<std::collections::BTreeMap<String, kod_config::ToolQuota>>>,
    /// Monotonic per-session turn id for the trace log (Tier 1.4).
    /// Hysteresis state for the per-turn Jev tool-category filter
    /// (P0 Fix 3). Keyed by transcript key so a swarm agent's filter
    /// state does not leak into the main session's.
    tool_filter_states: RwLock<HashMap<String, ToolFilterState>>,

    /// FNV-1a of the sorted tool definition bytes as of the last
    /// request per transcript key. `build_grounded_request` compares
    /// on each request and journals a change. The tools array lives
    /// inside the cached prefix (wire order: tools → system →
    /// messages), so ANY change here invalidates the whole prefix.
    /// A change caused by a late MCP registration is legitimate; a
    /// change with no cause is a bug, and the journal is the way to
    /// tell the difference.
    tool_surface_fingerprint: RwLock<HashMap<String, u64>>,
    /// P1 cache ledger. One per engine (not per transcript): a
    /// swarm agent and the interactive session may share an endpoint,
    /// and sharing one warm cache across both is the point.
    cache_ledger: std::sync::Mutex<kod_core_state::cache_ledger::CacheLedger>,
    /// P7: the current turn's sensitivity. Set at the start of each
    /// turn by the caller (a TUI/CLI knows the user's @-references;
    /// a swarm subtask has its brief's expected writes). The gate
    /// reads it when filtering the endpoint chain.
    current_sensitivity: RwLock<kod_core_state::sensitivity::Sensitivity>,

    /// P7: per-endpoint trust tier, from `EndpointConfig::trust`. An
    /// endpoint that declared no tier is treated as `standard`, which
    /// is the same default `TrustRequirement::satisfied_by` applies.
    /// Populated by `set_registry` from the loaded config; empty
    /// until a registry is installed, at which point every endpoint
    /// in the config has an entry.
    endpoint_trust: RwLock<std::collections::HashMap<String, String>>,
    /// Cached `(context_window, max_tokens)` for the current
    /// endpoint (harness review section 9 hygiene). Populated by
    /// `set_registry`, which every entry point calls before the
    /// first prompt. The pre-fix `prompt_allocation` re-read and
    /// re-parsed `~/.kod/config.toml` on every turn for two numbers
    /// that do not change within a session.
    budget_hint: std::sync::RwLock<(usize, usize)>,
    /// The caller's `RouterConfig.context_window`, kept so
    /// `budget_hint_for` can honour a caller that deliberately set a
    /// small window. The engine previously reloaded the user's
    /// on-disk config, so a caller that passed `context_window:
    /// 8192` (a test, a small-model deployment) got the user's
    /// window (often much larger) instead.
    config_window: usize,

    /// Per-model metadata keyed by `(endpoint, model id)`. Populated
    /// whenever a caller fetches `list_models()` — the TUI's `/model`
    /// refresh, the `serve` protocol handler, or any future explicit
    /// refresh. `budget_hint_for` consults it before falling back to
    /// the endpoint config, so once a caller has listed an endpoint's
    /// models, switching to one of them allocates against that
    /// model's reported context window rather than the endpoint's
    /// default. Empty until some caller populates it; every lookup
    /// degrades to the endpoint config on a miss.
    model_catalog:
        std::sync::RwLock<std::collections::HashMap<(String, String), kod_provider::ModelInfo>>,
    /// Circuit breaker for endpoint health (hygiene 3.2). A
    /// chronically failing endpoint is skipped in the chain for a
    /// cooldown instead of being retried as primary every turn.
    endpoint_health: std::sync::Mutex<kod_core_state::endpoint_health::EndpointHealth>,
    /// H-RL1: the longest provider-suggested rate-limit window (secs)
    /// the engine may sleep out before re-driving the failed request.
    /// Installed from the endpoint's `rate_limit_wait_secs` (default
    /// [`kod_config::DEFAULT_RATE_LIMIT_WAIT_SECS`]); `0` restores the
    /// legacy fail-fast. An atomic so the wait helpers read it without
    /// an async lock on the retry path.
    rate_limit_wait_budget_secs: std::sync::atomic::AtomicU64,
    /// P6: background read-only jobs. One runner per engine so the
    /// concurrency cap is shared across every spawn site.
    background: std::sync::Arc<crate::background::BackgroundJobRunner>,
    /// P6: when true this engine is a background job's child. The
    /// tool layer refuses anything not on the read-only whitelist —
    /// enforcement is at the tool call, not by a prompt, because a
    /// prompt cannot be trusted to hold.
    background_mode: std::sync::atomic::AtomicBool,
    /// P2: per-turn fidelity cache, keyed by transcript key. Each
    /// entry remembers what fidelity a turn was last scored at and
    /// against which query, so a call on the same topic does not
    /// re-score (and a topic change does).
    fidelity_cache: RwLock<HashMap<String, kod_core_routing::context_engine::FidelityCache>>,

    /// P3: the live tool inventory that `tool_search` reads. Shared
    /// between the tool and the engine so a registry change (MCP
    /// server attached, hot-reload) is visible to the search
    /// without re-registering the tool.
    tool_inventory: std::sync::Arc<std::sync::RwLock<kod_tools::tool_search::ToolInventory>>,
    next_turn_id: std::sync::atomic::AtomicU64,
    /// Append-only writer for `turns.jsonl`, next to the session log.
    /// `None` — the default — is the right shape for a test or a
    /// one-shot command that does not want a trace file.
    turn_trace_writer:
        std::sync::RwLock<Option<std::sync::Arc<kod_core_state::trace_writer::TraceWriter>>>,
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
    /// Session-scoped learned allow rules (Tier 2.3). Populated by
    /// the approval overlay's "always approve this" action. A
    /// matching call is pre-approved before the policy or taint
    /// gates are consulted.
    learned_allows: RwLock<std::collections::HashSet<LearnedAllow>>,

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
    /// with. H-R5: the runner calls `swarm.shutdown()` +
    /// `hub.clear_all()` at the end of `run()` — the pre-fix
    /// comment promised this, but neither call existed and every
    /// run leaked its agents.
    swarm_hub: Arc<kod_swarm::AgentCommunicationHub>,

    /// P1-c: the file-touch bus for the current swarm run, `None`
    /// outside one. Read by `run_tool_calls` to decide whether to
    /// install a per-call touch hook.
    swarm_file_bus: RwLock<Option<SwarmFileBus>>,
    /// Shared blackboard for the current swarm run (Tier 3.5).
    /// Auto-populated with write claims, discovered files, and
    /// completed subtasks; empty when no swarm is running.
    blackboard: kod_swarm::Blackboard,
    /// The engine's own identity on the hub. Stable for the life of
    /// the engine — the runner registers its own per-agent ids on
    /// top, and the note/read tools broadcast as this id.
    /// This engine's session identity. Generated once at

    /// Transcripts whose prompt should include the shared blackboard
    /// (Tier 3.5). Populated by the swarm runner before each agent
    /// runs; the interactive session never appears here, so its
    /// prompt stays as it was.
    blackboard_viewers: RwLock<std::collections::HashSet<String>>,

    /// construction and stable for the engine's lifetime. Used to

    /// attribute auto-extracted episodic facts to the session

    /// that produced them (design D2.5). Swarm-agent transcripts

    /// carry their own UUID in the transcript key, and that UUID

    /// is preferred when present.
    session_id: kod_types::SessionId,
    /// WS-A: one stable background session per engine, minted next to
    /// the transcript session id. Stamped onto every background LLM
    /// request when the active endpoint is a tab bridge, so the
    /// bridge keeps exactly one background chat session (one
    /// background tab) instead of a throwaway `anon-*` per request.
    background_session_id: kod_types::BackgroundSessionId,

    swarm_coordinator_id: kod_types::AgentId,
    /// The session's todo list. Shared across swarm agents and across
    /// every turn of the same engine.
    todo_list: kod_tools::TodoList,
    /// File checkpoint snapshots. `Some` when a checkpoint directory
    /// could be determined from the working directory; `None` when
    /// the home directory is unavailable (a stripped container, a
    /// test that has unset HOME). See [`kod_core_state::checkpoint`].
    checkpoints: Option<Arc<kod_core_state::checkpoint::CheckpointManager>>,
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
#[derive(Debug, Clone, PartialEq)]
pub enum ApprovalDecision {
    /// Run the call as the model proposed it.
    Approve,
    /// Run the call with these arguments substituted for the model's
    /// (Tier 2.3). The approval overlay's "edit" action sends this.
    ApproveWith {
        arguments: serde_json::Value,
    },
    Deny,
    /// Same as `Deny` in this version; the variant exists so that
    /// adding a "remember my choice" set later does not change the
    /// wire format.
    DenyAlways,
}

impl ApprovalDecision {
    /// True for both `Approve` and `ApproveWith`.
    pub fn is_approve(&self) -> bool {
        matches!(
            self,
            ApprovalDecision::Approve | ApprovalDecision::ApproveWith { .. },
        )
    }
}

/// Hysteresis state for the per-turn Jev tool-category filter (P0).
///
/// The tool list sits in the cached prefix (Anthropic caches the
/// ordered request stream up to the system marker, and tools precede
/// system on the wire), so every time Jev flips a category the whole
/// cached prefix — tools, system, repo map — goes cold. The
/// registry's own tools-by-name sort exists to keep the prefix
/// byte-stable; the per-turn filter was working against that.
///
/// This state records the last committed category set and the
/// classification that produced it. A refilter is allowed only when
/// the classified task signature differs from the committed one *and*
/// the committed set has survived `min_stable_turns` — enough turns
/// to have paid back the cache write it cost.
#[derive(Debug, Clone)]
pub(crate) struct ToolFilterState {
    /// Categories the committed filter kept. A refilter replaces this.
    pub enabled_categories: std::collections::HashSet<kod_types::ToolCategory>,
    /// The task signature the committed filter was computed for.
    pub committed_signature: String,
    /// Turns elapsed since the last commit. Incremented on every
    /// `filter_tool_definitions_with_hysteresis` call; reset to zero
    /// on commit.
    pub turns_since_change: u64,
    /// How many consecutive turns the committed set must survive
    /// before a different signature is allowed to replace it. Three
    /// is a design choice: two is too eager (a single mis-classified
    /// turn flips the set), five is too slow (a real task change
    /// waits half a minute on a chatty session).
    pub min_stable_turns: u64,
    /// One-shot gate: set when a commit changed the enabled set.
    /// The next `build_grounded_request` for this key consumes it and
    /// clears the transcript cache breakpoint for that single
    /// request. The round that first sees a changed prefix should not
    /// pay Anthropic's 1.25x cache-write premium for a prefix that
    /// may not survive the next round either.
    pub suppress_marker_once: bool,
}

impl ToolFilterState {
    /// A fresh state with no committed signature: the first call
    /// always refilters, since there is nothing to be stable against.
    fn fresh() -> Self {
        Self {
            enabled_categories: std::collections::HashSet::new(),
            committed_signature: String::new(),
            turns_since_change: 0,
            min_stable_turns: 3,
            suppress_marker_once: false,
        }
    }

    /// Whether a refilter is allowed for `task_sig`.
    ///
    /// Returns `false` when the signature is unchanged (the committed
    /// set is still correct) or when it changed but the committed
    /// set has not yet seasoned.
    fn should_refilter(&self, task_sig: &str) -> bool {
        if task_sig == self.committed_signature {
            return false;
        }
        self.turns_since_change >= self.min_stable_turns
    }

    /// Record that this turn passed. Called on every filter attempt,
    /// so `turns_since_change` tracks wall-clock turns rather than
    /// filter attempts.
    fn tick(&mut self) {
        self.turns_since_change = self.turns_since_change.saturating_add(1);
    }

    /// Commit a new signature and category set. Resets the stable
    /// counter so the *next* change must season again.
    fn commit(
        &mut self,
        task_sig: String,
        cats: std::collections::HashSet<kod_types::ToolCategory>,
    ) {
        self.committed_signature = task_sig;
        self.enabled_categories = cats;
        self.turns_since_change = 0;
    }
}

/// Bundle of values the three `process_*` entry points need after
/// classification, prompt build, and system-text construction.
///
/// S10 phase 1: the classification / memory-filter / prompt-build /
/// system-text pipeline used to be duplicated (plus one deliberate
/// plan-creation difference) across the three entry points. It now
/// lives in [`KodEngine::prepare_turn`]. Fields match the local
/// variables the callers used to introduce inline, so this is a
/// rename, not a rewrite.
pub(crate) struct TurnPreparation {
    pub response: crate::router::TaskResponse,
    pub task_type: crate::router::TaskType,
    pub refined_skills: Vec<String>,
    pub alloc: std::result::Result<
        kod_core_state::budget::Allocation,
        kod_core_state::budget::BudgetError,
    >,
    pub definitions: Vec<kod_types::ToolDefinition>,
    pub pending: String,
    pub system_text: String,
    pub initial_messages: Vec<kod_types::ChatMessage>,
}

impl KodEngine {
    /// Shared classification + prompt-build pipeline used by every
    /// `process_*` entry point.
    ///
    /// `retrieval_log_turn_id` is threaded through to
    /// [`KodEngine::classify_and_filter`]; `None` skips the retrieval
    /// log write (the goal path's choice), `Some(0)` is the collected
    /// path's placeholder, `Some(real_id)` is the streaming path.
    ///
    /// `create_plan` controls the Tier 2.1 model-authored plan. Only
    /// the collected path asks the model to plan on the first turn of
    /// a Complex/MultiStep task; the streaming paths rely on the model
    /// to reach for tools itself. See `process_for` for rationale.
    ///
    /// Ordering matters: the plan (if created) must be written to
    /// `self.plans` *before* `build_prompt_plan` renders `system_text`,
    /// otherwise the plan never reaches the model.
    /// Write the per-turn `PromptTrace` that powers `/debug last-prompt`
    /// and `/debug tokens`.
    ///
    /// Kept separate from `prepare_turn` because the goal path augments
    /// the prompt with a `## Goal` block *after* the pipeline runs — the
    /// snapshot must reflect what the model actually sees on turn 1, not
    /// what it saw before the goal was prepended. Callers pass their own
    /// final `pending` text and the allocation the pipeline produced.
    pub(crate) async fn snapshot_prompt(
        &self,
        key: &str,
        pending: &str,
        alloc: &std::result::Result<
            kod_core_state::budget::Allocation,
            kod_core_state::budget::BudgetError,
        >,
    ) {
        let trace = kod_core_state::budget::PromptTrace {
            text: pending.to_string(),
            alloc: alloc.as_ref().ok().copied(),
        };
        self.last_prompt
            .write()
            .await
            .insert(key.to_string(), trace);
    }

    /// P2-a: compact `key`'s transcript before the next render when
    /// the observed token count has crossed the hard threshold.
    ///
    /// Called from `prepare_turn` immediately before
    /// `render_history_for`, so the compacted transcript is what gets
    /// rendered and the current turn pays the smaller prompt. Returns
    /// the number of messages dropped (0 when no compaction ran).
    ///
    /// This is the *emergency* path only: it drops the oldest safe
    /// block and inserts a factual no-LLM summary. The background
    /// LLM summarization that replaces the dropped block with real
    /// prose is a follow-up; what matters here is that the transcript
    /// never grows past the window and never splits a tool pair.
    /// P2-a: spawn the background summarization for a transcript
    /// whose size has crossed the soft threshold.
    ///
    /// The task clones the provider (an `Arc`, cheap) and the two
    /// `Arc`-shared state maps, builds a prompt from the block that
    /// would be dropped, and writes the result to `pending_summaries`.
    /// When the hard threshold is crossed on a later turn,
    /// `maybe_compact_for` uses that text in place of the emergency
    /// summary: real prose rather than counts and file names.
    ///
    /// Fire-and-forget. A failure clears the in-flight mark so the
    /// next soft-threshold turn retries; the emergency path still
    /// bounds the context meanwhile, which is the property that has
    /// to hold.
    async fn spawn_compaction_summary(&self, key: &str, dropped: Vec<kod_types::ChatMessage>) {
        // One task per transcript; a second call while one is running
        // is a no-op rather than a duplicate request.
        {
            let mut in_flight = self.summaries_in_flight.write().await;
            if !in_flight.insert(key.to_string()) {
                return;
            }
        }

        let Some(provider) = self.current_provider().await else {
            self.summaries_in_flight.write().await.remove(key);
            return;
        };
        let options = self.generation_defaults.read().await.to_options();
        let prompt = build_summary_prompt(&dropped);

        // Clone the shared state; the task never touches `self`.
        let pending = self.pending_summaries.clone();
        let in_flight = self.summaries_in_flight.clone();
        let engine_key = key.to_string();

        tokio::spawn(async move {
            let result = provider.generate(&prompt, &options).await;
            match result {
                Ok(text) if !text.trim().is_empty() => {
                    pending
                        .write()
                        .await
                        .insert(engine_key.clone(), text.trim().to_string());
                }
                Ok(_) => {
                    tracing::warn!(key = %engine_key,
                        "compaction summary returned empty; emergency path will be used");
                }
                Err(e) => {
                    tracing::warn!(key = %engine_key, error = %e,
                        "compaction summary failed; emergency path will be used");
                }
            }
            in_flight.write().await.remove(&engine_key);
        });
    }

    /// Build the hook `execute_command` calls when the model sets
    /// `run_in_background` (P2-d).
    ///
    /// The hook spawns the command with its output going to a spool
    /// file, registers a job, and launches a watcher that fires a
    /// `SoftInterrupt::background` on completion or — when the caller
    /// set `stall_wake_seconds` — on a silence long enough to mean the
    /// command is wedged. The interrupt rides the same steer channel a
    /// user note does, so it lands at the next round boundary without
    /// the watcher needing the engine.
    ///
    /// Returns `None` when the spool cannot be created or the spawn
    /// fails, which the tool reads as "run it inline."

    /// Fire a speculative request that warms the provider's connection
    /// and its KV-cache prefix.
    ///
    /// Called when the user starts typing a fresh turn, so the real
    /// request — seconds later, once they finish composing — reads a
    /// cache that is already written. For a local model the same
    /// request keeps the weights resident, which is the larger win:
    /// `ollama` unloads after five idle minutes and the first real
    /// turn otherwise pays a full reload.
    ///
    /// **The economics are not free.** A request that writes the cache
    /// pays ~1.25x on the prefix; the real turn then reads at ~0.1x.
    /// That is a loss if the turn follows immediately and a win once
    /// the user has typed for a few seconds — which is why the caller
    /// fires on the first keystroke of a fresh turn, not on submit.
    ///
    /// One-shot per transcript: the latch is set here and cleared when
    /// a real turn begins. A run of keystrokes warms once.
    ///
    /// Never awaited by the caller. The request is discarded: this
    /// exists for its side effects on the provider, not for its
    /// output.

    /// A read-only snapshot of a transcript, for a client that wants
    /// to see what a session is doing **without attaching to it**.
    ///
    /// Attaching would disturb the very session being previewed — the
    /// peek must not touch the history, the steers, or the cancel
    /// flag. This reads the history under a shared lock, renders the
    /// last few turns, and returns. Nothing is mutated.
    ///
    /// `max_chars` bounds the tail: a peek is for orientation, and a
    /// caller that wants the whole transcript can read the session
    /// log.
    pub async fn peek_transcript(&self, key: &str, max_chars: usize) -> serde_json::Value {
        let (count, tail) = {
            let guard = self.history.read().await;
            let turns = guard.get(key);
            let count = turns.map(|t| t.len()).unwrap_or(0);
            let mut out = String::new();
            if let Some(turns) = turns {
                // The last 20 turns, newest-last, matching the order
                // the model would see them.
                for m in turns.iter().rev().take(20).rev() {
                    // Tool rows are structural; a peek shows prose.
                    if matches!(m.role, kod_types::MessageRole::Tool) {
                        continue;
                    }
                    out.push_str(&m.render_text());
                    out.push('\n');
                }
            }
            (count, out)
        };

        let total = tail.chars().count();
        let tail = if total > max_chars {
            tail.chars().skip(total - max_chars).collect::<String>()
        } else {
            tail
        };

        serde_json::json!({
            "key": key,
            "messages": count,
            "tail": tail,
            "truncated": total > max_chars,
        })
    }

    /// WS-C: whether the default endpoint is a tab bridge. The TUI
    /// and [`Self::prewarm`] consult this for the prewarm policy, and
    /// WS-A consults it for background-session stamping. Approximation:
    /// prewarm and background traffic both resolve through the default
    /// chain, so the default endpoint is the honest "active endpoint".
    async fn tab_bridge_active(&self) -> bool {
        match kod_config::KodConfig::load_cached() {
            Ok(c) => c.llm.default_endpoint().tab_bridge,
            Err(_) => false,
        }
    }

    /// WS-C: whether the keystroke prewarm probe may run. `auto`
    /// (default) disables it on tab-bridge endpoints, where the probe
    /// would warm the wrong tab.
    pub async fn prewarm_enabled(&self) -> bool {
        let (mode, tab_bridge) = match kod_config::KodConfig::load_cached() {
            Ok(c) => (c.llm.prewarm, c.llm.default_endpoint().tab_bridge),
            // An unreadable config must not change runtime behavior.
            Err(_) => return true,
        };
        match mode {
            kod_config::llm::PrewarmMode::On => true,
            kod_config::llm::PrewarmMode::Off => false,
            kod_config::llm::PrewarmMode::Auto => !tab_bridge,
        }
    }

    pub async fn prewarm(&self, key: &str) {
        // WS-C: policy gate first (defense in depth — the TUI checks
        // `prewarm_enabled` before spawning, but direct callers land
        // here too).
        if !self.prewarm_enabled().await {
            return;
        }
        // Latch first, so a burst of keystrokes does not queue a burst
        // of requests behind the first one.
        {
            let mut g = self.prewarmed.write().await;
            if !g.insert(key.to_string()) {
                return;
            }
        }

        let Some(provider) = self.current_provider().await else {
            return;
        };
        // No trace means no prompt has been built yet — a fresh
        // session has nothing cached to warm.
        let Some(trace) = self.last_prompt_trace_for(key).await else {
            return;
        };

        // Only the cacheable head is worth sending: the volatile tail
        // (environment, tool inventory, memory) differs every turn and
        // would not be a cache hit anyway.
        const VOLATILE_MARKER: &str = "## Volatile suffix";
        let cacheable = match trace.text.find(VOLATILE_MARKER) {
            Some(i) => trace.text[..i].trim_end(),
            None => return,
        };
        if cacheable.is_empty() {
            return;
        }

        let mut system = kod_provider::request::SystemPrompt::new();
        system = system.with(cacheable.to_string(), true);

        let model = self.current_model.read().await.clone();
        let req = kod_provider::request::CompletionRequest {
            system,
            messages: vec![kod_types::ChatMessage::text(
                kod_types::MessageId::new(),
                kod_types::MessageRole::User,
                "warm",
                time::OffsetDateTime::now_utc(),
            )],
            tools: Vec::new(),
            // One output token: the response is discarded, and a
            // larger budget is money spent for nothing.
            options: kod_provider::GenerationOptions {
                model: None,
                max_tokens: Some(1),
                temperature: Some(0.0),
                top_p: None,
                stop_sequences: Vec::new(),
                // A prewarm is not a reasoning call; it should fail
                // fast rather than wait out a thinking timeout.
                effort: Some(kod_provider::effort::EffortLevel::None),
                tool_choice: None,
            },
            model,
            cache_transcript: false,
            native_compaction_block: None,
            image_frames: Vec::new(),
            // Prewarm stays sessionless: it renders a literal "warm" probe
            // turn, which must never land in the session's tab or chain.
            // (On a tab backend it mints a throwaway anon session on some
            // other tab — pure cost, no benefit — which is why
            // `prewarm = "auto"` disables the probe there. The bridge
            // releases the ephemeral tab after the turn, so when the
            // probe does run it is recycled, not leaked.)
            session_id: None,
        };

        // Bounded: a provider that hangs must not leave a task
        // parked forever. Five seconds is longer than a warm cache
        // read and shorter than a user's typing. Note the timeout only
        // abandons kod's wait: on a bridge backend the turn keeps
        // running server-side and holds its tab to completion.
        let _ =
            tokio::time::timeout(std::time::Duration::from_secs(5), provider.complete(&req)).await;
    }

    /// Clear the prewarm latch so the next keystroke of a new turn
    /// warms again.
    pub(crate) async fn reset_prewarm(&self, key: &str) {
        self.prewarmed.write().await.remove(key);
    }

    pub fn build_background_hook(&self) -> kod_tools::context::BackgroundSpawnHook {
        let steers = std::sync::Arc::clone(&self.steers);
        let runner = std::sync::Arc::clone(&self.background);
        let working_dir = self.working_dir.clone();
        // Delta §11.4: the delivery queue the completion closure
        // enqueues into.
        let async_delivery = std::sync::Arc::clone(&self.async_delivery);

        kod_tools::context::BackgroundSpawnHook::new(
            move |command: &str, stall: Option<u64>, holder: &str| -> Option<String> {
                let job = runner.allocate_id();
                let id_str = job.0.to_string();

                let dir = dirs::home_dir()
                    .map(|h| h.join(".kod").join("background"))
                    .unwrap_or_else(std::env::temp_dir);
                sweep_background_spools(&dir);
                let spool_path = dir.join(format!("{}.log", job.0));

                let mut spool = match crate::output_spool::OutputSpool::create(&spool_path) {
                    Ok(s) => s,
                    Err(e) => {
                        tracing::warn!(error = %e, "background spool create failed");
                        return None;
                    }
                };

                runner.register(
                    job,
                    crate::background::JobKind::Shell {
                        command: command.chars().take(120).collect(),
                        spool: spool_path.clone(),
                    },
                );

                let full = format!("{command} 2>&1");
                let mut cmd = if cfg!(windows) {
                    let mut c = tokio::process::Command::new("cmd");
                    c.arg("/C").arg(&full);
                    c
                } else {
                    let mut c = tokio::process::Command::new("sh");
                    c.arg("-c").arg(&full);
                    c
                };
                cmd.current_dir(&working_dir);
                cmd.stdout(std::process::Stdio::piped());
                cmd.stderr(std::process::Stdio::null());

                let mut child = match cmd.spawn() {
                    Ok(c) => c,
                    Err(e) => {
                        runner.fail(job, format!("spawn failed: {e}"));
                        return None;
                    }
                };

                let Some(mut stdout) = child.stdout.take() else {
                    runner.fail(job, "child had no stdout pipe".to_string());
                    return None;
                };

                let stall_secs = stall.filter(|s| *s > 0).map(|s| s.max(30));
                let holder = holder.to_string();

                // The outer closure is `Fn` — it is called once per
                // background request — so every value the task owns
                // must be cloned out of it. The `Arc`s are cheap; the
                // job id is named in two notices and returned to the
                // caller, so the task takes a copy.
                let steers_task = std::sync::Arc::clone(&steers);
                let runner_task = std::sync::Arc::clone(&runner);
                let delivery_task = std::sync::Arc::clone(&async_delivery);
                let id_task = id_str.clone();

                // Guarded spawn: a panic inside the task becomes a
                // job failure rather than a permanently-Running job
                // whose caller waits forever.
                let runner_for_watch = std::sync::Arc::clone(&runner_task);
                runner_for_watch.spawn_guarded(job, async move {
                    use tokio::io::AsyncReadExt as _;
                    let mut buf = [0u8; 4096];
                    let mut stall_reported = false;
                    let stall_dur = stall_secs.map(std::time::Duration::from_secs);

                    loop {
                        let read = match stall_dur {
                            Some(d) => match tokio::time::timeout(d, stdout.read(&mut buf)).await {
                                Ok(r) => r,
                                Err(_) => {
                                    if !stall_reported {
                                        push_background_interrupt(
                                            &steers_task,
                                            &holder,
                                            format!(
                                                "background job {id_task} has produced no output for {secs}s",
                                                secs = stall_secs.unwrap_or(0),
                                            ),
                                        )
                                        .await;
                                        stall_reported = true;
                                    }
                                    continue;
                                }
                            },
                            None => stdout.read(&mut buf).await,
                        };

                        match read {
                            Ok(0) => break,
                            Ok(n) => {
                                if spool.append(&buf[..n]).is_err() {
                                    break;
                                }
                                stall_reported = false;
                            }
                            Err(_) => break,
                        }
                    }

                    let status = match tokio::time::timeout(
                        std::time::Duration::from_secs(60),
                        child.wait(),
                    )
                    .await
                    {
                        Ok(s) => s,
                        Err(_) => {
                            let _ = child.start_kill();
                            child.wait().await
                        }
                    };
                    let preview = spool.preview();
                    let summary = match status {
                        Ok(s) if s.success() => format!(
                            "completed; {} bytes of output.\n{}",
                            spool.written(),
                            preview,
                        ),
                        Ok(s) => format!(
                            "exited with {s}; {} bytes of output.\n{}",
                            spool.written(),
                            preview,
                        ),
                        Err(e) => format!("could not read exit status: {e}"),
                    };
                    runner_task.complete(job, summary.clone());
                    // Delta §11.4: enqueue into the batched delivery
                    // queue *and* push the immediate interrupt. The
                    // queue batches; the interrupt wakes an idle
                    // turn. A consumer that only wants one of the two
                    // can read the other path's behaviour, but both
                    // firing is the safe default: the queue survives
                    // a drop, the interrupt wakes a sleeping agent.
                    let epoch = delivery_task.lock().epoch();
                    delivery_task.lock().enqueue(
                        crate::async_delivery::AsyncResult {
                            job_id: job.0,
                            owner_id: holder.clone(),
                            kind: "shell".to_string(),
                            body: summary.clone(),
                            artifact: None,
                            epoch,
                        },
                    );
                    push_background_interrupt(
                        &steers_task,
                        &holder,
                        format!("background job {id_task} finished: {summary}"),
                    )
                    .await;
                });

                Some(id_str)
            },
        )
    }

    /// Delta §11.4: build the child-adoption hook. A command that
    /// outlived its deadline and is still running is moved here. The
    /// hook registers a job, spawns a task that drains both pipes
    /// into a spool, reaps the child, and delivers a completion
    /// notice — the same shape as `build_background_hook`, but the
    /// child and its pipes arrive already open rather than being
    /// spawned here.
    pub fn build_background_adopt_hook(&self) -> kod_tools::context::BackgroundAdoptHook {
        let steers = std::sync::Arc::clone(&self.steers);
        let runner = std::sync::Arc::clone(&self.background);
        let async_delivery = std::sync::Arc::clone(&self.async_delivery);

        kod_tools::context::BackgroundAdoptHook::new(move |detached| {
            let job = runner.allocate_id();
            let id_str = job.0.to_string();

            let dir = dirs::home_dir()
                .map(|h| h.join(".kod").join("background"))
                .unwrap_or_else(std::env::temp_dir);
            sweep_background_spools(&dir);
            let spool_path = dir.join(format!("{}.log", job.0));

            let mut spool = match crate::output_spool::OutputSpool::create(&spool_path) {
                Ok(s) => s,
                Err(e) => {
                    tracing::warn!(error = %e, "background spool create failed");
                    // Fall through: still register the job so the
                    // caller sees a running job, but without a spool
                    // the completion notice carries no preview.
                    return None;
                }
            };

            // Write the bytes the tool already read before handing
            // off. Without this the first chunk of output would be
            // lost.
            if !detached.prior_stdout.is_empty() {
                let _ = spool.append(&detached.prior_stdout);
            }
            if !detached.prior_stderr.is_empty() {
                let _ = spool.append(&detached.prior_stderr);
            }

            runner.register(
                job,
                crate::background::JobKind::Shell {
                    command: detached.command.chars().take(120).collect(),
                    spool: spool_path.clone(),
                },
            );

            let holder = detached.holder.clone();
            let steers_task = std::sync::Arc::clone(&steers);
            let runner_task = std::sync::Arc::clone(&runner);
            let delivery_task = std::sync::Arc::clone(&async_delivery);
            let id_task = id_str.clone();

            let mut child = detached.child;
            let mut stdout = detached.stdout;
            let mut stderr = detached.stderr;

            let runner_for_watch = std::sync::Arc::clone(&runner_task);
            runner_for_watch.spawn_guarded(job, async move {
                use tokio::io::AsyncReadExt as _;
                // Two buffers: the two `select!` arms must not borrow
                // the same slice mutably.
                let mut stdout_buf = [0u8; 4096];
                let mut stderr_buf = [0u8; 4096];
                // Drain both pipes concurrently. Either hitting EOF
                // or an error stops its read; the loop exits once
                // both are done.
                let mut stdout_done = false;
                let mut stderr_done = false;
                while !(stdout_done && stderr_done) {
                    tokio::select! {
                        r = stdout.read(&mut stdout_buf), if !stdout_done => {
                            match r {
                                Ok(0) | Err(_) => stdout_done = true,
                                Ok(n) => {
                                    let _ = spool.append(&stdout_buf[..n]);
                                }
                            }
                        }
                        r = stderr.read(&mut stderr_buf), if !stderr_done => {
                            match r {
                                Ok(0) | Err(_) => stderr_done = true,
                                Ok(n) => {
                                    let _ = spool.append(&stderr_buf[..n]);
                                }
                            }
                        }
                    }
                }
                let status =
                    match tokio::time::timeout(std::time::Duration::from_secs(60), child.wait())
                        .await
                    {
                        Ok(s) => s,
                        Err(_) => {
                            let _ = child.start_kill();
                            child.wait().await
                        }
                    };
                let preview = spool.preview();
                let summary = match status {
                    Ok(s) if s.success() => format!(
                        "completed (adopted); {} bytes of output.\n{}",
                        spool.written(),
                        preview,
                    ),
                    Ok(s) => format!(
                        "exited with {s} (adopted); {} bytes of output.\n{}",
                        spool.written(),
                        preview,
                    ),
                    Err(e) => format!("could not read exit status: {e}"),
                };
                runner_task.complete(job, summary.clone());
                let epoch = delivery_task.lock().epoch();
                delivery_task
                    .lock()
                    .enqueue(crate::async_delivery::AsyncResult {
                        job_id: job.0,
                        owner_id: holder.clone(),
                        kind: "shell".to_string(),
                        body: summary.clone(),
                        artifact: None,
                        epoch,
                    });
                push_background_interrupt(
                    &steers_task,
                    &holder,
                    format!("background job {id_task} finished: {summary}"),
                )
                .await;
            });

            Some(id_str)
        })
    }

    /// Install the hook on a per-call context.
    fn install_background_hook(&self, ctx: &mut ToolContext) {
        ctx.on_background_command = Some(self.build_background_hook());
        ctx.on_background_adopt = Some(self.build_background_adopt_hook());
    }

    async fn maybe_compact_for(&self, key: &str) -> usize {
        // Budget from the cached window; the `0` case (no registry
        // installed) makes `decide` return `None` and nothing runs.
        let (window, _max_out) = self.budget_hint.read().map(|g| *g).unwrap_or((0, 0));
        if window == 0 {
            return 0;
        }

        // Delta §2.4 (adoption): prefer the provider-anchored gauge.
        // Its value is the anchor's `prompt_tokens` (the exact number
        // the provider last charged for — system prompt, tools, and
        // messages) plus a char-arithmetic tail for messages appended
        // since. The `observed_usage` fallback below records only
        // `prompt + completion` from the last call, so it under-counts
        // by whatever the provider charged for the prompt *scaffolding*
        // (identity block, tool schemas, repo map). The gauge is the
        // more accurate of the two; the fallback exists for the first
        // turn of a session, when no anchor has been established yet,
        // and for a resumed session whose transcript was loaded from
        // disk without a preceding call.
        let used = match self.anchored_context_tokens(key).await {
            Some(n) => n,
            None => match self.observed_usage.read().await.get(key) {
                Some(&n) => n,
                None => {
                    let guard = self.history.read().await;
                    let chars: usize = guard
                        .get(key)
                        .map(|t| t.iter().map(|m| m.content.len()).sum())
                        .unwrap_or(0);
                    (chars / 4) as u64
                }
            },
        };

        let action = crate::compaction::decide(used, window as u64);
        if action == crate::compaction::Action::None {
            return 0;
        }

        // Delta §4.1: reduction before summarization. Try a mechanical
        // pass first — shake and prune are pure functions that do not
        // need the model, and a successful plan avoids the summary
        // call entirely. Only when the mechanical rungs find nothing
        // (or fall short) does the summary path run.
        if let Some(plan) = self.try_mechanical_compaction(key, window as u64).await {
            let affected = self.apply_compaction_plan(key, plan, window as u64).await;
            if affected > 0 {
                tracing::info!(
                    holder = key,
                    affected,
                    "mechanical compaction reduced the transcript before summarization",
                );
                return affected;
            }
        }

        // Compute the cut once, using a read guard, so both the
        // StartBackground and CompactNow paths agree on which
        // messages are in play.
        let (_cut, dropped_for_summary) = {
            let guard = self.history.read().await;
            let Some(turns) = guard.get(key) else {
                return 0;
            };
            let Some(cut) =
                crate::compaction::safe_cutoff(turns, crate::compaction::RECENT_TURNS_TO_KEEP)
            else {
                return 0;
            };
            if cut == 0 {
                // No safe cut drops anything: one enormous
                // un-splittable block. Leave it; the request is
                // rejected and the caller sees the real limit.
                return 0;
            }
            (cut, turns[..cut].to_vec())
        };

        if action == crate::compaction::Action::StartBackground {
            // Queue a background summary for the block that *would*
            // be dropped. It lands in `pending_summaries` for the turn
            // that later crosses the hard threshold.
            self.spawn_compaction_summary(key, dropped_for_summary)
                .await;
            return 0;
        }

        // CompactNow: take the pending summary if the background task
        // has finished, else fall back to the emergency text.
        let background_summary = self.pending_summaries.write().await.remove(key);

        let mut guard = self.history.write().await;
        let Some(turns) = guard.get_mut(key) else {
            return 0;
        };
        // Recompute against the live transcript; it may have grown
        // since the read-guard pass above.
        let Some(cut) =
            crate::compaction::safe_cutoff(turns, crate::compaction::RECENT_TURNS_TO_KEEP)
        else {
            return 0;
        };
        if cut == 0 {
            return 0;
        }
        // T3-H6: rotate-then-truncate keeps the retained suffix in place
        // rather than memmoving the whole tail forward like drain does.
        let dropped: Vec<kod_types::ChatMessage> = {
            let dropped: Vec<kod_types::ChatMessage> = turns[..cut].to_vec();
            turns.rotate_left(cut);
            turns.truncate(turns.len() - cut);
            dropped
        };
        let summary = background_summary
            .unwrap_or_else(|| crate::compaction::emergency_summary(&dropped, window as u64));
        let mut summary_msg = kod_types::ChatMessage::text(
            kod_types::MessageId::new(),
            kod_types::MessageRole::User,
            format!("## Previous Conversation Summary\n{summary}"),
            time::OffsetDateTime::now_utc(),
        );
        // Pinned so the render path never drops the summary for
        // budget reasons — losing it would lose the only record of
        // what was compacted away.
        summary_msg.metadata.pinned = true;
        turns.insert(0, summary_msg);

        tracing::info!(
            key,
            dropped = cut,
            used_tokens = used,
            window,
            "P2-a: emergency compaction ran",
        );
        cut
    }

    pub(crate) async fn prepare_turn(
        &self,
        key: &str,
        input: &str,
        retrieval_log_turn_id: Option<u64>,
        create_plan: bool,
    ) -> Result<TurnPreparation> {
        // A real turn is starting: the prewarm's speculation is over,
        // and the next fresh turn should warm again.
        self.reset_prewarm(key).await;

        let response = self
            .classify_and_filter(key, input, retrieval_log_turn_id)
            .await?;
        let (task_type, refined_skills) = self.refine_classification(key, input, &response).await;
        if create_plan {
            let options_for_plan = self.generation_defaults.read().await.to_options();
            if let Ok(provider) = self
                .resolve_provider_for_model_ref(&self.current_model.read().await.clone())
                .await
            {
                self.maybe_create_plan(key, input, task_type, &provider, &options_for_plan)
                    .await;
            }
        }
        // P2-a: compact before rendering, so the current turn pays
        // the smaller prompt. A no-op unless the observed usage has
        // crossed the hard threshold.
        let _ = self.maybe_compact_for(key).await;
        let history = self.render_history_for(key).await;
        self.remember_turn_for(key, true, input).await;
        let (alloc, definitions, pending) = self
            .build_budgeted_prompt(
                key,
                input,
                task_type,
                &history,
                response.memory_context.clone(),
            )
            .await?;
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
        // Delta 7.2: append any late LSP diagnostics a deferred
        // background pass queued since the last turn. A slow
        // language server's answer was not lost — it arrives with
        // the turn after the write that triggered it. Capped at 20
        // entries so a large error set does not dominate the prompt.
        let system_text = {
            let late = self.deferred_diagnostics.take(key);
            if late.is_empty() {
                system_text
            } else {
                let mut s = system_text;
                s.push_str("\n\n## LSP diagnostics (late)\n\n");
                s.push_str(&format!(
                    "{} diagnostic(s) arrived after the previous write:\n\n",
                    late.len(),
                ));
                for d in late.iter().take(20) {
                    s.push_str(&format!(
                        "{}:{}:{} {} {}\n",
                        d.file, d.line, d.column, d.severity, d.message,
                    ));
                }
                s
            }
        };
        let initial_messages: Vec<kod_types::ChatMessage> = {
            let guard = self.history.read().await;
            guard.get(key).cloned().unwrap_or_default()
        };
        Ok(TurnPreparation {
            response,
            task_type,
            refined_skills,
            alloc,
            definitions,
            pending,
            system_text,
            initial_messages,
        })
    }
}

// (The `which` helper moved to `kod_lsp::binary_for_path` when the
// LSP pool was introduced; `lsp_binary_for` now delegates there.)

/// Small owned snapshot of the parts of `KodEngine` that the baseline
/// refresh needs. Exists because `KodEngine::start` takes `&self` and
/// therefore cannot wrap `self` in an `Arc` for a background task.
struct BaselineRefresher {
    working_dir: std::path::PathBuf,
    check_baseline: Arc<RwLock<Option<Vec<kod_tools::check::Diagnostic>>>>,
}

impl BaselineRefresher {
    async fn refresh_check_baseline(&self) {
        // H-E13: run the check against the transcript's working
        // directory, not the engine's. A swarm agent
        // writing in its worktree was getting
        // diagnostics (and baseline overwrites) from
        // the main repo — the model saw errors it did
        // not introduce.
        match kod_tools::CheckTool::run_check(&self.working_dir, 30).await {
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
mod tests;

#[cfg(test)]
mod prop_tests;

#[cfg(test)]
mod diff_attachment_tests;

#[cfg(test)]
mod auto_check_tests;

#[cfg(test)]
mod coverage_decision_log;

#[cfg(test)]
mod coverage_retry_adjustment;

#[cfg(test)]
mod coverage_at_references;

#[cfg(test)]
mod coverage_mid_stream_switch;

#[cfg(test)]
mod coverage_offtrack_switch;

#[cfg(test)]
mod coverage_prompt_redaction;

#[cfg(test)]
mod coverage_tool_result_redaction;

#[cfg(test)]
mod coverage_tool_inventory_cache;

impl KodEngine {
    /// Restore the transcript for `key` from a session log.
    ///
    /// This is the P2 rehydration entry point: it reads the JSONL
    /// written by a prior process (via [`kod_core_state::session_log::read_session`]),
    /// rebuilds the tool calls that ran under `key` as prose turns,
    /// and appends them to the live history. The messages are
    /// User-role summaries so they survive the text-protocol
    /// `render_history` filter that deliberately skips Tool-role
    /// rows; see [`kod_core_state::session_log::rehydrate_prose_turns`] for
    /// the rationale and the structured sibling
    /// [`kod_core_state::session_log::rehydrate_turns`] for the future
    /// `CompletionRequest` path.
    ///
    /// Returns the number of messages appended. A missing or empty
    /// log yields `Ok(0)` and leaves history untouched.
    pub async fn rehydrate_from_log_for(&self, key: &str, path: &std::path::Path) -> Result<usize> {
        self.rehydrate_from_log_with(
            key,
            path,
            kod_core_state::session_log::RehydrationMode::Prose,
        )
        .await
    }

    /// Rehydrate with an explicit mode. `rehydrate_from_log_for` is
    /// the `Prose` convenience.
    pub async fn rehydrate_from_log_with(
        &self,
        key: &str,
        path: &std::path::Path,
        mode: kod_core_state::session_log::RehydrationMode,
    ) -> Result<usize> {
        let entries = kod_core_state::session_log::read_session(path)?;
        let messages = match mode {
            kod_core_state::session_log::RehydrationMode::Prose => {
                kod_core_state::session_log::rehydrate_prose_turns(&entries, key)
            }
            kod_core_state::session_log::RehydrationMode::Structured => {
                kod_core_state::session_log::rehydrate_turns(&entries, key)
            }
        };
        let count = messages.len();
        if count == 0 {
            return Ok(0);
        }
        let mut guard = self.history.write().await;
        guard.entry(key.to_string()).or_default().extend(messages);
        Ok(count)
    }
}

#[cfg(test)]
mod rehydrate_integration_tests;

#[cfg(test)]
mod p7_trust_filter_tests;

#[cfg(test)]
mod swarm_file_hook_tests;

#[cfg(test)]
mod rehydration_mode_tests;
