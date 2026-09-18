# KOD × Jev — Full Integration Roadmap (v2: Reusing the `jev` crate)

**Version**: 2.0
**Change from v1**: The roadmap now builds on the published `jev` crate (`v0.1.0`) instead of a custom client. Every insertion point calls the crate's `AsyncTypeSafeClient` directly, wrapped in a thin KOD adapter that adds caching, session-log instrumentation, and configurable fallbacks.

**Crate facts** (from crates.io and docs.rs):

- **Package**: `jev` v0.1.0
- **Types**: `TypeSafeClient` (blocking), `AsyncTypeSafeClient` (behind the `async` feature)
- **State**: `State::text(...)` or `State::detailed(text, &[(key, value)])`
- **Questions**: `Question::yes_no(...)`, `Question::score(..., &[levels])`, plus choice via `client.choose(...)`
- **Evaluation**: `client.evaluate(&state, questions)` returns typed answers with probabilities and confidence
- **Helpers**: `client.yes_no(...)`, `client.choose(...)`, `client.score(...)` for single questions
- **API key**: `TypeSafeClient::from_env()` reads `TYPESAFE_API_KEY`; `from_path` and `from_key` also available
- **Endpoint**: `https://api.typesafe.ai/v1/systemone` (or Vercel AI Gateway)
- **No serde import needed**: `State` and `Question` keep JSON an implementation detail
- **`verdict` function**: renders a yes probability as `yes, 88% sure` / `no, 70% sure`

**What the wrapper adds** (things the crate does not provide):

- Decision cache keyed by `(state, questions)` with TTL
- `SessionEntry::JevDecision` logging for every call
- Configurable confidence thresholds (`jev.thresholds.*`)
- Fail-open fallback to existing heuristics when Jev errors
- Single `JevDecisionProvider` trait so every call site has a uniform shape

---

## Phase 0 — Foundation

### P0.1 — KOD `JevClient` wrapper around `AsyncTypeSafeClient`

**Where**: `crates/kod-core/src/jev.rs` (new), exported from `kod-core/src/lib.rs`

**What**:
```rust
use jev::{AsyncTypeSafeClient, Question, Questions, State};

pub struct JevClient {
    inner: AsyncTypeSafeClient,
    cache: Arc<DashMap<u64, JevCacheEntry>>,
    config: JevConfig,
}

pub struct JevCacheEntry {
    answers: jev::Answers,        // or serde_json::Value
    cached_at: Instant,
    ttl: Duration,
}

impl JevClient {
    pub fn from_config(cfg: &JevConfig) -> Result<Self> {
        let inner = if let Some(key) = &cfg.api_key {
            AsyncTypeSafeClient::from_key(key)?
        } else {
            AsyncTypeSafeClient::from_env()?
        };
        Ok(Self { inner, cache: Default::default(), config: cfg.clone() })
    }

    pub async fn evaluate(&self, state: State, questions: Questions) -> Result<jev::Evaluation> {
        let key = cache_key(&state, &questions);
        if let Some(entry) = self.cache.get(&key) {
            if entry.cached_at.elapsed() < entry.ttl {
                return Ok(entry.answers.clone());
            }
        }
        let result = self.inner.evaluate(&state, questions).await?;
        self.cache.insert(key, JevCacheEntry { answers: result.clone(), cached_at: Instant::now(), ttl: self.config.cache_ttl });
        Ok(result)
    }
}
```

**Config additions** (`kod-config/src/llm.rs`, new `[jev]` section):
```toml
[jev]
enabled = true
# TYPESAFE_API_KEY env var is read automatically; api_key overrides it.
# model = "jev-latest"
cache_ttl_secs = 300
timeout_ms = 800
fail_open = true

[jev.thresholds]
task_classify_min = 0.6
tool_filter_min = 0.7
early_termination_min = 0.9
auto_approve_min = 0.95
memory_filter_min = 0.7
ambiguity_min = 0.85
```

**Why**: The `jev` crate already ships the HTTP client, typed questions, async support, retries, and cancellation. The wrapper adds the three things KOD needs across 25 call sites: caching, logging, and threshold config. Building a custom client would duplicate what the crate already does well.

**Effort**: S
**Impact**: Unblocks every subsequent phase
**Depends on**: nothing

### P0.2 — Fail-open contract

**Where**: `crates/kod-core/src/jev.rs`

