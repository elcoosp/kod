//! kod-core-state: persistent state, journals, ledgers, and cost
//! accounting. Extracted from kod-core to allow the engine, serve,
//! and swarm layers to compile against a smaller state surface.
//!
//! The parent crate (kod-core) re-exports every module here, so
//! existing `crate::session_log`-style paths keep resolving
//! unchanged.

#![allow(clippy::all)]

pub mod budget;
pub mod cache_journal;
pub mod cache_ledger;
pub mod cache_tracker;
pub mod checkpoint;
pub mod citations;
pub mod commit_lock;
pub mod context_gauge;
pub mod cost;
pub mod decisions;
pub mod deferred_diagnostics;
pub mod endpoint_health;
pub mod goals;
pub mod plan;
pub mod presence;
pub mod sensitivity;
pub mod session_log;
pub mod socket_path;
pub mod state;
pub mod steer;
pub mod trace;
pub mod trace_writer;
