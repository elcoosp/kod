//! LLM provider abstraction layer.
//!
//! This crate defines the traits and types that all LLM providers must implement.

pub mod registry;
pub mod request;
pub mod retry;
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
pub use traits::{GenerationOptions, LlmProvider};
pub use types::*;
