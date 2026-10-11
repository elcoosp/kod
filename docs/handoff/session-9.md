# Handoff — `kod` codebase, session 9 (continuing)

## TL;DR

Continuation of the bug-hunt arc that began at `727d055`. HEAD is now
`e3972b9` with roughly 80 commits since the handoff start, ~70 of them
this session's bug fixes, lints, and regression tests. Working tree is
clean, Clippy is clean, `cargo fmt --check` is clean, and the last full
`cargo nextest --workspace` run reported **4130 passing, 0 skipped**.

Two dead-wiring arcs were closed this session (turn-trace machinery,
swarm blackboard claims). One class-level source lint was added
(`no_bare_wall_clock_filenames_in_production`). A flaky TUI timing test
was defla ked. Three new arcs remain *unstarted* — they are enumerated
under "Where the next agent should pick up".

## Current repo state

| | |
|---|---|
| Repo | `/Users/adm/Documents/Repos/kod` |
| Branch | `main` |
| HEAD | `e3972b9` |
| Working tree | clean |
| Clippy | 0 warnings |
| `cargo fmt --check` | clean |
| Tests | **4130 passing**, 0 skipped (last full run) |
| Miri | installed (`miri 0.1.0 2026-08-28`), clean on the crates it runs |
| Disk | **11 GiB free** (was 6.7 GiB before clearing `target/*/incremental`) — monitor before builds |

## Environment quirks (carried forward from the previous handoff)

1. **`cargo clean` under disk pressure corrupts `target/`.** If a build
   fails with "unclosed delimiter" or "No such file", `rm -rf target`
   and rebuild. Do **not** run `cargo clean`.
2. **`kache` is the rustc-wrapper** — 19 GB cache at
   `~/Library/Caches/kache`. Never delete.
3. **`watchexec` runs on `./wr.sh`.** Many watchers may fire. Do not
   kill them.
4. **`.git/index.lock` goes stale** under concurrent git ops. Remove
   after checking age (>60s).
5. **`timeout(1)` is not on this macOS.** Use
   `perl -e 'alarm shift; exec @ARGV' <secs>`.
