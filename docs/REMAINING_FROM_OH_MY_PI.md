# Remaining work from the oh-my-pi borrow

Companion to `docs/brainstorms/kod_borrow_from_oh_my_pi.md`. Status of
every section after the 2026-09-30/10-01 implementation sessions.

Status legend: **DONE**, **PARTIAL** (primitive or core landed, a
consumer or a design piece remains), **ABSENT**, **DECIDED-NO**
(considered and declined, with the reason).

---

## Done

| § | Title | Where |
|---|---|---|
| 2 | Cache-coherent transcript editing | `kod-core/src/transcript_coherence.rs`, `context_gauge.rs` |
| 3 | Supersede pruning, shake, cache-warm guard | `prune.rs`, `shake.rs` |
| 4.1 | Ordered multi-method dispatcher | `compaction_dispatcher.rs` |
| 4.2 | Speculation lead band | `compaction.rs` (`lead_band_tokens`, `arm_threshold_tokens`) |
| 4.3 | Budget projection + no-reduction guard | `compaction_dispatcher.rs` (`CompactionAdmission`) |
| 4.4 | Anthropic native compaction lane | `kod-provider-anthropic/src/wire.rs`, `native_compaction.rs` |
| 5 | Shell output minimizer | `kod-minimize/` |
| 6 | `xd://` lazy tool mounting | `kod-tools/src/xd_handler.rs` |
| 7.1 | Hashline edit mode + tag-aware read | `kod-tools/src/edit_hashline.rs`, `edit_tool.rs`; `read_file` `numbered` mode |
| 7.2 | LSP write-through + deferred diagnostics | `kod-lsp/src/client.rs`, `kod-core/src/deferred_diagnostics.rs` |
| 7.3 | tree-sitter parse cache | `kod-ast/` (9 grammars + cache + extractors); `repomap.rs` |
| 7.4 | jfind semantic search | `kod-core/src/jfind.rs`, `jfind_tool.rs` |
| 7.5 | Internal-URL router | `kod-tools/src/internal_url.rs` |
| 7.6 | Hub tool | `kod-core/src/hub_tool.rs` (messaging + jobs; process ops N/A) |
| 7.7 | Fast-wins bundle | all 8 items — see below |
| 9.1–9.11 | Loop hardening | `retry.rs`, `retry_safety.rs`, `stream_guard.rs`, `unexpected_stop.rs`, `judgment.rs`, `auto_thinking.rs`, `pause_gate.rs`, `concurrency.rs`, image budget, run collector |
| 10 | Speculative read execution | `kod-core/src/speculation.rs` |
| 11.1–11.12 | Multi-agent / async / lifecycle | `kod-swarm/`, `async_delivery.rs`, `goals.rs`, `plan.rs`, `prewalk.rs`, `worktree_isolation_ownership.rs` |
| 12.1–12.8 | Memory upgrades | `kod-memory/` |
| 13.1–13.3 | Catalog, stats, if-bench | `kod-provider/src/catalog.rs`, `kod-stats/` |
| 14.1 | Secret placeholders | `kod-types/src/secret_placeholder.rs` |
| 14.2 | Capability discovery | `kod-skills/src/capability.rs` |
| 14.3 | TTSR | `kod-provider/src/ttsr.rs` |
| 14.4 | Conventional-commit validation + map-reduce | `kod-stats/src/commit.rs`, `commit_mapreduce.rs` |
| 14.5 | OTLP telemetry, query parser, exit diagnostics, web-retry | `kod-telemetry/`, `query_syntax.rs`, session-log exit markers, `web_retry.rs` |

### §7.7 detail

| Item | Status |
|---|---|
| 1. `useless` flag + truncation meta | DONE — `tool_result_meta.rs`, engine flag |
| 2. Non-interactive env | DONE — `env_policy.rs` |
| 3. Bash interceptor | DONE — `bash_interceptor.rs` |
| 4. Conflict-resolution loop | DONE — `conflict_handler.rs` (`conflict://`) |
| 5. Inline sloppy-edit recovery | DONE (strict) — `patch_text.rs`; fuzzy matching not built (see Decided-No) |
| 6. MCP tool cache | DONE — `kod-mcp/src/tool_cache.rs` |
| 7. Walker scan cache | DONE — `kod-tools/src/walk_cache.rs` |
| 8. Tool-choice queue | PARTIAL — `ToolChoice` directive on `GenerationOptions` honored by both providers; the design's abort policies (`requeue`/`drop`/`drop_sequence`) and the non-forcing pending-invoker path are not built |

---

## PARTIAL — a primitive exists, a piece remains

| § | What's done | What's left |
|---|---|---|
| 4.5 | Snapcompact bitmap frames wired | experimental; no change needed unless the design evolves |
| 7.2 | inline + deferred + version-echo + batch mode | none known |
| 9.10 | image budget + undecodable degrade | none known |
| 14.4 | validator + CLI `commit-check` + map-reduce | the **disposable agent** that reads a diff and proposes a commit needs a live model and a custom-tool surface — a design project, not a patch |
| 14.5 | query parser, exit diagnostics, web-retry, OTLP | **deferred custom tools** (`pushPendingAction` + `resolve`), **scan-plan tamper evidence**, **OTLP GenAI semconv**, **model roles + chains**, **RPC event-stream hygiene**, **two-phase extension load** — each needs a subsystem that does not exist today |
| 7.7 item 8 | `ToolChoice` directive | abort policies + non-forcing pending invokers |

---

## DECIDED-NO — considered and declined

| § | Decision | Why |
|---|---|---|
| 8 | In-process shell | **L** strategic rewrite. The design itself notes it trades kod's sandbox (bwrap/Landlock/Seatbelt) for latency on shell-heavy workflows. The bash interceptor + walker cache already capture the practical wins. Revisit only with a benchmark proving the latency is the bottleneck. |
| 7.7 item 5 (fuzzy) | Fuzzy patch matching | A strict search (exactly-once context match) ships; a fuzzy matcher is where a bug silently edits the wrong lines. Strict is safer; a model retries a rejected patch. |
| 4.5 | (nothing) | frames work as designed |

---

## Notes / follow-ups (not plan items)

- **Budget accounting** — `prompt_allocation` sums tool-schema bytes by `len()`, not tokens. It is capped at 3/4 of the budget so it cannot brick a small-window session, but a proper per-tool token count would let the reserve be proportional instead of clamped.
- **`9cd92ab`** — a mixed commit (`chore: rm bug report` carrying 7 files). Fixing it needs a history rewrite.
- **`docs/brainstorms/kod-code-audit-report.md`** — a static-analysis audit (127 findings). Being triaged separately; see the resolution notes added to that file.
