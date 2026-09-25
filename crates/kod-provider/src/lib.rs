//! LLM provider abstraction layer.
//!
//! This crate defines the traits and types that all LLM providers must implement.

pub mod concurrency;
pub mod native_compaction;
pub mod judgment;
pub mod registry;
pub mod replay;
pub mod request;
pub mod retry;
pub mod retry_safety;
pub mod stream_guard;
pub mod traits;
pub mod types;

/// Provider contract suite (§11.2). Enabled under the `testkit`
/// feature by each concrete provider crate.
#[cfg(feature = "testkit")]
pub mod testkit;

pub use registry::{EndpointEntry, ProviderRegistry};
pub use request::{
    CompletionRequest, ModelPricing, ModelRef, PromptCacheKind, ProviderCapabilities, SystemPrompt,
    SystemSegment,
};
pub use native_compaction::NativeCompaction;
pub use traits::{GenerationOptions, LlmProvider, ModelInfo};
pub use types::*;
pub mod effort;
pub mod validation;
pub mod structured;
pub use structured::{run_structured, extract_json};
