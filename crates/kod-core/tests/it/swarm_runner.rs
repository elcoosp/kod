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

use crate::common::install_test_provider::install_test_provider;

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

    async fn list_models(&self) -> kod_error::Result<Vec<kod_provider::ModelInfo>> {
        Ok(vec!["scripted-swarm".into()])
    }

    async fn generate(&self, prompt: &str, _opts: &GenerationOptions) -> kod_error::Result<String> {
        self.calls.lock().unwrap().push(prompt.to_string());

        if prompt.contains("Split this goal") {
            // Decompose call.
            return Ok(r#"[
                    {"name":"schema","description":"Write the SQL schema for a users table."},
                    {"name":"api","description":"Implement the GET /users handler."},
                    {"name":"tests","description":"Write integration tests for the users endpoint."}
                ]"#
            .to_string());
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
    ) -> Pin<Box<dyn futures::Stream<Item = kod_error::Result<StreamChunk>> + Send + '_>> {
        Box::pin(futures::stream::empty())
    }

    fn stream_with_tools<'a>(
        &'a self,
        prompt: &'a str,
        _tools: &'a [ToolDefinition],
        options: &'a GenerationOptions,
    ) -> Pin<Box<dyn futures::Stream<Item = kod_error::Result<StreamChunk>> + Send + 'a>> {
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
        embedder: None,
        skill_threshold: 0.3,
        context_window: 8192,
        short_term_capacity: 100,
        max_skills_per_query: 3,
        working_dir: temp.path().to_path_buf(),
        enable_memory: false,
    };
    let engine = KodEngine::new(cfg, db_path).unwrap();
    engine.start().await.unwrap();
    install_test_provider(&engine, provider).await;
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
    let decompose_calls = calls
        .iter()
        .filter(|p| p.contains("Split this goal"))
        .count();
    let merge_calls = calls
        .iter()
        .filter(|p| p.contains("Synthesize their work"))
        .count();
    // generate_with_tools is not used here; the streaming loop drives
    // per-agent. So we count only the two orchestration calls.
    assert_eq!(decompose_calls, 1);
    assert_eq!(merge_calls, 1);
}

