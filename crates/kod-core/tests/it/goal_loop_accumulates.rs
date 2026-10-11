//! The goal loop's per-turn accumulation (AD-01 migration guard).
//!
//! `process_goal_streaming_for` runs turn by turn until the model
//! declares the goal met. Each turn's `CompletionRequest` must include
//! everything the previous turns produced — assistant text, tool
//! calls, tool results. Pre-migration the text `pending` was mutated
//! in place, so turn 2 saw turn 1; the structured migration had to
//! preserve that by accumulating `messages` across turns.
//!
//! This test drives a two-turn goal with a mock provider that records
//! the message count per call and declares `GOAL MET` on turn 2. Turn
//! 2's request must carry more messages than turn 1's.

use async_trait::async_trait;
use futures::Stream;
use kod_core::KodEngine;
use kod_core::router::RouterConfig;
use kod_error::Result;
use kod_provider::request::CompletionRequest;
use kod_provider::traits::GenerationOptions;
use kod_provider::{GenerationResponse, LlmProvider, ModelRef, ProviderCapabilities, StreamChunk};
use kod_types::ToolDefinition;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use tempfile::TempDir;

struct TwoTurnProvider {
    calls: Arc<Mutex<Vec<usize>>>, // message count per call
    turn: Arc<Mutex<usize>>,
}

