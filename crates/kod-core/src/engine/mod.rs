//! Main KOD engine - orchestrates all subsystems.
//!
//! Coordinates the task router, LLM providers, skills, and memory
//! to process user requests end-to-end.

mod agent_loop;
mod approval;
mod at_refs;
mod compaction;
mod constants;
mod control;
mod hook;
mod jev;
mod jev_advisor;
mod lifecycle;
mod lsp;
mod markers;
mod memory;
mod plans;
mod policy;
mod prepare;
mod prewarm;
mod process;
pub mod render;
mod routing;
mod settings;
mod state;
mod swarm;
mod tool_dispatch;
mod transcript;
mod types;

pub use approval::*;
pub use at_refs::*;
pub(crate) use hook::BaselineRefresher;
pub(crate) use constants::*;
pub use markers::*;
pub(crate) use policy::LearnedAllow;
pub use render::*;
pub(crate) use render::strip_conversation_tail;
pub use settings::GenerationDefaults;
pub(crate) use types::*;

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

/// Marker announcing an automatic rate-limit wait on the streaming
/// chunk channel: `\0kod-rate-limit:<secs>\0<attempt>\0<max>`.
#[cfg(test)]
mod rate_limit_marker_tests;

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

// (The `which` helper moved to `kod_lsp::binary_for_path` when the
// LSP pool was introduced; `lsp_binary_for` now delegates there.)

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

#[cfg(test)]
mod rehydrate_integration_tests;

#[cfg(test)]
mod p7_trust_filter_tests;

#[cfg(test)]
mod swarm_file_hook_tests;

#[cfg(test)]
mod rehydration_mode_tests;
