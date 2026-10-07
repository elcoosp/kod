//! Consolidated integration tests for `kod-provider-openai`.
//!
//! Each former top-level `tests/*.rs` file is now a submodule here.
//! Cargo builds exactly one test binary from `tests/it/main.rs`,
//! instead of one per file.

mod contracts;
mod provider;
mod stream_retry;
