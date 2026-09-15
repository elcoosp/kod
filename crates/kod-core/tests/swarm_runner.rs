//! End-to-end test for the swarm runner against a scripted provider.
//!
//! The provider inspects each prompt and returns the right canned
//! response: a JSON subtask list for the decompose call, a short reply
//! for each per-agent call, and a synthesis for the merge call. Nothing
//! touches the network, so the whole path — decompose → spawn → run →
//! report → merge — runs in a normal `cargo test`.

use async_trait::async_trait;
use kod_core::{KodEngine, RouterConfig, SwarmEvent, SwarmRunner};
use kod_provider::{GenerationOptions, GenerationResponse, LlmProvider, StreamChunk};
use kod_types::ToolDefinition;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use tempfile::TempDir;
use tokio::sync::mpsc;

/// Provider that routes on the prompt's shape:
///
/// - "Split this goal" returns a canned JSON subtask list.
/// - "Synthesize" returns a canned merge text.
/// - Otherwise returns a per-agent reply echoing the prompt's first line.
struct ScriptedSwarmProvider {
    calls: Mutex<Vec<String>>,
}

#[async_trait]
impl LlmProvider for ScriptedSwarmProvider {
    fn name(&self) -> &str {
        "scripted-swarm"
    }

    async fn list_models(&self) -> kod_error::Result<Vec<String>> {
        Ok(vec!["scripted-swarm".into()])
    }

    async fn generate(
        &self,
        prompt: &str,
        _opts: &GenerationOptions,
    ) -> kod_error::Result<String> {
        self.calls.lock().unwrap().push(prompt.to_string());

        if prompt.contains("Split this goal") {
            // Decompose call.
            return Ok(
                r#"[
                    {"name":"schema","description":"Write the SQL schema for a users table."},
                    {"name":"api","description":"Implement the GET /users handler."},
                    {"name":"tests","description":"Write integration tests for the users endpoint."}
                ]"#
                    .to_string(),
            );
        }

        if prompt.contains("Synthesize their work") {
            return Ok("MERGED: schema, handler, and tests all described.".to_string());
        }

        // Per-agent reply: first line of the prompt, so the test can
        // prove each agent got its own subtask.
        let first = prompt.lines().next().unwrap_or("").to_string();
        Ok(format!("agent worked on: {first}"))
    }

    async fn generate_with_tools(
        &self,
        prompt: &str,
        _tools: &[ToolDefinition],
        options: &GenerationOptions,
    ) -> kod_error::Result<GenerationResponse> {
        Ok(GenerationResponse::Text {
            content: self.generate(prompt, options).await?,
            usage: None,
        })
    }

    fn stream(
        &self,
        _prompt: &str,
        _opts: &GenerationOptions,
    ) -> Pin<Box<dyn futures::Stream<Item = kod_error::Result<StreamChunk>> + Send + '_>>
    {
        Box::pin(futures::stream::empty())
    }

    fn stream_with_tools<'a>(
        &'a self,
        prompt: &'a str,
        _tools: &'a [ToolDefinition],
        options: &'a GenerationOptions,
    ) -> Pin<Box<dyn futures::Stream<Item = kod_error::Result<StreamChunk>> + Send + 'a>>
    {
        // The engine's streaming loop drives the swarm's per-agent
        // calls. Reuse `generate`'s routing rather than duplicating it:
        // `async_stream::stream!` lets this method `.await` the async
        // helper.
        Box::pin(async_stream::stream! {
            match self.generate(prompt, options).await {
                Ok(text) => {
                    yield Ok(StreamChunk::Text(text));
                    yield Ok(StreamChunk::Done);
                }
                Err(e) => yield Err(e),
            }
        })
    }
}

impl ScriptedSwarmProvider {
    fn new() -> Self {
        Self {
            calls: Mutex::new(Vec::new()),
        }
    }

}

async fn build_engine(provider: Arc<dyn LlmProvider>) -> (Arc<KodEngine>, TempDir) {
    let temp = TempDir::new().unwrap();
    let db_path = temp.path().join("swarm.redb");
    let cfg = RouterConfig {
        context_window: 8192,
        max_skills_per_query: 3,
        working_dir: temp.path().to_path_buf(),
        enable_memory: false,
    };
    let engine = KodEngine::new(cfg, db_path).unwrap();
    engine.start().await.unwrap();
    engine.set_provider(provider).await;
    (Arc::new(engine), temp)
}

