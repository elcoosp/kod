//! Cancel-during-streaming must return promptly.
//!
//! Bug-hunt 2026-10-23: `stream_round`'s inner loop had no cancel
//! check between `stream.next().await` calls. A cancel arriving
//! mid-stream was unobserved until the stream ended naturally --
//! up to `stream_idle_timeout` (120s default) -- and tool-call
//! chunks emitted before the stream ended were executed by the
//! caller before the next round's check fired.
//!
//! The fix wraps `stream.next()` in `tokio::select!` with a 100ms
//! poll of `is_cancelled_for(holder)`, returning `Err(InvalidState)`
//! directly on cancel.
//!
//! The test provider yields one text chunk, then sleeps for 60s.
//! Without the fix, `process_streaming` hangs for the full 60s
//! (inside the outer `timeout(stream_idle_timeout, ...)`); with the
//! fix, the 100ms poll fires and the function returns within a
//! fraction of a second. `MockProvider` cannot express this -- its
//! scripts stream to completion immediately -- so the test defines
//! its own provider.
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use futures::Stream;
use kod_core::{KodEngine, RouterConfig};
use kod_error::{KodError, Result};
use kod_provider::request::CompletionRequest;
use kod_provider::{
    GenerationOptions, GenerationResponse, LlmProvider, ModelInfo, ModelRef, ProviderCapabilities,
    ProviderRegistry, StreamChunk,
};
use kod_types::ToolDefinition;

const DEFAULT_KEY: &str = "session";

struct BlockingStreamer;

#[async_trait]
impl LlmProvider for BlockingStreamer {
    fn name(&self) -> &str {
        "blocking"
    }

    async fn list_models(&self) -> Result<Vec<ModelInfo>> {
        Ok(vec![ModelInfo::bare("blocking-model")])
    }

    async fn generate(&self, _p: &str, _o: &GenerationOptions) -> Result<String> {
        Err(KodError::Internal(
            "BlockingStreamer only supports streaming".to_string(),
        ))
    }

    async fn generate_with_tools(
        &self,
        _p: &str,
        _t: &[ToolDefinition],
        _o: &GenerationOptions,
    ) -> Result<GenerationResponse> {
        Err(KodError::Internal(
            "BlockingStreamer only supports streaming".to_string(),
        ))
    }

    fn stream(
        &self,
        _p: &str,
        _o: &GenerationOptions,
    ) -> Pin<Box<dyn Stream<Item = Result<StreamChunk>> + Send + '_>> {
        // The engine's streaming path calls `stream_completion`, not
        // `stream` -- `stream` is the older non-tool-calling entry
        // point. Keep it empty; the real behavior is in
        // `stream_completion` below.
        Box::pin(futures::stream::empty())
    }

    fn stream_completion<'a>(
        &'a self,
        _req: &'a CompletionRequest,
    ) -> Pin<Box<dyn Stream<Item = Result<StreamChunk>> + Send + 'a>> {
        // The trait's default `stream_completion` delegates to
        // `generate_with_tools`, which BlockingStreamer deliberately
        // does not implement -- so without this override the engine
        // would receive `Err(Internal("only supports streaming"))`
        // before the first chunk. Override it to return the blocking
        // stream directly, matching
        // `pause_gate_integration.rs`'s pattern.
        //
        // Yield one text chunk immediately, then sleep 60s. Without
        // the fix, `stream_round`'s loop waits the full 60s inside
        // `timeout(stream_idle_timeout, stream.next())`. With the
        // fix, the `select!`'s 100ms timer branch fires, sees the
        // cancel flag, and returns.
        Box::pin(async_stream::stream! {
            yield Ok(StreamChunk::Text("hello".to_string()));
            tokio::time::sleep(Duration::from_secs(60)).await;
            yield Ok(StreamChunk::Done);
        })
    }
}

async fn engine_with_blocker() -> (Arc<KodEngine>, tempfile::TempDir) {
    let tmp = tempfile::TempDir::new().unwrap();
    let cfg = RouterConfig {
        embedder: None,
        skill_threshold: 0.3,
        working_dir: tmp.path().to_path_buf(),
        enable_memory: false,
        max_skills_per_query: 3,
        context_window: 8192,
        short_term_capacity: 100,
    };
    let engine = Arc::new(KodEngine::new(cfg, tmp.path().join("t.redb")).expect("engine"));
    let provider: Arc<dyn LlmProvider> = Arc::new(BlockingStreamer);
    let mut registry = ProviderRegistry::new();
    registry.insert(
        "blocking",
        provider,
        ProviderCapabilities::conservative(),
        "blocking-model",
    );
    engine
        .set_registry(
            Arc::new(registry),
            ModelRef::new("blocking", "blocking-model"),
            None,
        )
        .await;
    engine.start().await.expect("start");
    (engine, tmp)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cancel_during_streaming_returns_promptly() {
    let (engine, _tmp) = engine_with_blocker().await;

    let (tx, mut rx) = tokio::sync::mpsc::channel::<String>(32);
    let engine_task = Arc::clone(&engine);
    let run = tokio::spawn(async move {
        engine_task
            .process_streaming_for(DEFAULT_KEY, "hello", &tx)
            .await
    });

    // Wait for the first chunk. The provider yields "hello" and then
    // sleeps, so receiving it proves the inner stream loop is inside
    // `stream.next().await` waiting for the next chunk (the 60s sleep).
    // If the engine errored before sending the first chunk, `recv()`
    // returns `None` (channel closed) without a timeout. Capture
    // `run`'s result in that case so the failure message is the
    // engine's actual error, not "channel open".
    let first_opt = tokio::time::timeout(Duration::from_secs(5), rx.recv())
        .await
        .expect("first chunk within 5s -- the engine did not send anything for 5s");
    let first = match first_opt {
        Some(s) => s,
        None => {
            // Channel closed before a chunk. The engine task must
            // have returned. Join it to surface the error.
            let joined = tokio::time::timeout(Duration::from_secs(1), run)
                .await
                .expect("engine task returned but its JoinHandle did not resolve");
            match joined {
                Ok(Ok(resp)) => {
                    panic!("engine returned Ok before sending a chunk; response: {resp:?}",)
                }
                Ok(Err(e)) => panic!(
                    "engine returned Err before sending a chunk: {e}
                     (likely cause: the registry is not wired the way \
                     stream_round expects, or the first round errored \
                     before reaching the provider stream)",
                ),
                Err(join_err) => panic!("engine task panicked: {join_err:?}"),
            }
        }
    };
    assert_eq!(first, "hello");

    // Now the loop is blocked inside stream.next(). Cancel on the key
    // the engine is using (DEFAULT_KEY == "session").
    engine.request_cancel_for(DEFAULT_KEY);

    // Without the fix, this hangs for 60s (the provider's sleep).
    // With the fix, the 100ms poll fires and returns within 2s.
    let finished = tokio::time::timeout(Duration::from_secs(2), run).await;
    assert!(
        finished.is_ok(),
        "process_streaming did not return within 2s of request_cancel \
         -- the inner stream loop ignores the cancel flag \
         (bug-hunt 2026-10-23)",
    );

    let joined = finished.expect("run returned");
    let result = joined.expect("task did not panic");
    let err = match result {
        Ok(_) => panic!("a cancelled turn must not return Ok"),
        Err(e) => e,
    };
    assert!(
        err.to_string().contains("cancel"),
        "error must name the cancel; got {err}",
    );
}