/// A swarm working in a directory with files must have those files in
/// its decompose prompt. The runner probes with `list_files` and
/// `grep`; the prompt's "Working directory context" block names the
/// top-level entries and, when a goal keyword is present, the files it
/// appears in.
#[tokio::test]
async fn test_decompose_sees_repo_context() {
    // Provider that records the decompose prompt for inspection.
    struct PromptCapture {
        decompose_prompt: Mutex<Option<String>>,
    }

    #[async_trait]
    impl LlmProvider for PromptCapture {
        fn name(&self) -> &str {
            "capture"
        }
        async fn list_models(&self) -> kod_error::Result<Vec<kod_provider::ModelInfo>> {
            Ok(vec![])
        }
        async fn generate(
            &self,
            prompt: &str,
            _o: &GenerationOptions,
        ) -> kod_error::Result<String> {
            if prompt.contains("Split this goal") {
                *self.decompose_prompt.lock().unwrap() = Some(prompt.to_string());
                return Ok(
                    r#"[{"name":"a","description":"do a"},{"name":"b","description":"do b"}]"#
                        .to_string(),
                );
            }
            Ok("agent reply".to_string())
        }
        async fn generate_with_tools(
            &self,
            _p: &str,
            _t: &[ToolDefinition],
            _o: &GenerationOptions,
        ) -> kod_error::Result<GenerationResponse> {
            Ok(GenerationResponse::Text {
                content: "agent reply".to_string(),
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
        ) -> Pin<Box<dyn futures::Stream<Item = kod_error::Result<StreamChunk>> + Send + 'a>>
        {
            Box::pin(futures::stream::iter(vec![
                Ok(StreamChunk::Text("agent reply".to_string())),
                Ok(StreamChunk::Done),
            ]))
        }
    }

    let provider = Arc::new(PromptCapture {
        decompose_prompt: Mutex::new(None),
    });
    let (engine, tmp) = build_engine(provider.clone()).await;

    // Seed the working directory with a distinctive file whose name
    // carries a goal keyword, so the grep half of the probe fires too.
    std::fs::write(tmp.path().join("payment_handler.rs"), "// stub").unwrap();
    std::fs::create_dir_all(tmp.path().join("payments")).unwrap();
    std::fs::write(tmp.path().join("payments/schema.sql"), "-- stub").unwrap();

    let runner = SwarmRunner::new(engine, 2, false).await.unwrap();
    let (tx, mut rx) = mpsc::channel::<SwarmEvent>(64);
    let drain = tokio::spawn(async move { while rx.recv().await.is_some() {} });
    let _ = runner.run("fix the payment handler", &tx).await.unwrap();
    drop(tx);
    let _ = drain.await;

    let prompt = provider
        .decompose_prompt
        .lock()
        .unwrap()
        .clone()
        .expect("decompose should have been called");

    assert!(
        prompt.contains("Working directory context"),
        "decompose prompt must carry the repo block: {prompt}"
    );
    assert!(
        prompt.contains("payment_handler.rs") || prompt.contains("payments"),
        "prompt should name the seeded files: {prompt}"
    );
}

/// Two agents writing the same file must show up in
/// `SwarmResponse.conflicts`, and the merge prompt must carry a warning
/// block naming the file and its authors.
#[tokio::test]
async fn test_swarm_detects_file_conflicts() {
    use kod_types::ToolCall;

    // Provider whose stream_with_tools asks each agent to write to
    // the same file, so every agent produces a write_file call on
    // `shared.txt`.
    struct SharedWriter {
        merge_prompts: Mutex<Vec<String>>,
    }

    #[async_trait]
    impl LlmProvider for SharedWriter {
        fn name(&self) -> &str {
            "shared-writer"
        }
        async fn list_models(&self) -> kod_error::Result<Vec<kod_provider::ModelInfo>> {
            Ok(vec![])
        }
        async fn generate(
            &self,
            prompt: &str,
            _o: &GenerationOptions,
        ) -> kod_error::Result<String> {
            if prompt.contains("Split this goal") {
                return Ok(
                    r#"[{"name":"a","description":"edit shared","capability":"coding"},{"name":"b","description":"edit shared too","capability":"refactoring"}]"#
                        .to_string(),
                );
            }
            if prompt.contains("multiple agents edited the same files") {
                self.merge_prompts.lock().unwrap().push(prompt.to_string());
                return Ok("MERGED with reconciliation".to_string());
            }
            Ok("done".to_string())
        }
        async fn generate_with_tools(
            &self,
            _p: &str,
            _t: &[ToolDefinition],
            _o: &GenerationOptions,
        ) -> kod_error::Result<GenerationResponse> {
            Ok(GenerationResponse::ToolCalls {
                calls: vec![ToolCall {
                    id: None,
                    tool_name: "write_file".to_string(),
                    arguments: serde_json::json!({
                        "path": "shared.txt",
                        "content": "from one agent"
                    }),
                }],
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
        ) -> Pin<Box<dyn futures::Stream<Item = kod_error::Result<StreamChunk>> + Send + 'a>>
        {
            // Two rounds per agent: first a write_file tool call, then
            // a text reply. The engine loop sees the tool call and runs
            // it, then gets Text and stops.
            Box::pin(futures::stream::iter(vec![
                Ok(StreamChunk::ToolCallStart {
                    index: 0,
                    id: None,
                    name: "write_file".to_string(),
                }),
                Ok(StreamChunk::ToolCallDelta {
                    index: 0,
                    arguments: serde_json::json!({
                        "path": "shared.txt",
                        "content": "from one agent"
                    })
                    .to_string(),
                }),
                Ok(StreamChunk::Done),
            ]))
        }
    }

    let provider = Arc::new(SharedWriter {
        merge_prompts: Mutex::new(Vec::new()),
    });
    let (engine, _tmp) = build_engine(provider.clone()).await;

    let runner = SwarmRunner::new(engine, 2, true).await.unwrap();
    let (tx, mut rx) = mpsc::channel::<SwarmEvent>(256);
    let drain = tokio::spawn(async move {
        let mut saw_conflict = false;
        while let Some(ev) = rx.recv().await {
            if matches!(ev, SwarmEvent::ConflictDetected { .. }) {
                saw_conflict = true;
            }
        }
        saw_conflict
    });

    let resp = runner.run("edit shared files", &tx).await.unwrap();
    drop(tx);
    let saw_conflict = drain.await.unwrap();

    assert!(saw_conflict, "runner should emit ConflictDetected");
    assert_eq!(
        resp.conflicts.len(),
        1,
        "exactly one conflict expected, got {:?}",
        resp.conflicts
    );
    let c = &resp.conflicts[0];
    assert!(
        c.file.contains("shared.txt"),
        "conflict file should be shared.txt: {}",
        c.file
    );
    assert_eq!(
        c.agents.len(),
        2,
        "two agents should be named: {:?}",
        c.agents
    );

    // The merge prompt should carry the warning block.
    let merge_prompts = provider.merge_prompts.lock().unwrap().clone();
    assert_eq!(merge_prompts.len(), 1, "one merge call expected");
    let mp = &merge_prompts[0];
    assert!(
        mp.contains("shared.txt"),
        "merge prompt must name the conflicting file: {mp}"
    );
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
        async fn list_models(&self) -> kod_error::Result<Vec<kod_provider::ModelInfo>> {
            Ok(vec![])
        }
        async fn generate(&self, _p: &str, _o: &GenerationOptions) -> kod_error::Result<String> {
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
        ) -> Pin<Box<dyn futures::Stream<Item = kod_error::Result<StreamChunk>> + Send + 'a>>
        {
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
        embedder: None,
        skill_threshold: 0.3,
        context_window: 8192,
        working_dir: temp.path().to_path_buf(),
        enable_memory: false,
        ..Default::default()
    };
    let engine = Arc::new(KodEngine::new(cfg, temp.path().join("swarm.redb")).unwrap());
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

/// Regression: a subtask with `expected_writes` that is failed-out by
/// a failed dependency must not leak its blackboard-viewer
/// subscription. Pre-fix, `set_blackboard_viewer(dispatch_key, true)`
/// fired at dispatch time (for every subtask with a write set), the
/// per-attempt cleanup never ran for a subtask gated out of a wave,
/// and the end-of-run cleanup loop cleared `working_dir` and
/// `write_globs` but not the viewer. On a daemon this leaks one entry
/// per gated subtask per run.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn gated_subtask_does_not_leak_blackboard_viewer() {
    // Chain a → b. A fails; B declares a write set, so its viewer was
    // subscribed at dispatch time. B is failed-out by the gate and
    // must still have its viewer cleared by the end of the run.
    struct ChainProvider;

    #[async_trait]
    impl LlmProvider for ChainProvider {
        fn name(&self) -> &str {
            "chain-leak"
        }
        async fn list_models(&self) -> kod_error::Result<Vec<kod_provider::ModelInfo>> {
            Ok(vec!["chain-leak".into()])
        }
        async fn generate(
            &self,
            prompt: &str,
            _o: &GenerationOptions,
        ) -> kod_error::Result<String> {
            if prompt.contains("Split this goal") {
                return Ok(r#"[
                    {"name":"a","description":"task A","depends_on":[],"expected_writes":[]},
                    {"name":"b","description":"task B","depends_on":["a"],"expected_writes":["src/b.rs"]}
                ]"#
                .to_string());
            }
            if prompt.contains("Synthesize their work") {
                return Ok("MERGED".to_string());
            }
            if prompt.contains("task A") {
                return Err(kod_error::KodError::Provider("A failed".into()));
            }
            Ok("ok".to_string())
        }
        async fn generate_with_tools(
            &self,
            prompt: &str,
            _t: &[ToolDefinition],
            o: &GenerationOptions,
        ) -> kod_error::Result<GenerationResponse> {
            Ok(GenerationResponse::Text {
                content: self.generate(prompt, o).await?,
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
    }

    let provider: Arc<dyn LlmProvider> = Arc::new(ChainProvider);
    let (engine, _tmp) = build_engine(provider).await;

    assert_eq!(
        engine.blackboard_viewer_count_for_tests().await,
        0,
        "no viewer before the run",
    );

    let runner = SwarmRunner::new(engine.clone(), 3, false).await.unwrap();
    let (tx, mut rx) = mpsc::channel::<SwarmEvent>(256);
    let drain = tokio::spawn(async move { while rx.recv().await.is_some() {} });
    let _ = runner.run("do the chain", &tx).await;
    drop(tx);
    let _ = drain.await;

    assert_eq!(
        engine.blackboard_viewer_count_for_tests().await,
        0,
        "every dispatch's blackboard-viewer subscription must be cleared \
         before the run returns, including the gated-out subtask",
    );
}

/// Regression: `forget_transcript` must clear every transcript-scoped
/// map on the engine, and the swarm's end-of-run cleanup must route
/// through it. Pre-fix, `forget_transcript` cleared only three maps
/// (history / last_prompt / context_gauges), the swarm's cleanup loop
/// cleared three more by hand, and every other per-transcript map —
/// tool_loop_guards, todo_trackers, plans, prewalks, write_globs,
/// working_dirs, blackboard_viewers, native_compaction_blocks,
/// image_frames, fidelity_cache, tool_filter_states, steers — leaked
/// one entry per dispatch. On a long-lived daemon that is unbounded.
///
/// This test drives a swarm whose only subtask makes an actual tool
/// call, so `tool_loop_guards`, `todo_trackers`, `steers`, and
/// `blackboard_viewers` all get populated for the dispatch key, and
/// asserts that none survive the run.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn swarm_run_leaves_no_per_transcript_state_behind() {
    use kod_types::ToolCall;

    // Provider that asks its single agent to run `list_files` once,
    // then reply with text. `list_files` is enough to trip the
    // tool-round path that touches `tool_loop_guards`, `todo_trackers`,
    // and `steers`.
    struct OneToolProvider;

    #[async_trait]
    impl LlmProvider for OneToolProvider {
        fn name(&self) -> &str {
            "one-tool"
        }
        async fn list_models(&self) -> kod_error::Result<Vec<kod_provider::ModelInfo>> {
            Ok(vec!["one-tool".into()])
        }
        async fn generate(
            &self,
            prompt: &str,
            _o: &GenerationOptions,
        ) -> kod_error::Result<String> {
            if prompt.contains("Split this goal") {
                return Ok(r#"[{"name":"probe","description":"list the tree","expected_writes":["out/x.txt"]}]"#
                    .to_string());
            }
            if prompt.contains("Synthesize their work") {
                return Ok("MERGED".to_string());
            }
            Ok("done".to_string())
        }
        async fn generate_with_tools(
            &self,
            _p: &str,
            _t: &[ToolDefinition],
            _o: &GenerationOptions,
        ) -> kod_error::Result<GenerationResponse> {
            Ok(GenerationResponse::ToolCalls {
                calls: vec![ToolCall {
                    id: None,
                    tool_name: "list_files".to_string(),
                    arguments: serde_json::json!({ "path": ".", "recursive": false }),
                }],
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
        ) -> Pin<Box<dyn futures::Stream<Item = kod_error::Result<StreamChunk>> + Send + 'a>>
        {
            // Round one: a tool call. Round two: text and stop.
            Box::pin(futures::stream::iter(vec![
                Ok(StreamChunk::ToolCallStart {
                    index: 0,
                    id: None,
                    name: "list_files".to_string(),
                }),
                Ok(StreamChunk::ToolCallDelta {
                    index: 0,
                    arguments: serde_json::json!({
                        "path": ".",
                        "recursive": false
                    })
                    .to_string(),
                }),
                Ok(StreamChunk::Done),
            ]))
        }
    }

    let provider: Arc<dyn LlmProvider> = Arc::new(OneToolProvider);
    let (engine, _tmp) = build_engine(provider).await;

    let runner = SwarmRunner::new(engine.clone(), 2, false).await.unwrap();
    let (tx, mut rx) = mpsc::channel::<SwarmEvent>(256);
    let drain = tokio::spawn(async move { while rx.recv().await.is_some() {} });
    let _ = runner.run("probe the tree", &tx).await;
    drop(tx);
    let _ = drain.await;

    // Every transcript-scoped map on the engine must be empty — the
    // dispatch created a fresh key, and the run's cleanup must have
    // forgotten it.
    let leaks = engine.transcript_scoped_key_count_for_tests().await;
    assert_eq!(
        leaks, 0,
        "swarm run left {leaks} per-transcript entries behind; \
         forget_transcript must clear every transcript-scoped map",
    );
    assert_eq!(
        engine.blackboard_viewer_count_for_tests().await,
        0,
        "blackboard viewer subscription must be cleared (subsumed by \
         forget_transcript)",
    );
}

/// Contract test for the pair of operations the retry loop relies on:
/// `forget_transcript` clears *every* transcript-scoped map (including
/// the per-dispatch subset — write-set claim, working-dir override,
/// blackboard-viewer subscription), and re-applying that per-dispatch
/// subset after a `forget_transcript` restores the exact state the
/// retry needs.
///
/// Pre-fix, `forget_transcript` cleared only three maps, so a retry
/// silently lost its `blackboard_viewer` subscription (and any other
/// per-transcript entry the run had populated). The fix has two
/// halves — make `forget_transcript` exhaustive, and re-apply the
/// per-dispatch subset at the top of every retry attempt. This test
/// pins both halves.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn forget_then_reapply_restores_per_dispatch_state() {
    struct NullProvider;

    #[async_trait]
    impl LlmProvider for NullProvider {
        fn name(&self) -> &str {
            "null"
        }
        async fn list_models(&self) -> kod_error::Result<Vec<kod_provider::ModelInfo>> {
            Ok(vec![])
        }
        async fn generate(&self, _: &str, _: &GenerationOptions) -> kod_error::Result<String> {
            Ok("ok".into())
        }
        async fn generate_with_tools(
            &self,
            _: &str,
            _: &[ToolDefinition],
            _: &GenerationOptions,
        ) -> kod_error::Result<GenerationResponse> {
            Ok(GenerationResponse::Text {
                content: "ok".into(),
                usage: None,
            })
        }
        fn stream(
            &self,
            _: &str,
            _: &GenerationOptions,
        ) -> Pin<Box<dyn futures::Stream<Item = kod_error::Result<StreamChunk>> + Send + '_>>
        {
            Box::pin(futures::stream::empty())
        }
    }

    let provider: Arc<dyn LlmProvider> = Arc::new(NullProvider);
    let (engine, _tmp) = build_engine(provider).await;

    let key = "swarm:test-dispatch";
    let globs = vec!["out/x.txt".to_string()];

    // Pre-state: nothing set.
    assert!(engine.write_globs_for(key).await.is_none());
    assert!(engine.only_write_globs_for_tests().await.is_none());
    assert_eq!(engine.blackboard_viewer_count_for_tests().await, 0);

    // Apply per-dispatch state (what the runner does at dispatch time).
    engine
        .set_transcript_write_globs(key, Some(globs.clone()))
        .await;
    engine.set_blackboard_viewer(key, true).await;
    assert_eq!(engine.write_globs_for(key).await, Some(globs.clone()));
    assert_eq!(engine.blackboard_viewer_count_for_tests().await, 1);

    // Forget — what the retry loop does between attempts.
    engine.forget_transcript(key).await;

    // Every transcript-scoped map is empty. This is the exhaustive
    // half of the fix: pre-fix `blackboard_viewer` survived here.
    assert!(engine.write_globs_for(key).await.is_none());
    assert_eq!(
        engine.blackboard_viewer_count_for_tests().await,
        0,
        "forget_transcript must clear the blackboard-viewer subscription",
    );
    assert_eq!(
        engine.transcript_scoped_key_count_for_tests().await,
        0,
        "forget_transcript must clear every transcript-scoped map",
    );

    // Re-apply — what the retry loop does at the top of each attempt.
    engine
        .set_transcript_write_globs(key, Some(globs.clone()))
        .await;
    engine.set_blackboard_viewer(key, true).await;

    // State is back, byte-for-byte.
    assert_eq!(engine.write_globs_for(key).await, Some(globs));
    assert_eq!(engine.blackboard_viewer_count_for_tests().await, 1);

    // Clean up so the run's own end-of-run invariant (nothing left
    // behind) is not affected by this test's scratch state.
    engine.forget_transcript(key).await;
    assert_eq!(engine.transcript_scoped_key_count_for_tests().await, 0);
}

/// Regression: two concurrent `SwarmRunner::run` on the same engine
/// must not proceed. `kod serve` spawns a task per `swarm` request;
/// without an exclusivity guard both runs call
/// `install_swarm_file_bus` (the second overwrites the first) and
/// both use the shared hub (the second's `clear_all()` wipes the
/// first's agents). The fix takes an exclusive slot at the top of
/// `run` and fails the second caller fast.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_swarm_runs_on_one_engine_are_refused() {
    // Provider whose per-agent call blocks until the test releases
    // it, so the first run is still in-flight when the second tries
    // to start.
    struct BlockingProvider {
        release: Arc<tokio::sync::Semaphore>,
        started: Arc<tokio::sync::Semaphore>,
    }

    #[async_trait]
    impl LlmProvider for BlockingProvider {
        fn name(&self) -> &str {
            "blocking"
        }
        async fn list_models(&self) -> kod_error::Result<Vec<kod_provider::ModelInfo>> {
            Ok(vec!["blocking".into()])
        }
        async fn generate(
            &self,
            prompt: &str,
            _o: &GenerationOptions,
        ) -> kod_error::Result<String> {
            if prompt.contains("Split this goal") {
                return Ok(r#"[{"name":"a","description":"do a"}]"#.to_string());
            }
            if prompt.contains("Synthesize their work") {
                return Ok("MERGED".to_string());
            }
            // Per-agent call: signal the test, wait for release.
            // Semaphore permits persist if nobody is waiting yet, so
            // the signal is not lost when the provider reaches this
            // line before the test has registered its waiter. A
            // `Notify::notify_waiters` here was the source of a
            // 120s+ flake: the signal fired, no waiter was registered,
            // and the test then hung on `.notified()`.
            self.started.add_permits(1);
            let _ = self.release.acquire().await;
            Ok("done".to_string())
        }
        async fn generate_with_tools(
            &self,
            prompt: &str,
            _t: &[ToolDefinition],
            o: &GenerationOptions,
        ) -> kod_error::Result<GenerationResponse> {
            Ok(GenerationResponse::Text {
                content: self.generate(prompt, o).await?,
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
            p: &'a str,
            _t: &'a [ToolDefinition],
            o: &'a GenerationOptions,
        ) -> Pin<Box<dyn futures::Stream<Item = kod_error::Result<StreamChunk>> + Send + 'a>>
        {
            Box::pin(async_stream::stream! {
                match self.generate(p, o).await {
                    Ok(text) => {
                        yield Ok(StreamChunk::Text(text));
                        yield Ok(StreamChunk::Done);
                    }
                    Err(e) => yield Err(e),
                }
            })
        }
    }

    let provider = Arc::new(BlockingProvider {
        release: Arc::new(tokio::sync::Semaphore::new(0)),
        started: Arc::new(tokio::sync::Semaphore::new(0)),
    });
    let (engine, _tmp) = build_engine(provider.clone()).await;

    let runner = Arc::new(SwarmRunner::new(engine.clone(), 2, false).await.unwrap());

    // Start the first run in the background; it will block on the
    // provider's per-agent call.
    let runner_a = runner.clone();
    let (tx_a, mut rx_a) = mpsc::channel::<SwarmEvent>(64);
    let handle_a = tokio::spawn(async move {
        let _drain = tokio::spawn(async move { while rx_a.recv().await.is_some() {} });
        runner_a.run("first swarm", &tx_a).await
    });
    // Wait for the provider to see the per-agent call, which means
    // run A has the slot. `acquire` consumes one permit; the
    // provider's `add_permits(1)` is what produced it.
    let _ = provider.started.acquire().await;

    // Second run on the same engine must be refused.
    let (tx_b, mut rx_b) = mpsc::channel::<SwarmEvent>(8);
    let drain_b = tokio::spawn(async move { while rx_b.recv().await.is_some() {} });
    let err = runner
        .run("second swarm", &tx_b)
        .await
        .expect_err("second concurrent run must be refused");
    drop(tx_b);
    let _ = drain_b.await;
    assert!(
        matches!(err, kod_error::KodError::InvalidState(_)),
        "expected InvalidState, got {err:?}",
    );
    assert!(
        err.to_string().contains("already running"),
        "error must name the conflict: {err}",
    );

    // Release run A and join it.
    provider.release.add_permits(1);
    let resp_a = handle_a.await.unwrap();
    assert!(
        resp_a.is_ok(),
        "first run must complete once the provider is released: {resp_a:?}",
    );

    // Slot must be released, so a third run is admitted.
    let (tx_c, mut rx_c) = mpsc::channel::<SwarmEvent>(8);
    let drain_c = tokio::spawn(async move { while rx_c.recv().await.is_some() {} });
    // Immediately release the provider so this run does not block.
    // The third run may spawn multiple agents, each of which
    // acquires one permit; issue enough to cover the wave.
    provider.release.add_permits(16);
    let resp_c = runner.run("third swarm", &tx_c).await;
    drop(tx_c);
    let _ = drain_c.await;
    assert!(
        resp_c.is_ok(),
        "a run after the first finished must be admitted: {resp_c:?}",
    );
}

/// Regression: a swarm run cancelled by dropping its future (the TUI
/// aborts `gen_task` on Esc; `serve` drops the run handle on client
/// disconnect) must not leave the engine holding a stale file-touch
/// bus. Pre-fix, `uninstall_swarm_file_bus` sat after an `await` at
/// the end of `run`, so a dropped future never reached it; the next
/// non-swarm tool call would see `Some(bus)` and install a
/// file-touch hook that published on a bus nobody was reading.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn cancelled_swarm_run_leaves_no_bus_installed() {
    struct BlockingProvider {
        started: Arc<tokio::sync::Notify>,
        release: Arc<tokio::sync::Notify>,
    }

    #[async_trait]
    impl LlmProvider for BlockingProvider {
        fn name(&self) -> &str {
            "blocking"
        }
        async fn list_models(&self) -> kod_error::Result<Vec<kod_provider::ModelInfo>> {
            Ok(vec!["blocking".into()])
        }
        async fn generate(
            &self,
            prompt: &str,
            _o: &GenerationOptions,
        ) -> kod_error::Result<String> {
            if prompt.contains("Split this goal") {
                return Ok(r#"[{"name":"a","description":"do a"}]"#.to_string());
            }
            if prompt.contains("Synthesize their work") {
                return Ok("MERGED".to_string());
            }
            self.started.notify_waiters();
            self.release.notified().await;
            Ok("done".to_string())
        }
        async fn generate_with_tools(
            &self,
            prompt: &str,
            _t: &[ToolDefinition],
            o: &GenerationOptions,
        ) -> kod_error::Result<GenerationResponse> {
            Ok(GenerationResponse::Text {
                content: self.generate(prompt, o).await?,
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
            p: &'a str,
            _t: &'a [ToolDefinition],
            o: &'a GenerationOptions,
        ) -> Pin<Box<dyn futures::Stream<Item = kod_error::Result<StreamChunk>> + Send + 'a>>
        {
            Box::pin(async_stream::stream! {
                match self.generate(p, o).await {
                    Ok(text) => {
                        yield Ok(StreamChunk::Text(text));
                        yield Ok(StreamChunk::Done);
                    }
                    Err(e) => yield Err(e),
                }
            })
        }
    }

    let provider = Arc::new(BlockingProvider {
        started: Arc::new(tokio::sync::Notify::new()),
        release: Arc::new(tokio::sync::Notify::new()),
    });
    let (engine, _tmp) = build_engine(provider.clone()).await;

    // Precondition: no bus installed.
    assert!(!engine.swarm_file_bus_installed_for_tests());

    let runner = Arc::new(SwarmRunner::new(engine.clone(), 2, false).await.unwrap());
    let (tx, mut rx) = mpsc::channel::<SwarmEvent>(64);
    let drain = tokio::spawn(async move { while rx.recv().await.is_some() {} });
    let runner_for_task = runner.clone();
    let run_handle = tokio::spawn(async move { runner_for_task.run("cancel me", &tx).await });

    // Wait for the provider's per-agent call, which only fires after
    // the bus is installed.
    provider.started.notified().await;
    assert!(
        engine.swarm_file_bus_installed_for_tests(),
        "the bus must be installed while a run is in-flight",
    );

    // Drop the run future mid-flight (TUI cancellation shape).
    run_handle.abort();
    let _ = run_handle.await;

    // Give the drop-triggered cleanup a moment.
    for _ in 0..50 {
        if !engine.swarm_file_bus_installed_for_tests() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }

    assert!(
        !engine.swarm_file_bus_installed_for_tests(),
        "a dropped swarm-run future must uninstall the file-touch bus",
    );

    drop(drain);
}

/// Regression: a swarm run cancelled by dropping its future must
/// leave no agents registered on the engine's shared hub. Pre-fix,
/// `swarm.shutdown()` + `hub.clear_all()` sat at the bottom of
/// `run()`; a dropped future never reached them, so every cancelled
/// run left its pool agents on the engine hub for the life of the
/// process — the blackboard kept returning the cancelled run's
/// entries.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn cancelled_swarm_run_leaves_no_agents_on_the_hub() {
    struct BlockingProvider {
        started: Arc<tokio::sync::Semaphore>,
        release: Arc<tokio::sync::Semaphore>,
    }

    #[async_trait]
    impl LlmProvider for BlockingProvider {
        fn name(&self) -> &str {
            "blocking"
        }
        async fn list_models(&self) -> kod_error::Result<Vec<kod_provider::ModelInfo>> {
            Ok(vec!["blocking".into()])
        }
        async fn generate(
            &self,
            prompt: &str,
            _o: &GenerationOptions,
        ) -> kod_error::Result<String> {
            if prompt.contains("Split this goal") {
                return Ok(r#"[{"name":"a","description":"do a"}]"#.to_string());
            }
            if prompt.contains("Synthesize their work") {
                return Ok("MERGED".to_string());
            }
            self.started.add_permits(1);
            let _ = self.release.acquire().await;
            Ok("done".to_string())
        }
        async fn generate_with_tools(
            &self,
            prompt: &str,
            _t: &[ToolDefinition],
            o: &GenerationOptions,
        ) -> kod_error::Result<GenerationResponse> {
            Ok(GenerationResponse::Text {
                content: self.generate(prompt, o).await?,
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
            p: &'a str,
            _t: &'a [ToolDefinition],
            o: &'a GenerationOptions,
        ) -> Pin<Box<dyn futures::Stream<Item = kod_error::Result<StreamChunk>> + Send + 'a>>
        {
            Box::pin(async_stream::stream! {
                match self.generate(p, o).await {
                    Ok(text) => {
                        yield Ok(StreamChunk::Text(text));
                        yield Ok(StreamChunk::Done);
                    }
                    Err(e) => yield Err(e),
                }
            })
        }
    }

    let provider = Arc::new(BlockingProvider {
        started: Arc::new(tokio::sync::Semaphore::new(0)),
        release: Arc::new(tokio::sync::Semaphore::new(0)),
    });
    let (engine, _tmp) = build_engine(provider.clone()).await;

    // Precondition: hub has no agents registered.
    assert_eq!(
        engine.swarm_hub().list_agents().await.len(),
        0,
        "a fresh engine hub has no agents",
    );

    let runner = Arc::new(SwarmRunner::new(engine.clone(), 2, false).await.unwrap());
    let (tx, mut rx) = mpsc::channel::<SwarmEvent>(64);
    let drain = tokio::spawn(async move { while rx.recv().await.is_some() {} });
    let runner_for_task = runner.clone();
    let run_handle = tokio::spawn(async move { runner_for_task.run("cancel me", &tx).await });

    // Wait for the per-agent call — by then the pool is on the hub.
    let _ = provider.started.acquire().await;
    assert!(
        engine.swarm_hub().list_agents().await.len() > 0,
        "a running swarm must have pool agents on the hub",
    );

    // Drop the run future mid-flight (TUI cancellation shape).
    run_handle.abort();
    let _ = run_handle.await;

    // The guard's spawned cleanup runs on the same runtime; poll.
    for _ in 0..50 {
        if engine.swarm_hub().list_agents().await.is_empty() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }

    assert_eq!(
        engine.swarm_hub().list_agents().await.len(),
        0,
        "a cancelled swarm run must leave no agents registered on the \
         engine's shared hub",
    );
    // Every transcript-scoped map on the engine must also be clear:
    // a dropped run future never reaches the runner's normal
    // end-of-run `forget_transcript` loop, so the guard's own
    // cleanup has to be what forgets them.
    assert_eq!(
        engine.transcript_scoped_key_count_for_tests().await,
        0,
        "a cancelled swarm run must forget every per-transcript entry \
         it created",
    );

    drop(drain);
}

/// Regression: `shutdown()` must drain the pending approval and
/// question maps. Pre-fix, a prompt parked on an approval oneshot
/// waited out the full `AWAIT_APPROVAL_SECS` (up to 120 s) after
/// `/quit`, because nothing told the parked sender its counterpart
/// was gone; and the entry itself lived until process teardown.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn shutdown_drains_pending_approvals_and_questions() {
    struct NullProvider;

    #[async_trait]
    impl LlmProvider for NullProvider {
        fn name(&self) -> &str {
            "null"
        }
        async fn list_models(&self) -> kod_error::Result<Vec<kod_provider::ModelInfo>> {
            Ok(vec![])
        }
        async fn generate(&self, _: &str, _: &GenerationOptions) -> kod_error::Result<String> {
            Ok("ok".into())
        }
        async fn generate_with_tools(
            &self,
            _: &str,
            _: &[ToolDefinition],
            _: &GenerationOptions,
        ) -> kod_error::Result<GenerationResponse> {
            Ok(GenerationResponse::Text {
                content: "ok".into(),
                usage: None,
            })
        }
        fn stream(
            &self,
            _: &str,
            _: &GenerationOptions,
        ) -> Pin<Box<dyn futures::Stream<Item = kod_error::Result<StreamChunk>> + Send + '_>>
        {
            Box::pin(futures::stream::empty())
        }
    }

    let provider: Arc<dyn LlmProvider> = Arc::new(NullProvider);
    let (engine, _tmp) = build_engine(provider).await;

    engine.plant_pending_approval_for_tests().await;
    engine.plant_pending_approval_for_tests().await;
    engine.plant_pending_question_for_tests().await;
    assert_eq!(engine.pending_approval_count_for_tests().await, 2);
    assert_eq!(engine.pending_question_count_for_tests().await, 1);

    engine.shutdown().await.unwrap();

    assert_eq!(
        engine.pending_approval_count_for_tests().await,
        0,
        "shutdown must drain the pending-approval map",
    );
    assert_eq!(
        engine.pending_question_count_for_tests().await,
        0,
        "shutdown must drain the pending-question map",
    );
}
