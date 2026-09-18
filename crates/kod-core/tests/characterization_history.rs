//! Characterization test for engine transcript rendering (A3).
//!
//! The A3 change swaps the engine's internal transcript from
//! `Vec<HistoryTurn>` (a private `{ user: bool, text: String }`) to
//! `Vec<ChatMessage>`. The rendering of the "Conversation so far"
//! section in the provider prompt MUST stay byte-identical, or every
//! session silently re-tokenizes a different prefix than it did
//! yesterday — the exact cache-defeat failure the roadmap warns about.
//!
//! Both tests below were green on the pre-swap engine. They stay green
//! after the swap, guarding the `ChatMessage::render_text` equivalence
//! with the private `HistoryTurn` format.

use async_trait::async_trait;
use futures::Stream;
use kod_core::{KodEngine, RouterConfig};
use kod_error::Result;
use kod_provider::{GenerationOptions, GenerationResponse, LlmProvider, StreamChunk};
use kod_types::ToolDefinition;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use tempfile::TempDir;

#[path = "common/install_test_provider.rs"]
mod install_test_provider_mod;
use install_test_provider_mod::install_test_provider;

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
    std::fs::write(dir.join("main.rs"), "pub fn main() {}\n").unwrap();
    let db_path = dir.join("test.redb");
    let cfg = RouterConfig {
        embedder: None,
        skill_threshold: 0.3,
        working_dir: dir.to_path_buf(),
        enable_memory: false,
        max_skills_per_query: 3,
        context_window: 8192,
        short_term_capacity: 100,
    };
    KodEngine::new(cfg, db_path).unwrap()
}

/// Truncate a captured prompt for error messages so a failed assertion
/// does not dump 40 KB of tool descriptions into the test log.
fn excerpt(s: &str) -> String {
    const LIMIT: usize = 400;
    if s.len() <= LIMIT {
        s.to_string()
    } else {
        format!("{}… [{} more bytes]", &s[..LIMIT], s.len() - LIMIT)
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn history_rendering_shape_is_stable_across_turns() {
    let temp = TempDir::new().unwrap();
    let engine = engine_in(temp.path());
    let capture = Arc::new(CaptureProvider::new());
    install_test_provider(&engine, capture.clone()).await;
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

    assert!(
        prompts[0].contains("User: first prompt"),
        "turn 1 should carry the user's message: {}",
        excerpt(&prompts[0])
    );
    assert!(
        !prompts[0].contains("(start of conversation)"),
        "legacy placeholder must not appear: {}",
        excerpt(&prompts[0])
    );

    assert!(
        prompts[1].contains("User: first prompt"),
        "turn 2 history missing the user side of turn 1: {}",
        excerpt(&prompts[1])
    );
    assert!(
        prompts[1].contains("Assistant: ack"),
        "turn 2 history missing the assistant side of turn 1: {}",
        excerpt(&prompts[1])
    );
    assert!(
        !prompts[1].contains("(start of conversation)"),
        "turn 2 must not carry the placeholder: {}",
        excerpt(&prompts[1])
    );

    assert!(
        prompts[2].contains("User: first prompt"),
        "turn 3 missing the first exchange: {}",
        excerpt(&prompts[2])
    );
    assert!(
        prompts[2].contains("User: second prompt"),
        "turn 3 missing the second exchange: {}",
        excerpt(&prompts[2])
    );
    let first_pos = prompts[2].find("User: first prompt").unwrap();
    let second_pos = prompts[2].find("User: second prompt").unwrap();
    assert!(
        first_pos < second_pos,
        "history ordering drifted (first should precede second): {}",
        excerpt(&prompts[2])
    );

    engine.shutdown().await.unwrap();
}

/// `render_history_for` walks the transcript newest-to-oldest and stops
/// as soon as accumulating the next (older) line would exceed the
/// budget. Consequence: the OLDEST messages drop first; the newest
/// always survive.
///
/// The engine floors `set_history_budget` at `MIN_HISTORY_CHAR_BUDGET`
/// (4_000 chars). A "tight 60-char budget" is silently raised to 4_000
/// and nothing drops — so the test must exceed the floor to exercise
/// the drop path. Ten turns of ~640 chars each sum to ~6_400, well
/// past the floor.
///
/// Each turn carries a unique `UNIQ_<i>_` marker so we can assert on
/// exactly which turns survived, not just whether "some marker" did.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn history_budget_drops_oldest_first() {
    let temp = TempDir::new().unwrap();
    let engine = engine_in(temp.path());
    let capture = Arc::new(CaptureProvider::new());
    install_test_provider(&engine, capture.clone()).await;
    engine.start().await.unwrap();

    // At the floor: 4_000 chars. Each seeded turn is
    // `UNIQ_<i>_` + 620 'x' chars ≈ 640 bytes; ten of them sum to
    // ≈ 6_400 chars, exceeding the floor by ~2_400, so roughly the
    // oldest three or four turns are dropped.
    engine.set_history_budget(4_000);

    const TURNS: usize = 10;
    const PAD: usize = 620;
    for i in 0..TURNS {
        let user = format!("UNIQ_{i:04}_USER_{}", "x".repeat(PAD));
        let asst = format!("UNIQ_{i:04}_ASST_{}", "x".repeat(PAD));
        engine.seed_turn(true, &user).await;
        engine.seed_turn(false, &asst).await;
    }

    engine.process("final question").await.unwrap();

    let prompts = capture.prompts();
    assert_eq!(prompts.len(), 1);
    let p = &prompts[0];

    // The newest turn always survives: dropping starts from the
    // oldest, and stopping only happens when the next-to-prepend
    // (older) line would overflow.
    assert!(
        p.contains("UNIQ_0009_USER"),
        "newest user turn dropped despite budget: {}",
        excerpt(p)
    );
    assert!(
        p.contains("UNIQ_0009_ASST"),
        "newest assistant turn dropped despite budget: {}",
        excerpt(p)
    );

    // The oldest turn never survives: the loop drops oldest-first.
    assert!(
        !p.contains("UNIQ_0000_USER"),
        "oldest user turn kept — drop order regressed: {}",
        excerpt(p)
    );
    assert!(
        !p.contains("UNIQ_0000_ASST"),
        "oldest assistant turn kept — drop order regressed: {}",
        excerpt(p)
    );

    // The drop must be *partial* — the budget exceeds one turn but
    // is well under all ten. If we dropped everything, the loop
    // broke early on the first iteration; if we dropped nothing, the
    // budget was not respected. Count distinct indices present.
    let mut present = std::collections::HashSet::new();
    for i in 0..TURNS {
        if p.contains(&format!("UNIQ_{i:04}_USER")) {
            present.insert(i);
        }
    }
    assert!(
        !present.is_empty(),
        "no turns survived — the loop broke before prepending anything"
    );
    assert!(
        present.len() < TURNS,
        "every turn survived — the budget was not respected ({} present)",
        present.len()
    );
    // Contiguity check: the survivors are exactly the newest N turns
    // for some N (i.e. indices form a suffix of 0..TURNS). A gap
    // would mean the drop logic skipped a middle turn, which is the
    // failure mode the "drop oldest first" contract forbids.
    let min_present = *present.iter().min().unwrap();
    for i in min_present..TURNS {
        assert!(
            present.contains(&i),
            "gap in survivors: turn {i} missing but a newer turn is present. \
             Drop must be oldest-first, never arbitrary. Present: {present:?}"
        );
    }

    engine.shutdown().await.unwrap();
}