6. **User runs scripts by writing a single fenced ` ```bash ` block.**
   They strip the fence and save to `wr.sh`.
7. **`cargo nextest list -p <crate> --lib`** to get full test paths when
   a filter returns "no tests". Nextest does not accept `a|b` filters.

## What this session did

### Bug classes fixed (each with a reproducer-first)

| Class | Representative commits |
|---|---|
| Wall-clock filename collisions (4 sites + 1 lint) | `167e609`, `3374db7`, `424860a`, `d35b47d` |
| Path-traversal in user-supplied names | `49b3fe3`, `cc0259b` |
| Dead-wired delta features (three arcs) | `ba2ba7c`, `7827642`, `2e7891f`, `e3972b9` |
| Redaction coverage gaps | `372ef05` |
| Cancellation cleanup (background results) | `09f102b` |
| Turn-trace machinery (opened, closed, outcome) | `7827642`, `2e7891f` |
| Swarm blackboard claims + done markers | `e3972b9` |
| TUI flaky timing test | `3374db7` |

### Source lints added

- `no_bare_wall_clock_filenames_in_production` — flags any format
  literal that ends in a filename extension, contains a clock-shaped
  placeholder, and contains none of the accepted discriminators
  (`pid`, counter, sub-second nanos). Escape hatch: a file that opens
  with `create_new(true)` and handles `AlreadyExists` is exempt
  (`plan::autosave_plan` uses this shape correctly).
- Extended `no_fixed_temp_filenames_in_production` to also flag `.tmp`
  names with fewer than 2 placeholders.
- Added `no_seconds_only_backup_filenames_in_production`.

### Test infrastructure added

- `crates/kod-core/tests/it/swarm_runner.rs` — several class-level
  regression tests for the swarm cancellation arc, the concurrent-run
  guard, the trace recording, and the blackboard publishing.
- `crates/kod-types/tests/it/source_lints.rs` — extended with the
  wall-clock lint.
- `crates/kod-core-state/src/session_log.rs` — the
  `a_secret_in_approval_edit_is_redacted_before_it_reaches_disk`
  regression.
- `crates/kod-core/src/fixture.rs` — the
  `default_path_rejects_a_name_that_escapes_the_fixtures_dir` regression.
- `crates/kod-core-quality/src/worktree.rs` — the
  `two_session_worktree_tokens_in_the_same_second_are_distinct`
  regression.

## Commit log summary (most recent 20)

```
e3972b9 fix(swarm): publish write claims and completions to the blackboard
06cbbf1 chore(test): use is_empty over len() > 0 in the trace regression test
2e7891f fix(engine): record mid-stream endpoint switch in the turn trace
7827642 fix(engine): open and close turn-trace rounds; set terminal outcome
372ef05 fix(state): redact Approval.edit before it reaches the session log
d35b47d test(types): lint bare wall-clock filenames in production
cc0259b fix(cli): validate new_name before probing the FS in skills rename
49b3fe3 fix(fixture): reject path-traversal names in default_path
424860a fix(quality): unique session-worktree token per invocation
3374db7 fix(tui): unique handoff and export filenames; deflake ttft test
167e609 fix(state): unique default session-log paths (pid + counter)
09f102b fix(engine): clear_history drops queued background-job results
ba2ba7c fix(tools): wire web_retry into web_fetch (dead delta 14.5 module)
2c31f86 fix(cli): unique backup name in init --force; lint bak- class
a5a0db7 fix(cli): unique backup and temp names in config migrate/init-from
91ed06d fix(engine): approval wait checks the raw transcript key for cancel
6721691 fix(engine): approval and question awaits observe cancel
cfc33f7 test(engine): pin shutdown drains pending approvals/questions
b1e0c5f fix(engine): drain pending approvals/questions in shutdown
e21dcbc fix(swarm): run guard forgets per-transcript state on cancellation
```

## Where the next agent should pick up

The user asked for three specific new arcs. **None of them were
started** — the session ended while the third grep was still running.
The next agent should begin with **arc 3**, then **arc 1**, then **arc 2**.

### Arc 3 (recommended first) — Concurrency stress with `MockProvider` through `SwarmRunner::run`

**Objective.** Spawn N agents hammering the swarm bus + `AsyncDelivery`
+ the engine's per-transcript maps with a scripted provider under
`--release` for 30–60s. This exercises the *runner's* use of the
messaging primitives under real scheduling load. The existing
`crates/kod-swarm/tests/it/stress.rs` covers the primitives directly,
and miri runs single-threaded, so this is a genuinely new class.

**Where to start.**

- `MockProvider` is at `crates/kod-provider/src/testkit.rs` (feature
  `testkit`, on by default; `kod-core` already depends on `kod-provider`
  in dev-deps).
- Script variants: `Script::Text`, `Script::Calls`, `Script::Mixed`,
  `Script::Error`.
- `crates/kod-core/src/swarm_runner.rs` — the scheduler that drives the
  messaging primitives (`broadcast_lifecycle` at lines 1330, 1574, 1773,
  1894; `steer_interrupt_for` at 902, 1813, 1848; the per-agent pump
  around 1346–1420; the watchdog around 925–1000).
- The four regression tests added this session (`swarm_runner.rs` at
  the tail of `crates/kod-core/tests/it/swarm_runner.rs`) are the
  scaffolding to extend — they already install a scripted provider,
  build an engine, and drive `runner.run(...)`.

**Test shapes worth writing.**
1. N=8 agents, all `Script::Calls` with tool calls that touch
   overlapping paths, `--release`, 30 s. Assert: every agent completes,
   no panics, `transcript_scoped_key_count_for_tests() == 0` at the end.
2. Interleave `Script::Error` and `Script::Text` so retries fire; assert
   the retry-budget accounting holds.
3. Drop the run future partway through; assert the `SwarmRunGuard`
   cleans up (bus, hub, transcript maps).

### Arc 1 — Goal budgets

**The gap.** `Goal` has `token_budget` and `time_budget_seconds` fields.
`GoalRuntime::observe_usage`/`observe_time` account against them.
`take_budget_steer` returns a one-shot flag when the budget is spent.
`process_goal_streaming` (at `crates/kod-core/src/engine/process.rs`,
around line 1560) reads all of this and emits a steer. **But
`GoalRuntime::start` creates `Goal::new(...)` with both budgets `None`,
and the TUI's `/goal <text>` command has no budget syntax.** So the
steer can never fire.

The engine exposes `set_goal_token_budget` and (implicitly, via
`goal_runtime.current_mut()`) the time budget — but nothing calls them.

**Design decision required.** What is the user-facing surface?

Option A: extend `/goal` to accept `--budget N` and `--time N` flags:

```
/goal --budget 50000 fix the failing tests
/goal --time 30m refactor the parser
```

Option B: a separate `/goal budget <N>` sub-command that attaches to the
active goal.

Option C: a config field `[goal] default_token_budget = N` that applies
to every goal.

Pick one before writing code. The engine plumbing is a two-line change
either way: call `set_goal_token_budget` (and add a sibling for time)
after `app.set_goal(...)`.

**Files to touch.**

- `crates/kod-tui/src/main_loop.rs` — the `/goal` arm (around line 2235).
- `crates/kod-tui/src/app/mod.rs` — `KodApp::set_goal`/`goal`/`clear_goal`
  (around line 1060).
- `crates/kod-core/src/engine/state.rs` — `set_goal_token_budget` is at
  line 482; add `set_goal_time_budget` alongside.
- The `process_goal_streaming` loop already reads the runtime; nothing
  else needs changing there.
- A regression test in `crates/kod-core/tests/it/` that drives a
  goal + budget through `process_goal_streaming` and asserts the
  budget steer fires. `crates/kod-core-state/src/goals.rs` has unit
  tests for the runtime; the integration test is the missing half.

### Arc 2 — Cold revive

**The gap.** `KodEngine::verify_cold_revive_surface(log_path, holder)`
is a fully-implemented, `pub` method. It reads the persisted
`SessionInit` entry, rebuilds the current surface fingerprint, and
returns a `ColdReviveVerdict` (`NoInitEntry`, `SurfaceMatches`,
`SurfaceMismatch { missing, added }` — variants at
`crates/kod-core/src/engine/types.rs`).

The underlying reader, `KodEngine::session_init_from_log` (at
`crates/kod-core/src/engine/state.rs` line 225), is called from inside
`verify_cold_revive_surface` (line 335). **But nothing calls
`verify_cold_revive_surface` either.** So the entire revive-safety
check is unreachable.

**Design decision required.** *When* is a cold revive attempted?

Option A: `kod serve` startup rehydrates each session's subagents if
the surface still matches.

Option B: a `kod sessions revive <id>` CLI subcommand that refuses when
the surface drifted.

Option C: the TUI's session-restore path (see
`crates/kod-tui/src/app/persistence.rs`) calls it before restoring a
transcript.

Option A is the most natural given the delta §11.2 context ("a cold
revive reads this to rebuild a subagent's tool surface after a
restart").

**Files to touch.**

- `crates/kod-core/src/engine/lifecycle.rs` — a revive orchestrator that
  calls `verify_cold_revive_surface` and then replays the session log
  through `kod_core_state::session_log::rehydrate_turns`.
- The caller: `kod serve`'s startup, or `kod sessions`.
- A regression: build an engine, save a session log with a fingerprint,
  restart with a different tool set, assert `SurfaceMismatch`; restart
  with the same set, assert `SurfaceMatches`.

## Gotchas the next agent will hit

1. **Deduplication of commits.** Several times this session a `git
   commit` landed with failing tests because a script continued past a
   test failure. **Always verify with `cargo nextest run --workspace`
   between changes, before commit.**
2. **`tests/it` integration tests are a separate crate.**
   `#[cfg(test)]`-gated helpers do not reach them. Use `for_tests`-
   suffixed accessors compiled unconditionally (see
   `crates/kod-core/src/engine/transcript.rs` for the pattern — there
   are six of them there now).
