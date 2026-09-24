//! Context-gauge adoption test (delta §2.4).
//!
//! The gauge records the provider's own `prompt_tokens` on the last
//! settled call. This test verifies that the number reaches the
//! compaction decision: after one call reports a very large usage,
//! `context_tokens_for` returns a value in the same order of
//! magnitude, which only the anchored path can produce (the
//! char-arithmetic fallback on a tiny transcript would report a
//! number under a hundred).

use async_trait::async_trait;
use futures::Stream;
use kod_core::router::RouterConfig;
use kod_core::KodEngine;
use kod_error::Result;
use kod_provider::request::CompletionRequest;
use kod_provider::{
    GenerationOptions, GenerationResponse, LlmProvider, ModelRef,
    ProviderCapabilities, ProviderRegistry, StreamChunk, TokenUsage,
};
use kod_types::ToolDefinition;
use std::pin::Pin;
use std::sync::Arc;
use tempfile::TempDir;

/// Reports a fixed, deliberately large prompt-token count on every
/// call. The number is the "provider's own answer" the gauge
/// anchors on.
struct ReportingProvider {
    prompt_tokens: usize,
}

#[async_trait]
impl LlmProvider for ReportingProvider {
    fn name(&self) -> &str {
        "reporting"
    }
    async fn list_models(&self) -> Result<Vec<kod_provider::ModelInfo>> {
        Ok(vec![])
    }
    async fn generate(&self, _p: &str, _o: &GenerationOptions) -> Result<String> {
        Ok("ok".to_string())
    }
    async fn generate_with_tools(
        &self,
        _p: &str,
        _t: &[ToolDefinition],
        _o: &GenerationOptions,
    ) -> Result<GenerationResponse> {
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
    fn stream_completion<'a>(
        &'a self,
        _req: &'a CompletionRequest,
    ) -> Pin<Box<dyn Stream<Item = Result<StreamChunk>> + Send + 'a>> {
        let prompt_tokens = self.prompt_tokens;
        Box::pin(async_stream::stream! {
            yield Ok(StreamChunk::Text("ok".to_string()));
            yield Ok(StreamChunk::Usage(TokenUsage {
                prompt_tokens,
                completion_tokens: 1,
                total_tokens: prompt_tokens + 1,
                cache_read_tokens: None,
                cache_creation_tokens: None,
            }));
            yield Ok(StreamChunk::Done);
        })
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
async fn the_anchor_reports_the_providers_own_number() {
    let tmp = TempDir::new().unwrap();
    let cfg = fixture_config(tmp.path());
    let engine = Arc::new(KodEngine::new(cfg, tmp.path().join("g.redb")).unwrap());

    let provider: Arc<dyn LlmProvider> = Arc::new(ReportingProvider {
        prompt_tokens: 5_000,
    });
    let mut reg = ProviderRegistry::new();
    reg.insert(
        "default",
        provider,
        ProviderCapabilities::conservative(),
        "",
    );
    engine
        .set_registry(Arc::new(reg), ModelRef::new("default", ""), None)
        .await;
    engine.start().await.unwrap();

    // First turn: no anchor yet. `context_tokens_for` must return None.
    let before = engine.context_tokens_for("", 0).await;
    assert!(
        before.is_none(),
        "a fresh engine must have no anchor; got {before:?}",
    );

    // Drive one turn. The provider reports prompt_tokens = 5,000.
    let (tx, mut rx) = tokio::sync::mpsc::channel::<String>(64);
    let drain = tokio::spawn(async move {
        while rx.recv().await.is_some() {}
    });
    let _ = engine.process_streaming("hello", &tx).await;
    drop(tx);
    let _ = drain.await;

    // After the turn, an anchor exists and reports a number in the
    // right order of magnitude. The exact number is
    // `prompt_tokens` (the anchor) plus a small tail (the turn's own
    // user prompt and reply, char-arithmetic).
    let after = engine
        .context_tokens_for("", 0)
        .await
        .expect("an anchor must exist after one settled call");
    assert_eq!(
        after, 5_000,
        "the anchor's value must be the provider's own prompt_tokens",
    );

    // With a tail, the estimate is anchor + tail.
    let with_tail = engine
        .context_tokens_for("", 100)
        .await
        .expect("anchor present");
    assert_eq!(with_tail, 5_100);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn compaction_fires_off_the_anchored_number() {
    // A 5,000-token anchor on an 8,192-token window is 61% of window
    // — under the soft threshold. A 7,000-token anchor is 85% — over
    // soft but under hard. The provider in this test reports 7,000
    // after one call; a second call's decision should see the
    // anchored number, not the tiny char estimate of the (short)
    // transcript.
    let tmp = TempDir::new().unwrap();
    let cfg = fixture_config(tmp.path());
    let engine = Arc::new(KodEngine::new(cfg, tmp.path().join("g.redb")).unwrap());

    let provider: Arc<dyn LlmProvider> = Arc::new(ReportingProvider {
        prompt_tokens: 7_000,
    });
    let mut reg = ProviderRegistry::new();
    reg.insert(
        "default",
        provider,
        ProviderCapabilities::conservative(),
        "",
    );
    engine
        .set_registry(Arc::new(reg), ModelRef::new("default", ""), None)
        .await;
    engine.start().await.unwrap();

    // One turn to establish the anchor.
    let (tx, mut rx) = tokio::sync::mpsc::channel::<String>(64);
    let drain = tokio::spawn(async move {
        while rx.recv().await.is_some() {}
    });
    let _ = engine.process_streaming("hello", &tx).await;
    drop(tx);
    let _ = drain.await;

    // The anchor is present and reports 7,000.
    let anchored = engine
        .context_tokens_for("", 0)
        .await
        .expect("anchor present");
    assert_eq!(anchored, 7_000);

    // The value is over 0.8 × 8192 = 6,553. A second turn would see
    // it as such. (We don't drive the second turn here; the
    // decision-math test lives in `compaction::tests::decide_*`.)
    assert!(
        anchored as f64 / 8_192.0 > 0.8,
        "7,000 / 8192 = {} should clear the soft threshold",
        anchored as f64 / 8_192.0,
    );
}
