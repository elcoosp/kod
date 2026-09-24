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
| `ProviderConcurrency` | `kod-provider/src/concurrency.rs` | `acquire()`/drop bracket around every streaming HTTP request in both provider crates | `concurrency.rs` unit tests; retry-hit assertions in `kod-provider-anthropic/tests/stream_retry.rs` |
| `StreamGuard` | `kod-provider/src/stream_guard.rs` | fed every model-authored chunk in both providers' SSE loops; `StallVerdict::Loop` → transient `KodError::Provider` | `stream_guard.rs` unit tests (`feed_chunk` covered); `stream_retry.rs` exercises the delivered-vs-retried fork |
| `ReplaySafety` (`AttemptTracker` + `EmptyCompletionRetry`) | `kod-provider/src/retry_safety.rs` | attempt loop in both providers' `stream_completion` paths: retry only while nothing has committed; `EmptyCompletionRetry` covers the clean-but-empty case | `retry_safety.rs` unit tests; `kod-provider-anthropic/tests/stream_retry.rs` **and** `kod-provider-openai/tests/stream_retry.rs` each pin the three-way fork (HTTP error / empty / committed) |
| `RetryHints` (`extract_retry_hints`) | `kod-provider/src/retry.rs` | Anthropic native-Messages error paths extract hints from response headers before constructing `KodError`; the OpenAI-compatible path cannot (its `AdkError` transport exposes no headers) | `retry.rs` unit tests; no integration test yet |
| `AutoThinking` | `kod-core/src/auto_thinking.rs` | `resolve_turn_effort` in `engine/mod.rs`: an endpoint with `effort = "auto"` classifies the turn via a `judge`-role `JudgmentClient`, using the model's ladder from `model_catalog` | `auto_thinking.rs` unit tests (classifier in isolation); no engine-level integration test yet |
| `UnexpectedStopClassifier` | `kod-core/src/unexpected_stop.rs` | `diagnose_unexpected_stop` in `engine/mod.rs`, called at both post-stream sites after `remember_turn_for`. **Diagnostic only**: verdict is logged, no corrective is emitted yet | `unexpected_stop.rs` unit tests (classifier in isolation); no engine-level integration test yet |
| `ToolSearchTool` | `kod-tools/src/tool_search.rs` | registered at `engine.start()` from the engine's `tool_inventory` (refreshed in `refresh_tool_inventory`) | `tool_search.rs` unit tests |
| `BatchTool` | `kod-tools/src/batch.rs` | registered at `engine.start()` with a `Weak<ToolRegistry>` backreference so it cannot outlive the tools it dispatches to | `batch.rs` unit tests |
| `Advisor EmissionGuard` | `kod-swarm/src/advisor.rs` + `kod-core/src/advisor_tools.rs` | the `advise` tool runs the four-stage pipeline (empty / noise / rank-aware dedupe / budget) and routes admitted notes through the engine's steer queue; `begin_update` resets the per-turn budget at the top of `process_streaming_with_model_for` | `advisor.rs` unit tests (34); `advisor_tools.rs` (12, including the doc's 114-Stops incident) |

## Deferred to a future slice, not-yet-built

These are named in the design note but not implemented; each is
larger than the primitives above.

- Anthropic native compaction lane (`compact-2026-01-12`).
- Snapcompact rasterizer + inline tool-result imaging.
- Handoff summarizer (the `handoff` rung of the dispatcher).
- Reversible keyed secret placeholders.
- Internal-URL router (`xd://`, `memory://`, `artifact://`, …).
- Speculative read execution.
- WorkPool / park-revive / IrcBus / YieldQueue (the swarm layer).
- `xf`-style shell output minimizer.

## How to update this file

Add a row to the wired table when a primitive gains a real caller.
Move a row from "orphaned" to "wired" in the same commit that
installs the caller — do not leave the table stale. A stale status
table is worse than none, because it teaches a reader to distrust it.
