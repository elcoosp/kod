//! Consolidated integration tests for `kod-tui`.
//!
//! Each former top-level `tests/*.rs` file is now a submodule here.
//! Cargo builds exactly one test binary from `tests/it/main.rs`,
//! instead of one per file.

mod app;
mod event;
mod main_loop;
mod no_raw_terminal_writes;
mod swarm_e2e;
mod ui;
