//! Tool-loop-guard integration test (delta §9.4).
//!
//! Verifies the guard's corrective reaches the model: a scripted
//! provider that issues the same `list_files` call three times in a
//! row should, on the fourth round, receive a request whose message
//! list carries the corrective System text.

use async_trait::async_trait;
use futures::Stream;
use kod_core::router::RouterConfig;
use kod_core::KodEngine;
use kod_error::Result;
use kod_provider::request::CompletionRequest;
use kod_provider::{
    GenerationOptions, GenerationResponse, LlmProvider, ModelRef,
    ProviderCapabilities, ProviderRegistry, StreamChunk,
};
use kod_types::{ToolCall, ToolDefinition};
use std::pin::Pin;
use std::sync::Mutex;
use std::sync::Arc;
use tempfile::TempDir;

/// Records every `complete()` call's `messages` field, and issues a
/// scripted sequence: N-1 tool calls followed by a final text.
struct ScriptedLoop {
    /// Messages field from each `complete()` call, in order.
    seen_messages: Mutex<Vec<Vec<kod_types::ChatMessage>>>,
    /// Number of tool-call rounds to issue before returning text.
    tool_rounds: usize,
    /// Current round.
    round: Mutex<usize>,
}

impl ScriptedLoop {
    fn new(tool_rounds: usize) -> Self {
        Self {
            seen_messages: Mutex::new(Vec::new()),
            tool_rounds,
            round: Mutex::new(0),
        }
    }
}

