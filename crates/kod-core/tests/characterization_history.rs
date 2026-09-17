//! Characterization test for engine transcript rendering (A3).
//!
//! The A3 change swaps the engine's internal transcript from
//! `Vec<HistoryTurn>` (a private `{ user: bool, text: String }`) to
//! `Vec<ChatMessage>`. The rendering of the "Conversation so far"
//! section in the provider prompt MUST stay byte-identical, or every
//! session silently re-tokenizes a different prefix than it did
//! yesterday — the exact cache-defeat failure the roadmap warns about.
//!
//! The test drives a real `KodEngine` with a capture-only provider and
//! asserts what each turn's outgoing prompt contains. It runs on the
//! pre-change code first (baseline: green), then again post-change
//! (guard: still green). Substring checks are used rather than a full
//! byte snapshot because the prompt contains environment-dependent
//! paths; the specific shape `render_history_for` emits is what
//! matters.

use async_trait::async_trait;
use futures::Stream;
use kod_core::{KodEngine, RouterConfig};
use kod_error::Result;
use kod_provider::{GenerationOptions, GenerationResponse, LlmProvider, StreamChunk};
use kod_types::ToolDefinition;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use tempfile::TempDir;

/// A provider that records every prompt it receives and always
/// responds with the same text ("ack"). No tool calls, no streaming.
struct CaptureProvider {
    prompts: Mutex<Vec<String>>,
}

impl CaptureProvider {
    fn new() -> Self {
        Self {
            prompts: Mutex::new(Vec::new()),
        }
    }

    fn prompts(&self) -> Vec<String> {
        self.prompts.lock().unwrap().clone()
    }
}

#[async_trait]
impl LlmProvider for CaptureProvider {
    fn name(&self) -> &str {
        "capture"
    }

    async fn list_models(&self) -> Result<Vec<String>> {
        Ok(vec!["capture-model".to_string()])
    }

    async fn generate(&self, _prompt: &str, _options: &GenerationOptions) -> Result<String> {
        Ok("ack".to_string())
    }

    async fn generate_with_tools(
        &self,
        prompt: &str,
        _tools: &[ToolDefinition],
        _options: &GenerationOptions,
    ) -> Result<GenerationResponse> {
        self.prompts.lock().unwrap().push(prompt.to_string());
        Ok(GenerationResponse::Text {
            content: "ack".to_string(),
            usage: None,
        })
    }

    fn stream(
        &self,
        _prompt: &str,
        _options: &GenerationOptions,
    ) -> Pin<Box<dyn Stream<Item = Result<StreamChunk>> + Send + '_>> {
        Box::pin(futures::stream::empty())
    }
}

fn engine_in(dir: &std::path::Path) -> KodEngine {
    // Seed a minimal source file so the repomap is non-empty (an
    // empty map skips a section, weakening the history rendering
    // assertions by removing surrounding context).
    std::fs::write(dir.join("main.rs"), "pub fn main() {}\n").unwrap();
    let db_path = dir.join("test.redb");
    let cfg = RouterConfig {
        working_dir: dir.to_path_buf(),
        enable_memory: false,
        max_skills_per_query: 3,
        context_window: 8192,
        short_term_capacity: 100,
    };
    KodEngine::new(cfg, db_path).unwrap()
}

/// Turn 1: history is empty → `(start of conversation)` placeholder.
/// Turn 2: history contains turn 1's exchange.
/// Turn 3: history contains turns 1 and 2, in order.
///
/// The format `"User: {text}"` / `"Assistant: {text}"` is what
/// `render_history_for` emits and what the model sees.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn history_rendering_shape_is_stable_across_turns() {
    let temp = TempDir::new().unwrap();
    let engine = engine_in(temp.path());
    let capture = Arc::new(CaptureProvider::new());
    engine.set_provider(capture.clone()).await;
    engine.start().await.unwrap();

    engine.process("first prompt").await.unwrap();
    engine.process("second prompt").await.unwrap();
    engine.process("third prompt").await.unwrap();

    let prompts = capture.prompts();
    assert_eq!(
        prompts.len(),
        3,
        "expected 3 provider calls, got {}",
        prompts.len()
    );

    // Turn 1: no prior history.
    assert!(
        prompts[0].contains("(start of conversation)"),
        "turn 1 should carry the placeholder: {}",
        prompts[0]
    );

    // Turn 2: history contains the turn-1 exchange.
    assert!(
        prompts[1].contains("User: first prompt"),
        "turn 2 history missing the user side of turn 1: {}",
        prompts[1]
    );
    assert!(
        prompts[1].contains("Assistant: ack"),
        "turn 2 history missing the assistant side of turn 1: {}",
        prompts[1]
    );
    assert!(
        !prompts[1].contains("(start of conversation)"),
        "turn 2 must not carry the placeholder: {}",
        prompts[1]
    );

    // Turn 3: both prior exchanges visible, in order.
    assert!(
        prompts[2].contains("User: first prompt"),
        "turn 3 missing the first exchange: {}",
        prompts[2]
    );
    assert!(
        prompts[2].contains("User: second prompt"),
        "turn 3 missing the second exchange: {}",
        prompts[2]
    );
    let first_pos = prompts[2].find("User: first prompt").unwrap();
    let second_pos = prompts[2].find("User: second prompt").unwrap();
    assert!(
        first_pos < second_pos,
        "history ordering drifted (first should precede second): {}",
        prompts[2]
    );

    engine.shutdown().await.unwrap();
}

/// `render_history_for` prepends the newest lines and stops when the
/// accumulated length exceeds the budget. Assert that the OLDEST
/// entries drop first, so the invariant "newest turns always preserved"
/// holds through the representation swap.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn history_budget_drops_oldest_first() {
    let temp = TempDir::new().unwrap();
    let engine = engine_in(temp.path());
    let capture = Arc::new(CaptureProvider::new());
    engine.set_provider(capture.clone()).await;
    engine.start().await.unwrap();

    // Tight budget: only a couple of turns' worth of chars fit.
    engine.set_history_budget(120);

    // Seed distinctive turns directly through the public seed_turn
    // API, bypassing the provider call.
    engine.seed_turn(true, "ALPHA_MARKER_user_turn_one").await;
    engine.seed_turn(false, "ALPHA_MARKER_asst_turn_one").await;
    engine.seed_turn(true, "OMEGA_MARKER_user_turn_two").await;
    engine.seed_turn(false, "OMEGA_MARKER_asst_turn_two").await;

    engine.process("final question").await.unwrap();

    let prompts = capture.prompts();
    assert_eq!(prompts.len(), 1);
    assert!(
        prompts[0].contains("OMEGA_MARKER"),
        "newest turn dropped despite budget: {}",
        prompts[0]
    );
    assert!(
        !prompts[0].contains("ALPHA_MARKER"),
        "oldest turn kept despite tight budget — drop order regressed: {}",
        prompts[0]
    );

    engine.shutdown().await.unwrap();
}
