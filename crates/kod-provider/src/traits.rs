//! Provider traits for LLM integration.

use crate::request::{CompletionRequest, ProviderCapabilities};
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
    /// Default capabilities. Providers that care — Anthropic for
    /// explicit cache support, OpenAI for pricing — override this.
    /// The conservative default disables nothing that works (tools on,
    /// streaming_tools off), which is the safe side of the trade.
    fn capabilities(&self) -> ProviderCapabilities {
        ProviderCapabilities::conservative()
    }

    /// Structured completion (AD-01). The default implementation
    /// renders the request back to a text prompt and delegates to
    /// `generate_with_tools` — the bridge during the D1 migration.
    ///
    /// Providers that support multi-role messages override this to
    /// build the wire body from `req.messages` and `req.system`
    /// directly, so prompt caching and per-message role semantics are
    /// preserved. When every provider implements this, the legacy
    /// `generate`/`generate_with_tools` methods are removed.
    async fn complete(&self, req: &CompletionRequest) -> Result<GenerationResponse> {
        let prompt = req.render_text();
        self.generate_with_tools(&prompt, &req.tools, &req.options).await
    }

    // NOTE (A2): a `stream_completion(&CompletionRequest)` counterpart
    // to `complete` cannot have a working default implementation. Its
    // stream borrows the prompt for the stream's lifetime, and the
    // prompt is a temporary owned by the default body — the borrow
    // checker rejects it. Providers that stream will implement
    // `stream_completion` directly in A4, where the engine migrates its
    // call sites and the exact lifetime shape becomes concrete. Until
    // then, the streaming call sites continue to use `stream_with_tools`
    // with a rendered text prompt, which is the same behaviour the
    // default adapter would have produced.

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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::request::{CompletionRequest, ModelRef, SystemPrompt};
    use kod_types::{ChatMessage, MessageId, MessageRole, ToolDefinition};
    use std::sync::Mutex;
    use time::OffsetDateTime;

    /// A provider that records the prompt its legacy method receives,
    /// and does not override `complete()`. Asserting the prompt
    /// contains the rendered request proves the default adapter runs.
    struct RecordingProvider {
        last_prompt: Mutex<Option<String>>,
    }

    #[async_trait]
    impl LlmProvider for RecordingProvider {
        fn name(&self) -> &str {
            "recording"
        }
        async fn list_models(&self) -> Result<Vec<String>> {
            Ok(vec![])
        }
        async fn generate(
            &self,
            _prompt: &str,
            _opts: &GenerationOptions,
        ) -> Result<String> {
            Ok(String::new())
        }
        async fn generate_with_tools(
            &self,
            prompt: &str,
            _tools: &[ToolDefinition],
            _opts: &GenerationOptions,
        ) -> Result<GenerationResponse> {
            *self.last_prompt.lock().unwrap() = Some(prompt.to_string());
            Ok(GenerationResponse::Text {
                content: "ok".to_string(),
                usage: None,
            })
        }
        fn stream(
            &self,
            _p: &str,
            _o: &GenerationOptions,
        ) -> Pin<Box<dyn Stream<Item = Result<StreamChunk>> + Send + '_>> {
            Box::pin(futures::stream::empty())
        }
    }

    #[tokio::test]
    async fn test_complete_default_delegates_to_generate_with_tools() {
        let provider = RecordingProvider {
            last_prompt: Mutex::new(None),
        };

        let mut req = CompletionRequest::new(ModelRef::new("e", "m"));
        req.system = SystemPrompt::new()
            .with("You are kod.", true)
            .with("Env: test", false);
        req.messages = vec![ChatMessage::text(
            MessageId::new(),
            MessageRole::User,
            "hello",
            OffsetDateTime::now_utc(),
        )];

        let resp = provider.complete(&req).await.unwrap();
        match resp {
            GenerationResponse::Text { content, .. } => assert_eq!(content, "ok"),
            _ => panic!("expected Text"),
        }

        let seen = provider.last_prompt.lock().unwrap().clone().unwrap();
        assert!(
            seen.contains("## System"),
            "default adapter should render the system block: {seen}"
        );
        assert!(
            seen.contains("You are kod."),
            "default adapter should include the system text: {seen}"
        );
        assert!(
            seen.contains("Env: test"),
            "default adapter should include the volatile segment: {seen}"
        );
        assert!(
            seen.contains("User: hello"),
            "default adapter should render the message transcript: {seen}"
        );
    }

    #[tokio::test]
    async fn test_capabilities_default_is_conservative() {
        let provider = RecordingProvider {
            last_prompt: Mutex::new(None),
        };
        let caps = provider.capabilities();
        assert!(caps.tools);
        assert!(!caps.streaming_tools);
        assert_eq!(caps.prompt_cache, crate::request::PromptCacheKind::None);
    }

}
