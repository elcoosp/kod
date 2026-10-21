//! `SwarmRunner::run` end-to-end over a scripted mock provider.
//!
//! The runner is the entire embedded-swarm path: decompose the goal,
//! drive one agent per subtask, merge the results. `swarm.rs` in the
//! CLI is a thin printer around it, and `swarm_from_config.rs` pins
//! the config mapping — but nothing exercised `run()` itself, so every
//! branch inside it (event sequence, merge-or-concatenate, per-agent
//! dispatch) was uncovered.
//!
//! The mock returns one subtask at decompose time, one text reply for
//! the agent, and — because `merge_results = false` — no third call.
//! The script has exactly two entries and is consumed in order, so
//! the test is deterministic.
use std::sync::Arc;

use kod_config::SwarmConfig;
use kod_core::swarm_runner::{SwarmEvent, SwarmRunner};
use kod_core::{KodEngine, RouterConfig};
use kod_provider::testkit::{MockProvider, Script};
use kod_provider::{ModelRef, ProviderCapabilities, ProviderRegistry};
use tokio::sync::mpsc;
use tokio::time::{Duration, timeout};

/// One well-formed subtask as the decompose prompt expects it. The
/// shape matches what `parse_subtasks` reads: `name`, `description`,
/// plus the optional `expected_writes`, `capability`, `depends_on`.
const ONE_SUBTASK: &str = r#"[{"name":"alpha","description":"do the thing","expected_writes":[],"capability":"coding","depends_on":[]}]"#;

async fn engine_with_mock(script: Vec<Script>, tmp: &tempfile::TempDir) -> Arc<KodEngine> {
    engine_with_mock_window(script, tmp, 32768).await
}

async fn engine_with_mock_window(
    script: Vec<Script>,
    tmp: &tempfile::TempDir,
    context_window: usize,
) -> Arc<KodEngine> {
    // The swarm's system prompt + tool schemas is ~17 KB. 32768 gives
    // ~24,576 chars of prompt budget (1.5x headroom); 8192 gives
    // ~6,144 chars, small enough that every agent fails with
    // "request too long" — used by the permanent-error test below.
    let cfg = RouterConfig {
        context_window,
        short_term_capacity: 100,
        ..Default::default()
    };
    let engine = Arc::new(KodEngine::new(cfg, tmp.path().join("swarm.redb")).unwrap());

    let provider = Arc::new(MockProvider::scripted("mock", script));
    let mut registry = ProviderRegistry::new();
    registry.insert(
        "mock",
        provider,
        ProviderCapabilities::conservative(),
        "mock-model",
    );
    let mut routing = kod_config::RoutingConfig::default();
    routing
        .by_task
        .insert("Simple".to_string(), "mock".to_string());
    engine
        .set_registry(
            Arc::new(registry),
            ModelRef::new("mock", "mock-model"),
            Some(routing),
        )
        .await;

    engine.start().await.unwrap();
    engine
}

/// The happy path: one subtask, one agent, merge disabled. Asserts the
/// event sequence names a decomposition and an agent lifecycle, and
/// that the response carries the concatenation rather than an
/// LLM-merged summary.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn run_produces_decomposed_and_agent_events_over_a_mock_provider() {
    let tmp = tempfile::TempDir::new().unwrap();
    let script = vec![
        Script::Text(ONE_SUBTASK.to_string()),
        Script::Text("agent said done".to_string()),
    ];
    let engine = engine_with_mock(script, &tmp).await;

    let cfg = SwarmConfig {
        max_agents: 2,
        merge_results: false,
        isolation: kod_config::Isolation::Shared,
        ..Default::default()
    };
    let runner = SwarmRunner::from_config(engine.clone(), &cfg)
        .await
        .expect("from_config");

    let (tx, mut rx) = mpsc::channel::<SwarmEvent>(256);
    let collector = tokio::spawn(async move {
        let mut out = Vec::new();
        while let Some(ev) = rx.recv().await {
            out.push(ev);
        }
        out
    });

    let resp = timeout(Duration::from_secs(30), runner.run("do the thing", &tx))
        .await
        .expect("run must not hang")
        .expect("run should succeed over a scripted provider");
    drop(tx);
    let events = collector.await.expect("collector");

    // The decompose step fired and named a subtask.
    let decomposed = events
        .iter()
        .any(|e| matches!(e, SwarmEvent::Decomposed(_)));
    assert!(decomposed, "no Decomposed event; got {events:?}");

    // At least one agent started and completed.
    let started = events
        .iter()
        .any(|e| matches!(e, SwarmEvent::AgentStarted { .. }));
    let completed = events
        .iter()
        .any(|e| matches!(e, SwarmEvent::AgentCompleted { .. }));
    assert!(started, "no AgentStarted event; got {events:?}");
    assert!(completed, "no AgentCompleted event; got {events:?}");

    // merge_results = false: the response says so, and the merged body
    // carries the agent's text.
    assert!(
        !resp.merged_by_model,
        "merge_results=false must not produce an LLM-merged response",
    );
    assert!(
        resp.merged.contains("agent said done"),
        "concatenated merge lost the agent text: {:?}",
        resp.merged,
    );

    engine.shutdown().await.expect("shutdown");
}