#[async_trait]
impl LlmProvider for TwoTurnProvider {
    fn name(&self) -> &str {
        "two-turn"
    }
    async fn list_models(&self) -> Result<Vec<kod_provider::ModelInfo>> {
        Ok(vec![])
    }
    async fn generate(&self, _p: &str, _o: &GenerationOptions) -> Result<String> {
        Ok(String::new())
    }
    async fn generate_with_tools(
        &self,
        _p: &str,
        _t: &[ToolDefinition],
        _o: &GenerationOptions,
    ) -> Result<GenerationResponse> {
        Ok(GenerationResponse::Text {
            content: String::new(),
            usage: None,
        })
    }
    async fn complete(&self, req: &CompletionRequest) -> Result<GenerationResponse> {
        self.calls.lock().unwrap().push(req.messages.len());
        Ok(GenerationResponse::Text {
            content: "goal met summary".into(),
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
        req: &'a CompletionRequest,
    ) -> Pin<Box<dyn Stream<Item = Result<StreamChunk>> + Send + 'a>> {
        self.calls.lock().unwrap().push(req.messages.len());
        let mut turn = self.turn.lock().unwrap();
        *turn += 1;
        let text = if *turn >= 2 {
            // Declare the goal met on turn 2 so the loop stops after
            // two provider calls.
            "done\nGOAL MET"
        } else {
            "still working"
        }
        .to_string();
        drop(turn);
        Box::pin(async_stream::stream! {
            yield Ok(StreamChunk::Text(text));
            yield Ok(StreamChunk::Done);
        })
    }
}

async fn engine_with_provider(provider: Arc<TwoTurnProvider>) -> (TempDir, Arc<KodEngine>) {
    let tmp = TempDir::new().unwrap();
    let db = tmp.path().join("t.redb");
    let cfg = RouterConfig {
        skill_threshold: 0.3,
        context_window: 8192,
        short_term_capacity: 100,
        working_dir: tmp.path().to_path_buf(),
        enable_memory: false,
        max_skills_per_query: 3,
        embedder: None,
    };
    let engine = Arc::new(KodEngine::new(cfg, db).unwrap());
    let mut registry = kod_provider::ProviderRegistry::new();
    registry.insert(
        "default",
        provider,
        ProviderCapabilities::conservative(),
        "test",
    );
    engine
        .set_registry(Arc::new(registry), ModelRef::new("default", "test"), None)
        .await;
    engine.start().await.unwrap();
    (tmp, engine)
}

/// A variant of `engine_with_provider` that accepts any provider as a
/// trait object. The budget regression only needs a running engine; it
/// does not inspect the provider afterward, so it does not need the
/// concrete type.
async fn engine_with_any_provider(
    provider: Arc<dyn LlmProvider>,
) -> (TempDir, Arc<KodEngine>) {
    let tmp = TempDir::new().unwrap();
    let db = tmp.path().join("t.redb");
    let cfg = RouterConfig {
        skill_threshold: 0.3,
        context_window: 8192,
        short_term_capacity: 100,
        working_dir: tmp.path().to_path_buf(),
        enable_memory: false,
        max_skills_per_query: 3,
        embedder: None,
    };
    let engine = Arc::new(KodEngine::new(cfg, db).unwrap());
    let mut registry = kod_provider::ProviderRegistry::new();
    registry.insert(
        "default",
        provider,
        ProviderCapabilities::conservative(),
        "test",
    );
    engine
        .set_registry(Arc::new(registry), ModelRef::new("default", "test"), None)
        .await;
    engine.start().await.unwrap();
    (tmp, engine)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn goal_loop_second_turn_sees_first_turn_output() {
    let provider = Arc::new(TwoTurnProvider {
        calls: Arc::new(Mutex::new(Vec::new())),
        turn: Arc::new(Mutex::new(0)),
    });
    let (_tmp, engine) = engine_with_provider(provider.clone()).await;

    let (tx, mut rx) = tokio::sync::mpsc::channel::<String>(64);
    let drain = tokio::spawn(async move { while rx.recv().await.is_some() {} });

    let _ = engine
        .process_goal_streaming("do the work", "finish the task", &tx)
        .await;
    drop(tx);
    let _ = drain.await;

    let calls = provider.calls.lock().unwrap().clone();
    assert!(
        calls.len() >= 2,
        "the goal loop should have run at least two turns, got {}",
        calls.len(),
    );
    // Turn 1 carries the initial history (the user's turn). Turn 2
    // must carry strictly more — the turn-1 assistant reply plus the
    // per-turn nudge — because `goal_messages` accumulated them.
    assert!(
        calls[1] > calls[0],
        "turn 2 must see more messages than turn 1 (turn 1 had {}, turn 2 had {}); \
         a regression that reset `messages` per turn would leave them equal",
        calls[0],
        calls[1],
    );
}


/// Regression: `/goal --budget N` must actually cap the loop. Pre-fix
/// `GoalRuntime::start` created a goal with both budgets `None` and
/// nothing else ever set them, so the budget check in
/// `process_goal_streaming_for` was unreachable — `agent_retries`-style
/// dead code. This test drives the goal loop with a provider that
/// always reports 200 prompt tokens and never declares GOAL MET. With
/// `token_budget = 100` the runtime must flip to `BudgetLimited` after
/// turn 1, the loop must break (not run MAX_GOAL_TURNS), and exactly
/// one steer must appear on the chunk channel.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn goal_budget_flips_to_budget_limited_and_emits_one_steer() {
    struct BudgetBuster;

    #[async_trait]
    impl LlmProvider for BudgetBuster {
        fn name(&self) -> &str {
            "budget-buster"
        }
        async fn list_models(&self) -> Result<Vec<kod_provider::ModelInfo>> {
            Ok(vec![])
        }
        async fn generate(&self, _p: &str, _o: &GenerationOptions) -> Result<String> {
            Ok(String::new())
        }
        async fn generate_with_tools(
            &self,
            _p: &str,
            _t: &[ToolDefinition],
            _o: &GenerationOptions,
        ) -> Result<GenerationResponse> {
            Ok(GenerationResponse::Text {
                content: String::new(),
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
            // The goal loop reads `stream_completion`, not
            // `stream_with_tools`. Emit one turn's worth of text plus
            // a Usage chunk that pushes the goal over its token cap.
            // No `GOAL MET` — the budget is the only thing that may
            // stop the loop.
            Box::pin(futures::stream::iter(vec![
                Ok(StreamChunk::Text("still working".into())),
                Ok(StreamChunk::Usage(kod_provider::TokenUsage {
                    prompt_tokens: 200,
                    completion_tokens: 1,
                    ..Default::default()
                })),
                Ok(StreamChunk::Done),
            ]))
        }
    }

    let (_tmp, engine) = engine_with_any_provider(Arc::new(BudgetBuster)).await;

    let (tx, mut rx) = tokio::sync::mpsc::channel::<String>(64);
    let drain = tokio::spawn(async move {
        let mut steers = 0usize;
        while let Some(s) = rx.recv().await {
            if s.contains("goal budget exhausted") {
                steers += 1;
            }
        }
        steers
    });

    let resp = engine
        .process_goal_streaming_with_budget(
            "do the work",
            "finish the task",
            kod_core::GoalBudget {
                token: Some(100),
                time_seconds: None,
            },
            &tx,
        )
        .await
        .expect("the goal loop must return Ok when only the budget stops it");
    drop(tx);
    let steers = drain.await.unwrap();
    assert_eq!(
        steers, 1,
        "the budget steer is once per goal, saw {steers} on the chunk channel",
    );

    let text = resp.text.unwrap_or_default();
    assert!(
        text.contains("budget exhausted"),
        "the terminal text must report the budget stop; got: {text}",
    );

    // The runtime records the terminal status; a subsequent prompt
    // re-evaluates the same goal, so the flag must persist too.
    let g = engine
        .current_goal()
        .await
        .expect("the goal is retained after budget-limited stop");
    assert_eq!(g.status, kod_core::GoalStatus::BudgetLimited);
    assert!(g.budget_steer_emitted, "the steer must be marked emitted");
    assert!(
        g.tokens_used >= 100,
        "the observed usage must be recorded (got {})",
        g.tokens_used,
    );
}
