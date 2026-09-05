//! Provider traits for LLM integration.

use crate::{GenerationResponse, StreamChunk, response_chunks};
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

    /// Stream a completion with tool calling support: live `Text` chunks
    /// plus `ToolCallStart`/`ToolCallDelta`/`Done` framing.
    ///
    /// The default implementation collects [`generate_with_tools`] and
    /// replays it as chunks (no live tokens, but every implementor works).
    /// Providers with SSE support should override for real token streaming.
    fn stream_with_tools<'a>(
        &'a self,
        prompt: &'a str,
        tools: &'a [ToolDefinition],
        options: &'a GenerationOptions,
    ) -> Pin<Box<dyn Stream<Item = Result<StreamChunk>> + Send + 'a>> {
        Box::pin(futures::stream::unfold(
            StreamReplay::Fetch,
            move |mut state| async move {
                loop {
                    match state {
                        StreamReplay::Fetch => {
                            match self.generate_with_tools(prompt, tools, options).await {
                                Ok(response) => {
                                    state =
                                        StreamReplay::Emit(response_chunks(response).into_iter());
                                }
                                Err(e) => return Some((Err(e), StreamReplay::End)),
                            }
                        }
                        StreamReplay::Emit(mut chunks) => match chunks.next() {
                            Some(chunk) => {
                                state = StreamReplay::Emit(chunks);
                                return Some((Ok(chunk), state));
                            }
                            None => return None,
                        },
                        StreamReplay::End => return None,
                    }
                }
            },
        ))
    }
}

/// Internal state for [`LlmProvider::stream_with_tools`]'s default replay.
enum StreamReplay {
    Fetch,
    Emit(std::vec::IntoIter<StreamChunk>),
    End,
}

#[derive(Debug, Clone, Default)]
pub struct GenerationOptions {
    pub model: Option<String>,
    pub max_tokens: Option<usize>,
    pub temperature: Option<f32>,
    pub top_p: Option<f32>,
    pub stop_sequences: Vec<String>,
}
