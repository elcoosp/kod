//! Deterministic replay provider (Tier 1.5).
//!
//! A `ReplayProvider` answers every request from a captured list of
//! rounds. It walks them in order; on the Nth call it returns the
//! Nth captured response verbatim. Never touches the network.
//!
//! This provider is a test/CI affordance — its `name()` is `"replay"`.

use crate::request::{CompletionRequest, ProviderCapabilities};
use crate::traits::GenerationOptions;
use crate::traits::LlmProvider;
use crate::types::GenerationResponse;
use crate::types::{StreamChunk, TokenUsage};
use async_trait::async_trait;
use futures::Stream;
use kod_error::{KodError, Result};
use kod_types::{ToolCall, ToolDefinition};
use std::pin::Pin;
use std::sync::Mutex;

/// One round to replay.
#[derive(Debug, Clone)]
pub struct ReplayRound {
    pub text: String,
    pub tool_calls: Vec<ReplayToolCall>,
    pub usage: Option<TokenUsage>,
}

#[derive(Debug, Clone)]
pub struct ReplayToolCall {
    pub id: Option<String>,
    pub name: String,
    pub arguments: serde_json::Value,
}

/// Replays a fixed sequence of rounds.
pub struct ReplayProvider {
    rounds: Vec<ReplayRound>,
    cursor: Mutex<usize>,
    /// Every `CompletionRequest` this provider saw, in call order.
    /// Replay uses this to compare the shape of the request the
    /// current engine sent against the shape the fixture recorded.
    captured: Mutex<Vec<crate::request::CompletionRequest>>,
}

impl ReplayProvider {
    pub fn new(rounds: Vec<ReplayRound>) -> Self {
        Self {
            rounds,
            cursor: Mutex::new(0),
            captured: Mutex::new(Vec::new()),
        }
    }

    fn take(&self) -> Option<ReplayRound> {
        let mut cur = self.cursor.lock().ok()?;
        let idx = *cur;
        if idx >= self.rounds.len() {
            return None;
        }
        *cur += 1;
        Some(self.rounds[idx].clone())
    }

    /// Number of rounds remaining.
    pub fn remaining(&self) -> usize {
        let cur = self.cursor.lock().map(|g| *g).unwrap_or(0);
        self.rounds.len().saturating_sub(cur)
    }

    /// A snapshot of every request the provider has seen.
    pub fn captured(&self) -> Vec<crate::request::CompletionRequest> {
        self.captured.lock().map(|g| g.clone()).unwrap_or_default()
    }

    /// Rewind the cursor. Useful for a fresh test.
    pub fn rewind(&self) {
        if let Ok(mut g) = self.cursor.lock() {
            *g = 0;
        }
    }
}

#[async_trait]
impl LlmProvider for ReplayProvider {
    fn name(&self) -> &str {
        "replay"
    }

    async fn list_models(&self) -> Result<Vec<String>> {
        Ok(vec!["replay-fixture".to_string()])
    }

    async fn generate(&self, _prompt: &str, _options: &GenerationOptions) -> Result<String> {
        match self.take() {
            Some(r) => Ok(r.text),
            None => Err(KodError::InvalidState(
                "replay fixture exhausted".to_string(),
            )),
        }
    }

    async fn generate_with_tools(
        &self,
        _prompt: &str,
        _tools: &[ToolDefinition],
        _options: &GenerationOptions,
    ) -> Result<GenerationResponse> {
        let Some(r) = self.take() else {
            return Err(KodError::InvalidState(
                "replay fixture exhausted".to_string(),
            ));
        };
        if r.tool_calls.is_empty() {
            Ok(GenerationResponse::Text {
                content: r.text,
                usage: r.usage,
            })
        } else {
            let calls: Vec<ToolCall> = r
                .tool_calls
                .into_iter()
                .map(|c| ToolCall {
                    id: c.id,
                    tool_name: c.name,
                    arguments: c.arguments,
                })
                .collect();
            Ok(GenerationResponse::ToolCalls {
                calls,
                usage: r.usage,
            })
        }
    }

