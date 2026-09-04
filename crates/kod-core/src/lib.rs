//! Core engine for KOD - coordinates all subsystems.
//!
//! This crate integrates skills, memory, tools, swarm, and LLM providers
//! into a unified task routing and execution engine.

pub mod router;
pub mod engine;
pub mod context;
pub mod config;

pub use router::{RouterConfig, TaskRouter, TaskType, TaskResponse};
pub use engine::KodEngine;
pub use context::{EngineContext, EngineContextBuilder};
pub use config::EngineConfig;
