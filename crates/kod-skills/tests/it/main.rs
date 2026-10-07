//! Consolidated integration tests for `kod-skills`.
//!
//! Each former top-level `tests/*.rs` file is now a submodule here.
//! Cargo builds exactly one test binary from `tests/it/main.rs`,
//! instead of one per file.

mod examples;
mod integration;
mod loader;
mod matcher;
mod parser;
mod watcher;
