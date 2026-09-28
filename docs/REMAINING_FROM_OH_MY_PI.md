# Remaining work from the oh-my-pi borrow

Companion to `docs/brainstorms/kod_borrow_from_oh_my_pi.md`. An audit of
every item in that brainstorm against the tree, taken section by
section. Only the **outstanding** items are detailed here; a section
marked "done" is listed for completeness so a reader knows it was
checked and not skipped.

Status legend:

* **ABSENT** — nothing in the tree.
* **PARTIAL** — a primitive exists; the consumer, guard, or config that
  makes it useful does not.
* **PRIMITIVE-ONLY** — the module and its unit tests exist; no caller.

Sizes (S / M / L) are the brainstorm's own estimates.

---

## Done — no action

These sections are fully wired and are not repeated below.

| § | Title | Where |
|---|---|---|
| 2 | Cache-coherent transcript editing | `kod-core/src/transcript_coherence.rs`, `context_gauge.rs` |
| 3 | Supersede pruning, shake, cache-warm guard | `prune.rs`, `shake.rs` |
| 5 | Shell output minimizer | `kod-minimize/` |
| 6 | `xd://` lazy tool mounting | `kod-tools/src/xd_handler.rs` |
| 7.5 | Internal-URL router | `kod-tools/src/internal_url.rs` |
| 9.1 | Oneshot retry kit + hint extraction | `kod-provider/src/retry.rs` |
| 9.2 | Replay-safe stream retry | `kod-provider/src/retry_safety.rs` |
| 9.3 | Thinking-loop guard | `kod-provider/src/stream_guard.rs` |
| 9.4 | Unexpected-stop classifier + tool-loop guard | `unexpected_stop.rs`, `tool_loop_guard.rs` |
| 9.5 | Judgment framework | `kod-provider/src/judgment.rs` |
| 9.6 | Auto-thinking | `kod-core/src/auto_thinking.rs` |
| 9.8 | Pause gate | `kod-core/src/pause_gate.rs` |
| 9.9 | Provider concurrency bracket | `kod-provider/src/concurrency.rs` |
| 9.11 | Run collector | `kod-core/src/run_collector.rs` |
| 10 | Speculative read execution | `kod-core/src/speculation.rs` |
| 11.1 | WorkPool | `kod-swarm/src/work_pool.rs` |
| 11.2 | Park/revive | `kod-swarm/src/agent_registry.rs` |
| 11.3 | IrcBus | `kod-swarm/src/irc_bus.rs` |
| 11.4 | Async job delivery + live-child auto-detach | `kod-core/src/async_delivery.rs`, `kod-tools/src/context.rs` |
| 11.5 | YieldQueue | `kod-swarm/src/yield_queue.rs` |
| 11.6 | Goals runtime | `kod-core/src/goals.rs` |
| 11.7 | TodoTracker nudge | `kod-tools/src/todo_tracker.rs` |
| 11.8 | Advisor emission guard | `kod-swarm/src/advisor.rs` |
| 11.9 | Cleanse scheduler | `kod-swarm/src/cleanse.rs` |
| 11.10 | Plan-mode hardening kit | `plan.rs`, `engine/mod.rs`, `llm.rs::RoutingConfig.plan` |
| 11.11 | Isolation-ownership GC | `worktree_isolation_ownership.rs` |
| 11.12 | Prewalk | `kod-core/src/prewalk.rs` |
| 12.1 | Weibull curves | `kod-memory/src/retrieval.rs` |
| 12.2 | Veracity consolidation | `kod-memory/src/veracity.rs` + engine |
| 12.3 | Sharpshooter | `kod-memory/src/sharpshooter.rs` + engine |
| 12.4 | Memory-write redaction | `manager.rs::redact_text` |
| 12.5 | Pipeline hygiene pack | `hygiene.rs`, `fusion.rs` |
| 12.6 | Retention cadence | `kod-memory/src/retention.rs` |
| 12.7 | Mental models | `kod-memory/src/mental_models.rs` + engine |
| 12.8 | Tier, RRF, intent blend, MMR | `tier.rs`, `fusion.rs` |
| 13.1 | Model catalog | `kod-provider/src/catalog.rs` + engine |
| 13.2 | Stats: behavioral + per-request | `kod-stats/src/request.rs`, `behavioral.rs` |
| 13.3 | if-bench | `kod-stats/src/if_bench.rs` + `kod if-bench` |
| 14.1 | Secret placeholders | `kod-types/src/secret_placeholder.rs` |
| 14.2 | Capability discovery | `kod-skills/src/capability.rs` |
| 14.3 | TTSR | `kod-provider/src/ttsr.rs` |
| 14.4 | Conventional-commit validation | `kod-stats/src/commit.rs` + `kod commit-check` |
| 14.5 | OTLP telemetry | `kod-telemetry/` |
