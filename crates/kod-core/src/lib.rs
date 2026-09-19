//! Core engine for KOD - coordinates all subsystems.
//!
//! This crate integrates skills, memory, tools, and LLM providers
//! into a unified task routing and execution engine.

pub mod acp;
pub mod budget;
pub mod checkpoint;
pub mod citations;
pub mod config;
pub mod cost;
pub mod context;
pub mod doctor;
pub mod engine;
pub mod hooks;
pub mod jev;
pub mod lsp_tools;
pub mod mcp_adapters;
pub mod memory_tools;
pub mod provider_setup;
pub mod repomap;
pub mod router;
pub mod serve;
pub mod trace;
pub mod session_log;
pub mod swarm_adapters;
pub mod swarm_runner;
pub mod worktree;

pub use engine::KodEngine;
pub use trace::{RoundKind, ToolOutcomeKind, TurnId, TurnOutcome, TurnTrace, TurnTraceBuilder};
pub use cost::{CostSnapshot, CostTracker, SoftWarningTrigger};
pub use jev::{Decision, DecisionSource, JevClient, JevError};

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