**What**: Wrap every call site so that a Jev failure degrades to the pre-Jev heuristic:
```rust
pub enum DecisionSource { Jev, Heuristic, Llm }

pub struct Decision<T> {
    pub value: T,
    pub confidence: f32,
    pub source: DecisionSource,
}
```

When `config.fail_open = true` (the default), `JevClient` methods return `Err` only when the caller has explicitly opted into fail-closed. Call sites use a helper that catches the error, runs the existing heuristic, and logs a `DecisionSource::Heuristic`.

**Why**: The crate's `from_env` fails at runtime when `TYPESAFE_API_KEY` is unset or blank. KOD must keep working with Jev disabled — the existing keyword router, glob overlap detector, and approval dialog are all valid fallbacks.

**Effort**: S
**Impact**: Prevents a new class of "Jev down, KOD down" incidents
**Depends on**: P0.1

### P0.3 — Session log instrumentation

**Where**: `crates/kod-core/src/session_log.rs`, new `SessionEntry::JevDecision` variant

**What**:
```rust
JevDecision {
    timestamp_ms: u64,
    holder: String,
    purpose: String,
    state_preview: String,
    questions_summary: String,
    answers: serde_json::Value,
    confidence: f32,
    latency_ms: u64,
    cached: bool,
    source: String,  // "jev" | "heuristic" | "llm"
}
```

Every call site emits one entry. `/debug jev` and `/jev stats` read them back.

**Why**: Thresholds cannot be tuned without data. The `jev` crate does not log decisions — KOD has to.

**Effort**: S
**Impact**: Enables measurement of every subsequent phase
**Depends on**: P0.1

---

## Phase 1 — The Big Wins (perceived speed)

These five items change how fast KOD *feels*. Ship them first.

### P1.1 — Tool inventory pre-filtering

**Where**: `crates/kod-core/src/engine.rs`, `run_streaming_loop` and `run_collected_loop`, before the `provider.stream_with_tools(...)` call.

**What**:
```rust
// One Jev call, one boolean per tool category.
let mut questions = Questions::new();
for cat in &["filesystem", "shell", "git", "web", "lsp", "memory", "swarm"] {
    questions.insert(cat.to_string(), Question::yes_no(format!(
        "Does the user's request require a {cat} tool?"
    )));
}
let state = State::detailed(
    &format!("User request: {input}\nLast tool result: {last_summary}"),
    &[("working_dir", &cwd.to_string_lossy())],
);

let eval = jev_client.evaluate(state, questions).await?;
let allowed: Vec<&str> = eval.yes_keys_with_probability_above(
    config.thresholds.tool_filter_min
);
let filtered: Vec<ToolDefinition> = definitions.iter()
    .filter(|d| allowed.contains(&category_of(d)))
    .cloned().collect();
```

**Why**: Tool definitions are the largest fixed cost per round. A 20-tool registry at ~150 tokens per schema burns ~3000 tokens before the user's request is read. Reducing to 6 tools cuts TTFT by 30–50% on small models.

**Fallback**: On Jev failure, pass the full inventory (current behaviour).

**Effort**: M
**Impact**: **Very high** — 30–50% TTFT reduction
**Depends on**: P0.1, P0.3

### P1.2 — Interruptible streaming (early response termination)

**Where**: `crates/kod-core/src/engine.rs`, `stream_round`, inside the chunk loop.

**What**: Every 5 chunks, evaluate:
```rust
let mut questions = Questions::new();
questions.insert("is_complete".into(), Question::yes_no(
    "Does the accumulated response fully answer the user's request?"
));
questions.insert("is_off_track".into(), Question::yes_no(
    "Has the model drifted from the user's request?"
));
let state = State::detailed(
    &format!("User request: {input}\nResponse so far: {accumulated}"),
    &[("tool_results_count", &tool_results.len().to_string())],
);
let eval = jev_client.evaluate(state, questions).await?;

if eval.probability("is_complete") > 0.9 || eval.probability("is_off_track") > 0.9 {
    break;  // drop the stream
}
```

The evaluation runs concurrently with the stream; the loop continues reading chunks while Jev works, and only breaks when Jev answers.

**Why**: Models routinely produce 3–10× more tokens than the answer requires. A model that answers in 200 tokens but continues to 1500 for "explanation" wastes 1300 tokens and 3–8 seconds.

**Guardrails**: Never break before 100 tokens; never break before the response has one complete sentence; log every early break as a `JevDecision` for false-positive analysis.

