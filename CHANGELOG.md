# Changelog

## Unreleased — production readiness pass

Fixes from a full-workspace production-readiness review. Every item
below is covered by a test; the targeted suites are green.

### Security

- **PolicyEngine installed on every command that builds an engine.**
  `kod prompt`, `kod run`, `kod swarm`, and `kod replay --execute`
  were building engines *without* installing a policy; the engine
  fallback is allow-all, so the `standard` preset's write-approval
  and deny rules silently did nothing on those paths.
- **Parallel read-only rounds honour denials.** A `Deny` decision
  (policy or hook) was checked only in the serial mutating branch;
  a read-only round mapped every call straight to `execute_tool`,
  so a denied `read_file` / `grep` / `web_fetch` still ran.
- **`kod replay --execute` requires `--yes`.** A recorded session
  log is effectively an executable script of tool calls; without
  the explicit flag the command now refuses, with a summary of the
  destructive calls it would have run.
- **Project `.kod/policy.toml` can only narrow.** A repo shipping
  `preset = "yolo"` or `[tools.write_file] mode = "allow"` could
  escalate past the user's global policy with no consent gate; the
  effective preset is now `min(global, project)` and per-tool mode
  is `max(global, project)`.
- **Path resolution normalizes `..` before glob matching.**
  `src/../secrets/x` matched a `src/**` allowlist lexically while
  the tool wrote `<wd>/secrets/x`.
- **Hooks no longer splice model arguments into `sh -c`.** The
  template references `$KOD_PATH`-style env vars, and the runner
  caps output at 8 KiB and enforces a 30 s timeout.
- **`.git/` is denied for `write_file` / `patch_file`.** The "git
  mutations go through the git tool" contract was previously only
  enforced on the shell path.
- **Sandbox profile tempfile is `O_EXCL` + 0600.** The pre-fix
  `/tmp/kod-sandbox-<pid>.json` was guessable and world-readable.
- **`execute_command` runs in the context working directory and
  with secret-shaped env vars stripped.** The child no longer
  inherits `ANTHROPIC_API_KEY` / `OPENAI_API_KEY` / tokens.

### Data integrity

- **Semantic memory works.** The embedder's dimension cache was
  only warmed inside `embed()`, and every `embed()` call was gated
  on `dims() > 0` — a circular gate that made semantic scoring
  permanently dead. `store_with_metadata` now embeds at write time;
  `rebuild_index` embeds missing entries and persists.
- **Checkpoint ids survive >10 000 snapshots.** The zero-padded
  counter rendered 5 digits past 9 999 and sorted before 9999, so
  retention started deleting the newest snapshots.
- **Checkpoint restore snapshots the current file first.** A
  mistaken restore no longer permanently destroys the working
  version. Size checks now go through `metadata()`, not a full read.
- **Swarm dispatch keys are per-subtask.** Two subtasks on the same
  pool agent shared one transcript key; one agent's failure path
  cancelled and wiped its peer's history mid-flight.
- **Swarm cleanup preserves unmerged branches.** `Drop` used to
  `git branch -D` unmerged agent commits after a conflict.
- **Session log writes are atomic single `write_all` calls, and a
  truncated final line is tolerated** rather than failing the read.
- **Memory store caps content at 4 KiB and dedups by content hash;
  retrieval write-back re-reads each entry by id** so it no longer
  resurrects entries consolidation just deleted.
- **Short-term memory uses a single lock** (the pre-fix two-lock
  shape had an inversion that could deadlock) and `store` replaces
  in place when the id already exists.
- **History cap is pinned-aware.** A user-pinned turn is no longer
  silently destroyed by tool-round persistence.

### Provider wire layer

- **Anthropic SSE decoder is byte-safe.** Multi-byte characters
  split across TCP chunks were previously decoded per-chunk with
  `from_utf8_lossy`, corrupting streamed text and tool arguments.
- **Both providers use the shared `RetryPolicy`.** The retry module
  in `kod-provider` was dead code; the OpenAI path used a substring
  classifier and Anthropic had no retry at all.
- **`StopReason` is a `StreamChunk` variant** so a truncated
  response is distinguishable from a clean finish.
- **Legacy `stream` terminates on `Err`** instead of emitting
  `Err` followed by `Usage` + `Done`.

