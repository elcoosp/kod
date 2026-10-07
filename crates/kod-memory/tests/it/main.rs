//! Consolidated integration tests for `kod-memory`.
//!
//! Each former top-level `tests/*.rs` file is now a submodule here.
//! Cargo builds exactly one test binary from `tests/it/main.rs`,
//! instead of one per file.

mod consolidation;
mod long_term;
mod short_term;
