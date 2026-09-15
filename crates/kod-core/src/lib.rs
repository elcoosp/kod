//! Core engine for KOD - coordinates all subsystems.
//!
//! This crate integrates skills, memory, tools, and LLM providers
//! into a unified task routing and execution engine.

pub mod config;
pub mod context;
pub mod engine;
pub mod repomap;
pub mod router;
pub mod session_log;
pub mod swarm_runner;

pub use config::EngineConfig;
pub use context::{EngineContext, EngineContextBuilder};
pub use engine::KodEngine;
pub use router::{RouterConfig, TaskResponse, TaskRouter, TaskType};
pub use swarm_runner::{
    AgentOutcome, AgentResult, Subtask, SwarmEvent, SwarmResponse, SwarmRunner,
};
