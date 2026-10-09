//! F2d-11: does a failed subtask gate its dependents?
//!
//! Decompose returns a chain a → b → c. Subtask `a` fails; `b`
//! depends on `a`; `c` depends on both. The test records the order in
//! which the three agents' prompts reach the provider, which reveals
//! the wave composition.

use async_trait::async_trait;
use kod_core::{KodEngine, RouterConfig, SwarmEvent, SwarmRunner};
use kod_provider::{GenerationOptions, GenerationResponse, LlmProvider, StreamChunk};
use kod_types::ToolDefinition;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use tempfile::TempDir;
use tokio::sync::mpsc;

use crate::common::install_test_provider::install_test_provider;

#[derive(Default)]
struct Order {
    calls: Mutex<Vec<String>>,
}

struct ChainProvider {
    order: Arc<Order>,
}

#[async_trait]
impl LlmProvider for ChainProvider {
    fn name(&self) -> &str {
        "chain"
    }
    async fn list_models(&self) -> kod_error::Result<Vec<kod_provider::ModelInfo>> {
        Ok(vec!["chain".into()])
    }
    async fn generate(&self, prompt: &str, _o: &GenerationOptions) -> kod_error::Result<String> {
        if prompt.contains("Split this goal") {
            return Ok(r#"[
                {"name":"a","description":"task A","depends_on":[]},
                {"name":"b","description":"task B","depends_on":["a"]},
                {"name":"c","description":"task C","depends_on":["a","b"]}
            ]"#
            .to_string());
        }
        if prompt.contains("Synthesize their work") {
            return Ok("MERGED".to_string());
        }
        // Identify which subtask by its description token.
        for tag in ["task A", "task B", "task C"] {
            if prompt.contains(tag) {
                self.order.calls.lock().unwrap().push(tag.to_string());
                if tag == "task A" {
                    // Fail subtask A.
                    return Err(kod_error::KodError::Provider("A failed".into()));
                }
                return Ok(format!("did {tag}"));
            }
        }
        Ok("(unknown)".to_string())
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
    ) -> Pin<Box<dyn futures::Stream<Item = kod_error::Result<StreamChunk>> + Send + '_>> {
        Box::pin(futures::stream::empty())
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn failed_dependency_gates_dependents() {
    let tmp = TempDir::new().unwrap();
    let cfg = RouterConfig {
        working_dir: tmp.path().to_path_buf(),
        enable_memory: false,
        ..RouterConfig::default()
    };
    let db = tmp.path().join("t.redb");
    let engine = Arc::new(KodEngine::new(cfg, db).unwrap());
    let order = Arc::new(Order::default());
    install_test_provider(
        &engine,
        Arc::new(ChainProvider {
            order: order.clone(),
        }),
    )
    .await;
    engine.start().await.unwrap();

    let runner = SwarmRunner::new(engine.clone(), 4, false).await.unwrap();
    let (tx, mut rx) = mpsc::channel::<SwarmEvent>(256);
    let drain = tokio::spawn(async move { while rx.recv().await.is_some() {} });
    let resp = runner.run("do the chain", &tx).await;
    drop(tx);
    let _ = drain.await;

    let calls = order.calls.lock().unwrap().clone();

    // F2d-11: A failed, so B (depends on A) and C (depends on A and B)
    // must NOT run. Only A's prompt reaches the provider.
    //
    // A itself retries per the runner's default `agent_retries = 1`:
    // the first attempt fails, the retry runs, and the retry also
    // fails (the test provider always fails for `task A`). Both
    // attempts are A; no other subtask runs.
    //
    // (Pre-fix, the retry path was unreachable — a `request_cancel_for`
    // before the `is_cancelled_for` check made the branch always
    // return "a coordinator cancelled this agent". That is why the
    // pre-fix test asserted a single call; it was describing the bug.)
    let non_a: Vec<&String> = calls.iter().filter(|c| c.as_str() != "task A").collect();
    assert!(
        non_a.is_empty(),
        "a failed dependency must gate its dependents; only A ran, got {calls:?}",
    );
    assert!(
        !calls.is_empty(),
        "A must have run (at least once) before failing: {calls:?}",
    );

    let r = resp.expect("run completes even when a subtask fails");
    // All three subtasks still appear in the response — the two that
    // were skipped are reported as failures, not silently dropped.
    assert_eq!(r.per_agent.len(), 3, "every subtask is reported");
    let completed = r
        .per_agent
        .iter()
        .filter(|a| matches!(a.outcome, kod_core::AgentOutcome::Completed(_)))
        .count();
    let failed = r
        .per_agent
        .iter()
        .filter(|a| matches!(a.outcome, kod_core::AgentOutcome::Failed(_)))
        .count();
    assert_eq!(completed, 0, "A failed, so nothing completed");
    assert_eq!(failed, 3, "A failed and B/C were failed-out by the gate");
}
