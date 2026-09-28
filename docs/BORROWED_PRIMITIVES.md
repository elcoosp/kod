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
- **§14.5 OTLP telemetry** — absent.

| Memory hygiene (§12.5) | `kod-memory/src/hygiene.rs` + write/recall wiring | `strip_memory_tags` drops a `<memories>` block before it is stored; `frame_recalled_block` wraps recalled entries with a precedence note; `has_substantive_content` rejects placeholder turns | 16 unit tests |
| Episodic tiers (§12.8) | `kod-memory/src/tier.rs` + score wiring | tier by age (<30d / >=30d / >=180d) with weights 1.0/0.5/0.25 folded into the score; tier-3 bodies compressed to 300 chars at retrieval | 13 unit tests |
| Veracity primitives (§12.2) | `kod-memory/src/veracity.rs` | `fact_content_id` (cf_ + SHA-256 over length-prefixed SPO), `Veracity` weights, `raise_confidence`, `should_supersede`. Two gaps documented: no NFC, no triple in `ExtractedFact` | 17 unit tests |
| Sharpshooter admission (§12.3) | `kod-memory/src/sharpshooter.rs` | `prompt_is_eligible` + `evidence_is_grounded` gate a `DecisionDelta`; `Friction` ranks corrective/regression/subtle; `DecisionKind` maps to a target file | 16 unit tests |
| Retention cursor (§12.6) | `kod-memory/src/retention.rs` | rolling hash chain over the retained prefix; `advance` returns the new-message count or None on a rewrite (in-place edit, rewind, branch); `RetentionCadence` | 12 unit tests |
| Mental models (§12.7) | `kod-memory/src/mental_models.rs` | create-only seeds; `fill` at a transcript boundary only; `render_block` in id order so the prompt bytes stay stable | 13 unit tests |
| Behavioral metrics (§13.2) | `kod-stats/src/behavioral.rs` + engine wiring | five lexical signals (negation/repetition/blame/anguish/yelling) folded per user turn | 12 tests |
| Per-request analytics (§13.2) | `kod-stats/src/request.rs` + engine wiring | `RequestRecord` + `Aggregates`: error rate, cache hit rate, cache savings (can go negative), avg TTFT, tokens/sec | 14 tests |
| if-bench (§13.3) | `kod-stats/src/if_bench.rs` + `kod-cli/commands/admin.rs::run_if_bench` | full: scoring primitives plus `prompt_for_turn` / `drive` (the driver half); `kod if-bench` runs the eval against the default endpoint and prints the per-turn result and the depth against `par` | tests in `if_bench` (driver + scoring) |
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
| Raw-vs-indexed content projection (§12.5) | `kod-memory/src/manager.rs` (`index_text_for_embedding`) | strips `<memories>` blocks, a leading role prefix on the first non-empty line, and heading `#` markers before the embed call; raw content is stored unchanged | 6 tests in `manager.rs` |
| Fire-and-forget embeddings (§12.5) | `kod-memory/src/manager.rs` (`spawn_embed`, `flush_embeddings`) + `router::flush_embeddings` + shutdown | `store_with_metadata` persists the entry and spawns the embed task; `flush_embeddings` waits on an in-flight counter with a tokio Notify wakeup; `KodEngine::shutdown` awaits the flush before `close_memory` | engine + manager tests |
| Post-consolidation refresh (§12.7) | `kod-core/src/engine/mod.rs` (`fill_mental_models`, `refresh_mental_models_after_consolidation`) | re-fills models whose seed trigger is `AfterConsolidation`; the `SessionStart` models are left frozen; a caller that owns the engine invokes it after `router.consolidate_memory()` reports a change | engine tests |
| Per-TTL cache-write pricing (§13.1) | `kod-provider/src/request.rs` (`ModelPricing::cache_write_1h_per_mtok_usd`, `cost_for_usage`) + `types.rs` (`TokenUsage::cache_creation_1h_tokens`) | the 5-minute and 1-hour write tiers carry separate rates (1.25x and 2x input by default); `cost_for_usage` splits the write count and bills each slice at its rate; a provider that reports one tier sets the subset to `None` and the whole write prices at the base rate | provider + provider-anthropic tests |
| 1-hour cache-write extraction (§13.1) | `kod-provider-anthropic/src/wire.rs` (`AnthropicStreamState::cache_creation_1h_input_tokens`, `message_start`); `provider.rs` (non-stream extractor) | parses the nested `cache_creation.ephemeral_1h_input_tokens` object Anthropic sends; clamps a mis-reported nested count to the flat total; a reply with no nested object yields `None` | 4 tests in wire.rs + provider.rs |
| Isolation ownership (§11.11) | `kod-core/src/worktree_isolation_ownership.rs` + `worktree.rs` (`reap_dead_worktrees` called from `WorktreeManager::detect`; `create` writes the marker; `cleanup` removes it) | each worktree carries `.kod-isolation-owner.json` with a pid + start-token; `reap_dead` returns worktrees whose owner is provably gone; detect() calls `git worktree remove --force` on each; a marker-less or live-owned worktree is skipped | 6 tests in the new module + existing worktree tests |
| Prewalk (§11.12) | `kod-core/src/prewalk.rs` (`PrewalkState`, `Prewalk::arm`/`is_mutating_tool`) + `engine::arm_prewalk` / `maybe_fire_prewalk` in `run_tool_calls_with_speculations` + TUI `/prewalk <model>` | a one-way mid-session model handoff: arming injects a "plan deliberately" nudge; the first mutating tool call (write_file/patch_file/edit/git_commit) splices the nudge, switches `current_model`, and pushes a checklist | 7 tests + engine + TUI |
| Plan autosave (§11.10) | `kod-core/src/plan.rs` (`slugify`, `autosave_plan`, `plan_dir_for_working_dir`) + engine `autosave_plan_for` called from plan creation | the slug/date file lands under `~/.kod/plans/<fnv1a-of-cwd>/`; O_EXCL with a numeric-suffix retry; per-transcript working dir override honoured for swarm agents | plan.rs tests + engine tests |
| Plan read-compaction protection (§11.10) | `kod-core/src/shake.rs` + `prune.rs` (`protected_paths` field on `ShakeConfig`/`PruneConfig`); `compaction_dispatcher.rs` (`CompactionContext.protected_paths`); engine `try_mechanical_compaction` fills it per turn | a `read_file` tool result whose call path is in the set is skipped by shake and prune; the model can declare paths via a new `PlanUpdate::ReferencePath { path, drop? }` action | shake/prune + engine + plan tests |
| Plan handoff (§11.10) | `kod-swarm/src/brief.rs` (`ContextBrief.plan_text`, `render_brief` emits a `## Approved plan` section) + `brief_assembly.rs` (`ParentContext.plan_text`) + `swarm_runner` fills it from `engine.plan_for("session")` | every subtask brief carries the parent's approved plan, ordered above the repository map; empty when the parent has no plan | 1 test in brief.rs |
| Plan-mode toggle + subagent clamp (§11.10) | `kod-core/src/engine/mod.rs` (`plan_mode: HashSet<String>`, `is_in_plan_mode`, `set_plan_mode`; `build_grounded_request` clamps the tool set to read-only tools when the transcript is in plan mode) + TUI `"/plan-mode"` command | a user enters plan mode with `/plan-mode on`; the tool set clamps to `read_file`/`list_files`/`grep`/`file_info`/`web_search`/`tool_search`; the clamp is applied after the Jev hysteresis filter and MCP trim so a mode toggle cannot be overridden by a per-turn classification | TUI test + engine tests |
| Plan-model role (§11.10) | `kod-config/src/llm.rs` (`RoutingConfig.plan: Option<String>`) + `kod-core/src/engine/mod.rs` (`resolve_chain_for_task` consults the plan endpoint when the default transcript is in plan mode) | a `[llm.routing] plan = "…"` block routes every plan-mode turn to that endpoint; an unknown endpoint falls through to the ordinary chain, matching the by_task safety rule; the switch fires at the turn boundary so a mid-stream toggle cannot swap the model under a stream | config + engine tests |
| Model catalog (§13.1) | `kod-provider/src/catalog.rs` (`ModelMeta`, `resolve`, `builtin`, `ProviderPriority`, `TimeBasedPricing`) + `kod-core/src/engine/mod.rs` (`pricing_for` and `budget_hint_for` consult the catalog after the configured values miss) | a dozen well-known models with context window, per-TTL pricing, effort ladder, intelligence / tps scores, long-context tier, and peak-window off-peak pricing; `resolve` tolerates dialect drift (prefixes, `-latest`, dashes / underscores / dots); priority ranks first-party > aggregator > gateway | 15 tests in the catalog + engine fallback tests |
| Capability discovery registry (§14.2) | `kod-skills/src/capability.rs` (`CapabilityRegistry`, `DiscoverySource`, `Band`, `default_sources`, `load_via_registry`) + `kod-config/src/config.rs` (`skills_dirs` reads the foreign directories) | priority-banded discovery: a higher band wins a duplicate key (project > global > foreign); `discover_with_suppression` removes an item *after* it claims its dedup slot. Two consumers: `skills_dirs` (every `load_from_dirs` caller now sees `.claude` / `.cursor` / `.gemini` / `.codex`) and `load_via_registry` (band-priority dedup before parse) | 12 tests + config tests |
| Cold-revive surface verification (§11.2) | `kod-core/src/engine/mod.rs` (`surface_fingerprint`, `verify_cold_revive_surface`, `ColdReviveVerdict`) | `record_session_init` extracts the fingerprint through the shared helper; `verify_cold_revive_surface(log_path, holder)` reads the persisted entry, recomputes the fingerprint, and returns `NoInitEntry` / `SurfaceMatches` / `SurfaceDrifted { missing, added }` | engine tests |
| Periodic sharpshooter consolidation (§12.3) | `kod-core/src/engine/mod.rs` (`sharpshooter_due`, `drain_due_sharpshooter`) | the memory-consolidation task sets an `AtomicBool` when a pass moved facts; `drain_due_sharpshooter` runs at the top of every `process_for` and `process_streaming_for` turn, so the consolidation happens on the engine's own async context | engine tests |

