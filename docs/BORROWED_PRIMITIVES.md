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

## Config surface landed, engine-side reader pending

| Primitive | Config field | Blocked on |
|---|---|---|
| `AutoThinking` | `EndpointConfig.effort` (`"auto"` sentinel) + `ModelInfo.efforts` | Judge-role resolution: the classifier's `JudgmentClient` has no engine path to a `judge` model role. |

## Present and tested in isolation; no engine caller yet

Each of these is a self-contained primitive with a full test suite.
None is broken; none is inert because of a bug. They await a policy
question the doc leaves open, or a supporting tool.

| Primitive | Module | Blocked on |
|---|---|---|
| `UnexpectedStopClassifier` | `kod-core/src/unexpected_stop.rs` | Judge role (same as AutoThinking). Classifier is a call to the judgment framework; the framework exists, the role resolution does not. |
| `Advisor EmissionGuard` | `kod-swarm/src/advisor.rs` | The `advise` tool itself. The guard is the tool's admission policy; the tool does not exist yet. |
| `ReplaySafety` (`AttemptTracker` + `EmptyCompletionRetry`) | `kod-provider/src/retry_safety.rs` | Adoption inside each provider crate's `stream_completion` / `complete`. Clean change but per-crate. |
| `StreamGuard` (exact-cycle + header-runaway) | `kod-provider/src/stream_guard.rs` | Same: adoption inside each provider's SSE loop. |
| `RetryHints` (`extract_retry_hints`) | `kod-provider/src/retry.rs` | Same: called by a provider's error path before constructing `KodError::RateLimited`. |
| `ProviderConcurrency` | `kod-provider/src/concurrency.rs` | Same: an `acquire`/drop bracket around each provider's request. |
| `ToolSearchTool` | `kod-tools/src/tool_search.rs` | Engine tool inventory wiring (the tool exists; the engine does not yet mount it). |
| `BatchTool` | `kod-tools/src/batch.rs` | Registry wiring. |
| `UnexpectedStopClassifier` | (see above) | — |

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