### Tools

- **`patch_file` holds its path lock across read + diff + write.**
  Two concurrent patches previously diffed against the same
  original; the second silently reverted the first.
- **`git` and `check` children are `kill_on_drop`** so a timed-out
  tool no longer leaves a zombie process holding the target-dir
  lock.
- **Writes are atomic** (temp + fsync + rename).
- **The diff parser honors file headers only before the first hunk**
  (so a `--- ` line inside a hunk survives), strips/re-emits CRLF,
  and clamps the `@@ -0,0` new-file header.
- **Invalid globs in a forbidden list fail closed**, not open.

### Engine

- **Auto-check paths carry the round's structured transcript** —
  the model now learns the write outcome under the default flags.
- **Structured tool-result messages are capped** (16 KiB) so a
  256 KB `read_file` repeated over 40 rounds cannot grow the
  transcript past the endpoint's window.
- **Token usage accumulates** across rounds; the pre-fix shape
  kept only the last round's report.
- **Provider stream has an idle-chunk deadline** (120 s) and
  preserves partials on error.
- **`cancels` is a `parking_lot::RwLock`**, no `block_on` on the
  async hot path.
- **Jev memory filter keeps the original on over-filter** (the doc
  comment said it did; the code returned `Vec::new()`).
- **Diagnostic triage fails open** when Jev answers only some of a
  batch instead of hiding the unanswered ones.

### TUI

- **Esc with completions open keeps the typed draft.**
- **Phase-change check is async** instead of
  `block_in_place(block_on(...))` in the event handler.
- **`/regenerate` and `/delete` rewind the engine transcript**, not
  only the display.
- **Bracketed paste is enabled**, so a code block paste no longer
  submits its first line as a prompt.

### CLI / CI

- **`kod run` returns the engine outcome** (the pre-fix `let _ =`
  made every failure exit 0).
- **`kod swarm -n N` honors the requested agent count.**
- **`kod skills new` generates parseable frontmatter** (the
  template carried 9-space continuation indents from source).
- **Release signing is gated on a step output**, not on a
  workflow-level `env` that could not see `secrets`.
- **`live-anthropic` runs when the secret is present** — the
  pre-fix job-level `if:` on `secrets.*` always evaluated false.
- **`RUSTFLAGS: -D warnings` is scoped to the lint job**, and
  `--locked` is set on every CI cargo invocation.
- **MSRV is declared (1.85).**

### Structural

- **`engine_from_config`.** The seven CLI commands that built a
  `KodEngine` (chat, swarm, agent, prompt, streaming-prompt, acp,
  serve) each carried an 80-line hand-rolled bootstrap. `run_agent`
  never set `network_access`, `run_swarm` never set hooks or limits,
  and the P0-1 missing-policy bug existed because three of the seven
  forgot the policy install. All seven now route through one helper
  with an explicit `EngineBootstrapOptions`; the shared shape is the
  only shape.
- **`install_session_recorder` / `install_jev`.** The two remaining
  per-command installs that were safe to factor — both were
  duplicated verbatim between chat, prompt, agent, and swarm.

### Documentation

- CONTRIBUTING.md MSRV corrected.
- Function docs that promised behaviour the code did not have
  (the ACP approval-batch fallback, the LSP `ensure_open`
  content, the swarm hub teardown, the memory filter's
  fail-open) now match the code.

## Unreleased — follow-up batch

A second batch landed after the first round of hardening. Every item
below is tested; the workspace suite is green.

### Persistence

- **Plans and decisions survive restarts.** `state.json` next to the
  trace log carries both maps. Every mutation writes atomically
  (temp + rename); a crash mid-write leaves the previous state
  intact. `/plan` and `/decisions` see the restored values on the
  next session.

### Redaction depth

- **In-prompt redaction (opt-in).** `[security.redact] in_prompt =
  true` runs the redactor over outgoing messages *and* every tool
  result before the prompt is built. Off by default — the model
  needs the code it is editing.
- **`/stats` reports redaction counts.** Each rule's hit count for
  the session, aggregated from the `SessionEntry::Redaction`
  lines.

### Fixture fidelity

