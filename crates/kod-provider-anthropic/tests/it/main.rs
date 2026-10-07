//! Consolidated integration tests for `kod-provider-anthropic`.
//!
//! Each former top-level `tests/*.rs` file is now a submodule here.
//! Cargo builds exactly one test binary from `tests/it/main.rs`,
//! instead of one per file.

mod complete_retry;
mod contracts;
mod live;
mod prompt_cache_stability;
mod stream_retry;
