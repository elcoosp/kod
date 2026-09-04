//! LlmProvider trait implementation for Ollama.

use crate::{client::OllamaClient, generate::GenerateRequest, streaming};
use async_trait::async_trait;
use kod_error::Result;
use kod_provider::{GenerationOptions, GenerationResponse, LlmProvider, StreamChunk};
use kod_types::ToolDefinition;
use futures::Stream;

/// Adapter that implements LlmProvider for Ollama
pub struct OllamaLlmProvider {
    client: OllamaClient,
}

impl OllamaLlmProvider {
    pub fn new(base_url: impl Into<String>) -> Self {
        Self {
            client: OllamaClient::new(base_url),
        }
    }

    pub fn with_model(mut self, model: impl Into<String>) -> Self {
        self.client = self.client.with_model(model);
        self
    }
}

// Extension trait for GenerationOptions
pub trait GenerationOptionsExt {
    fn to_ollama_options(&self) -> crate::generate::GenerateOptions;
}

impl GenerationOptionsExt for GenerationOptions {
    fn to_ollama_options(&self) -> crate::generate::GenerateOptions {
        crate::generate::GenerateOptions {
            temperature: self.temperature,
            top_p: self.top_p,
            num_predict: self.max_tokens.map(|t| t as i32),
            stop: if self.stop_sequences.is_empty() {
                None
            } else {
                Some(self.stop_sequences.clone())
            },
            ..Default::default()
        }
    }
}

#[async_trait]
impl LlmProvider for OllamaLlmProvider {
    fn name(&self) -> &str {
        "ollama"
    }

    async fn list_models(&self) -> Result<Vec<String>> {
        let models = self.client.list_models().await?;
        Ok(models.into_iter().map(|m| m.name).collect())
    }

    async fn generate(&self, prompt: &str, options: &GenerationOptions) -> Result<String> {
        let model = options.model.as_deref()
            .unwrap_or(self.client.default_model());

        let request = GenerateRequest::new(model, prompt);

        let ollama_options = options.to_ollama_options();
        let request = request.with_options(ollama_options);

        let response = self.client.generate(&request).await?;
        Ok(response.response)
    }

    async fn generate_with_tools(
        &self,
        prompt: &str,
        tools: &[ToolDefinition],
        options: &GenerationOptions,
    ) -> Result<GenerationResponse> {
        let model = options.model.as_deref()
            .unwrap_or(self.client.default_model());

        let request = GenerateRequest::new(model, prompt);
        let (text, tool_calls) = self.client.generate_with_tools(&request, tools).await?;

        if tool_calls.is_empty() {
            Ok(GenerationResponse::Text { content: text })
        } else if text.is_empty() {
            Ok(GenerationResponse::ToolCalls { calls: tool_calls })
        } else {
            Ok(GenerationResponse::Mixed {
                content: text,
                calls: tool_calls,
            })
        }
    }

    fn stream(
        &self,
        prompt: &str,
        options: &GenerationOptions,
    ) -> std::pin::Pin<Box<dyn Stream<Item = Result<StreamChunk>> + Send + '_>> {
        let model = options.model.clone()
            .unwrap_or_else(|| self.client.default_model().to_string());

        let client = &self.client;
        let request_model = model.clone();
        let request_prompt = prompt.to_string();

        Box::pin(async_stream::stream! {
            let request = GenerateRequest::new(&request_model, &request_prompt);

            match client.generate_stream(&request).await {
                Ok(mut stream) => {
                    use futures::StreamExt;

                    while let Some(event_result) = stream.next().await {
                        match event_result {
                            Ok(event) => {
                                match event {
                                    streaming::StreamEvent::Chunk { response, .. } => {
                                        if !response.is_empty() {
                                            yield Ok(StreamChunk::Text(response));
                                        }
                                    }
                                    streaming::StreamEvent::Done { .. } => {
                                        yield Ok(StreamChunk::Done);
                                        break;
                                    }
                                }
                            }
                            Err(e) => {
                                yield Err(e);
                                break;
                            }
                        }
                    }
                }
                Err(e) => {
                    yield Err(e);
                }
            }
        })
    }
}