- **Tool arguments flow to fixtures.** `TurnTrace::ToolCallTrace`
  now carries the arguments and a result summary; `kod fixture save`
  copies both. A replay can drive the tool round trip instead of
  only comparing the prompt's shape.
- **`kod trace replay <id>`.** Re-drives one turn from `turns.jsonl`
  against a fresh engine and diffs the request summary. `--strict`
  exits non-zero on divergence.

### CLI parity

TUI commands now have CLI mirrors that read the same persisted
state:

- `kod jev status | stats | test | tune | tune set | tune reset`
- `kod budget show`
- `kod limits show`
- `kod plan show | json`
- `kod decisions show | json`

### Testability

- **`JevDecider` trait.** The engine stores `Arc<dyn JevDecider>`
  instead of the concrete `JevClient`. Tests inject a
  `ScriptedJev` that returns deterministic verdicts, so the
  mid-stream switch path is exercised end to end without a network
  round-trip.
- **Mid-stream switch is tested.** A `ScriptedJev` returns an
  `OffTrack` verdict after the primary has emitted enough text; the
  test asserts the final stream contains both the primary's marker
  and the fallback's.

### Fixes

- `/remember` and `/memory` require an engine — they previously
  opened a second `MemoryManager` on the same redb file and
  collided with the engine's own handle.
- The approval dialog's legend fits on one line.

## Unreleased — production hardening

A large batch of features landed on top of the v0.1.0 baseline. Every
item below is tested; the workspace suite is green. The shape of the
changes, in the order they matter for a new user:

### Speed: tools and routing get out of the way

- **Tool inventory pre-filtering** — Jev picks which tool categories
  the request needs; the model sees only those. Cuts TTFT 30–50 % on
  small models. Opt-in via `[jev] enabled = true`.
- **Interruptible streaming** — every 5 chunks after 200 chars, Jev
  judges whether the reply already answers the request. If so, the
  stream is cut. Model-agnostic, gated by `early_termination_min`.
- **Per-round model routing** — planning, tool execution, synthesis,
  and summary rounds can each route to a different endpoint via
  `[jev.round_routing]`. Route a summary to a local model, keep the
  planner on cloud.
- **Prose vs reasoning filter (TUI)** — every 5 chunks, Jev
  classifies the running text. Reasoning and restatement fold into
  the existing spinner instead of the chat. A 20-second safety valve
  (`[jev] reasoning_timeout_secs`) forces the buffer to render as
  prose if the classifier has been hiding text too long.

### Tokens: less input, same answers

- **Dynamic prompt budget** — the fixed 50/20/20/10 share between
  history, skills, memory, and repomap is now reweighted per round by
  Jev.
- **Tool result compression** — large `read_file` results drop the
  lines Jev judges irrelevant; the shape is preserved with an
  elision marker so line numbers stay meaningful.
- **Grep/search ranking** — a search with more than 20 hits is
  ranked by Jev and trimmed to the top few.
- **Diff hunk triage** — context-only hunks are elided from the
  prompt; the unified-diff header is preserved.
- **Memory entry filter** — the hybrid retrieval's top-N is filtered
  again by Jev relevance before the prompt is built.

### Friction: fewer dialogs, fewer re-asks

- **Confidence-gated auto-approval** — a tool call that Jev thinks
  the user would almost certainly approve, and that is not
  destructive, runs without a dialog. Session-scoped.
- **Learned allows** — `l` in an approval dialog teaches a
  session-wide allow for that exact call. `/learned clear` forgets.
- **Partial-hunk approval** — `h` in a `patch_file` dialog enters
  hunk selection. Toggle with Space, commit with Enter; the engine
  runs the policy gate on the filtered patch.
- **Edit-and-approve** — `e` opens a JSON editor for the call's
  arguments. Enter sends `ApproveWith` with the modified args; the
  policy gate re-runs on the edit.
- **Ambiguity pre-detection** — a request Jev judges ambiguous
  prompts for clarification before the LLM sees it.
- **Ask-user context answer** — an `ask_user` the request already
  answers is answered from context instead of interrupting.
- **Per-command sandbox decision** — `Auto` mode asks Jev whether
  the command needs OS-level sandboxing; `safe` runs unbubbled,
  `filesystem_risk` keeps the sandbox.

