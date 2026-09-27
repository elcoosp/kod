# Borrowed primitives — wired vs orphaned

Companion to `plan-to-cat-scripts.md`. Records which of the borrowed
delta primitives are live in the running engine and which are
present-but-orphaned, so a future reader does not have to walk the
git log to find out. Update this file when the status changes.

## Wired into `KodEngine` (or a provider loop) and exercised by tests

| Primitive | Module | Integration site | Test |
|---|---|---|---|
| `ContextGauge` | `kod-core/src/context_gauge.rs` | observed in `record_cost_with_head`; read via `context_tokens_for` + `anchored_context_tokens` from `maybe_compact_for` | `tests/context_gauge_adoption.rs` |
| `CompactionDispatcher` | `kod-core/src/compaction_dispatcher.rs` | invoked by `maybe_compact_for` before the summary path | `tests/mechanical_compaction.rs` |
| `PauseGate` | `kod-core/src/pause_gate.rs` | three check points: `run_collected_loop`, `run_streaming_loop`, `run_tool_calls` | `tests/pause_gate_integration.rs` |
| `ToolLoopGuard` | `kod-core/src/tool_loop_guard.rs` | `maybe_emit_loop_corrective` after every `run_tool_calls` | `tests/tool_loop_guard_integration.rs` |
| `ProviderConcurrency` (§9.9) | `kod-provider/src/concurrency.rs` | `acquire()`/drop bracket around every streaming HTTP request in both provider crates | `concurrency.rs` unit tests; retry-hit assertions in `kod-provider-anthropic/tests/stream_retry.rs` |
| `StreamGuard` (§9.3) | `kod-provider/src/stream_guard.rs` | fed every model-authored chunk in both providers' SSE loops; `StallVerdict::Loop` → transient `KodError::Provider` | `stream_guard.rs` unit tests; `stream_retry.rs` exercises the delivered-vs-retried fork |
| `ReplaySafety` (§9.2) | `kod-provider/src/retry_safety.rs` | attempt loop in both providers' `stream_completion`: retry only while nothing has committed; `EmptyCompletionRetry` covers the clean-but-empty case | `retry_safety.rs` unit tests; `kod-provider-anthropic/tests/stream_retry.rs` and `kod-provider-openai/tests/stream_retry.rs` each pin the three-way fork |
| `RetryHints` (§9.1) | `kod-provider/src/retry.rs` | Anthropic native-Messages error paths extract hints from response headers; the OpenAI path cannot (its `AdkError` transport exposes no headers) | `retry.rs` unit tests |
| `AutoThinking` (§9.6) | `kod-core/src/auto_thinking.rs` | `resolve_turn_effort` in `engine/mod.rs`: an endpoint with `effort = "auto"` classifies the turn via a `judge`-role `JudgmentClient` | `auto_thinking.rs` unit tests |
| `UnexpectedStopClassifier` (§9.4) | `kod-core/src/unexpected_stop.rs` | `diagnose_unexpected_stop` at both post-stream sites after `remember_turn_for`. **Diagnostic only** | `unexpected_stop.rs` unit tests |
| `ToolSearchTool` | `kod-tools/src/tool_search.rs` | registered at `engine.start()` from the engine's `tool_inventory` | `tool_search.rs` unit tests |
| `BatchTool` | `kod-tools/src/batch.rs` | registered at `engine.start()` with a `Weak<ToolRegistry>` backreference. Now `Discoverable` (§6) | `batch.rs` unit tests |
| `Advisor EmissionGuard` (§11.8) | `kod-swarm/src/advisor.rs` + `kod-core/src/advisor_tools.rs` | the `advise` tool runs the four-stage pipeline and routes through the steer queue; `begin_update` resets the per-turn budget | `advisor.rs` (34); `advisor_tools.rs` (12, incl. the 114-Stops incident) |
| Internal-URL router (§7.5) | `kod-tools/src/internal_url.rs` | `ProtocolRouter` + `ProtocolHandler` + `ResolveContext`; `read_file`/`write_file` dispatch on handled schemes; engine installs `ArtifactHandler` + `MemoryHandler` | `internal_url.rs` (25 unit + 7 integration); `memory_handler.rs` (6) |
| Shell-output minimizer (§5) | `kod-minimize/` | command classifier, 6 TOML defs, pipeline stages plus native Rust filters (`cargo-json`, `pytest-json`); `execute_command` runs stdout through it and offloads the raw as `artifact://` | 54 unit tests; 5 integration tests in `kod-tools/tests/minimizer_integration.rs` |
| Secret placeholders (§14.1) | `kod-types/src/secret_placeholder.rs` + `secret_sources.rs`; engine wiring | per-install HMAC key; env scanner, 12 vendor regexes, connection-URL passwords; obfuscate outbound, deobfuscate tool args | 21 vault + 25 scanner unit tests; 4 end-to-end in `kod-core/tests/secret_placeholder_integration.rs` |
| Handoff compaction (§4.1) | `compaction_dispatcher.rs` (`HandoffMethod`) | one LLM call producing a briefing document; `CompactionPlan::Summary` over the older half. Sits last in the engine ladder | `tests/handoff_compaction.rs` |
| Provider-native compaction (§4.4) | `kod-provider-anthropic/src/wire.rs`; `LlmProvider::native_compact`; `RemoteMethod` | Anthropic `compact-2026-01-12` beta. `apply_compaction_plan` stores the block per-transcript; `build_grounded_request` attaches it; `build_messages_body` prepends it | wire-layer (11); end-to-end replay test |
| Speculative read execution (§10) | `kod-core/src/speculation.rs` + engine | mid-stream recognition of a `read_file` call; background read; TOCTOU digest validation; `ToolContext.prefetched_read` consumed by `ReadFileTool` | 11 primitive + 8 partial-JSON tests |
| Snapcompact rasterization (§4.5) | `kod-core/src/snapcompact.rs` + `SnapcompactMethod` | dependency-free text→PNG (embedded 8×8 font, stored-mode-deflate PNG encoder, hand-rolled base64); full-frame compaction + inline tool-result imaging | 11 rasterizer tests |
| `xd://` lazy tool mounting (§6) | `kod-tools/src/xd_handler.rs` + engine demotion | `ToolDefinition.load_mode` (Essential/Discoverable); the scheme lists/describes/executes tools; `build_grounded_request` filters Discoverable tools | 7 handler tests |
| WorkPool (§11.1) | `kod-swarm/src/work_pool.rs` | keep-alive worker slots consuming batches; least-loaded dispatch; yield contract; `FreshAgents` mode | 20 tests |
| Park/revive registry (§11.2) | `kod-swarm/src/agent_registry.rs` | Idle/Active/Parked/Dead; generation-keyed coalescing; depth walk; TTL candidates. Registry persistence + cold revive + `SessionInit` log entry + reader | 27 tests |
| IrcBus (§11.3) | `kod-swarm/src/irc_bus.rs` | delivery receipts, bounded mailbox (`MAILBOX_CAP = 100`), `send_await` with correlation ids | 18 tests |
| YieldQueue (§11.5) | `kod-swarm/src/yield_queue.rs` | thunks evaluated at drain time; Streaming/Idle flush modes; `skip_idle_flush` | 16 tests |
| Goals runtime (§11.6) | `kod-core/src/goals.rs` + goal-loop wiring | one objective with token + wall-clock budget; delta = `(prompt − cache_read) + output`; `BudgetLimited` with one deduped steer | 18 unit tests |
| Todo nudge tracker (§11.7) | `kod-tools/src/todo_tracker.rs` + three engine points | prelude (first turn), mid-run reconcile (12 mutating calls, cap 2/cycle), completion reminder (latched) | 19 unit tests |
| Async-result delivery (§11.4) | `kod-core/src/async_delivery.rs` + engine | owner-routed batching; `INLINE_CAP` truncation with `agent://` pointer; session epochs drop stale results | 12 unit tests |
| Guarded job/wave spawn (§11.4) | `BackgroundJobRunner::spawn_guarded`; swarm wave tasks | keeps the `JoinHandle`, maps `JoinError::Panic` to `fail_if_running`; swarm waves wrapped in `catch_unwind` so one agent's panic fails alone | 3 job-guard tests |
| Run collector (§9.11) | `kod-core/src/run_collector.rs` + engine | per-run stop-reason histogram, per-tool status counters, coverage, cost-unavailable reasons; `/stats` prints it | 13 unit tests |