**Effort**: M
**Impact**: **Very high** — 30–70% generation time reduction on verbose models
**Depends on**: P0.1, P0.3

### P1.3 — Per-round model routing

**Where**: `crates/kod-core/src/engine.rs`, the per-round loop in `run_streaming_loop`.

**What**: Resolve the model chain per round instead of per `process_*` call:
```rust
let mut questions = Questions::new();
questions.insert("round_kind".into(), Question::score(
    "What kind of round is this?",
    &["planning", "tool_execution", "synthesis", "summary"]
));
questions.insert("needs_large_context".into(), Question::yes_no(
    "Does this round need the full conversation history?"
));
let state = State::detailed(
    &format!("Task: {task_type:?}\nRounds completed: {round_idx}\nLast error: {last_err:?}"),
    &[("accumulated_tokens", &pending.len().to_string())],
);
let eval = jev_client.evaluate(state, questions).await?;
// Route to the endpoint the kind maps to (config: [jev.round_routing])
```

Add `[jev.round_routing]` to config:
```toml
[jev.round_routing]
planning = "cloud-anthropic"
tool_execution = "local-ollama"
synthesis = "cloud-anthropic"
summary = "local-ollama"
```

**Why**: The first round needs the smart model. The summary round does not. Routing the summary round to a small local model cuts that round's latency by 5–20×.

**Effort**: S
**Impact**: **Very high** — 30–60% session wall-clock reduction
**Depends on**: P0.1, P0.3

### P1.4 — Prose vs reasoning chunk classification

**Where**: `crates/kod-tui/src/main_loop.rs`, the chunk pump in `dispatch_prompt`.

**What**:
```rust
let mut questions = Questions::new();
questions.insert("kind".into(), Question::score(
    "What kind of text is this streamed chunk?",
    &["prose_answer", "reasoning", "restatement", "code_block"]
));
let state = State::text(&format!(
    "User request: {input}\nRecent buffer: {buffer_tail}"
));
let eval = jev_client.evaluate(state, questions).await?;
match eval.score_level("kind") {
    "prose_answer" | "code_block" => event_tx.send(Event::ResponseChunk(chunk)),
    "reasoning" | "restatement" => buffer_for_collapsed_row(chunk),
    _ => {}
}
```

**Why**: Users perceive a model as slow when they see it thinking. Filtering "Let me first..." boilerplate makes the TUI *appear* 3–5× faster with zero actual latency change.

**Effort**: S
**Impact**: **High** — perception of speed
**Depends on**: P0.1

### P1.5 — Jev decision cache (inside the wrapper)

**Where**: `crates/kod-core/src/jev.rs` (P0.1 already includes it)

**What**: Cache key is `fnv1a_hex(state_text + questions_json)`. TTL from `jev.cache_ttl_secs`. Persist to `~/.kod/jev_cache.json` for cross-session hits on repeated commands.

**Why**: Users repeat themselves. "Run the tests", "check the build", "list src/". A cached decision is a **free** decision.

**Effort**: S (already in P0.1)
**Impact**: **High** — compounds with every other Jev integration
**Depends on**: P0.1

---

## Phase 2 — Token Reduction (input side)

### P2.1 — Dynamic prompt budget per round

**Where**: `crates/kod-core/src/budget.rs`, `PromptBudget::allocate`; caller: `build_prompt_with_budget`.

**What**:
```rust
let mut questions = Questions::new();
questions.insert("needs_repomap".into(), Question::yes_no("Does this round need the repo map?"));
questions.insert("needs_full_history".into(), Question::yes_no("Does this round need the full history?"));
questions.insert("needs_skill_instructions".into(), Question::yes_no("Does this round need skill instructions?"));
questions.insert("priority_section".into(), Question::score(
    "Which section matters most this round?",
    &["history", "memory", "skills", "repomap"]
));
```

Then compute the allocation from the booleans instead of the fixed 50/20/20/10 split.

**Why**: A follow-up question needs history but not repomap. A first-time question needs repomap but not history. Jev's per-round allocation cuts 20–40% of prompt tokens without losing relevant context.

**Effort**: M
**Impact**: **High** — 20–40% prompt token reduction
**Depends on**: P0.1

### P2.2 — Tool result compression

**Where**: `crates/kod-core/src/engine.rs`, `run_tool_calls`, replacing the fixed `cap_rendered_result` truncation.

