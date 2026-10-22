//! Consolidated integration tests for `kod-cli`.
//!
//! Each former top-level `tests/*.rs` file is now a submodule here.
//! Cargo builds exactly one test binary from `tests/it/main.rs`,
//! instead of one per file.

mod acp_handshake;
mod admin_config_only;
mod admin_models_mock;
mod agent_mock;
pub mod common;
mod config_migrate;
mod config_remaining_paths;
mod fixtures_cli;
mod integration_tests;
mod memory_admin_paths;
mod memory_paths;
mod observability_json;
mod observability_paths;
mod observability_render;
mod policy_explain;
mod prompt_mock;
mod prompt_remote_paths;
mod replay;
mod skills_config_cli;
mod skills_missing_paths;
mod skills_success_paths;
mod swarm_cli_mock;
mod truncate_helper_sites;
#[cfg(unix)]
mod tui_e2e;
