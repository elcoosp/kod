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
| Internal-URL router (§7.5) | `kod-tools/src/internal_url.rs` | `ProtocolRouter` + `ProtocolHandler` + `ResolveContext`; the `read_file` and `write_file` tools dispatch on handled schemes; the engine installs a router holding an `ArtifactHandler` + a `MemoryHandler` into every per-call `ToolContext`, and exposes `store_artifact` for offload sites | `internal_url.rs` (25 unit + 7 integration); `memory_handler.rs` (6); `minimizer_integration.rs` round-trips artifact:// through read_file |
| Shell-output minimizer (§5) | `kod-minimize/` | new crate: command classifier, 5 TOML defs (git-status, git-log, cargo-check, cargo-test, pytest), pipeline stages; `execute_command` runs captured stdout through it and offloads the raw capture as an `artifact://` | 54 unit tests in the crate; 5 integration tests in `kod-tools/tests/minimizer_integration.rs` (real git repo, minimize + offload + round-trip) |
| Secret placeholders (§14.1) | `kod-types/src/secret_placeholder.rs` + `secret_sources.rs`; engine wiring in `kod-core/src/engine/mod.rs` | per-install HMAC key + forward/reverse maps; env-scanner (name-heuristic), vendor regexes (12 patterns with literal-prefix short-circuits), connection-URL password extractor; obfuscate on the outbound path (system segments + message content), deobfuscate tool args on the inbound path; CLI/TUI install a vault at startup | 21 vault unit tests + 25 source-scanner unit tests; 4 end-to-end tests in `kod-core/tests/secret_placeholder_integration.rs` (round-trip with a recording provider) |
| Handoff compaction (§4.1) | `kod-core/src/compaction_dispatcher.rs` (`HandoffMethod`) | one LLM call producing a briefing document (Objective / Done / State / Next / Constraints); returned as `CompactionPlan::Summary` covering the older half. `CompactionContext` gains a `ProviderHandle` (None when no provider → `Unavailable`). Sits *last* in the engine's ladder: mechanical rungs run first, handoff catches the case where they cannot reduce | dispatcher unit tests; integration tests in `kod-core/tests/handoff_compaction.rs` (scripted provider produces a handoff, the plan is applied) |
| Provider-native compaction (§4.4) | `kod-provider-anthropic/src/wire.rs` (`build_compaction_body`, `parse_compaction_block`, `prepend_compaction_block`); `LlmProvider::native_compact`; `RemoteMethod` in the dispatcher | Anthropic `compact-2026-01-12` beta: the API summarizes, returns a `compaction` block with an `encrypted_content` token. `RemoteMethod` calls it, produces a `NativeSummary` plan; `apply_compaction_plan` stores the block per-transcript; `build_grounded_request` attaches it to the next request; `build_messages_body` prepends it to the first user message | wire-layer unit tests (11); dispatcher unit tests; end-to-end replay test |

## Primitive landed; engine wiring is a follow-up

| Primitive | Module | What's landed | What's missing |
|---|---|---|---|
| Speculative read execution (§10) | `kod-core/src/speculation.rs` | `Evidence` + `capture_evidence` + `read_with_evidence` + `validate` + `consume`: the TOCTOU shape, with a digest gate that catches the same-inode same-size same-mtime edit (11 tests). | The streaming-loop hook. Value comes from overlapping the read with the provider's generation tail, which requires recognizing a `read_file` candidate mid-stream — incremental JSON parsing in the `partials` map at `stream_round`'s `ToolCallDelta` arm. A round-level prefetch (the easy shape) has no benefit: `run_tool_calls` fires after the round completes. |

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
