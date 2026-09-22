//! End-to-end test: `/swarm <goal>` on a TuiLoop with a real
//! `KodEngine` backed by a scripted provider.
//!
//! `test_swarm_state_machine` (in app.rs) tests how a swarm event
//! mutates app state. `test_swarm_command_routing` (in main_loop.rs)
//! tests that `/swarm` reaches `dispatch_swarm`. Neither covers the
//! whole path: command → spawned runner → event pump → app state →
//! merged answer in the chat. This test does.

use async_trait::async_trait;
use kod_core::{KodEngine, RouterConfig};
use kod_provider::{GenerationOptions, GenerationResponse, LlmProvider, StreamChunk};
use kod_tui::{Event, TuiLoop};
use kod_types::ToolDefinition;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;
use tempfile::TempDir;

/// Minimal scripted provider:
///   - "Split this goal" → two subtasks
///   - "Synthesize their work" → a merged answer
///   - otherwise → a per-agent reply
struct ScriptedProvider;

#[async_trait]
impl LlmProvider for ScriptedProvider {
    fn name(&self) -> &str {
        "scripted"
    }
    async fn list_models(&self) -> kod_error::Result<Vec<kod_provider::ModelInfo>> {
        Ok(vec!["scripted".into()])
    }
    async fn generate(&self, prompt: &str, _o: &GenerationOptions) -> kod_error::Result<String> {
        if prompt.contains("Split this goal") {
            return Ok(
                r#"[{"name":"a","description":"do work a"},{"name":"b","description":"do work b"}]"#
                    .to_string(),
            );
        }
        if prompt.contains("Synthesize their work") {
            return Ok("MERGED E2E".to_string());
        }
        Ok("per-agent reply".to_string())
    }
    async fn generate_with_tools(
        &self,
        prompt: &str,
        _t: &[ToolDefinition],
        options: &GenerationOptions,
    ) -> kod_error::Result<GenerationResponse> {
        Ok(GenerationResponse::Text {
            content: self.generate(prompt, options).await?,
            usage: None,
        })
    }
    fn stream(
        &self,
        _p: &str,
        _o: &GenerationOptions,
    ) -> Pin<Box<dyn futures::Stream<Item = kod_error::Result<StreamChunk>> + Send + '_>> {
        Box::pin(futures::stream::empty())
    }
    fn stream_with_tools<'a>(
        &'a self,
        prompt: &'a str,
        _t: &'a [ToolDefinition],
        options: &'a GenerationOptions,
    ) -> Pin<Box<dyn futures::Stream<Item = kod_error::Result<StreamChunk>> + Send + 'a>> {
        let prompt = prompt.to_string();
        let opts = options.clone();
        Box::pin(async_stream::stream! {
            match self.generate(&prompt, &opts).await {
                Ok(text) => {
                    yield Ok(StreamChunk::Text(text));
                    yield Ok(StreamChunk::Done);
                }
                Err(e) => yield Err(e),
            }
        })
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn swarm_command_runs_end_to_end() {
    // Build an engine with a scripted provider and a temp DB.
    let temp = TempDir::new().unwrap();
    let cfg = RouterConfig {
        skill_threshold: 0.3,
        context_window: 8192,
        working_dir: temp.path().to_path_buf(),
        enable_memory: false,
        ..Default::default()
    };
    let engine = KodEngine::new(cfg, temp.path().join("swarm.redb")).unwrap();
    engine.start().await.unwrap();
    {
        let mut reg = kod_provider::ProviderRegistry::new();
        reg.insert(
            "default",
            Arc::new(ScriptedProvider),
            kod_provider::ProviderCapabilities::conservative(),
            "",
        );
        engine
            .set_registry(
                Arc::new(reg),
                kod_provider::ModelRef::new("default", ""),
                None,
            )
            .await;
    }

    // Wire it into a TuiLoop without going through init_engine (which
    // would need a real config file and network).
    let mut tui = TuiLoop::new();
    tui.set_engine(Arc::new(engine));

    // Fire the command. dispatch_swarm spawns a background task; the
    // events will arrive on the loop's own EventHandler.
    tui.handle_command("/swarm do the thing").await.unwrap();

    // Pump events until SwarmComplete, with a wall-clock bound so a
    // hang is a test failure, not a hang.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    let mut saw_decompose = false;
    let mut saw_merging = false;
    loop {
        let timeout = tokio::time::timeout_at(deadline, tui.next_event()).await;
        let event = match timeout {
            Ok(e) => e,
            Err(_) => panic!("swarm did not complete within 10s"),
        };
        match &event {
            Event::SwarmDecomposed(subs) => {
                assert_eq!(subs.len(), 2, "scripted decompose yields two");
                saw_decompose = true;
            }
            Event::SwarmMerging => saw_merging = true,
            _ => {}
        }
        let done = matches!(event, Event::SwarmComplete(_));
        tui.handle_event(event).await.unwrap();
        if done {
            break;
        }
    }
    assert!(saw_decompose, "decompose event not observed");
    assert!(saw_merging, "merge phase not observed");

    // The merged answer landed as the last assistant message.
    let last_assistant = tui
        .app()
        .messages()
        .iter()
        .rev()
        .find(|m| m.role == kod_types::MessageRole::Assistant)
        .expect("an assistant reply should be present");
    assert!(
        last_assistant.content.contains("MERGED E2E"),
        "got: {}",
        last_assistant.content
    );
    assert!(!tui.app().is_generating(), "generation state cleared");

    // No engine shutdown call: the engine is a plain `Arc<KodEngine>`
    // that drops with the test. `TuiLoop::main_loop` is not running, so
    // `quit()` would set a flag nothing reads.
}

/// `/swarm` while a generation is running is refused, not queued. The
/// single-agent path already prints a message; this pins that the swarm
/// path uses the same guard.
#[tokio::test]
async fn swarm_command_refuses_when_busy() {
    let temp = TempDir::new().unwrap();
    let cfg = RouterConfig {
        skill_threshold: 0.3,
        context_window: 8192,
        working_dir: temp.path().to_path_buf(),
        enable_memory: false,
        ..Default::default()
    };
    let engine = KodEngine::new(cfg, temp.path().join("swarm.redb")).unwrap();
    engine.start().await.unwrap();
    {
        let mut reg = kod_provider::ProviderRegistry::new();
        reg.insert(
            "default",
            Arc::new(ScriptedProvider),
            kod_provider::ProviderCapabilities::conservative(),
            "",
        );
        engine
            .set_registry(
                Arc::new(reg),
                kod_provider::ModelRef::new("default", ""),
                None,
            )
            .await;
    }

    let mut tui = TuiLoop::new();
    tui.set_engine(Arc::new(engine));

    // Pretend a prompt is already running.
    tui.app_mut().begin_generation();

    tui.handle_command("/swarm do the thing").await.unwrap();
    let last = tui.app().messages().last().unwrap();
    assert!(
        last.content.contains("already running"),
        "expected refusal, got: {}",
        last.content
    );
}