**What**:
```rust
let mut questions = Questions::new();
// One score per line of the tool result (bounded to 50 lines max).
for (i, line) in result_lines.iter().enumerate().take(50) {
    questions.insert(format!("line_{i}"), Question::score(
        "How relevant is this line to the user's request?",
        &["irrelevant", "context", "essential"]
    ));
}
questions.insert("summary".into(), Question::yes_no("Should the dropped lines be summarized?"));
```

Keep lines scored `essential`; replace dropped lines with a one-line summary.

**Why**: A `read_file` of 400 lines for a task that needs 20 is the norm. Jev-aware selection keeps the 20 relevant lines and summarizes the rest.

**Effort**: M
**Impact**: **High** — 40–70% tool-result token reduction
**Depends on**: P0.1

### P2.3 — Grep/glob result ranking

**Where**: `crates/kod-tools/src/tools.rs`, `GrepTool::execute` and `SearchFilesTool`.

**What**: When a grep returns more than 20 matches, rank them:
```rust
let mut questions = Questions::new();
questions.insert("most_relevant".into(), Question::score(
    "Which match best addresses the user's request?",
    &["match_0", "match_1", ..., "match_19"]
));
```

Return the top 10 matches by score.

**Why**: Grep on `fn main` in a workspace returns dozens of hits. The model has to read all of them to find the relevant one.

**Effort**: S
**Impact**: **Medium-high** — 30–50% grep token reduction
**Depends on**: P0.1

### P2.4 — Diff hunk triage

**Where**: `crates/kod-core/src/engine.rs`, the diff attachment block in `run_tool_calls`.

**What**:
```rust
let mut questions = Questions::new();
for (i, hunk) in hunks.iter().enumerate() {
    questions.insert(format!("hunk_{i}"), Question::score(
        "Is this hunk relevant to the user's request?",
        &["context_only", "relevant", "critical"]
    ));
}
```

Drop `context_only` hunks; replace with `… (N lines elided, no change to <symbol>)`.

**Why**: Most of a diff is context lines the model does not need to re-read.

**Effort**: S
**Impact**: **Medium** — 50–80% diff token reduction
**Depends on**: P0.1

### P2.5 — Memory entry compression

**Where**: `crates/kod-memory/src/manager.rs`, `retrieve_long_term_hybrid`, and `router.rs`, the memory truncation block.

**What**: Two-stage — Jev filters (P4.1), then compresses each surviving entry:
```rust
questions.insert("relevant_excerpt".into(), Question::score(
    "Which part of this memory is relevant to the query?",
    &["none", "header", "body", "detail"]
));
```

**Why**: A 500-token entry sharing one relevant sentence with the query wastes 480 tokens.

**Effort**: S
**Impact**: **Medium** — 60–80% memory token reduction
**Depends on**: P0.1, P4.1

---

## Phase 3 — Reliability & Friction

### P3.1 — Confidence-gated auto-approval

**Where**: `crates/kod-core/src/engine.rs`, the `Decision::Ask` block in `run_tool_calls`.

**What**:
```rust
let mut questions = Questions::new();
questions.insert("likely_approved".into(), Question::yes_no(
    "Would the user almost certainly approve this call?"
));
questions.insert("risk_level".into(), Question::score(
    "How risky is this operation?",
    &["read_only", "reversible", "destructive", "irreversible"]
));
let state = State::detailed(
    &format!("Tool: {}\nArgs: {}\nRequest: {input}", call.tool_name, call.arguments),
    &[("recent_approvals", &recent_approvals_json)],
);
let eval = jev_client.evaluate(state, questions).await?;

if eval.probability("likely_approved") > config.thresholds.auto_approve_min
    && eval.score_level("risk_level") != "destructive"
    && eval.score_level("risk_level") != "irreversible"
    && session_has_approved_similar(&call, 2)
{
    // skip the dialog
}
```

**Why**: The approval dialog is KOD's biggest friction source. Users approve `cargo test` repeatedly. Session-accumulated confidence eliminates ~80% of dialogs without weakening the guarantee for risky operations.

**Effort**: S
**Impact**: **High** — friction nearly eliminated
**Depends on**: P0.1, P0.3

### P3.2 — Cross-round approval batching

**Where**: `crates/kod-core/src/engine.rs`, the approval batch builder.

