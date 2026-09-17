//! LLM provider abstraction layer.
//!
//! This crate defines the traits and types that all LLM providers must implement.

pub mod retry;
pub mod request;
pub mod traits;
pub mod types;

pub use request::{
    CompletionRequest, ModelPricing, ModelRef, PromptCacheKind, ProviderCapabilities,
    SystemPrompt, SystemSegment,
};
pub use traits::{GenerationOptions, LlmProvider};
pub use types::*;
