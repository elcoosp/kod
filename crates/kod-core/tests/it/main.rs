//! Consolidated integration tests for `kod-core`.
//!
//! Each former top-level `tests/*.rs` file is now a submodule here.
//! Cargo builds exactly one test binary from `tests/it/main.rs`,
//! instead of one per file.

mod background_shell;
mod capability_routing;
mod characterization_history;
#[cfg(unix)]
mod characterization_prompts;
pub mod common;
mod compaction_admission;
mod config_integration;
mod context;
mod context_gauge_adoption;
mod engine;
mod fallback_chain;
mod fallback_no_duplicate_rounds;
mod format_call_brief_cjk;
mod gauge_invalidation_on_compaction;
mod git_tools;
mod goal_loop_accumulates;
mod golden_prefix;
mod handoff_compaction;
mod integration;
mod mcp_roundtrip;
mod mechanical_compaction;
mod metrics;
mod native_compaction_replay;
mod pause_gate_integration;
mod policy_gate;
mod prompt_plan;
mod provider_seam;
mod registry_wiring;
mod router;
mod sandbox_status;
mod secret_placeholder_integration;
#[cfg(unix)]
mod serve_roundtrip;
mod session_log_compat;
mod steers_reach_the_provider;
mod stream_cancel;
mod stream_tool_args_cap;
mod structured_prompt_shape;
mod structured_provider_path;
mod swarm_failed_dep_gates;
mod swarm_from_config;
mod swarm_mock_e2e;
mod swarm_runner;
mod tool_loop;
mod tool_loop_guard_integration;
mod transcript_working_dir;
mod truncate_helper_sites;
mod vault_secondary_paths;