**What**:
```rust
questions.insert("group_id".into(), Question::score(
    "Which logical change does this item belong to?",
    &["change_a", "change_b", "change_c", "standalone"]
));
```

Group items across rounds into one dialog with a combined summary.

**Why**: A single logical change (add a function, add a test, update a doc comment) currently produces three separate dialogs.

**Effort**: M
**Impact**: **Medium-high**
**Depends on**: P0.1, P3.1

### P3.3 — Ambiguity pre-detection

**Where**: `crates/kod-core/src/engine.rs`, `process_streaming_for`, before the router call.

**What**:
```rust
let mut questions = Questions::new();
questions.insert("is_ambiguous".into(), Question::yes_no(
    "Is this request ambiguous without more context?"
));
questions.insert("ambiguity_type".into(), Question::score(
    "What kind of ambiguity?",
    &["missing_target", "unclear_scope", "multiple_interpretations", "missing_constraint"]
));
questions.insert("clarifying_question".into(), Question::yes_no(
    "Should KOD ask a clarifying question before proceeding?"
));
```

If `is_ambiguous > 0.85`, ask the question before calling the LLM.

**Why**: When a user says "fix the bug", the LLM guesses, reads files, makes a wrong edit, and the user has to undo and clarify. A 300ms Jev check saves a 30-second round-trip.

**Effort**: S
**Impact**: **High** — prevents wrong-turn branches
**Depends on**: P0.1

### P3.4 — Sandbox decision per command

**Where**: `crates/kod-tools/src/context.rs`, `SandboxResolver::invocation`.

**What**:
```rust
questions.insert("needs_sandbox".into(), Question::yes_no(
    "Does this shell command need OS-level sandboxing?"
));
questions.insert("risk_level".into(), Question::score(
    "Command risk level",
    &["safe", "network_risk", "filesystem_risk", "destructive"]
));
```

`cat`, `cargo test` → no sandbox. `curl | sh`, `rm -rf` → sandbox + approval.

**Why**: bwrap costs ~50ms per invocation and occasionally breaks builds. Running every command sandboxed is wasteful.

**Effort**: S
**Impact**: **Medium** — faster commands, no security regression
**Depends on**: P0.1

### P3.5 — Ask-user necessity check

**Where**: `crates/kod-core/src/engine.rs`, the `ask_user` interception.

**What**:
```rust
questions.insert("can_be_answered_from_context".into(), Question::yes_no(
    "Can this question be answered from the available context?"
));
questions.insert("possible_answer".into(), Question::yes_no(
    "Does a plausible answer exist in the context?"
));
```

If yes, inject the answer as the tool result — no user interruption.

**Why**: Models ask "which file should I modify?" when the user already said "the auth module".

**Effort**: S
**Impact**: **Medium-high** — fewer interruptions
**Depends on**: P0.1

---

## Phase 4 — Quality & Intelligence

### P4.1 — Memory semantic relevance filtering

**Where**: `crates/kod-memory/src/manager.rs`, `retrieve_long_term_hybrid`, after the hybrid scorer.

**What**:
```rust
questions.insert("relevant_by_id".into(), Question::score(
    "How relevant is this memory to the query?",
    &["irrelevant", "weak", "relevant", "essential"]
));
```

Keep entries scored `relevant` or `essential` with confidence > 0.7.

**Why**: The `MIN_SCORE = 0.15` threshold is blunt. Jev's calibrated judgment lets the engine keep 5 entries with confidence > 0.7 instead of 20 with score > 0.15.

**Effort**: S
**Impact**: **Medium-high**
**Depends on**: P0.1

### P4.2 — Skill semantic matching

**Where**: `crates/kod-skills/src/matcher.rs`, `SkillMatcher::score_skill`.

**What**: Add a Jev pass on top of the substring matcher:
```rust
questions.insert("relevant_by_name".into(), Question::score(
    "How relevant is this skill to the user's request?",
    &["irrelevant", "weak", "relevant", "essential"]
));
```

**Why**: Substring matching misses "design a landing page" → `ui-ux-designer`. Jev catches it.

**Effort**: S
**Impact**: **Medium**
**Depends on**: P0.1

### P4.3 — Task classification (augment the keyword router)

**Where**: `crates/kod-core/src/router.rs`, `TaskRouter::classify_task`.