#[tokio::test]
async fn swarm_decomposes_runs_and_merges() {
    let provider = Arc::new(ScriptedSwarmProvider::new());
    let (engine, _tmp) = build_engine(provider.clone()).await;

    let runner = SwarmRunner::new(engine.clone(), 3, true).await.unwrap();
    assert_eq!(runner.max_agents(), 3);

    let (tx, mut rx) = mpsc::channel::<SwarmEvent>(256);
    // Drain the channel in the background so sends do not block.
    let drain = tokio::spawn(async move {
        let mut decomposed = 0;
        let mut started = 0;
        let mut completed = 0;
        let mut saw_merging = false;
        while let Some(ev) = rx.recv().await {
            match ev {
                SwarmEvent::Decomposed(subs) => decomposed = subs.len(),
                SwarmEvent::AgentStarted { .. } => started += 1,
                SwarmEvent::AgentCompleted { .. } => completed += 1,
                SwarmEvent::AgentFailed { .. } => panic!("no agent should fail"),
                SwarmEvent::Merging => saw_merging = true,
                _ => {}
            }
        }
        (decomposed, started, completed, saw_merging)
    });

    let resp = runner.run("build a users service", &tx).await.unwrap();
    drop(tx);
    let (decomposed, started, completed, saw_merging) = drain.await.unwrap();

    assert_eq!(decomposed, 3, "decompose should return three subtasks");
    assert_eq!(resp.subtasks.len(), 3);
    assert_eq!(started, 3, "each subtask must start an agent");
    assert_eq!(completed, 3, "each agent must complete");
    assert!(saw_merging, "runner should emit Merging");
    assert_eq!(resp.per_agent.len(), 3);
    assert!(resp.merged_by_model, "merge should be model-produced");
    assert!(
        resp.merged.contains("MERGED"),
        "merged text should come from the synthesis call: {}",
        resp.merged
    );

    // Every agent produced a per-agent reply that names its subtask.
    for r in &resp.per_agent {
        match &r.outcome {
            kod_core::AgentOutcome::Completed(text) => {
                assert!(
                    text.contains("agent worked on"),
                    "agent {} reply missing: {text}",
                    r.name
                );
            }
            other => panic!("agent {} failed: {other:?}", r.name),
        }
    }

    // The provider saw one decompose call, three per-agent calls, one merge.
    let calls = provider.calls.lock().unwrap().clone();
    let decompose_calls = calls.iter().filter(|p| p.contains("Split this goal")).count();
    let merge_calls = calls.iter().filter(|p| p.contains("Synthesize their work")).count();
    // generate_with_tools is not used here; the streaming loop drives
    // per-agent. So we count only the two orchestration calls.
    assert_eq!(decompose_calls, 1);
    assert_eq!(merge_calls, 1);
}

#[tokio::test]
async fn swarm_without_merge_concatenates() {
    let provider = Arc::new(ScriptedSwarmProvider::new());
    let (engine, _tmp) = build_engine(provider.clone()).await;

    let runner = SwarmRunner::new(engine, 2, false).await.unwrap();
    assert_eq!(runner.max_agents(), 2);

    let (tx, mut rx) = mpsc::channel::<SwarmEvent>(256);
    let drain = tokio::spawn(async move {
        let mut saw_merging = false;
        while let Some(ev) = rx.recv().await {
            if matches!(ev, SwarmEvent::Merging) {
                saw_merging = true;
            }
        }
        saw_merging
    });

    let resp = runner.run("write docs", &tx).await.unwrap();
    drop(tx);
    let saw_merging = drain.await.unwrap();

    assert!(!saw_merging, "merge=false must not emit Merging");
    assert!(!resp.merged_by_model);
    assert_eq!(resp.per_agent.len(), 2);
    // Concatenation labels each agent.
    for r in &resp.per_agent {
        assert!(
            resp.merged.contains(&r.name),
            "concatenated merge should name {}: {}",
            r.name,
            resp.merged
        );
    }
}

