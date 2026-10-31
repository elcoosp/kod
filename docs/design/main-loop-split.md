# main_loop.rs split — design proposal

**Status:** proposal, no code changes yet.
**Blocker for:** item 5 of the session handoff's five-item list.

## Problem

`KodEngine::handle_command` at `crates/kod-tui/src/main_loop.rs:2248`
is 3256 lines across 67 match arms. The function serves every slash
command, reads ~20 `self.*` fields, and hands work off to a dozen
different subsystems (engine, swarm_pool, background runner,
provider registry, memory manager, checkpoint manager, policy).

Extracting one arm leaves a 3256-line function. This is a dispatch
shape problem, not a "move these lines" problem.

## Candidates

### (a) Per-group methods

Move related arms to `handle_<group>_commands(&mut self, parts: &mut
SplitWhitespace, ...) -> Result<()>`. `handle_command`'s match arm
becomes `"/cache" | "/limits" | "/context" | "/theme" | "/tools" |
"/whoami" => self.handle_observation_commands(parts).await?,`.

**Proof-of-pattern group:** the six read-only arms:

| Arm | Reads | Mutates |
|---|---|---|
| `/cache` | app, engine.cache_snapshot | nothing |
| `/limits` | app, engine.tool_quotas | nothing |
| `/context` | app | nothing |
| `/theme` | app, keybindings | theme name only |
| `/tools` | engine.tools | nothing |
| `/whoami` | app, engine | nothing |

Every one reads state and pushes a `system_message`. No awaits on
`spawn_background_*`. No `&mut self.app` held across an await point.
The method signature is `async fn handle_observation_commands(&mut
self, parts: &mut SplitWhitespace) -> Result<()>` — one `&mut self`
borrow, dropped at the end of the method.

**Second group:** memory arms (`/memory`, `/remember`, `/forget`,
`/search`). Also read-only, but reach `engine.memory()` which is
async. Test after group one proves the pattern holds across awaits.

**Third group:** mutation arms (`/clear`, `/undo`, `/compact`,
`/retry`, `/regenerate`). These mutate `self.app.messages` and
`self.engine.history`. More borrow-checker risk; test after the
first two groups.

**Deferred:** `/review`, `/swarm`, `/handoff`, `/overnight` — these
spawn background jobs and hold `self.background` across awaits. Do
them last, one at a time.

### (b) Command registry

`HashMap<&str, fn(&mut TuiLoop, SplitWhitespace) -> BoxFuture<Result<()>>>`.

Every arm becomes a top-level `async fn`. Signature change at every
call site; tests that call `handle_command` still work, but tests
that exercise an individual arm need updating. Blast radius: 67
arms + every arm's test.

### (c) Handler trait

`trait SlashHandler { async fn handle(&self, parts, tui) -> Result<()> }` with
one impl per command, dispatched via a `Vec<Box<dyn SlashHandler>>`.

Cleanest long-term. Largest blast radius. The `&self` vs `&mut self`
question is nontrivial — `&mut tui` as the second argument avoids
storing state in the handler, but every handler then needs to import
the whole `TuiLoop` surface.

## Recommendation

(a) first, one group at a time:

1. `handle_observation_commands` — 6 read-only arms, ~200 lines out.
   Gate. Commit.
2. `handle_memory_commands` — 4 arms, ~150 lines out. Gate. Commit.
3. `handle_mutation_commands` — 5 arms. Gate. Commit.
4. Individual async-job arms, one per commit, `/review` last.

After group 3, `handle_command` is ~2700 lines — visibly better but
still large. Groups 4a–4d each shave 50–200 more. The end state is a
`handle_command` that dispatches to 20 methods, each under 200 lines.

## Invariants

- After each group, `cargo nextest run -p kod-tui` passes with the
  same count as before the move.
- `handle_command`'s public signature is unchanged.
- Behavior identical — same messages, same jobs spawned, same errors
  returned. `handle_command` still returns `Result<()>`; the group
  methods return `Result<()>` and `?` them through.
- No new `pub` visibility — the group methods are `async fn` (private
  to the `impl TuiLoop` block).

## What makes this safe to gate per group

The existing tests call `handle_command("/arm")` and assert on
`tui.app.messages().last()`. Moving an arm to a method that pushes
the same message is invisible to the test. If a group's method
returns the wrong message or a different error, the test catches it.

The six read-only arms have 4 existing tests (`test_slash_skills_*`,
`test_slash_help_*`, `commands::*` on `tui_e2e`). Every one exercises
`handle_command` end-to-end. The pattern is verified before any
mutation arm is touched.

## Why not now

The `&mut self` borrow checker rules on this file are not obvious
from reading. Each group's first commit is the empirical test of
whether the borrow shapes align. The handoff's audit method applies:
candidates from a grep are hypotheses; the compiler is the
confirmation.

One group, one commit, one gate cycle. Do not batch.

## Shared-helper extraction attempt

Attempted to extract three helpers from `tools.rs` into a new
`crates/kod-tools/src/shared.rs`:

  - `next_temp_seq`
  - `atomic_write`
  - `kill_child_tree`

The extraction script found all three functions correctly and wrote
`shared.rs` with the function bodies. The module declaration in
`lib.rs` and the `use crate::shared::{...}` in `tools.rs` both
landed. Compilation failed with `E0603: module is private` — the
exact dependency the compiler names is in the next session's
transcript; the pattern (single source file, byte-offset
extraction, compile check, revert on failure) has been tried on
`FileInfoTool` (succeeded) and on the `RouterConfig` fixtures
(failed three times with a different cause each time).

The next session's opener for the tool split is:
1. Reproduce the extraction with `cargo check` captured to a log
   with `grep -B2 -A8 '^error'` (not `tail -30`).
2. Read the specific E0603 error — it names the private module
   that `shared.rs` depends on.
3. If the dependency is `walk_cache`, check whether its API needs
   `pub(crate)` exposure for the new module, or whether the
   helper should stay in `tools.rs` where it has visibility.
4. Re-run the extraction with the visibility fix.

The doc this session shipped (the three main-loop-split candidates
above) stands regardless. The shared-helper extraction is a
five-minute fix once the E0603 names its module.