    fn stream(
        &self,
        _prompt: &str,
        _options: &GenerationOptions,
    ) -> Pin<Box<dyn Stream<Item = Result<StreamChunk>> + Send + '_>> {
        use futures::StreamExt;
        let answer = self
            .take()
            .map(|r| {
                let mut chunks: Vec<Result<StreamChunk>> = Vec::new();
                if !r.text.is_empty() {
                    chunks.push(Ok(StreamChunk::Text(r.text.clone())));
                }
                for (index, call) in r.tool_calls.iter().enumerate() {
                    chunks.push(Ok(StreamChunk::ToolCallStart {
                        index,
                        id: call.id.clone(),
                        name: call.name.clone(),
                    }));
                    let args_str = serde_json::to_string(&call.arguments).unwrap_or_default();
                    chunks.push(Ok(StreamChunk::ToolCallDelta {
                        index,
                        arguments: args_str,
                    }));
                }
                if let Some(u) = r.usage {
                    chunks.push(Ok(StreamChunk::Usage(u)));
                }
                chunks.push(Ok(StreamChunk::Done));
                chunks
            })
            .unwrap_or_else(|| {
                vec![Err(KodError::InvalidState(
                    "replay fixture exhausted".to_string(),
                ))]
            });
        futures::stream::iter(answer).boxed()
    }

    fn capabilities(&self) -> ProviderCapabilities {
        ProviderCapabilities {
            tools: true,
            streaming_tools: true,
            pricing: None,
            ..ProviderCapabilities::conservative()
        }
    }

    /// Replay never overrides the structured path — the default
    /// `complete` (which delegates to `generate_with_tools`) is
    /// exactly what we want.
    async fn complete(&self, req: &CompletionRequest) -> Result<GenerationResponse> {
        // Capture the request before delegating.
        if let Ok(mut g) = self.captured.lock() {
            g.push(req.clone());
        }
        let prompt = req.render_text();
        self.generate_with_tools(&prompt, &req.tools, &req.options)
            .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures::StreamExt;

    fn sample_rounds() -> Vec<ReplayRound> {
        vec![
            ReplayRound {
                text: "first".into(),
                tool_calls: vec![],
                usage: Some(TokenUsage {
                    prompt_tokens: 10,
                    completion_tokens: 2,
                    total_tokens: 12,
                }),
            },
            ReplayRound {
                text: String::new(),
                tool_calls: vec![ReplayToolCall {
                    id: Some("call_1".into()),
                    name: "read_file".into(),
                    arguments: serde_json::json!({"path": "x"}),
                }],
                usage: None,
            },
        ]
    }

    #[tokio::test]
    async fn generate_returns_rounds_in_order() {
        let p = ReplayProvider::new(sample_rounds());
        let a = p
            .generate("ignored", &GenerationOptions::default())
            .await
            .unwrap();
        assert_eq!(a, "first");
        let b = p
            .generate("ignored", &GenerationOptions::default())
            .await
            .unwrap();
        assert_eq!(b, "");
    }

    #[tokio::test]
    async fn generate_past_the_end_errors() {
        let p = ReplayProvider::new(sample_rounds());
        let _ = p.generate("x", &GenerationOptions::default()).await;
        let _ = p.generate("x", &GenerationOptions::default()).await;
        let r = p.generate("x", &GenerationOptions::default()).await;
        assert!(r.is_err());
    }

    #[tokio::test]
    async fn rewind_resets_cursor() {
        let p = ReplayProvider::new(sample_rounds());
        let _ = p.generate("x", &GenerationOptions::default()).await;
        assert_eq!(p.remaining(), 1);
        p.rewind();
        assert_eq!(p.remaining(), 2);
    }

    #[tokio::test]
    async fn stream_emits_tool_call_frames() {
        let p = ReplayProvider::new(sample_rounds());
        let _ = p.generate("x", &GenerationOptions::default()).await;
        let mut s = p.stream("x", &GenerationOptions::default());
        let mut saw_start = false;
        let mut saw_done = false;
        while let Some(item) = s.next().await {
            match item.unwrap() {
                StreamChunk::ToolCallStart { name, .. } => {
                    assert_eq!(name, "read_file");
                    saw_start = true;
                }
                StreamChunk::Done => saw_done = true,
                _ => {}
            }
        }
        assert!(saw_start);
        assert!(saw_done);
    }

    #[tokio::test]
    async fn provider_name_is_stable() {
        let p = ReplayProvider::new(vec![]);
        assert_eq!(p.name(), "replay");
    }

    #[tokio::test]
    async fn list_models_reports_a_placeholder() {
        let p = ReplayProvider::new(vec![]);
        let models = p.list_models().await.unwrap();
        assert!(!models.is_empty());
    }
}
