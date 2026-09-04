//! LLM provider abstraction layer.
//!
//! This crate defines the traits and types that all LLM providers must implement.

pub mod traits;
pub mod types;

pub use traits::{GenerationOptions, LlmProvider};
pub use types::*;
