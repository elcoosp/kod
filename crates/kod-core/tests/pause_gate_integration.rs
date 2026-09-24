//! Pause gate integration test (delta §9.8).
//!
//! Verifies the wiring end-to-end: a paused engine's turn does not
//! complete until the gate is resumed, and resumes cleanly when it
//! is.

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
use kod_types::ToolDefinition;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;
use tempfile::TempDir;

struct TextProvider;

#[async_trait]
impl LlmProvider for TextProvider {
    fn name(&self) -> &str {
        "text-provider"
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
        Box::pin(async_stream::stream! {
            yield Ok(StreamChunk::Text("ok".to_string()));
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
async fn a_paused_engine_does_not_complete_a_turn_until_resumed() {
    let tmp = TempDir::new().unwrap();
    let cfg = fixture_config(tmp.path());
    let engine = Arc::new(KodEngine::new(cfg, tmp.path().join("p.redb")).unwrap());

    let provider: Arc<dyn LlmProvider> = Arc::new(TextProvider);
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

    // Pause before spawning the turn. The gate is checked at the top
    // of `run_streaming_loop`'s first round; the process will park
    // there.
    assert!(!engine.is_paused());
    engine.pause();
    assert!(engine.is_paused());

    let (tx, mut rx) = tokio::sync::mpsc::channel::<String>(64);
    let engine_for_task = engine.clone();
    let handle = tokio::spawn(async move {
        let r = engine_for_task.process_streaming("hello", &tx).await;
        drop(tx);
        r
    });

    // Drain the chunk receiver in the background so the send does not
    // block if any chunk slips through before the pause check fires.
    let drain = tokio::spawn(async move {
        while rx.recv().await.is_some() {}
    });

    // Wait long enough for the process to have reached the pause
    // check (prepare_turn_for runs in tens of ms). If the wiring is
    // broken, the process completes here and the timeout below would
    // time out.
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert!(
        !handle.is_finished(),
        "the engine must not complete a turn while paused; it finished in under 200ms",
    );

    // Resume. The parked check returns and the turn finishes.
    engine.resume();
    assert!(!engine.is_paused());

    let result = tokio::time::timeout(Duration::from_secs(3), handle)
        .await
        .expect("the turn must complete within 3 s after resume")
        .expect("task join");
    let _ = drain.await;
    result.expect("process_streaming must succeed after resume");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_unpaused_engine_completes_normally() {
    // Sanity: the pause gate wiring does not add latency when the
    // gate is running. A turn completes in well under a second.
    let tmp = TempDir::new().unwrap();
    let cfg = fixture_config(tmp.path());
    let engine = Arc::new(KodEngine::new(cfg, tmp.path().join("p.redb")).unwrap());

    let provider: Arc<dyn LlmProvider> = Arc::new(TextProvider);
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

    let (tx, mut rx) = tokio::sync::mpsc::channel::<String>(64);
    let drain = tokio::spawn(async move {
        while rx.recv().await.is_some() {}
    });
    let start = std::time::Instant::now();
    let _ = engine.process_streaming("hi", &tx).await;
    drop(tx);
    let _ = drain.await;
    assert!(
        start.elapsed() < Duration::from_secs(3),
        "an unpaused turn should complete promptly; took {:?}",
        start.elapsed(),
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn pause_is_idempotent_and_resume_clears_it() {
    let tmp = TempDir::new().unwrap();
    let cfg = fixture_config(tmp.path());
    let engine = KodEngine::new(cfg, tmp.path().join("p.redb")).unwrap();
    engine.start().await.unwrap();

    assert!(!engine.is_paused());
    engine.pause();
    engine.pause();
    assert!(engine.is_paused());
    engine.resume();
    engine.resume();
    assert!(!engine.is_paused());
}