## Not yet built — grep-verified

Every entry below was checked against the tree, not recalled. A
"done" claim in this file is worth only as much as the check behind
it; two §12 items were wrongly listed as missing until a grep found
them.

### Present (verified)

- **§12.1 Weibull forgetting curves** — `kod-memory/src/retrieval.rs`:
  `weibull_shape_for` (per-type `(k, eta)` table), `Decay::Exponential`
  fallback, `decay_at`.
- **§12.4 memory-write redaction** — `manager.rs::store_with_metadata`
  redacts content and tags via `redact_text` before the dedup check.
- **§11.4 background *suggestion*** — `execute_command` reports
  `background_suggested` past 60 s.

### Absent (verified)

- **§11.4 live-child auto-detach** — the suggestion is there; handing
  off a still-running child (pipes + pinned read futures) is not.
- **§11.10 plan-mode hardening**, **§11.11 worktree isolation GC**,
  **§11.12 prewalk** — absent.
- **§12.5 background embeddings** — `store_with_metadata` still embeds
  inline; a `pending_extractions` fire-and-forget queue drained at
  shutdown is not there.
- **§12.5 raw-vs-indexed content projection** — the store has one
  `content` field; there is no separate `embed_text` column that
  strips role markers before the embed call.
- **§13 catalog metadata** — no `kod-catalog`; per-request stats and
  if-bench landed as `kod-stats` (see above).
