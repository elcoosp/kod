//! A single streamed tool call's arguments must not accumulate
//! without bound.
//!
//! Bug-hunt 2026-10-23: `stream_round` appended every
//! `StreamChunk::ToolCallDelta` to the per-call `Partial::args`
//! with no size check. The only bound in the file was
//! `MAX_TOOL_ROUNDS`, which caps rounds, not per-call payload, so
//! a broken model (or a prompt injection through a tool
//! description) could stream unbounded args and the engine would
//! hold all of it in memory until the round ended.
//!
//! The fix caps `entry.args` at `MAX_TOOL_ARGS_BYTES` and returns
//! `Err(InvalidState)` directly on overflow -- not through
//! `stream_error` / `partial_error`, which is the path that runs
//! partial tool calls.
//!
//! # The test provider
//!
//! `OversizedArgsStreamer` yields one `ToolCallStart`, then a
//! single `ToolCallDelta` whose arguments exceed 8 MB, then
//! `Done`. Without the cap, `process_streaming` returns `Ok` with
//! an 8 MB tool call in `resp.tool_calls`. With it, the engine
//! returns `Err` when the delta pushes `entry.args` past 8 MB.
use std::pin::Pin;
use std::sync::Arc;

use async_trait::async_trait;
use futures::Stream;
use kod_core::{KodEngine, RouterConfig};
use kod_error::{KodError, Result};
use kod_provider::request::CompletionRequest;
use kod_provider::{
    GenerationOptions, GenerationResponse, LlmProvider, ModelRef, ProviderCapabilities,
    ProviderRegistry, StreamChunk,
};
use kod_types::ToolDefinition;

struct OversizedArgsStreamer;

#[async_trait]
impl LlmProvider for OversizedArgsStreamer {
    fn name(&self) -> &str {
        "oversized-args"
    }
    async fn list_models(&self) -> Result<Vec<kod_provider::ModelInfo>> {
        Ok(vec![kod_provider::ModelInfo::bare("oversized-model")])
    }
    async fn generate(&self, _p: &str, _o: &GenerationOptions) -> Result<String> {
        Err(KodError::Internal(
            "OversizedArgsStreamer only supports streaming".to_string(),
        ))
    }
    async fn generate_with_tools(
        &self,
        _p: &str,
        _t: &[ToolDefinition],
        _o: &GenerationOptions,
    ) -> Result<GenerationResponse> {
        Err(KodError::Internal(
            "OversizedArgsStreamer only supports streaming".to_string(),
        ))
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
            yield Ok(StreamChunk::ToolCallStart {
                index: 0,
                id: Some("call-1".to_string()),
                name: "write_file".to_string(),
            });
            // A single 9 MB delta. 8 MB is the cap; this exceeds it
            // by 1 MB, so the check fires on the same iteration that
            // pushed past the bound.
            let huge = "x".repeat(9 * 1024 * 1024);
            yield Ok(StreamChunk::ToolCallDelta {
                index: 0,
                arguments: format!(r#"{{"path":"big.txt","content":"{huge}"}}"#),
            });
            yield Ok(StreamChunk::Done);
        })
    }
}

async fn engine_with_oversized() -> (Arc<KodEngine>, tempfile::TempDir) {
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
    let provider: Arc<dyn LlmProvider> = Arc::new(OversizedArgsStreamer);
    let mut registry = ProviderRegistry::new();
    registry.insert(
        "oversized",
        provider,
        ProviderCapabilities::conservative(),
        "oversized-model",
    );
    engine
        .set_registry(
            Arc::new(registry),
            ModelRef::new("oversized", "oversized-model"),
            None,
        )
        .await;
    engine.start().await.expect("start");
    (engine, tmp)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn oversized_tool_call_args_are_rejected() {
    let (engine, _tmp) = engine_with_oversized().await;

    let (tx, mut rx) = tokio::sync::mpsc::channel::<String>(32);
    let engine_task = Arc::clone(&engine);
    let run = tokio::spawn(async move { engine_task.process_streaming("test", &tx).await });

    // Drain the channel so the send never blocks.
    let drain = tokio::spawn(async move { while rx.recv().await.is_some() {} });

    let finished = tokio::time::timeout(std::time::Duration::from_secs(5), run)
        .await
        .expect("process_streaming must not hang on the oversized args");

    let result = finished.expect("task did not panic");
    let _ = drain.await;

    let err = match result {
        Ok(resp) => panic!(
            "an oversized tool-call payload must be an error; got Ok with {} tool calls: {:?}",
            resp.tool_calls.len(),
            resp.tool_calls
                .iter()
                .map(|c| c.tool_name.as_str())
                .collect::<Vec<_>>(),
        ),
        Err(e) => e,
    };
    let msg = err.to_string();
    assert!(
        msg.contains("exceed") && msg.contains("byte cap"),
        "error must name the cap; got: {msg}",
    );
    assert!(
        msg.contains("write_file"),
        "error must name the tool; got: {msg}",
    );
}