**What**:
```rust
let mut questions = Questions::new();
questions.insert("task_type".into(), Question::score(
    "Which task type best describes this request?",
    &["Simple", "CodeModification", "Debugging", "Research", "Testing", "Documentation", "Complex", "MultiStep"]
));
questions.insert("requires_tools".into(), Question::yes_no("Will this need filesystem or shell tools?"));
questions.insert("urgency".into(), Question::score("Urgency", &["low", "normal", "high", "critical"]));
```

Fall back to the keyword matcher when `task_type` confidence < 0.6.

**Why**: The current classifier misorders priorities. Jev returns a distribution plus a confidence the engine can act on.

**Effort**: S
**Impact**: **Medium-high**
**Depends on**: P0.1

### P4.4 — Check diagnostic classification

**Where**: `crates/kod-core/src/engine.rs`, the auto-check block in `run_tool_calls`.

**What**:
```rust
questions.insert("new_errors_introduced".into(), Question::score(
    "How many new errors did the write introduce?",
    &["none", "one", "few", "many"]
));
questions.insert("errors_resolved".into(), Question::score(
    "How many pre-existing errors did the write resolve?",
    &["none", "one", "few", "many"]
));
```

**Why**: The syntactic `diag_key` diff treats a line-shifted error as new. Jev understands that "unused variable `x` at line 42" and "unused variable `x` at line 45" are the same error.

**Effort**: S
**Impact**: **Medium**
**Depends on**: P0.1

### P4.5 — Citation semantic verification

**Where**: `crates/kod-core/src/citations.rs`, `check_and_annotate` (currently explicitly refuses to do this).

**What**:
```rust
questions.insert("citation_supports_claim".into(), Question::yes_no(
    "Does the cited line actually support the claim made in the prose?"
));
questions.insert("is_fabricated".into(), Question::yes_no(
    "Does the citation appear fabricated rather than read from the file?"
));
```

The `## Citation check` block gains a `(semantic: unverified)` annotation when Jev says the citation is present but does not support the claim.

**Why**: The current check only verifies the file exists and the line is in range. Jev catches actual hallucinated citations.

**Effort**: S
**Impact**: **Medium** — real citation verification
**Depends on**: P0.1

### P4.6 — Swarm capability & write-set validation

**Where**: `crates/kod-core/src/swarm_runner.rs`, `capability_for` and `parse_subtasks`.

**What**:
```rust
questions.insert("capability".into(), Question::score(
    "Which capability best describes this subtask?",
    &["coding", "testing", "documentation", "code-review", "planning", "research", "debugging", "refactoring"]
));
questions.insert("globs_match_description".into(), Question::yes_no(
    "Do these file globs plausibly match the subtask description?"
));
```

**Why**: The `role_preamble` already assigns roles. Reliable classification means the correct preamble reaches the correct agent. Valid globs mean fewer merge conflicts.

**Effort**: S
**Impact**: **Medium-high** in swarm mode
**Depends on**: P0.1

### P4.7 — Swarm overlap detection (semantic)

**Where**: `crates/kod-core/src/swarm_runner.rs`, `detect_overlap` and `globs_overlap`.

**What**: Add a Jev semantic check on top of the glob comparison:
```rust
questions.insert("colliding_pairs".into(), Question::score(
    "Which subtask pairs touch the same conceptual file?",
    &["pair_0_1", "pair_0_2", "pair_1_2", "none"]
));
```

**Why**: Two subtasks can touch the same conceptual file (`migrations/001.sql` and `db/schema.sql`) without sharing a glob prefix.

**Effort**: S
**Impact**: **Medium** in swarm mode
**Depends on**: P0.1, P4.6

### P4.8 — Handoff structured extraction (two-phase)

**Where**: `crates/kod-tui/src/main_loop.rs`, the `/handoff` handler.

**What**:
**Phase 1 (Jev)**: per-message booleans:
```rust
questions.insert("is_decision".into(), Question::yes_no("Does this message contain a durable decision?"));
questions.insert("is_unfinished_task".into(), Question::yes_no("Does this message describe an unfinished task?"));
questions.insert("is_file_reference".into(), Question::yes_no("Does this message reference a file path?"));
```

**Phase 2 (LLM)**: render the extracted facts into the Markdown handoff.

**Why**: The handoff is only as good as the extraction. Jev's per-message judgments are faster and more reliable than asking the LLM to read the whole transcript.

**Effort**: M
**Impact**: **Medium**
**Depends on**: P0.1

---

## Phase 5 — Advanced & Meta

