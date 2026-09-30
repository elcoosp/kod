//! Anthropic Messages API provider.
//!
//! # Two wire paths
//!
//! The modern paths — [`AnthropicProvider::complete`], the streaming
//! loop, and native compaction — build the request body in the local
//! [`wire`] module. That module exists because `adk-model`'s Anthropic
//! client takes a single `system: String` and flattens a multi-segment
//! system prompt before the wire call, which is fatal to prompt
//! caching: Anthropic's `cache_control: {"type": "ephemeral"}` is a
//! marker on a *block* inside a system array, not on a top-level
//! string. [`wire::system_blocks`] emits the structured form and places
//! the breakpoint on the last cacheable segment.
//!
//! The legacy text paths (`generate` / `generate_with_tools`) still go
//! through `adk_model::anthropic::Anthropic`; they predate the
//! structured request and carry no cache hints.
//!
//! # What the wrapper adds over a bare wire body
//!
//! - The `LlmProvider` trait surface (name, `list_models`, generate,
//!   `generate_with_tools`, stream, capabilities).
//! - The `ProviderCapabilities` matrix, so the router knows Anthropic
//!   supports explicit prompt caching (`cache_control`) and the TUI
//!   can display pricing when configured.
//! - An error message naming the Anthropic endpoint instead of a
//!   generic HTTP failure, matching the OpenAI provider's diagnostic
//!   quality.
//! - Native-compaction support (`compact-2026-01-12` beta): the
//!   `native_compact` request/response, built in [`wire`] like the
//!   other structured paths.
//!
//! See ADR-05 for why this crate is a native wire rather than a thin
//! `adk-model` wrapper.

mod provider;
pub mod wire;

pub use provider::AnthropicProvider;