### Quality: better defaults, better behaviour

- **Task classification refinement** — the keyword router's verdict
  is overridden by Jev when Jev is more confident.
- **Skill semantic matching** — the substring matcher's results are
  unioned with Jev's semantic scoring, catching skills the lexical
  match missed.
- **Diagnostic line-shift handling** — a diagnostic that moved
  because of an earlier edit is no longer reported as new.
- **Citation semantic verification** — after the syntactic
  file:line check, a semantic pass catches citations whose lines
  do not support the claim.
- **Response quality gate** — a reply Jev judges off-track gets an
  advisory at the end, suggesting `/regenerate`.
- **Tool outcome classification** — every slow or failed tool call
  is classified (success / partial / failure / irrelevant) and
  logged as a `ToolOutcome` session entry.
- **Plan artifact** — Complex and MultiStep tasks get a plan on the
  first turn, re-rendered into every subsequent system prompt.
  `/plan` views and edits it. A `plan_update` tool lets the model
  keep it in sync.
- **Decisions log** — durable decisions (preference, approach, file
  change, constraint) are extracted automatically and survive FIFO
  history truncation. `/decisions` views and prunes.
- **Phase-aware handoff** — a confident phase change suggests
  `/handoff`.

### Safety: new default protections

- **Secret redaction** — the session log redacts API keys, JWTs,
  PEM blocks, and high-entropy tokens near key-ish keywords before
  writing. `/redact test <string>` shows what would be redacted.
- **Read-protection policy** — `.env`, `**/*.pem`, `.aws/credentials`,
  `.ssh/**`, and similar paths are redacted (default) or refused on
  read. Configured under `[policy.read_protection]`.
- **Cost caps** — `[limits] max_cost_usd_per_session` and
  `max_cost_usd_per_turn` refuse a round that would exceed them.
  `/budget` shows the current spend; `/budget raise <usd>` lifts the
  cap for the session.
- **Tool quotas** — `[limits.tools]` caps per-tool calls per turn
  and per session. A `per_command` sub-cap catches
  retry-the-same-broken-thing loops.
- **Trust boundaries** — `web_fetch` and MCP output is marked
  `trust=untrusted` in the prompt. A high-impact tool whose call was
  suggested by untrusted content forces an approval dialog
  regardless of policy. `/trust` shows the current taint; `/trust
  clear` resets it.

### Observability: fewer mysteries

- **Turn traces** — one JSONL record per turn with rounds, tools,
  retries, cost, tokens, Jev decisions. `/trace` renders the tree;
  `kod trace list/show/json` does the same from the shell.
- **`/jev stats`** — the Jev share of decisions, cache hit rate, and
  average latency for the current session.
- **`/jev tune`** — set any threshold, persist to config, hot-reload
  the client.
- **`/jev test`** — pings the endpoint with a trivial question and
  reports the reply.
- **`kod fixture save/replay/list`** — save a session's turns as a
  deterministic fixture; replay detects prompt-shape drift. `--first-round-only`
  is safe in CI.
- **`/limits`** — per-tool counters, one table.
- **`/blackboard`** — the shared swarm key-value store.

### UX: fewer keys to remember

- **Command palette (Ctrl+K)** — every slash command and keybinding
  in one fuzzy-searchable list. Enter accepts, Esc closes.
- **Session schema version** — `tui_session.json` now carries a
  `schema_version`; old bare-array files are still loaded.

### Error handling

- **TurnFailure taxonomy** — timeouts, auth errors, refused
  prompts, context overflow, malformed JSON, and hallucinated tool
  names are distinguished. Same-endpoint retries use different
  strategies (lower temperature, constrained output, shrink
  history, reinject tools) before falling through to the next
  endpoint. Non-retryable classes (auth, policy, budget, content
  filter) surface immediately.

### Behaviour changes worth noting

- **`/remember` and `/memory` now require an engine.** They
  previously opened a second `MemoryManager` on the same redb file,
  which collided with the engine's own handle. Both now report
  "Engine not initialized" rather than failing with a redb lock
  error.
- **Session log has new entry kinds.** `Redaction`, `MemoryRetrieval`,
  `ToolOutcome`, and `JevDecision` are all append-only; an older
  kod build reading a newer log skips them with a warning.