### P5.1 — MCP tool selection

**Where**: `crates/kod-core/src/mcp_adapters.rs`, `McpHost::startup_tools`, and the per-round filter.

**What**:
```rust
questions.insert("selected_tools".into(), Question::score(
    "Which MCP tool should handle this request?",
    &mcp_tool_names  // up to 255 options
));
```

Only the selected MCP tools enter the LLM prompt for that round.

**Why**: An MCP filesystem server exposes ~10 tools. A GitHub MCP exposes ~20. Jev's selection keeps MCP from degrading TTFT.

**Effort**: M
**Impact**: **Medium** in MCP-heavy sessions
**Depends on**: P0.1, P1.1

### P5.2 — Dynamic endpoint routing

**Where**: `crates/kod-core/src/provider_setup.rs`, `LlmConfig::route_for_task`.

**What**:
```rust
questions.insert("endpoint".into(), Question::score(
    "Which endpoint should handle this task?",
    &endpoint_names
));
questions.insert("needs_fallback".into(), Question::yes_no("Is the primary endpoint likely to fail?"));
```

Fallback to the static `[llm.routing.by_task]` table on Jev failure.

**Why**: A user with a local Ollama and cloud Anthropic should not hand-tune routing. Jev learns from the session.

**Effort**: M
**Impact**: **Medium**
**Depends on**: P0.1

### P5.3 — Session log semantic classification

**Where**: `crates/kod-core/src/session_log.rs`, the `ToolCall` write path.

**What**:
```rust
questions.insert("outcome".into(), Question::score(
    "What was the outcome of this tool call?",
    &["success", "partial", "failure", "irrelevant"]
));
questions.insert("user_visible_impact".into(), Question::score(
    "Impact on the user's task",
    &["none", "minor", "significant", "critical"]
));
```

New `SessionEntry::ToolOutcome` variant.

**Why**: `kod replay` can filter by semantic outcome; `/debug tokens` can distinguish "calls that advanced the task" from "wasted calls".

**Effort**: S
**Impact**: **Medium**
**Depends on**: P0.1, P0.3

### P5.4 — Response quality gate

**Where**: `crates/kod-core/src/engine.rs`, end of `process_streaming_for`.

**What**:
```rust
questions.insert("answers_the_question".into(), Question::yes_no("Does the response answer the user's request?"));
questions.insert("is_hallucinating".into(), Question::yes_no("Does the response contain unsupported claims?"));
questions.insert("citations_valid".into(), Question::yes_no("Are all cited locations accurate?"));
```

If `answers_the_question < 0.5` and confidence > 0.85, suggest `/regenerate` with a reason.

**Why**: Saves the user from re-prompting. A cheap Jev gate catches off-track responses before the user notices.

**Effort**: S
**Impact**: **Medium**
**Depends on**: P0.1

### P5.5 — Phase-aware handoff suggestion

**Where**: `crates/kod-tui/src/app.rs`, `KodApp::maybe_compact` and the response completion path.

**What**:
```rust
questions.insert("phase_changed".into(), Question::yes_no("Has the session shifted to a new phase?"));
questions.insert("old_phase".into(), Question::score("Previous phase", &["exploring", "coding", "debugging", "testing", "refactoring", "documenting"]));
questions.insert("new_phase".into(), Question::score("New phase", &["exploring", "coding", "debugging", "testing", "refactoring", "documenting"]));
```

When `phase_changed` is true and confidence > 0.8, suggest `/handoff`.

**Why**: A session that starts by exploring and ends by fixing bugs carries dead context. A well-timed handoff resets the transcript to a clean slate.

**Effort**: S
**Impact**: **Medium**
**Depends on**: P0.1

### P5.6 — Streaming provider switch on quality drop

**Where**: `crates/kod-core/src/engine.rs`, `stream_round`.

**What**:
```rust
questions.insert("is_on_track".into(), Question::yes_no("Is the model's response on track to answer the request?"));
questions.insert("quality_score".into(), Question::score("Response quality", &["poor", "fair", "good", "excellent"]));
```

If `is_on_track < 0.5` and `rounds_completed < 3`, switch to the next model in the fallback chain mid-turn.

**Why**: Today the fallback chain only fires on errors. A small local model that starts confidently on the wrong track runs to completion, and the user has to `/retry` manually.

**Effort**: M
**Impact**: **Medium-high** in local-model sessions
**Depends on**: P0.1, P1.3