/// **A permanent error is not retried.** Before the fix, the swarm
/// retried every failure once regardless of classification, so an
/// agent whose request could never fit re-ran and printed
/// `AgentRetrying (2/2): request too long`. The engine's own retry
/// ladders gate on `is_retryable()`; the swarm's did not, because
/// the KodError was stringified before the decision. This test pins
/// the corrected behaviour: exactly one `AgentStarted`, zero
/// `AgentRetrying`, one `AgentFailed` whose message carries the
/// real reason.
///
/// The window is deliberately 8192 — small enough that the swarm's
/// system prompt + tool schemas (~17 KB) cannot fit — so the engine
/// produces an `InvalidParameters` (classified permanent) on the
/// first attempt.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn permanent_error_is_not_retried() {
    let tmp = tempfile::TempDir::new().unwrap();
    // Only the decompose reply is consumed: the agent call fails
    // before the mock is reached, because the prompt does not fit.
    let script = vec![Script::Text(ONE_SUBTASK.to_string())];
    let engine = engine_with_mock_window(script, &tmp, 8192).await;

    let cfg = SwarmConfig {
        max_agents: 2,
        merge_results: false,
        isolation: kod_config::Isolation::Shared,
        agent_retries: 1, // allow one retry in principle; the fix must skip it
        ..Default::default()
    };
    let runner = SwarmRunner::from_config(engine.clone(), &cfg)
        .await
        .expect("from_config");

    let (tx, mut rx) = mpsc::channel::<SwarmEvent>(256);
    let collector = tokio::spawn(async move {
        let mut out = Vec::new();
        while let Some(ev) = rx.recv().await {
            out.push(ev);
        }
        out
    });

    // The run may return Ok (with no successful agents) or Err; we
    // do not care which. We care about the *events*.
    let _ = timeout(Duration::from_secs(30), runner.run("do the thing", &tx)).await;
    drop(tx);
    let events = collector.await.expect("collector");

    let starts = events
        .iter()
        .filter(|e| matches!(e, SwarmEvent::AgentStarted { .. }))
        .count();
    let retries = events
        .iter()
        .filter(|e| matches!(e, SwarmEvent::AgentRetrying { .. }))
        .count();
    let failures = events
        .iter()
        .filter(|e| matches!(e, SwarmEvent::AgentFailed { .. }))
        .count();

    assert_eq!(
        starts, 1,
        "expected exactly one AgentStarted (no retry); got {events:?}",
    );
    assert_eq!(
        retries, 0,
        "a permanent error must not emit AgentRetrying; got {events:?}",
    );
    assert_eq!(
        failures, 1,
        "expected exactly one AgentFailed; got {events:?}",
    );

    // The failure message should name the real reason — not a generic
    // "retries exhausted".
    let failed_msg = events
        .iter()
        .find_map(|e| match e {
            SwarmEvent::AgentFailed { error, .. } => Some(error.clone()),
            _ => None,
        })
        .expect("AgentFailed present");
    assert!(
        failed_msg.contains("too long") || failed_msg.contains("Invalid tool parameters"),
        "failure message did not carry the real reason: {failed_msg}",
    );

    engine.shutdown().await.expect("shutdown");
}