- **`[limits]`, `[jev]`, and `[policy.read_protection]` are new
  config blocks.** All three default to safe values; a config that
  omits them behaves exactly as before.

---

## Prior history

All notable changes to KOD will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added

- **P0 — cache accounting, transcript breakpoint, filter hysteresis.**
  `TokenUsage` carries `cache_read_tokens` / `cache_creation_tokens`
  and `ModelPricing` carries the matching rate tiers; the Anthropic
  wire parser folds the two fields into `prompt_tokens` so cost math
  has one number. The messages array carries a second
  `cache_control` marker on its last block so the growing transcript
  is read back at the cache rate. The per-turn Jev tool-category
  filter is gated by `ToolFilterState` hysteresis, and a one-shot
  marker gate suppresses the transcript breakpoint for the single
  request after a filter change.
- **P1 — switch-penalty-aware routing.** `CacheLedger` records the
  last request head each endpoint served and the cache tokens it
  reported. `resolve_chain_for_task_gated` gates a hop to a cold
  endpoint against the projected re-processing cost, with a 1.5x
  margin. The ledger observes from the same call that records cost.
- **P3 — `tool_search` (partial).** A tool that scores a
  natural-language query against each registered tool's name,
  description, and category and returns the top N full schemas; the
  inventory is shared with the engine and refreshed after
  registration. **Not yet wired:** the per-turn Jev tool-category
  filter is still the default path. The intent was for
  `tool_search` to let the model pull schemas on demand so the
  per-turn filter could be retired (see P0 hysteresis for the
  interim mitigation). That replacement has not happened.
- **P4 — conditional AGENTS.md.** `InstructionChain` loads
  `AGENTS.md` / `CLAUDE.md` root-to-leaf, shadows same-named
  sections, and renders the ones whose guards match the turn.
  Fences: `::: when task=…`, `path=…`, `lang=…`. Rendered into the
  volatile prompt slot so a different set does not invalidate the
  prefix cache.
- **P5 — typed subagent briefs.** `ContextBrief` and
  `SubagentReport` are the two records; `assemble_brief` fills the
  first from a `ParentContext`; the swarm runner renders a brief per
  subtask and parses the reply with `parse_report`. `merge_report`
  routes the report's fields to the parent's decisions log, steers
  queue, and conflict check; a `SwarmEvent::BoundaryViolation`
  surfaces writes outside the declared globs.
- **P7 — sensitivity-aware routing.** `Sensitivity` classifies a
  turn by its touched paths; `TrustRequirement` maps that to a
  minimum endpoint tier. `EndpointConfig` gains a `trust` field
  (trusted / standard / untrusted, default standard), and the chain
  filter drops endpoints below the requirement.
- **P8 — relevance-ranked search.** `SearchBackend` with a
  `RipgrepBackend` that shells out to `rg --json` when it is on
  PATH and falls back to the in-process walker otherwise.
  `heatmap_truncate` scores hits by term overlap and keeps the top
  N. `GrepTool` uses ripgrep when available and re-ranks its
  results.


- **P6 — background jobs (partial).** `BackgroundJobRunner` holds
  job state behind a `DashMap` and caps concurrent jobs with a
  semaphore. `READ_ONLY_TOOLS` is the whitelist, enforced at
  `KodEngine::run_tool` when the engine is in background mode.
  `/jobs` lists jobs and `/review` spawns one. **Not yet
  implemented:** `spawn_background_review` registers the job and
  completes it with a placeholder summary; the child engine is not
  constructed, and no actual cross-model review runs. The runner,
  the enforcement, and the TUI surface are real; the review itself
  is a stub. See `docs/design/p2-p5-p6.md` § P6 for what the
  construction needs.

### Changed

- **Hygiene (harness review section 9).**
  `OpenAICompatibleProvider::capabilities()` reports the matrix it
  actually implements (`prompt_cache = Automatic`,
  `streaming_tools = true`) instead of the conservative default.
  Anthropic `list_models` returns a curated model-family list
  rather than an empty vec. `prompt_allocation` reads a cached
  budget hint instead of re-parsing `~/.kod/config.toml` every
  turn. `EndpointHealth` is a three-strikes circuit breaker that
  skips a chronically failing endpoint for a cooldown. Dead
  `record_cost` removed in favour of `record_cost_with_head`.
