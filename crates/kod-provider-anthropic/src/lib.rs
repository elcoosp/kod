//! Anthropic Messages API provider, backed by `adk-model`.
//!
//! The spike (ADR-04) confirmed `adk-model 2.2` ships a native
//! Anthropic client, so this provider is a wrapper — the same shape as
//! `kod-provider-openai::OpenAICompatProvider` around
//! `adk_model::openai_compatible::OpenAICompatible`. The wire format,
//! SSE parsing, and tool-call assembly all come from `adk-model`.
//!
//! # What the wrapper adds
//!
//! - The `LlmProvider` trait surface (name, list_models, generate,
//!   generate_with_tools, stream, capabilities).
//! - The `ProviderCapabilities` matrix, so the router knows Anthropic
//!   supports explicit prompt caching (`cache_control`) and the TUI
//!   can display pricing when configured.
//! - An error message naming the Anthropic endpoint instead of a
//!   generic HTTP failure, matching the OpenAI provider's diagnostic
//!   quality.
//!
//! # What the wrapper does NOT do (yet)
//!
//! `cache_control` is not placed on the system prompt's cacheable
//! segments. `adk-model`'s Anthropic client takes a single `system:
//! String`; the ADK wrapper flattens multi-segment prompts before the
//! wire call. Emitting `cache_control: {"type": "ephemeral"}` on the
//! last cacheable segment requires either an `adk-model` API that
//! accepts segments, or a local wire module. That work is scoped for
//! A5b — the D1 registry and the honest capability declaration ship
//! today.

mod provider;
pub mod wire;

pub use provider::AnthropicProvider;