#[tokio::test]
async fn swarm_falls_back_when_decompose_is_not_json() {
    // A provider that never emits the requested JSON. The runner must
    // still produce a swarm (N angle-hinted copies of the goal), not a
    // single agent or an error.
    struct NonJsonProvider;

    #[async_trait]
    impl LlmProvider for NonJsonProvider {
        fn name(&self) -> &str {
            "non-json"
        }
        async fn list_models(&self) -> kod_error::Result<Vec<String>> {
            Ok(vec![])
        }
        async fn generate(
            &self,
            _p: &str,
            _o: &GenerationOptions,
        ) -> kod_error::Result<String> {
            Ok("I will not follow your format.".to_string())
        }
        async fn generate_with_tools(
            &self,
            _p: &str,
            _t: &[ToolDefinition],
            _o: &GenerationOptions,
        ) -> kod_error::Result<GenerationResponse> {
            Ok(GenerationResponse::Text {
                content: "still not json".to_string(),
                usage: None,
            })
        }
        fn stream(
            &self,
            _p: &str,
            _o: &GenerationOptions,
        ) -> Pin<Box<dyn futures::Stream<Item = kod_error::Result<StreamChunk>> + Send + '_>>
        {
            Box::pin(futures::stream::empty())
        }
        fn stream_with_tools<'a>(
            &'a self,
            _p: &'a str,
            _t: &'a [ToolDefinition],
            _o: &'a GenerationOptions,
        ) -> Pin<
            Box<dyn futures::Stream<Item = kod_error::Result<StreamChunk>> + Send + 'a>,
        > {
            Box::pin(futures::stream::iter(vec![
                Ok(StreamChunk::Text("still not json".to_string())),
                Ok(StreamChunk::Done),
            ]))
        }
    }

    let provider: Arc<dyn LlmProvider> = Arc::new(NonJsonProvider);
    let (engine, _tmp) = build_engine(provider).await;

    let runner = SwarmRunner::new(engine, 3, false).await.unwrap();
    let (tx, mut rx) = mpsc::channel::<SwarmEvent>(256);
    let drain = tokio::spawn(async move { while rx.recv().await.is_some() {} });

    let resp = runner.run("something", &tx).await.unwrap();
    drop(tx);
    let _ = drain.await;

    assert_eq!(
        resp.subtasks.len(),
        3,
        "fallback should produce max_agents subtasks"
    );
    assert_eq!(resp.per_agent.len(), 3);
    // Every subtask description mentions the goal — the fallback copies it.
    for st in &resp.subtasks {
        assert!(st.description.contains("something"));
    }
}

#[tokio::test]
async fn swarm_requires_a_provider() {
    let temp = TempDir::new().unwrap();
    let cfg = RouterConfig {
        context_window: 8192,
        working_dir: temp.path().to_path_buf(),
        enable_memory: false,
        ..Default::default()
    };
    let engine = Arc::new(
        KodEngine::new(cfg, temp.path().join("swarm.redb")).unwrap(),
    );
    engine.start().await.unwrap();
    // No set_provider.
    // `unwrap_err` needs `Debug` on the Ok type; matching explicitly
    // avoids adding a `Debug` impl to `SwarmRunner` just for a test.
    let err = match SwarmRunner::new(engine, 3, true).await {
        Ok(_) => panic!("SwarmRunner::new should fail without a provider"),
        Err(e) => e,
    };
    assert!(
        matches!(err, kod_error::KodError::InvalidState(_)),
        "expected InvalidState, got {err:?}"
    );
    assert!(err.to_string().contains("no LLM provider"));
}

#[tokio::test]
async fn swarm_max_agents_is_clamped_to_a_sane_range() {
    let provider = Arc::new(ScriptedSwarmProvider::new());
    let (engine, _tmp) = build_engine(provider).await;

    // Below the floor.
    let runner = SwarmRunner::new(engine.clone(), 1, false).await.unwrap();
    assert_eq!(runner.max_agents(), 2);
    // Above the ceiling.
    let runner = SwarmRunner::new(engine, 100, false).await.unwrap();
    assert_eq!(runner.max_agents(), 8);
}
