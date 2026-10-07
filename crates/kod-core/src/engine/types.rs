//! Shared round-scoped types for the engine.
//!
//! Extracted from `engine/mod.rs`. Plain data structures the round
//! loop and tool dispatch pass between each other. Struct fields are
//! `pub(crate)` so sibling engine modules can construct and read
//! them; enum variants inherit the enum's visibility.

use super::*;

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

pub(crate) struct RoundContext<'a> {
    pub(crate) system_text: &'a str,
    pub(crate) model_ref: &'a ModelRef,
    pub(crate) definitions: &'a [ToolDefinition],
    pub(crate) options: &'a GenerationOptions,
    pub(crate) holder: &'a str,
    /// Turn trace builder (Tier 1.4). `None` when no trace writer is
    /// installed — every trace call is a no-op in that case.
    pub(crate) trace: Option<&'a std::sync::Mutex<kod_core_state::trace::TurnTraceBuilder>>,
    /// Next endpoint in the chain after this one (P5.6). The
    /// streaming loop uses it for mid-stream switching: when Jev
    /// flags the reply off-track, `stream_round` swaps to this
    /// endpoint's stream in place. `None` for the collected path,
    /// the goal loop, and the last endpoint in a chain.
    pub(crate) fallback: Option<&'a ModelRef>,
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
    pub(crate) denied: std::collections::HashMap<usize, String>,
    pub(crate) need_approval: std::collections::HashSet<usize>,
    pub(crate) decisions: Vec<(usize, kod_config::PolicyDecision)>,
    pub(crate) policy: Option<std::sync::Arc<kod_config::PolicyEngine>>,
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
    pub(crate) text: String,
    pub(crate) calls: Vec<ToolCall>,
    pub(crate) usage: Option<kod_provider::TokenUsage>,
    pub(crate) retry_suggested: bool,
    pub(crate) speculations: Vec<Option<kod_core_tools::speculation::SpeculativeRead>>,
    /// The provider's `stop_reason` for the round, when reported
    /// (`"end_turn"`, `"max_tokens"`, `"tool_use"`, …). Threaded out
    /// so the run collector's stop-reason histogram is populated.
    pub(crate) stop_reason: Option<String>,
    /// M-13: a mid-stream error, carried ALONGSIDE the partial text
    /// and calls the loop assembled. The caller decides whether to
    /// surface it. Pre-fix, the error `return Err(err)`ed the partials
    /// away even though the H-E11 comment promised they survive.
    pub(crate) partial_error: Option<kod_error::KodError>,
}

pub(crate) struct ToolRound {
    pub(crate) results: Vec<ToolResult>,
    pub(crate) prompt_block: String,
    pub(crate) elapsed_ms: Vec<u64>,
    /// Structured form of this round: one assistant message carrying
    /// the calls (with ids), then one `Role::Tool` message per call
    /// linked by `tool_call_id` (design §2 AD-02). Appended to the
    /// per-transcript history by the caller. The text rendering of
    /// the transcript skips these for now (see `render_history_for`),
    /// so the pre-migration prompt bytes are unchanged; they are the
    /// payload the eventual `CompletionRequest` migration (AD-01)
    /// will ship on the wire.
    pub(crate) messages: Vec<kod_types::ChatMessage>,
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
