//! Core engine for KOD - coordinates all subsystems.
//!
//! This crate integrates skills, memory, tools, swarm, and LLM providers
//! into a unified task routing and execution engine.

pub mod config;
pub mod context;
pub mod engine;
pub mod router;

pub use config::EngineConfig;
pub use context::{EngineContext, EngineContextBuilder};
pub use engine::KodEngine;
pub use router::{RouterConfig, TaskResponse, TaskRouter, TaskType};
