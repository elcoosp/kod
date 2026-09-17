//! Core engine for KOD - coordinates all subsystems.
//!
//! This crate integrates skills, memory, tools, and LLM providers
//! into a unified task routing and execution engine.

pub mod budget;
pub mod checkpoint;
pub mod citations;
pub mod config;
pub mod context;
pub mod doctor;
pub mod engine;
pub mod hooks;
pub mod lsp_tools;
pub mod mcp_adapters;
pub mod memory_tools;
pub mod provider_setup;
pub mod repomap;
pub mod router;
pub mod session_log;
pub mod swarm_runner;
pub mod worktree;

pub use engine::KodEngine;
pub use memory_tools::{MemorySaveTool, MemorySearchTool};
pub use provider_setup::build_registry;
pub use router::{RouterConfig, TaskResponse, TaskRouter, TaskType};
pub use worktree::{MergeReport, WorktreeInfo, WorktreeManager};
pub use swarm_runner::{
    AgentOutcome, AgentResult, Subtask, SwarmEvent, SwarmResponse, SwarmRunner,
    WorktreeMergeOutcome,
};
