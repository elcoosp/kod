#![cfg(test)]
//! P5.6 — the full mid-stream switch path, end to end.
//!
//! A scripted `JevDecider` returns a confident `is_off_track`
//! verdict after the primary provider has emitted enough text to
//! pass the early-termination floor. The engine must:
//!
//! 1. Break out of the primary stream.
//! 2. Open the fallback stream via `fallback_stream_for_off_track`.
//! 3. Keep the accumulated text and continue with the fallback.
//!
//! The test asserts the final text contains both the primary's
//! marker and the fallback's marker — i.e. the switch happened
//! mid-round and the consumer saw one uninterrupted reply.
use super::*;
use async_trait::async_trait;
use kod_config::JevThresholds;
use kod_provider::{
    GenerationOptions, GenerationResponse, LlmProvider, ProviderCapabilities, ProviderRegistry,
    StreamChunk,
};
use std::sync::Arc;

/// A provider whose `stream_completion` yields a fixed sequence
/// of chunks and then `Done`. Every chunk is one segment of the
/// marker text.
struct ChunkedProvider {
    name: String,
    chunks: Vec<String>,
}

#[async_trait]
impl LlmProvider for ChunkedProvider {
    fn name(&self) -> &str {
        &self.name
    }
    async fn list_models(&self) -> kod_error::Result<Vec<kod_provider::ModelInfo>> {
        Ok(vec![])
    }
    async fn generate(&self, _p: &str, _o: &GenerationOptions) -> kod_error::Result<String> {
        Ok(self.chunks.join(""))
    }
    async fn generate_with_tools(
        &self,
        _p: &str,
        _t: &[kod_types::ToolDefinition],
        _o: &GenerationOptions,
    ) -> kod_error::Result<GenerationResponse> {
        Ok(GenerationResponse::Text {
            content: self.chunks.join(""),
            usage: None,
        })
    }
    fn stream(
        &self,
        _p: &str,
        _o: &GenerationOptions,
    ) -> std::pin::Pin<Box<dyn futures::Stream<Item = kod_error::Result<StreamChunk>> + Send + '_>>
    {
        let chunks: Vec<_> = self
            .chunks
            .iter()
            .map(|c| Ok(StreamChunk::Text(c.clone())))
            .chain(std::iter::once(Ok(StreamChunk::Done)))
            .collect();
        Box::pin(futures::stream::iter(chunks))
    }
    fn capabilities(&self) -> ProviderCapabilities {
        ProviderCapabilities {
            tools: true,
            streaming_tools: true,
            ..ProviderCapabilities::conservative()
        }
    }
    fn stream_completion<'a>(
        &'a self,
        _req: &'a kod_provider::CompletionRequest,
    ) -> std::pin::Pin<Box<dyn futures::Stream<Item = kod_error::Result<StreamChunk>> + Send + 'a>>
    {
        self.stream("", &GenerationOptions::default())
    }
}

/// A `JevDecider` that answers every question with a fixed
/// probability derived from the question text.
struct ScriptedJev {
    config: kod_config::JevConfig,
    /// Probability returned for questions containing
    /// `"is_off_track"`. `1.0` guarantees an off-track verdict.
    off_track_p: f32,
}

#[async_trait]
impl crate::jev::JevDecider for ScriptedJev {
    fn thresholds(&self) -> &JevThresholds {
        &self.config.thresholds
    }
    fn config(&self) -> &kod_config::JevConfig {
        &self.config
    }
    fn reasoning_timeout(&self) -> std::time::Duration {
        std::time::Duration::from_secs(0)
    }
    fn clear_cache(&self) {}
    fn cache_len(&self) -> usize {
        0
    }
    fn cache_enabled(&self) -> bool {
        false
    }
    async fn evaluate_yes_no(
        &self,
        _state: &serde_json::Value,
        _question: &str,
    ) -> std::result::Result<crate::jev::Decision<bool>, crate::jev::JevError> {
        // Neutral by default. The mid-stream switch path uses
        // the batch method; a single-call path would need a
        // different scripting strategy.
        Ok(crate::jev::Decision::jev(true, 0.0))
    }
    async fn evaluate_yes_no_batch(
        &self,
        _state: &serde_json::Value,
        questions: &[(String, String)],
    ) -> std::result::Result<Vec<(String, f32)>, crate::jev::JevError> {
        // `should_early_terminate` keys its questions with
        // `is_off_track` and `is_complete`; return the scripted
        // probability for the off-track key and a neutral 0.5
        // for everything else.
        Ok(questions
            .iter()
            .map(|(k, _q)| {
                let p = if k == "is_off_track" {
                    self.off_track_p
                } else {
                    0.5
                };
                (k.clone(), p)
            })
            .collect())
    }
    async fn evaluate_score(
        &self,
        _state: &serde_json::Value,
        _question: &str,
        _levels: &[&str],
    ) -> std::result::Result<crate::jev::Decision<String>, crate::jev::JevError> {
        Ok(crate::jev::Decision::jev("unknown".to_string(), 1.0))
    }
    async fn evaluate_choice(
        &self,
        _state: &serde_json::Value,
        _question: &str,
        _options: &[&str],
    ) -> std::result::Result<crate::jev::Decision<String>, crate::jev::JevError> {
        Ok(crate::jev::Decision::jev("unknown".to_string(), 1.0))
    }
    fn with_thresholds(
        &self,
        new: JevThresholds,
    ) -> std::result::Result<Arc<dyn crate::jev::JevDecider>, crate::jev::JevError> {
        let mut cfg = self.config.clone();
        cfg.thresholds = new;
        Ok(Arc::new(ScriptedJev {
            config: cfg,
            off_track_p: self.off_track_p,
        }))
    }
}

