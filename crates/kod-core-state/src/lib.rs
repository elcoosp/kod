//! kod-core-state: persistent state, journals, ledgers, and cost
//! accounting. Extracted from kod-core to allow the engine, serve,
//! and swarm layers to compile against a smaller state surface.
//!
//! The parent crate (kod-core) re-exports every module here, so
//! existing `crate::session_log`-style paths keep resolving
//! unchanged.

#![allow(clippy::all)]

pub mod session_log;
pub mod checkpoint;
pub mod trace;
pub mod trace_writer;
pub mod cost;
pub mod budget;
pub mod cache_journal;
pub mod cache_ledger;
pub mod cache_tracker;
pub mod context_gauge;
pub mod decisions;
pub mod plan;
pub mod state;
pub mod goals;
pub mod presence;
pub mod citations;
pub mod endpoint_health;
pub mod commit_lock;
pub mod deferred_diagnostics;
pub mod sensitivity;
pub mod steer;
pub mod socket_path;
