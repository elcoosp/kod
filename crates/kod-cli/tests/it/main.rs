//! Consolidated integration tests for `kod-cli`.
//!
//! Each former top-level `tests/*.rs` file is now a submodule here.
//! Cargo builds exactly one test binary from `tests/it/main.rs`,
//! instead of one per file.

mod acp_handshake;
mod admin_config_only;
mod admin_export_config;
mod admin_init_profile_policy;
mod admin_models_mock;
mod agent_mock;
#[cfg(unix)]
mod chat_remote_guard;
pub mod common;
mod config_migrate;
mod config_remaining_paths;
mod doctor_fix_paths;
mod fixture_replay_serve;
mod fixtures_cli;
mod fixture_save_traces;
mod integration_tests;
mod jev_edit_search;
mod map_root_override;
mod memory_admin_paths;
mod memory_manager_paths;
mod memory_paths;
mod observability_json;
mod observability_paths;
mod observability_render;
mod observability_tune_migrate;
mod policy_explain;
mod prompt_mock;
mod prompt_remote_daemon;
mod prompt_remote_paths;
mod replay;
mod sessions_dir_override;
mod sessions_paths;
mod skills_config_cli;
mod skills_missing_paths;
mod skills_success_paths;
mod swarm_cli_mock;
mod truncate_helper_sites;
#[cfg(unix)]
mod tui_e2e;
mod update_mock;
