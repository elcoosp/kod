//! Core engine for KOD - coordinates all subsystems.
//!
//! This crate integrates skills, memory, tools, and LLM providers
//! into a unified task routing and execution engine.

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
pub mod pause_gate;
pub mod run_collector;
pub mod swarm_adapters;
pub mod swarm_runner;
pub mod tool_quota;
pub mod unexpected_stop;
pub use kod_core_state::cost::{CostSnapshot, CostTracker, SoftWarningTrigger};
pub use kod_core_state::decisions::{DecisionAuthor, DecisionKind, DecisionLog, DecisionRecord};
pub use engine::KodEngine;
pub use fixture::{
    Fixture, RequestSummary, ResponseFixture, RoundFixture, ToolResultFixture, diff_rounds,
};
pub use jev::{Decision, DecisionSource, JevClient, JevError};
pub use kod_core_state::plan::{Plan, PlanStatus, PlanStep, PlanUpdate};
pub use kod_core_routing::retry_strategy::{RetryAction, TurnFailure, choose_action};
pub use kod_core_state::state::{EngineState, STATE_SCHEMA_VERSION, StateStore};
pub use tool_quota::{QuotaVerdict, ToolCounts, check as check_quota};
pub use kod_core_state::trace::{RoundKind, ToolOutcomeKind, TurnId, TurnOutcome, TurnTrace, TurnTraceBuilder};
pub use kod_core_state::trace_writer::{TraceWriter, read_traces};

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
pub use kod_core_routing::provider_setup::build_registry;
pub use router::{RouterConfig, TaskResponse, TaskRouter, TaskType};
pub use swarm_runner::{
    AgentOutcome, AgentResult, Subtask, SwarmEvent, SwarmResponse, SwarmRunner,
    WorktreeMergeOutcome,
};
pub use kod_core_quality::worktree::{MergeReport, WorktreeInfo, WorktreeManager};
pub mod router;
pub mod mcp_adapters;
pub mod memory_handler;
pub mod memory_tools;
pub mod advisor_tools;
pub mod hub_tool;
pub mod jfind_tool;
pub mod overnight;