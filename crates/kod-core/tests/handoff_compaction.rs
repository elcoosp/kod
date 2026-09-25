//! Handoff compaction integration test (delta §4.1).
//!
//! Proves the handoff rung actually reaches the provider and its
//! output is used as the compaction summary — as opposed to the
//! unit tests, which exercise the method in isolation.
//!
//! The setup is the same as `mechanical_compaction.rs`'s: a
//! transcript over the soft threshold, one `process_streaming` call
//! to trigger `maybe_compact_for`. The difference is that this test
//! uses content the mechanical rungs cannot elide (no large fenced
//! blocks, no superseded reads), so the ladder falls through to
//! handoff and the provider's reply is the summary text.

use async_trait::async_trait;
use futures::Stream;
use kod_core::router::RouterConfig;
use kod_core::KodEngine;
use kod_error::Result;
use kod_provider::request::CompletionRequest;
use kod_provider::{
    GenerationOptions, GenerationResponse, LlmProvider, ProviderCapabilities,
    ProviderRegistry, StreamChunk,
};
use kod_types::ToolDefinition;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use tempfile::TempDir;

/// A provider that:
///
/// * returns a distinctive handoff-document string when asked to
///   summarize (the handoff prompt contains "briefing document");
/// * returns "acknowledged" for anything else (the turn's own
///   provider call).
///
/// The distinction lets the test assert the summary the engine
/// recorded came from the handoff call and not from the reply.
struct HandoffProvider {
    calls: Arc<Mutex<Vec<String>>>,
}

#[async_trait]
impl LlmProvider for HandoffProvider {
    fn name(&self) -> &str {
        "handoff-provider"
    }
    async fn list_models(&self) -> Result<Vec<kod_provider::ModelInfo>> {
        Ok(vec![])
    }
    async fn generate(&self, prompt: &str, _o: &GenerationOptions) -> Result<String> {
        self.calls.lock().unwrap().push(prompt.to_string());
        if prompt.contains("briefing document") {
            Ok("## Objective\nDo the thing.\n\n\
                ## What has been done\n(none)\n\n\
                ## Current state\n(none)\n\n\
                ## What to do next\n(none)\n\n\
                ## Constraints and preferences\n(none)"
                .to_string())
        } else {
            Ok("acknowledged".to_string())
        }
    }
    async fn generate_with_tools(
        &self,
        _p: &str,
        _t: &[ToolDefinition],
        _o: &GenerationOptions,
    ) -> Result<GenerationResponse> {
        Ok(GenerationResponse::Text {
            content: "acknowledged".into(),
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
    async fn complete(&self, _req: &CompletionRequest) -> Result<GenerationResponse> {
        Ok(GenerationResponse::Text {
            content: "acknowledged".into(),
            usage: None,
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
async fn handoff_is_used_when_mechanical_rungs_cannot_reduce() {
    let tmp = TempDir::new().unwrap();
    let cfg = fixture_config(tmp.path());
    let engine = Arc::new(KodEngine::new(cfg, tmp.path().join("h.redb")).unwrap());

    let calls = Arc::new(Mutex::new(Vec::new()));
    let provider: Arc<dyn LlmProvider> = Arc::new(HandoffProvider {
        calls: Arc::clone(&calls),
    });
    let mut reg = ProviderRegistry::new();
    reg.insert(
        "default",
        provider,
        ProviderCapabilities::conservative(),
        "",
    );
    engine
        .set_registry(Arc::new(reg), kod_provider::ModelRef::new("default", ""), None)
        .await;
    engine.start().await.unwrap();

    // Seven turns of *prose* — no fenced blocks (so `shake` finds
    // nothing over its fence threshold), no reads (so `prune` finds
    // nothing superseded). This is the case where mechanical rungs
    // cannot reduce and handoff runs.
    //
    // Each turn is a chunk of ordinary text, sized so the total
    // clears the threshold: 7 * 3,800 chars ≈ 26,600 chars ≈ 6,650
    // tokens against 8,192 — over the 0.80 soft threshold.
    let chunk = "the parser needs to handle trailing commas \
                 and the lexer is not emitting them correctly, \
                 which causes the type checker to fail on input \
                 that should be valid; this is the issue we are \
                 working on right now"
        .repeat(30);
    for _ in 0..7 {
        engine.seed_turn(true, &chunk).await;
    }

    let pre = engine.history_for("").await;
    assert_eq!(pre.len(), 7, "seven seeded turns expected");

    // Drive the turn.
    let (tx, mut rx) = tokio::sync::mpsc::channel::<String>(64);
    let _ = engine.process_streaming("trigger", &tx).await;
    drop(tx);
    while rx.recv().await.is_some() {}

    // The handoff prompt must have been sent — "briefing document"
    // is the marker phrase unique to `build_handoff_prompt`.
    let seen = calls.lock().unwrap().clone();
    let handoff_calls: Vec<&String> = seen
        .iter()
        .filter(|p| p.contains("briefing document"))
        .collect();
    assert!(
        !handoff_calls.is_empty(),
        "the handoff prompt was not sent; {} calls seen, first prompt: {:?}",
        seen.len(),
        seen.first().map(|s| &s[..120.min(s.len())]),
    );

    // And the summary landed: the older half of the transcript was
    // replaced with a message carrying the handoff sections.
    let post = engine.history_for("").await;
    let has_handoff_summary = post.iter().any(|t| {
        t.content.contains("## Objective") && t.content.contains("## Current state")
    });
    assert!(
        has_handoff_summary,
        "the handoff summary was not stored; post-turn contents: {:?}",
        post.iter()
            .map(|t| &t.content[..60.min(t.content.len())])
            .collect::<Vec<_>>(),
    );
}