3. **Proptest result type.** A `proptest!` block cannot mix
   `return Ok(())` with unit-returning tails. Make all branches
   consistent.
4. **Wrapper-parser tables are duplicated** between `kod-risk` and
   `kod-config` (no shared dependency). Any change to one must mirror
   the other; the class-level regression suite does not yet cover the
   `kod-config` side.
5. **`cargo fmt --check` before commit** — two commits landed dirty
   earlier and needed a `--force-with-lease` amend.
6. **The `Plan`/`TraceWriter`/`SessionRecorder` files** all use a
   `create_new(true)` + retry-numeric-suffix pattern (`autosave_plan`)
   or a pid+counter+nanos temp-and-rename. If you add a new file
   writer, mirror one of these; the lint will fire otherwise.
7. **`SessionEntry` variants are redacted selectively.** If you add a
   new variant with free-form text or a JSON payload, add a match arm
   to `SessionRecorder::record`'s redaction block in
   `crates/kod-core-state/src/session_log.rs` (around line 330). The
   `Approval.edit` leak fixed this session (`372ef05`) is the class.

## Files changed this session (vs `727d055`)

Roughly 59 files, +4500 / -300. The high-signal additions are the
`for_tests` accessors on `KodEngine`, the `SwarmRunGuard` in
`crates/kod-core/src/engine/swarm.rs`, the `record_tool_round` method
on `TurnTraceBuilder`, the `swarm_run_guard` acquisition at the top of
`SwarmRunner::run`, the `next_temp_seq`/`next_session_seq`/
`next_worktree_seq`/`next_handoff_seq` helpers distributed across four
crates, and the extended `source_lints.rs`.

## Message conventions for this user

- Reply as a single fenced ` ```bash ` block when emitting a script.
- No prose before/after the fence when emitting a script.
- When reporting, keep it tight. If nothing was found, say so — do not
  invent fixes.
- The user does not want excuses.

## What NOT to do

- Do not `pkill watchexec`.
- Do not delete `~/Library/Caches/kache`.
- Do not run `cargo clean` — `rm -rf target` is the safe shape.
- Do not use `head`/`tail` to split file content across scripts.
- Do not use `#` comment lines inside bash scripts.
- Do not add speculative fixes without a reproducer.

---

**Where to start:** Arc 3 (concurrency stress). It is concrete, needs
no design decision, and exercises a class the existing tests do not
reach.