- **§14.2 capability discovery registry** — absent.
- **§14.5 OTLP telemetry** — absent.
- **Cold-revive surface rebuild** — `SessionInit` and its reader are
  landed; the consumer that rebuilds a session's tool surface from
  the entry is not.
- **Periodic sharpshooter consolidation** — `consolidate_sharpshooter_now`
  is called at `shutdown()`; a periodic tick that reuses the
  memory-consolidation interval is a follow-up.
- **`AfterConsolidation` mental-model refresh** — the seeds carry the
  trigger, but the reload at the next transcript boundary is not
  wired; the models are frozen for the session.

| Memory hygiene (§12.5) | `kod-memory/src/hygiene.rs` + write/recall wiring | `strip_memory_tags` drops a `<memories>` block before it is stored; `frame_recalled_block` wraps recalled entries with a precedence note; `has_substantive_content` rejects placeholder turns | 16 unit tests |
| Episodic tiers (§12.8) | `kod-memory/src/tier.rs` + score wiring | tier by age (<30d / >=30d / >=180d) with weights 1.0/0.5/0.25 folded into the score; tier-3 bodies compressed to 300 chars at retrieval | 13 unit tests |
| Veracity primitives (§12.2) | `kod-memory/src/veracity.rs` | `fact_content_id` (cf_ + SHA-256 over length-prefixed SPO), `Veracity` weights, `raise_confidence`, `should_supersede`. Two gaps documented: no NFC, no triple in `ExtractedFact` | 17 unit tests |
| Sharpshooter admission (§12.3) | `kod-memory/src/sharpshooter.rs` | `prompt_is_eligible` + `evidence_is_grounded` gate a `DecisionDelta`; `Friction` ranks corrective/regression/subtle; `DecisionKind` maps to a target file | 16 unit tests |
| Retention cursor (§12.6) | `kod-memory/src/retention.rs` | rolling hash chain over the retained prefix; `advance` returns the new-message count or None on a rewrite (in-place edit, rewind, branch); `RetentionCadence` | 12 unit tests |
| Mental models (§12.7) | `kod-memory/src/mental_models.rs` | create-only seeds; `fill` at a transcript boundary only; `render_block` in id order so the prompt bytes stay stable | 13 unit tests |
| Behavioral metrics (§13.2) | `kod-stats/src/behavioral.rs` + engine wiring | five lexical signals (negation/repetition/blame/anguish/yelling) folded per user turn | 12 tests |
| Per-request analytics (§13.2) | `kod-stats/src/request.rs` + engine wiring | `RequestRecord` + `Aggregates`: error rate, cache hit rate, cache savings (can go negative), avg TTFT, tokens/sec | 14 tests |
| if-bench (§13.3) | `kod-stats/src/if_bench.rs` | scoring half: `action_for_turn`, `apply`, `cat_sound_at`, `depth`, `parse_reported_array` | tests in the crate |
| MCP HTTP policy (§14.5) | `kod-mcp/src/http_policy.rs` | `Origin` parse + `same_as`; `decide_redirect` refuses a method-changing redirect of a non-GET and drops configured headers cross-origin; reserved-header list; hop cap 5 | 17 tests |
| Cleanse scheduler (§11.9) | `kod-swarm/src/cleanse.rs` | file-sticky dispatch: one worker per file, batch budget shared across agents, released files re-claimable | 15 tests |
| Veracity confidence update (§12.2) | `kod-memory/src/manager.rs` (`store_with_metadata`) | an identical-content re-mention raises the entry's `confidence` via `veracity::raise_confidence` and persists it; `MemoryMetadata.confidence` carries the value | `manager.rs` test `a_re_mention_raises_confidence` |
| Veracity contradiction resolution (§12.2) | `MemoryManager::consolidate` pass 3 | walks the symmetric `contradicts` graph once per unordered pair; supersedes the lower-confidence side when both carry a `confidence` and they differ; ties and unset sides are left alone. `ConsolidationReport.resolved_contradictions` counts the resolutions | 4 tests in `manager.rs::contradiction_resolution_tests` |
| Sharpshooter extraction (§12.3) | `kod-memory/src/sharpshooter.rs` + `engine::maybe_extract_decisions` | `build_prompt` / `parse_reply` / `admit` produce and gate a `DecisionDelta`; the engine extracts per user turn at both post-stream sites | `sharpshooter.rs` tests + engine hook |
| Sharpshooter consolidation (§12.3) | `kod-memory/src/sharpshooter.rs` (`build_consolidation_prompt`, `truncate_to_ceiling`) + `Engine::consolidate_sharpshooter_now` | groups deltas by the target file each `DecisionKind` names; one small-model rewrite per file; result truncated to `FILE_LINE_CEILING` (120 lines) and written under `<working_dir>/.kod/decisions/<file>`; invoked at `shutdown()` | tests for prompt shape + truncation |
| Query-embed cache + cap (§12.5) | `kod-memory/src/manager.rs` (`QueryEmbedCache`) | bounded LRU on `query_embed_cache`, 512 entries; query capped at 8192 chars before the embed call; cleared when the embedder is swapped | 3 cache unit tests |
| @-import expansion (§12.8) | `kod-config/src/instructions.rs` (`expand_imports`) | a lone `@path` line in an `AGENTS.md` inlines the referenced file; code-fence aware; depth 5; cycle-safe; best-effort | 9 tests in `instructions.rs` |
| Polyphonic RRF (§12.8) | `kod-memory/src/manager.rs` (`retrieve_long_term_hybrid`) | four voices (vector / fact / importance / temporal) fused by `fusion::reciprocal_rank_fusion`; intent-weighted composite preserved per entry for the MMR relevance term | `retrieval.rs` + `manager.rs` tests |
| Intent-confidence blend (§12.8) | `kod-memory/src/fusion.rs` (`classify_intent_with_confidence`, `blend_weights`) + manager | the classifier reports its cue count; the manager blends intent weights toward neutral by `min(0.3 + 0.15*matches, 1.0)` | 8 tests in `fusion.rs` |
| Mental-model bootstrap (§12.7) | `kod-core/src/engine/mod.rs` (`bootstrap_mental_models` called from `start()`) + `render_mental_model_block` | seeds the design's three models (preferences / conventions / decisions), fills each from the store under a soft token cap, and freezes the block into the cacheable prefix | engine tests + `mental_models.rs` |