| TTSR stream rules (§14.3) | `kod-provider/src/ttsr.rs` | rules matched against streaming output: `RuleScope` (Text/Thinking/Tool with name+path patterns), `InterruptMode` (Never/ProseOnly/ToolOnly/Always), `RepeatMode` (Once/Gap). A bad regex drops the rule rather than stopping the stream. `builtin_rules()` ships no-TODO-in-diff and no-secret-in-prose | 16 tests |
| Conventional-commit validation (§14.4) | `kod-stats/src/commit.rs` + `kod-cli/commands/admin.rs::run_commit_check` + `kod commit-check` | validator: type / summary / details / paths, type-path consistency; `score_change` weights details; `format_message` renders. CLI: reads `git diff --cached --name-only`, composes a proposal from the flags, prints the message or the rejection reason and exits non-zero on rejection | 22 unit tests + CLI tests |

| TTSR stream rules (§14.3) | `kod-provider/src/ttsr.rs` | rules matched against streaming output: `RuleScope` (Text/Thinking/Tool with name+path patterns), `InterruptMode` (Never/ProseOnly/ToolOnly/Always), `RepeatMode` (Once/Gap). A bad regex drops the rule rather than stopping the stream. `builtin_rules()` ships no-TODO-in-diff and no-secret-in-prose | 16 tests |
| Conventional-commit validation (§14.4) | `kod-stats/src/commit.rs` | `CommitProposal` validated for type/summary/details/paths and type-path consistency (docs->*.md, ci->.github, build->Cargo.toml); `score_change` weights details by importance; `format_message` renders | 22 tests |

## How to update this file

Add a row to the wired table when a primitive gains a real caller.
Move a row out of "Not yet built" in the same commit that installs
the caller — do not leave the table stale. A stale status table is
worse than none, because it teaches a reader to distrust it.
