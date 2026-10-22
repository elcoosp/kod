//! Consolidated integration tests for `kod-memory`.
//!
//! Each former top-level `tests/*.rs` file is now a submodule here.
//! Cargo builds exactly one test binary from `tests/it/main.rs`,
//! instead of one per file.

mod consolidate_panic;
mod consolidation;
mod embed_safe_lint;
mod embedder_untested;
mod embedder_panic;
mod long_term;
mod long_term_untested;
mod manager_untested;
mod short_term;
mod pending_memory_untested;