| TTSR stream rules (§14.3) | `kod-provider/src/ttsr.rs` | rules matched against streaming output: `RuleScope` (Text/Thinking/Tool with name+path patterns), `InterruptMode` (Never/ProseOnly/ToolOnly/Always), `RepeatMode` (Once/Gap). A bad regex drops the rule rather than stopping the stream. `builtin_rules()` ships no-TODO-in-diff and no-secret-in-prose | 16 tests |
| Conventional-commit validation (§14.4) | `kod-stats/src/commit.rs` | `CommitProposal` validated for type/summary/details/paths and type-path consistency (docs->*.md, ci->.github, build->Cargo.toml); `score_change` weights details by importance; `format_message` renders | 22 tests |

| TTSR stream rules (§14.3) | `kod-provider/src/ttsr.rs` | rules matched against streaming output: `RuleScope` (Text/Thinking/Tool with name+path patterns), `InterruptMode` (Never/ProseOnly/ToolOnly/Always), `RepeatMode` (Once/Gap). A bad regex drops the rule rather than stopping the stream. `builtin_rules()` ships no-TODO-in-diff and no-secret-in-prose | 16 tests |
| Conventional-commit validation (§14.4) | `kod-stats/src/commit.rs` | `CommitProposal` validated for type/summary/details/paths and type-path consistency (docs->*.md, ci->.github, build->Cargo.toml); `score_change` weights details by importance; `format_message` renders | 22 tests |

## How to update this file

Add a row to the wired table when a primitive gains a real caller.
Move a row out of "Not yet built" in the same commit that installs
the caller — do not leave the table stale. A stale status table is
worse than none, because it teaches a reader to distrust it.
