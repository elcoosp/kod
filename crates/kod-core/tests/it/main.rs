//! Consolidated integration tests for `kod-core`.
//!
//! Each former top-level `tests/*.rs` file is now a submodule here.
//! Cargo builds exactly one test binary from `tests/it/main.rs`,
//! instead of one per file.

pub mod common;
mod background_shell;
mod capability_routing;
mod characterization_history;
mod characterization_prompts;
mod compaction_admission;
mod config_integration;
mod context;
mod context_gauge_adoption;
mod engine;
mod fallback_chain;
mod fallback_no_duplicate_rounds;
mod git_tools;
mod goal_loop_accumulates;
mod golden_prefix;
mod handoff_compaction;
mod integration;
mod mechanical_compaction;
mod metrics;
mod native_compaction_replay;
mod pause_gate_integration;
mod policy_gate;
mod prompt_plan;
mod registry_wiring;
mod router;
mod sandbox_status;
mod secret_placeholder_integration;
mod serve_roundtrip;
mod session_log_compat;
mod steers_reach_the_provider;
mod structured_prompt_shape;
mod structured_provider_path;
mod swarm_failed_dep_gates;
mod swarm_from_config;
mod swarm_runner;
mod tool_loop;
mod tool_loop_guard_integration;
mod transcript_working_dir;