#[async_trait]
impl LlmProvider for ScriptedLoop {
    fn name(&self) -> &str {
        "scripted-loop"
    }
    async fn list_models(&self) -> Result<Vec<kod_provider::ModelInfo>> {
        Ok(vec![])
    }
    async fn generate(&self, _p: &str, _o: &GenerationOptions) -> Result<String> {
        Ok(String::new())
    }
    async fn generate_with_tools(
        &self,
        _p: &str,
        _t: &[ToolDefinition],
        _o: &GenerationOptions,
    ) -> Result<GenerationResponse> {
        Ok(GenerationResponse::Text {
            content: "done".to_string(),
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
    async fn complete(&self, req: &CompletionRequest) -> Result<GenerationResponse> {
        self.seen_messages
            .lock()
            .unwrap()
            .push(req.messages.clone());
        let mut r = self.round.lock().unwrap();
        let current = *r;
        *r += 1;
        if current < self.tool_rounds {
            // Same call every round: a loop.
            Ok(GenerationResponse::ToolCalls {
                calls: vec![ToolCall {
                    id: Some(format!("c{current}")),
                    tool_name: "list_files".to_string(),
                    arguments: serde_json::json!({"path": "."}),
                }],
                usage: None,
            })
        } else {
            Ok(GenerationResponse::Text {
                content: "ok, stopping".to_string(),
                usage: None,
            })
        }
    }
}

fn fixture_config(dir: &std::path::Path) -> RouterConfig {
    RouterConfig {
        skill_threshold: 0.3,
        context_window: 8192,
        short_term_capacity: 100,
        working_dir: dir.to_path_buf(),
        enable_memory: false,
        max_skills_per_query: 3,
        embedder: None,
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_corrective_reaches_the_provider_after_three_identical_rounds() {
    let tmp = TempDir::new().unwrap();
    std::fs::write(tmp.path().join("marker.txt"), "x").unwrap();
    let cfg = fixture_config(tmp.path());
    let engine = Arc::new(KodEngine::new(cfg, tmp.path().join("g.redb")).unwrap());

    // Four rounds: three identical list_files calls (triggering the
    // default threshold of 3) then a text reply.
    let provider = Arc::new(ScriptedLoop::new(3));
    let mut reg = ProviderRegistry::new();
    reg.insert(
        "default",
        provider.clone(),
        ProviderCapabilities::conservative(),
        "",
    );
    engine
        .set_registry(Arc::new(reg), ModelRef::new("default", ""), None)
        .await;
    engine.start().await.unwrap();

    let (tx, mut rx) = tokio::sync::mpsc::channel::<String>(64);
    let drain = tokio::spawn(async move {
        while rx.recv().await.is_some() {}
    });
    let _ = engine.process_streaming("loop test", &tx).await;
    drop(tx);
    let _ = drain.await;

    let seen = provider.seen_messages.lock().unwrap().clone();
    // 4 provider calls: 3 tool-call rounds + 1 final text.
    assert_eq!(
        seen.len(),
        4,
        "expected 4 complete() calls, got {}",
        seen.len(),
    );

    // The first three should carry no corrective — the guard's
    // threshold is 3 identical rounds, so it fires on the round that
    // *observes* the third identical call, and the corrective lands
    // on the next request. Call 0, 1, 2 are the first, second, and
    // third tool-call rounds; call 3 is the one that sees the
    // corrective.
    for (i, msgs) in seen.iter().take(3).enumerate() {
        let has_corrective = msgs.iter().any(|m| {
            matches!(m.role, kod_types::MessageRole::System)
                && m.content.contains("tool-loop corrective")
        });
        assert!(
            !has_corrective,
            "call {i} should not have a corrective yet: {:?}",
            msgs.iter().map(|m| &m.content).collect::<Vec<_>>(),
        );
    }

    // The fourth call must carry the corrective.
    let last = seen.last().unwrap();
    let corrective = last
        .iter()
        .find(|m| {
            matches!(m.role, kod_types::MessageRole::System)
                && m.content.contains("tool-loop corrective")
        })
        .unwrap_or_else(|| {
            panic!(
                "fourth call should carry the corrective; got messages: {:?}",
                last.iter().map(|m| &m.content).collect::<Vec<_>>(),
            )
        });
    assert!(
        corrective.content.contains("list_files"),
        "corrective must name the repeated tool: {}",
        corrective.content,
    );
    assert!(
        corrective.content.contains("3 rounds"),
        "corrective must name the count: {}",
        corrective.content,
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn no_corrective_when_the_rounds_differ() {
    // Counterpart: two *different* tool rounds should not trigger the
    // guard. The provider issues two different calls, then text.
    struct DifferentCalls {
        seen: Mutex<Vec<Vec<kod_types::ChatMessage>>>,
        round: Mutex<usize>,
    }
    #[async_trait]
    impl LlmProvider for DifferentCalls {
        fn name(&self) -> &str {
            "different"
        }
        async fn list_models(&self) -> Result<Vec<kod_provider::ModelInfo>> {
            Ok(vec![])
        }
        async fn generate(&self, _p: &str, _o: &GenerationOptions) -> Result<String> {
            Ok(String::new())
        }
        async fn generate_with_tools(
            &self,
            _p: &str,
            _t: &[ToolDefinition],
            _o: &GenerationOptions,
        ) -> Result<GenerationResponse> {
            Ok(GenerationResponse::Text {
                content: "done".to_string(),
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
        async fn complete(&self, req: &CompletionRequest) -> Result<GenerationResponse> {
            self.seen.lock().unwrap().push(req.messages.clone());
            let mut r = self.round.lock().unwrap();
            let current = *r;
            *r += 1;
            if current < 2 {
                Ok(GenerationResponse::ToolCalls {
                    calls: vec![ToolCall {
                        id: Some(format!("c{current}")),
                        tool_name: "list_files".to_string(),
                        // Different path each round.
                        arguments: serde_json::json!({"path": format!("sub{current}")}),
                    }],
                    usage: None,
                })
            } else {
                Ok(GenerationResponse::Text {
                    content: "done".to_string(),
                    usage: None,
                })
            }
        }
    }

    let tmp = TempDir::new().unwrap();
    std::fs::write(tmp.path().join("marker.txt"), "x").unwrap();
    let cfg = fixture_config(tmp.path());
    let engine = Arc::new(KodEngine::new(cfg, tmp.path().join("g.redb")).unwrap());
    let provider = Arc::new(DifferentCalls {
        seen: Mutex::new(Vec::new()),
        round: Mutex::new(0),
    });
    let mut reg = ProviderRegistry::new();
    reg.insert(
        "default",
        provider.clone(),
        ProviderCapabilities::conservative(),
        "",
    );
    engine
        .set_registry(Arc::new(reg), ModelRef::new("default", ""), None)
        .await;
    engine.start().await.unwrap();

    let (tx, mut rx) = tokio::sync::mpsc::channel::<String>(64);
    let drain = tokio::spawn(async move {
        while rx.recv().await.is_some() {}
    });
    let _ = engine.process_streaming("diff test", &tx).await;
    drop(tx);
    let _ = drain.await;

    let seen = provider.seen.lock().unwrap().clone();
    for (i, msgs) in seen.iter().enumerate() {
        let has_corrective = msgs.iter().any(|m| {
            matches!(m.role, kod_types::MessageRole::System)
                && m.content.contains("tool-loop corrective")
        });
        assert!(
            !has_corrective,
            "call {i} should not have a corrective when rounds differ",
        );
    }
}