impl ScriptedJev {
    /// Answer a *question text* — used by the single-call
    /// method. The switch path uses the batch method with keys
    /// and does not call this.
    #[allow(dead_code)]
    fn p_for(&self, question: &str) -> f32 {
        let l = question.to_lowercase();
        if l.contains("off track") || l.contains("off_track") {
            self.off_track_p
        } else {
            0.5
        }
    }
}

#[tokio::test]
async fn off_track_verdict_switches_to_fallback_mid_stream() {
    // ------------------------------------------------------------------
    // Setup
    // ------------------------------------------------------------------
    let tmp = tempfile::TempDir::new().unwrap();
    let cfg = RouterConfig {
        working_dir: tmp.path().to_path_buf(),
        enable_memory: false,
        ..RouterConfig::default()
    };
    let engine = KodEngine::new(cfg, tmp.path().join("test.redb")).unwrap();

    // Primary emits enough text to pass `EARLY_TERM_MIN_CHARS`
    // (400) and enough chunks to hit the every-5 check. The
    // primary's text contains a sentence-terminator so the
    // helper's sentence gate passes.
    // 40 chunks × ~17 chars ≈ 680 chars, well above the 400
    // character floor `EARLY_TERM_MIN_CHARS` enforces. The
    // every-5 check fires at chunk 5, 10, …, and passes once
    // the text is long enough.
    let primary_chunks: Vec<String> = (0..40).map(|i| format!("primary line {i}. ")).collect();
    let primary: Arc<dyn LlmProvider> = Arc::new(ChunkedProvider {
        name: "primary".to_string(),
        chunks: primary_chunks,
    });
    let fallback: Arc<dyn LlmProvider> = Arc::new(ChunkedProvider {
        name: "fallback".to_string(),
        chunks: vec!["FALLBACK_MARKER_TEXT.".to_string()],
    });

    let mut reg = ProviderRegistry::new();
    reg.insert(
        "primary",
        primary,
        ProviderCapabilities {
            tools: true,
            streaming_tools: true,
            ..ProviderCapabilities::conservative()
        },
        "m",
    );
    reg.insert(
        "fallback",
        fallback,
        ProviderCapabilities {
            tools: true,
            streaming_tools: true,
            ..ProviderCapabilities::conservative()
        },
        "m",
    );

    let mut routing = kod_config::RoutingConfig::default();
    // Route the classifier's verdict for "hello" (Simple) to
    // primary, with fallback second.
    routing
        .by_task
        .insert("Simple".to_string(), "primary".to_string());
    routing.fallback.push("fallback".to_string());
    engine
        .set_registry(Arc::new(reg), ModelRef::new("primary", "m"), Some(routing))
        .await;

    // Scripted Jev: off-track fires on the first check.
    let mut jev_cfg = kod_config::JevConfig::default();
    jev_cfg.enabled = true;
    engine.set_jev_decider(Arc::new(ScriptedJev {
        config: jev_cfg,
        off_track_p: 0.99,
    }));

    engine.start().await.unwrap();

    // ------------------------------------------------------------------
    // Run
    // ------------------------------------------------------------------
    let (tx, mut rx) = tokio::sync::mpsc::channel::<String>(64);
    // `KodEngine` is not `Clone`; wrap in `Arc` for the shared
    // handle the drain task and the process call both need.
    let engine = std::sync::Arc::new(engine);
    let drain = tokio::spawn(async move {
        let mut acc = String::new();
        while let Some(chunk) = rx.recv().await {
            acc.push_str(&chunk);
        }
        acc
    });

    let _ = engine.process_streaming_for("session", "hello", &tx).await;
    drop(tx);
    let streamed = drain.await.unwrap();

    let _ = engine.shutdown().await;

    // ------------------------------------------------------------------
    // Assert
    // ------------------------------------------------------------------
    // The primary's marker must be present (accumulated text is
    // kept) AND the fallback's marker must be present (the
    // switch happened).
    assert!(
        streamed.contains("primary line"),
        "expected the primary's text in the stream, got: {streamed}",
    );
    assert!(
        streamed.contains("FALLBACK_MARKER_TEXT"),
        "expected the fallback's text after the mid-stream switch,              got: {streamed}",
    );
}
