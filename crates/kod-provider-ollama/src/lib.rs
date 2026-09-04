//! Ollama LLM provider implementation.

pub mod client;
pub mod generate;
pub mod provider;
pub mod streaming;
pub mod tools;

pub use client::{ModelInfo, OllamaClient, OllamaClientBuilder};
pub use generate::{GenerateOptions, GenerateRequest, GenerateResponse};
pub use provider::{GenerationOptionsExt, OllamaLlmProvider};
pub use streaming::{GenerationStream, StreamAccumulator, StreamEvent};
pub use tools::{format_tools_for_ollama, parse_tool_calls};
