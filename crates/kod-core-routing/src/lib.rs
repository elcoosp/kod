//! kod-core-routing: task routing, retry policy, provider
//! construction, and the engine context. Extracted from kod-core
//! so the engine and CLI can depend on a smaller routing surface.

#![allow(clippy::all)]

pub mod retry_strategy;
pub mod provider_setup;
pub mod config;
pub mod context;
pub mod context_engine;
