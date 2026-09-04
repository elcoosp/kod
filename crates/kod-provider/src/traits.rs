//! Provider traits for LLM integration.

use crate::{GenerationResponse, StreamChunk};
use async_trait::async_trait;
use futures::Stream;
use kod_error::Result;
use kod_types::ToolDefinition;
use std::pin::Pin;

#[async_trait]
pub trait LlmProvider: Send + Sync {
    /// Get the provider name
    fn name(&self) -> &str;

    /// List available models
    async fn list_models(&self) -> Result<Vec<String>>;

    /// Generate a completion without tools
    async fn generate(&self, prompt: &str, options: &GenerationOptions) -> Result<String>;

    /// Generate a completion with tool calling support
    async fn generate_with_tools(
        &self,
        prompt: &str,
        tools: &[ToolDefinition],
        options: &GenerationOptions,
    ) -> Result<GenerationResponse>;

    /// Generate a streaming completion
    fn stream(
        &self,
        prompt: &str,
        options: &GenerationOptions,
    ) -> Pin<Box<dyn Stream<Item = Result<StreamChunk>> + Send + '_>>;
}

#[derive(Debug, Clone, Default)]
pub struct GenerationOptions {
    pub model: Option<String>,
    pub max_tokens: Option<usize>,
    pub temperature: Option<f32>,
    pub top_p: Option<f32>,
    pub stop_sequences: Vec<String>,
}