- **`/help` opens the overlay.** The command routed to the same
  `toggle_help` the `?` key and F1 use; before, it pushed a text
  block that left the overlay unreachable from the command the
  widget advertises.

### Fixed

- **Repomap must not walk a non-repo directory.** `looks_like_a_repo`
  gates the walk on a VCS directory or a project manifest, so
  `kod tui` from `$HOME` no longer walks the whole tree per prompt.
  `MAX_REPO_FILES` caps the walk.
- **Byte-boundary panics in `html_to_text` and
  `parse_unified_diff`.** Found by the fuzz suite; both sliced a
  string on a byte index that could land mid-character.
- **`replay --execute` no longer demands `--yes` for a read-only
  log.**
- **`/help` and the transcript cache breakpoint:** see Changed.

- **Cache marker gate (P0).** The transcript cache breakpoint is now
  suppressed for the single request that follows a tool-filter
  change. `ToolFilterState` carries a one-shot flag, armed on a
  commit that changed the enabled set and consumed by
  `build_grounded_request`; that request skips Anthropic's 1.25x
  cache-write premium for a prefix that is about to churn. All other
  requests carry the marker.
### Added
### Fixed
- **`deny.toml` now carries a real policy** (design §5.3 B4):
  the file existed but its `[licenses]` section carried no
  allow-list and its `[sources]` section was missing entirely, so
  `cargo deny check` passed trivially and enforced nothing. The
  new policy adds an explicit licence allow-list covering the
  licenses in the dependency tree (MIT, Apache-2.0 with and
  without the LLVM-exception, BSD, ISC, MPL-2.0, CC0-1.0,
  Unlicense, Unicode, BSL-1.0, Zlib); a `[sources]` section that
  denies any registry other than crates.io and any git
  dependency; an `[advisories]` section that blocks on
  vulnerabilities and warns on `unmaintained`/`unsound`; and a
  small `[bans]` deny list for `openssl-sys` (the workspace uses
  rustls) and `fastembed` (removed as a phantom dependency). A
  licence outside the allow-list is now a build failure at PR
  time, which is the whole point of the check.

- **Live Anthropic API tests + skippable CI job** (design §4
  D1.5): `crates/kod-provider-anthropic/tests/live.rs` carries
  four `#[ignore]`d tests — a text completion, a streaming text
  completion, a tool-use round trip (which exercises the
  `tool_call_id` plumbing against the real wire), and a
  two-call cache-control sanity check. Every test re-checks
  `ANTHROPIC_API_KEY` and returns cleanly when it is absent, so a
  maintainer who runs `--ignored` locally without the variable
  set sees a skip rather than a failure. The CI job
  `live-anthropic` runs the same command gated on the repository
  secret — a fork PR is skipped, a maintainer push runs — which
  is the design's explicit "skip, do not fail" contract.
- **`kod doctor` reports the `[lsp]` section** (design §4 D5.2):
  two new checks — `lsp.auto_diagnostics` reports the effective
  post-write diagnostics switch and names the config knob that
  disabled it when it is off; `lsp.settle_ms` warns on values
  outside the recommended 200 ms–10 s range. Closes the loop on
  the config section: it is now visible in `kod doctor`, not only
  in `kod config show-merged`.
