//! Consolidated integration tests for `kod-tools`.
//!
//! Each former top-level `tests/*.rs` file is now a submodule here.
//! Cargo builds exactly one test binary from `tests/it/main.rs`,
//! instead of one per file.

mod minimizer_integration;
mod registry;
mod sandbox_containment;
mod tools;
