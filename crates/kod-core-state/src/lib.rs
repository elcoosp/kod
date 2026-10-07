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

/// Open a file for append with owner-only (0600) permissions on unix.
/// Creates any missing parent directories. Session and trace files
/// carry user prompts and tool-call arguments; the default 0644 is
/// world-readable on a shared host.
pub(crate) fn open_owner_only_append(
    path: &std::path::Path,
) -> Result<std::fs::File, kod_error::KodError> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(kod_error::KodError::Io)?;
    }
    let mut opts = std::fs::OpenOptions::new();
    opts.create(true).append(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    opts.open(path).map_err(kod_error::KodError::Io)
}
