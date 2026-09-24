//! Core engine for KOD - coordinates all subsystems.
//!
//! This crate integrates skills, memory, tools, and LLM providers
//! into a unified task routing and execution engine.

pub mod acp;
pub mod auto_thinking;
pub mod budget;
pub mod checkpoint;
pub mod citations;
pub mod cache_ledger;
pub mod cache_journal;
pub mod cache_tracker;
pub mod context_engine;
pub mod context_gauge;
pub mod background;
pub mod endpoint_health;
pub mod sensitivity;
pub mod compaction;
pub mod compaction_dispatcher;
pub mod config;
pub mod context;
pub mod cost;
pub mod decisions;
pub mod doctor;
pub mod advisor_tools;
pub mod engine;
pub mod fixture;
pub mod hooks;
pub mod jev;
pub mod lsp_tools;
pub mod mcp_adapters;
pub mod memory_handler;
pub mod memory_tools;
pub mod overnight;
pub mod output_spool;
pub mod pause_gate;
pub mod plan;
pub mod provider_setup;
pub mod repomap;
pub mod retry_strategy;
pub mod router;
pub mod serve;
pub mod session_log;
pub mod shake;
pub mod state;
pub mod steer;
pub mod swarm_adapters;
pub mod swarm_runner;
pub mod tool_loop_guard;
pub mod tool_quota;
pub mod trace;
pub mod trace_writer;
pub mod transcript_coherence;
pub mod unexpected_stop;
pub mod worktree;

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
pub mod preflight;
pub mod presence;
pub mod prune;
pub mod commit_lock;