### P5.7 — `/jev stats` command

**Where**: `crates/kod-tui/src/main_loop.rs`, new `/jev` slash command.

**What**: Read `SessionEntry::JevDecision` entries from the session log and report:
```
Jev decisions this session: 47
  Jev:       38  (81%)
  Heuristic:  6  (13%)
  Fallback:   3   (6%)

Estimated savings vs. LLM:
  Tokens saved:   ~14,200
  Time saved:     ~42s
  Cost saved:     ~$0.018

Cache hits:      18 (38%)
Avg Jev latency: 240ms
```

**Why**: Measurement is the only way to justify the integration and tune thresholds.

**Effort**: S
**Impact**: **Medium**
**Depends on**: P0.3

### P5.8 — `/jev` configuration command

**Where**: `crates/kod-tui/src/main_loop.rs`, new `/jev` command.

**What**:
```
/jev on | off              — toggle all Jev integration
/jev strict | balanced | lenient  — set confidence thresholds
/jev cache clear           — drop the decision cache
/jev stats                 — see P5.7
/jev test                  — smoke test against the endpoint
```

**Why**: Users must be able to disable Jev if it misbehaves. A strict default (high confidence thresholds, more LLM fallback) with a lenient opt-in for power users.

**Effort**: S
**Impact**: **Low-medium**
**Depends on**: P5.7

---

## Cross-cutting concerns

### C.1 — Threshold tuning

Every Jev boolean decision is a probability. The `[jev.thresholds]` table (P0.1) is the single source of truth for every decision. A user can tighten or loosen globally.

### C.2 — Observability

Every Jev call logs a `SessionEntry::JevDecision`. Add `/debug jev` to show the last N decisions with inputs, outputs, and latencies. When a decision looks wrong, the user sees exactly which question Jev answered incorrectly.

### C.3 — Model version pinning

Jev's calibration is valid for a specific model version. Pin it in config (`jev.model = "jev-latest"`) and log it with every decision. If the model changes, invalidate the cache and retune thresholds.

### C.4 — Privacy

Jev state includes user requests, file paths, and tool arguments. Add `[jev] redact_paths = true` to hash absolute paths before sending state. This is a both-ways decision: Jev's judgment quality drops slightly, but the user keeps control.

---

## Suggested implementation schedule

| Sprint | Deliverables | Cumulative impact |
|--------|--------------|-------------------|
| **Sprint 1** | P0.1, P0.2, P0.3, P1.5 | Foundation + cache ready |
| **Sprint 2** | P1.1, P1.3 | Tool filtering + per-round routing: 40–60% TTFT |
| **Sprint 3** | P1.2, P1.4 | Early termination + prose filter: session feels 3–5× faster |
| **Sprint 4** | P2.1, P2.2 | Dynamic budget + tool result compression: 30% prompt reduction |
| **Sprint 5** | P3.1, P3.3, P3.4, P3.5 | Friction elimination |
| **Sprint 6** | P4.1, P4.2, P4.3, P4.4 | Quality: memory, skills, routing, feedback |
| **Sprint 7** | P4.5, P4.6, P4.7, P4.8 | Deeper integrations |
| **Sprint 8** | P5.1–P5.8 + C.* | Meta: MCP, dynamic endpoints, stats, tuning |

Each sprint is independently shippable. After Sprint 3, KOD is dramatically faster. Sprints 4+ compound.

---

## Success metrics (target after Sprint 5)

| Metric | Before | Target | Measurement |
|--------|--------|--------|-------------|
| Avg TTFT (local 7B, 20-tool registry) | 3.5 s | 1.2 s | TUI status bar `ttft` |
| Avg tokens per turn | 6200 | 3400 | `/debug tokens` |
| LLM calls per 30-turn session | 30 | 18 | Session log count |
| Approval dialogs per session | 12 | 2 | Session log count |
| Session wall clock (same task) | 100% | 45% | Manual benchmark |
| Cost per session (cloud endpoint) | 100% | 35% | `/stats` cost display |

---

## The one-line summary

**Every bounded decision in KOD today is a paragraph in a prompt. The `jev` crate turns each into a 200-millisecond, sub-cent call that never wastes tokens and never hallucinates.** The roadmap above lists every place this can happen, ordered so the first three sprints alone make KOD feel like a different product. The wrapper adds only caching, logging, and threshold config — everything else comes from the published crate.
