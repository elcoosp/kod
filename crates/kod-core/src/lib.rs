//! Core engine for KOD - coordinates all subsystems.
//!
//! This crate integrates skills, memory, tools, and LLM providers
//! into a unified task routing and execution engine.

// Re-exports from kod-core-tools so `crate::<module>` keeps resolving.
pub use kod_core_tools::jfind;
pub use kod_core_tools::lsp_tools;



pub use kod_core_tools::tool_loop_guard;
pub use kod_core_tools::speculation;
pub use kod_core_tools::prewalk;


// Re-exports from kod-core-quality so `crate::<module>` keeps resolving.
pub use kod_core_quality::repomap;
pub use kod_core_quality::prune;
pub use kod_core_quality::shake;
pub use kod_core_quality::worktree;
pub use kod_core_quality::worktree_isolation_ownership;
pub use kod_core_quality::preflight;
pub use kod_core_quality::transcript_coherence;
pub use kod_core_quality::auto_thinking;
pub use kod_core_quality::snapcompact;

// Re-exports from kod-core-routing so `crate::<module>` keeps resolving.
pub use kod_core_routing::retry_strategy;
pub use kod_core_routing::provider_setup;
pub use kod_core_routing::config;
pub use kod_core_routing::context;
pub use kod_core_routing::context_engine;

// Re-exports from kod-core-state so `crate::<module>` keeps resolving.
pub use kod_core_state::session_log;
pub use kod_core_state::checkpoint;
pub use kod_core_state::trace;
pub use kod_core_state::trace_writer;
pub use kod_core_state::cost;
pub use kod_core_state::budget;
pub use kod_core_state::cache_journal;
pub use kod_core_state::cache_ledger;
pub use kod_core_state::cache_tracker;
pub use kod_core_state::context_gauge;
pub use kod_core_state::decisions;
pub use kod_core_state::plan;
pub use kod_core_state::state;
pub use kod_core_state::goals;
pub use kod_core_state::presence;
pub use kod_core_state::citations;
pub use kod_core_state::endpoint_health;
pub use kod_core_state::commit_lock;
pub use kod_core_state::deferred_diagnostics;
pub use kod_core_state::sensitivity;
pub use kod_core_state::steer;

pub mod async_delivery;
pub mod background;
pub mod compaction;
pub mod compaction_dispatcher;
pub mod doctor;
pub mod engine;
pub mod fixture;
pub mod hooks;
pub mod jev;
pub mod output_spool;
pub mod overnight;
pub mod pause_gate;
pub mod run_collector;
pub mod swarm_adapters;
pub mod swarm_runner;
pub mod tool_quota;
pub mod unexpected_stop;
pub use cost::{CostSnapshot, CostTracker, SoftWarningTrigger};
pub use decisions::{DecisionAuthor, DecisionKind, DecisionLog, DecisionRecord};
pub use engine::KodEngine;
pub use fixture::{
    Fixture, RequestSummary, ResponseFixture, RoundFixture, ToolResultFixture, diff_rounds,
};
pub use jev::{Decision, DecisionSource, JevClient, JevError};
pub use plan::{Plan, PlanStatus, PlanStep, PlanUpdate};
pub use retry_strategy::{RetryAction, TurnFailure, choose_action};
pub use state::{EngineState, STATE_SCHEMA_VERSION, StateStore};
pub use tool_quota::{QuotaVerdict, ToolCounts, check as check_quota};
pub use trace::{RoundKind, ToolOutcomeKind, TurnId, TurnOutcome, TurnTrace, TurnTraceBuilder};
pub use trace_writer::{TraceWriter, read_traces};

/// Build a `JevClient` from config and install it on `engine`.
///
/// Called by the CLI and TUI at startup. Returns `Ok(true)` when
/// a client was installed, `Ok(false)` when the config has
/// `enabled = false` (the default), and `Err` only when the
/// config is enabled but the client could not be built — a
/// misconfigured integration should be a loud failure at
/// startup, not a silent no-op.
pub fn install_jev_from_config(
    engine: &KodEngine,
    config: &kod_config::JevConfig,
) -> Result<bool, JevError> {
    let client = JevClient::from_config(config)?;
    if let Some(c) = client {
        engine.set_jev_client(c);
        Ok(true)
    } else {
        Ok(false)
    }
}
pub use memory_tools::{MemorySaveTool, MemorySearchTool};
pub use provider_setup::build_registry;
pub use router::{RouterConfig, TaskResponse, TaskRouter, TaskType};
pub use swarm_runner::{
    AgentOutcome, AgentResult, Subtask, SwarmEvent, SwarmResponse, SwarmRunner,
    WorktreeMergeOutcome,
};
pub use worktree::{MergeReport, WorktreeInfo, WorktreeManager};
pub mod router;
pub mod mcp_adapters;
pub mod memory_handler;
pub mod memory_tools;
pub mod advisor_tools;
pub mod hub_tool;
pub mod jfind_tool;