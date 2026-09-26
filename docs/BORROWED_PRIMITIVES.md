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
- **§11.9 cleanse loop** — only a doc-comment reference in
  `work_pool.rs`; no `cleanse` module.
- **§11.10 plan-mode hardening**, **§11.11 worktree isolation GC**,
  **§11.12 prewalk** — absent.
- **§12.2 veracity consolidation** — `MemoryManager::consolidate` does
  archival + near-duplicate fusion; the Bayesian confidence update and
  contradiction resolution are not implemented, though
  `MemoryEntry.superseded_by` / `.contradicts` fields exist.
- **§12.3 sharpshooter** (friction-gated decision memory) — absent.
- **§12.5 memory pipeline hygiene** — absent.
- **§12.6 retention cadence / rolling hash** — absent.
- **§12.7 mental models** — absent.
- **§12.8 smaller memory borrows** (episodic tier degradation,
  polyphonic RRF, query-intent biasing, MMR) — absent.
- **§13 catalog metadata, per-request stats, if-bench** — absent; no
  catalog or stats crate.
- **§14.2 capability discovery registry** — absent.
- **§14.3 TTSR** — absent.
- **§14.4 agentic commit** — absent.
- **§14.5 OTLP telemetry, MCP header/origin policy** — absent.
- **Cold-revive surface rebuild** — `SessionInit` and its reader are
  landed; the consumer that rebuilds a session's tool surface from
  the entry is not.

## How to update this file

Add a row to the wired table when a primitive gains a real caller.
Move a row out of "Not yet built" in the same commit that installs
the caller — do not leave the table stale. A stale status table is
worse than none, because it teaches a reader to distrust it.