- **`kod memory add <text>`
- **`/check` and `/handoff` slash-command coverage**: two
- **Swarm panel and `@N` focus state coverage**: `KodApp`'s swarm
- **`kod serve` NDJSON round-trip test**
- **The tool-only reply summary now sees the tool results**: the
- **TUI slash-command coverage: `/policy`, `/map`, `/grep`,
- **A project's `.kod/policy.toml` can now explicitly pin
- **`stream_completion` default and `tokens_per_sec` tests**:
- Engine agentic loop no longer holds the provider read lock across the
- Provider `list_models()` errors are no longer collapsed to an empty list
- Config `load_default()` is forgiving of a corrupt file (warns and falls
- Multiple substring-matcher and word-boundary fixes in `TaskRouter::classify_task`
- **Sandbox status mapping + containment tests** (design §11.3,
- **Structured-prompt and capability-routing integration tests**:
- **`kod config migrate` + `kod policy explain` end-to-end tests**:
- **`/map` + `/grep` TUI handlers, cold-start bench, PR size gate**:
- **`[lsp]` config section** (design §4 D5.2): two new knobs.
- **`LspManager` unit tests**
- **`SwarmRunner::from_config` and `kod acp` test coverage**: the
- **Consolidation tests + deterministic embedder mock**
- **`kod acp` + TUI `/policy`** (design §11.2, §6.2): a new
- **`[tools] preset` is now honoured** (design §6.1): the global
- **Consolidation now fuses near-duplicate entries** (design D2.5,
- **Engine calls the structured provider methods** (design §2
- **AD-15 complete: `Approval`, `MemoryWrite`, `Diagnostics` are
- **CI: dedicated contracts + golden jobs** (design §11.4): the
- **Anthropic wire-level cache stability test**
- **Sandbox badge + streaming rate in the TUI** (design D3.3,
- **`stream_completion` added to the trait contract suite**
- **Engine migrated to `CompletionRequest`** (design §2 AD-01, §4
- **Swarm run-budget knobs are now read from the config**
- **Working default for `LlmProvider::stream_completion`**: the
- **Provider request-shape contract tests** (design D1.5, §11.2):
- **Anthropic `stream_completion`** (design §4 D1.2, A5b): the
- **`memory.embedding_endpoint` is now read** (design D2.1): the
- **`LlmProvider::stream_completion`** (design §2 AD-01, §4 D1.4
- **Markdown render cache** (design D6.5): the chat widget
- **Swarm heartbeat watchdog** (design D4.3): the agent's streaming
- **Capability-pool swarm dispatch** (design D4.3): the swarm
- **`kod policy forget <n>`** (design §6.2): the CLI verb the
- **Anthropic `cache_control` on the last cacheable system segment**
- **`PromptPlan` (design §2 AD-16)**: `TaskRouter::build_prompt_plan`
- **Swarm runner honours the planner's capability assignment**
- **Policy-default + metrics test coverage**: new integration tests
- **Global run timeout for `kod swarm`** (design D4.3):
- **Periodic memory consolidation** (design D2.5):
- **Agent panel shows each swarm agent's model, and `@N` focus**
- **Per-capability swarm model routing** (design D1.4 PR A7):
- **Explicit redb close on shutdown** (design D0.3).
- **Multi-language LSP pool wired into the engine**
- **Multi-language LSP pool** (`kod_lsp::LspManager`): one
- **Golden prefix test** (`crates/kod-core/tests/golden_prefix.rs`):
- **`can_execute_command` no longer pattern-matches dangerous
- **`/debug tokens` reports the prompt allocation table** (AD-14):
- **`auto_lsp` is now independent of `auto_check`**: a successful
- **`/remember <text>`** in the TUI: stores a durable fact with the
- **Multi-endpoint LLM routing**: named `[[llm.endpoints]]` blocks with
- **Anthropic Messages API provider** (`kod-provider-anthropic`), a
- **MCP client** (`kod-mcp`): spawns Model Context Protocol servers,
- **LSP client** (`kod-lsp`): diagnostics, definition, references, and
- **Landlock sandbox backend**: on Linux kernels ≥ 5.13 without
- **Unix-socket daemon** (`kod serve`): NDJSON protocol, peer-UID
- **Policy engine** (`kod-config::policy`): layered presets, per-tool
- **Batch approval dialog**: one modal per round with `y` / `n` /
- **Pin & `/handoff`**: pin turns so they survive history compaction;
- **Research citation verification**: `file:line` citations in
- **Prompt cache budget** (`kod-core/src/budget.rs`): per-section token
- **TUI polish**: markdown rendering in assistant replies, USD cost
- **`kod config migrate`**: brings a v1 config to `config_version = 2`,
- **`kod policy show|explain`** subcommands.
- **Release pipeline**: musl and aarch64-linux builds, `.sha256` per
- **Provider contract suite** (`kod-provider::testkit`): every
- **Prompt characterization snapshots** (`kod-core/tests/characterization_prompts.rs`):
- Placeholder handlers in `TaskRouter::process_input` that returned
- **`kod doctor` reports detected language servers** (one row per
- `LlmConfig` schema is now v2: named endpoints plus a routing table.
- `SwarmKnowledge` and `SharedWorkspace` are gone. The swarm's
- `StreamChunk::ToolCallStart` / `ToolCallDelta` carry an index and an
- `ChatMessage` carries `tool_calls` and `tool_call_id`; the engine's
- `kod doctor` — first-run diagnostics (config, endpoint, skills, memory, git)
- `kod init` — onboarding wizard output
- `kod models` — list provider models with `--filter`
- `kod completions <shell>` — bash/zsh/fish/elvish/powershell completion
- `kod checkpoint list|restore|clear` — file-level undo for `write_file` /
- `kod update` — check GitHub for a newer release (read-only)
- `kod skills --json` and `kod skills-validate` — machine-readable skill
- `web_fetch` tool — HTTP/HTTPS fetch with SSRF protection
- `git_status` and `git_diff` tools — read-only git inspection
- `tools.confirm_writes` — opt-in write approval; TUI dialog and CLI stdin
- `llm.network_access` — opt-in network access for `web_fetch`
- Approval dialog in TUI (`y`/`n`/Esc); CLI chat prompts on stdin
- `--sandbox` flag on `kod chat` — run shell commands under bwrap/sandbox-exec
- Unified-diff attachment on `write_file` / `patch_file` results
- `Engine::process()` without a provider now returns an `InvalidState` error
- `LlmConfig::validate()` clamps out-of-range values (temperature, context

### Fixed
- **Episodic facts are now attributed to a session** (design D2.5):
- **Collected loop also drains steers at the top of the round**:
- **Routing table keys are now validated** (design §2 AD-06, §12):
- **Tool rounds and goal turns accumulate on the structured path**:
- **`/steer` now reaches the model on the structured path**: the
- **`KodConfig::default()` now stamps `config_version = 2`** —
- **Session log + `kod replay` compatibility tests** (design §11.3,
- **Streaming, session-log, and HTML-export tests**: three
- **End-of-session extraction writes Episodic entries** (design
- **Cacheable/volatile split preserved through to the wire**
- **No double-send of the user turn on the structured path**: the
- **`@N` focus + session deny-rule tests**: unit tests for
- **Structured tool rounds in the transcript** (design §2 AD-02,
- **Default policy preset is now `standard`** (writes require
- **Sandbox defaults to `Auto`**: `bwrap` / `sandbox-exec` /
- `kod sessions` — inspect/export/clear the TUI session file
- Session log (`KOD_SESSION_LOG`) and `kod replay` — every tool call recorded

### Removed
- **Placebo config fields and dead CLI flags** (design §5.3
- **`/export-html` + phantom-dep removal** (design H1, D6.5): the

## [0.1.0] - 2026-09-04

### Added
- Project structure and workspace setup
- Core type definitions (ids, messages, skills, memory, tools)
- Error handling system
- Configuration management
- LLM provider abstraction
- Ollama provider implementation
- Skills system:
  - Markdown parsing with YAML front matter
  - Skill loader with directory scanning
  - Skill matcher with pattern-based matching
  - Hot reloading with file system watching
- Memory system:
  - Short-term memory (in-memory with capacity limits)
  - Long-term memory (redb persistent storage)
  - Episodic memory (vector-based for semantic search)
  - Memory manager with unified interface
  - Context builder for LLM prompts
- Tool system:
  - Tool registry and trait definitions
  - File system tools (read, write, list)
  - Git tools (status, diff)
  - Tool executor with timeout handling
  - Permission-based sandboxing
- Agent swarm:
  - Agent lifecycle management
  - Direct messaging between agents
  - Shared workspace with file locking
  - Task coordination and decomposition
  - Agent swarm manager
- TUI:
  - Event handling system
  - Application state management
  - Chat widget with message display
  - Agent panel
  - Input handling with history
  - Streaming response display
- CLI:
  - Command structure (chat, skills, config)
  - Command handlers
  - Main entry point
  - Configuration integration
  - Verbose mode with build info
- Integration tests
- Performance benchmarks
- CI/CD pipelines
- Documentation (README, CHANGELOG, CONTRIBUTING, ARCHITECTURE)
