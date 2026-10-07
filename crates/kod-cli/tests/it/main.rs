//! Consolidated integration tests for `kod-cli`.
//!
//! Each former top-level `tests/*.rs` file is now a submodule here.
//! Cargo builds exactly one test binary from `tests/it/main.rs`,
//! instead of one per file.

pub mod common;
mod acp_handshake;
mod config_migrate;
mod integration_tests;
mod policy_explain;
mod replay;
