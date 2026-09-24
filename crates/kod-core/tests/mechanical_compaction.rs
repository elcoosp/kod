//! Mechanical compaction integration test (delta §4.1).
//!
//! Verifies that when `maybe_compact_for` decides the transcript is
//! over threshold, the engine's dispatcher tries shake before the
//! summary path, and that a successful shake plan actually elides
//! content in the stored transcript.
//!
//! # The fixture
//!
//! `record_turn_for` caps each turn at `MAX_TURN_CHARS = 4_000`, so
//! the fixture's per-turn content must stay under that to survive
//! seeding un-mutated. Seven turns of a ~3,808-char fenced block:
//!
//! * Each turn ≈ 952 tokens.
//! * Seven turns ≈ 6,664 tokens against the default 8,192-token
//!   window → ratio ≈ 0.813, over the 0.80 soft threshold.
//!   `decide` returns `StartBackground`, which is enough to trigger
//!   the mechanical attempt (the engine runs it before the soft/hard
//!   branch).
//! * Under the engine's `aggressive()` shake config
//!   (`protect_tokens = 4_000`), messages 0 and 1 are candidates:
//!   their suffixes (5,712 and 4,760 tokens) exceed the protect
//!   window, and their fences (952 tokens) exceed the 400-token
//!   fence threshold.
//!
//! Assertions: the pre-run transcript is intact, and after one
//! `process_streaming` call at least one turn contains the shake
//! placeholder.

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
use std::sync::Arc;
use tempfile::TempDir;

/// A provider that returns a fixed text on any call. Used to drive
/// one turn so `prepare_turn_for` runs (and with it
/// `maybe_compact_for`).
struct TextProvider {
    reply: String,
}

#[async_trait]
impl LlmProvider for TextProvider {
    fn name(&self) -> &str {
        "text-provider"
    }
    async fn list_models(&self) -> Result<Vec<kod_provider::ModelInfo>> {
        Ok(vec![])
    }
    async fn generate(&self, _p: &str, _o: &GenerationOptions) -> Result<String> {
        Ok(self.reply.clone())
    }
    async fn generate_with_tools(
        &self,
        _p: &str,
        _t: &[ToolDefinition],
        _o: &GenerationOptions,
    ) -> Result<GenerationResponse> {
        Ok(GenerationResponse::Text {
            content: self.reply.clone(),
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
        let reply = self.reply.clone();
        Box::pin(async_stream::stream! {
            yield Ok(StreamChunk::Text(reply));
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
async fn mechanical_compaction_elides_large_fences_before_summarizing() {
    let tmp = TempDir::new().unwrap();
    let cfg = fixture_config(tmp.path());
    let engine = Arc::new(KodEngine::new(cfg, tmp.path().join("m.redb")).unwrap());

    // Register the text provider.
    let provider: Arc<dyn LlmProvider> = Arc::new(TextProvider {
        reply: "acknowledged".into(),
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

    // Seven turns of a fenced block just under the per-turn cap.
    // 3,808 chars total (3,800 body + fences), under MAX_TURN_CHARS
    // (4,000), so seeding does not truncate.
    let body = "x".repeat(3_800);
    let content = format!("```\n{body}\n```");
    for _ in 0..7 {
        engine.seed_turn(true, &content).await;
    }

    // Confirm the fixture survived seeding un-mutated.
    let pre = engine.history_for("").await;
    assert_eq!(
        pre.len(),
        7,
        "expected seven seeded turns, got {}",
        pre.len(),
    );
    for (i, turn) in pre.iter().enumerate() {
        assert!(
            turn.content.starts_with("```"),
            "seeded turn {i} was mutated before the run: {:?}",
            &turn.content[..40.min(turn.content.len())],
        );
        assert!(
            !turn.content.contains("truncated"),
            "seeded turn {i} was truncated by MAX_TURN_CHARS: len {}",
            turn.content.len(),
        );
    }

    // Drive one turn. The dispatcher should fire in `prepare_turn_for`
    // and elide at least one fence.
    let (tx, mut rx) = tokio::sync::mpsc::channel::<String>(64);
    let _ = engine.process_streaming("trigger", &tx).await;
    drop(tx);
    while rx.recv().await.is_some() {}

    // Read the transcript back. It now has the original 7 seeded
    // turns plus the two the run itself recorded: the user's
    // "trigger" input and the provider's "acknowledged" reply. That
    // is `prepare_turn_for` / `remember_turn_for` doing exactly what
    // they always do; the mechanical compaction runs *before* those
    // two turns are stored, so the elision it produced is against
    // the 7-turn state and its effects land on turns 0 and 1.
    let post = engine.history_for("").await;
    assert_eq!(
        post.len(),
        9,
        "expected 7 seeded + 1 trigger user turn + 1 reply, got {}",
        post.len(),
    );
    // The first seven are the seeded turns, in order; the last two
    // are the run's own record.
    for i in 0..7 {
        assert!(
            post[i].content.starts_with("```") || post[i].content.contains("elided by shake"),
            "seeded turn {i} was neither fenced nor elided: {:?}",
            &post[i].content[..60.min(post[i].content.len())],
        );
    }
    assert!(
        post[7].content.contains("trigger"),
        "turn 7 should be the user's trigger input: {:?}",
        post[7].content,
    );
    assert!(
        post[8].content.contains("acknowledged"),
        "turn 8 should be the provider's reply: {:?}",
        post[8].content,
    );

    let elided: Vec<usize> = post
        .iter()
        .enumerate()
        .filter(|(_, t)| t.content.contains("elided by shake"))
        .map(|(i, _)| i)
        .collect();
    // The eligible candidates were turns 0 and 1 (their suffixes clear
    // the 4,000-token protect window). Assert they are the elided
    // ones — the protect rule is the whole reason turns 2.. are not.
    assert_eq!(
        elided,
        vec![0, 1],
        "expected exactly turns 0 and 1 elided by the protect-window math; post contents: {:?}",
        post.iter()
            .map(|t| &t.content[..60.min(t.content.len())])
            .collect::<Vec<_>>(),
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn no_mechanical_compaction_on_a_small_transcript() {
    // Counterpart: a transcript under the threshold must not be
    // touched. Verifies the mechanical path only fires after
    // `decide` has already returned a non-None action.
    let tmp = TempDir::new().unwrap();
    let cfg = fixture_config(tmp.path());
    let engine = Arc::new(KodEngine::new(cfg, tmp.path().join("m.redb")).unwrap());

    let provider: Arc<dyn LlmProvider> = Arc::new(TextProvider {
        reply: "ok".into(),
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

    // Just two turns — a total well under the soft threshold.
    let content = "```\nshort body\n```";
    engine.seed_turn(true, content).await;
    engine.seed_turn(true, content).await;

    let (tx, mut rx) = tokio::sync::mpsc::channel::<String>(64);
    let _ = engine.process_streaming("trigger", &tx).await;
    drop(tx);
    while rx.recv().await.is_some() {}

    let post = engine.history_for("").await;
    for (i, turn) in post.iter().enumerate() {
        assert!(
            !turn.content.contains("elided by shake"),
            "turn {i} was elided despite a sub-threshold transcript: {:?}",
            turn.content,
        );
    }
}
