# Handoff — kod, session 17

**Continues:** session 16 (`docs/handoff/session-16.md`).
**Last stable release:** `v0.1.11` (`98bbd1c`) — no release cut this session.
**Last verified commit:** `9b66120 test(core): two concurrent clients on the serve unix socket`.
**Working tree state at handoff:** clean.

Session 17 was a short continuation of the behavioral-integration arc.
It closed the highest-value gap the session-16 handoff named — ACP
`session/cancel` — and did nothing else substantive. The discipline
from session 16's L16-L19 notes ("read the tests before naming a gap")
was applied at the start and paid off: the engine side of cancel was
already well covered, so the test built here is narrow and exact.

## What landed

| Commit | What | Gate |
|---|---|---|
| `a88e582` | ACP `session/cancel` dispatch + stop-reason mapping (2 tests) | kod-core-serve 72/72 |
| `fe039cd` | drop dead `write_call` helper from the new file (cleanup) | n/a |
| `72e9408` | `session-16.md`: mark cancel gap CLOSED, advance last-verified | n/a |

### The two cancel tests

`crates/kod-core-serve/tests/it/acp_cancel.rs`:

1. **`session_cancel_reaches_the_engine_and_maps_to_cancelled_stop_reason`**
   Pre-cancel shape (cancel notification, then prompt) so the engine's
   round-entry flag check fires deterministically. Drives the whole
   bridge path: `session/cancel` -> `request_cancel_for` ->
   `process_streaming_for` sees the flag -> `Err(...cancelled...)` ->
   `{stopReason: "cancelled"}`.

2. **`non_cancel_provider_error_maps_to_refusal_stop_reason`**
   The other side of the same match arm at `acp.rs:571`. A provider
   error whose message does not contain "cancelled" must map to
   `{stopReason: "refusal"}`, not `cancelled`, not `end_turn`, not a
   JSON-RPC error. Without this, a regression that widened the
   cancelled branch would silently mislabel every provider failure as
   a user cancel.

## The read-then-verify discipline, in action

Before writing the test, one exploration ran `grep` over the existing
engine tests. Result: the engine side of cancel was **already
covered**:

- `tool_loop::test_cancel_stops_process` — pre-cancel at the engine
  (flag set, then `process()` returns `Err(...cancelled...)`).
- `policy_gate::cancel_while_pending_approval_does_not_wait_the_timeout`
  — mid-approval cancel, and the approval wait observes the cancel
  flag instead of hanging to the 120s timeout.

What neither covers is the **ACP bridge**: the dispatch of the
`session/cancel` notification (`acp.rs:303-309`) and the stop-reason
mapping (`acp.rs:571`). That is what `acp_cancel.rs` tests. Without
the pre-read, the temptation would have been to re-test the engine —
duplicating `tool_loop` and `policy_gate` with extra setup.

This is exactly the L16-L19 lesson paying forward. Note it in future
handoffs: the cost of a `grep` is one round; the cost of building a
redundant test is several.

## Deliberately not tested

The **true mid-turn ACP cancel** (cancel arriving while the approval
gate is held, not before the prompt starts). That shape needs a
provider that blocks. The engine-level half of it is covered by
`policy_gate.rs`. The bridge half — dispatch + mapping — is identical
for pre- and mid-turn cancels at the ACP layer (`run_prompt` maps the
same error to the same stop reason either way). If a future change to
`run_prompt` introduces a mid-turn-only bridge bug, that is the next
test to add. It is not worth writing now against code that does not
currently distinguish the two cases.

## Remaining gaps (after this session)

1. ~~ACP `session/cancel`~~ — **CLOSED** (`a88e582`).
2. **ACP corrupt-batch deny-all** (`acp.rs:606-616`). Narrow; needs a
   fake engine or a fault-injection hook that does not exist. Lower
   value than (1) — the happy-path approve/deny is covered.
3. **LSP live tests** — `#[ignore]`d by design (require
   rust-analyzer). CI-policy decision, not missing coverage.

Everything else session 16 listed as "verified covered" is still
covered. Nothing new appeared.

## What session 18 should consider (options, not tasks)

The behavioral-integration arc has produced six commits of seam tests
across the provider adapter, ACP session/approval/cancel, and MCP
tool-dispatch boundaries. Every gap that needed code has been closed.
**The next class of work is not more tests.** Three options, each
with a different shape:

### Option A — operational integration (the real remaining gap)

Run a real editor session against:
- a real MCP filesystem server (spawn `npx
  @modelcontextprotocol/server-filesystem` and drive a turn through
  it);
- a real language server (`rust-analyzer` against a fixture);
- a long-running daemon (the `kod serve` unix socket, multiple
  clients, shutdown semantics).

This is integration testing against live dependencies, not fakes.
Every prior test used a scripted or mock peer; this exercises the
real thing. **Decision needed before code:** CI cost. `npx` spawns a
Node process; `rust-analyzer` is a 50 MB binary; the current CI does
not have these on `$PATH`. Options: (a) nightly job only; (b)
feature-gated `#[ignore]`s; (c) require a manual
`cargo nextest run --workspace --run-ignored` in release prep.
Choose before writing anything.

### Option B — cut a release

`v0.1.11` is the last tag. Six behavioral test commits and three
docs/chore commits have landed since. They are test-only, not
behavior changes — a release would be a "no-op" bump for `cargo`
semantics. If you want the tag, `v0.1.11` -> `v0.1.11-test-arc` (or
`v0.1.12`) is one commit. Not recommended unless a downstream
consumer keys on tags.

### Option C — accept the arc is done

The static audit, bug hunt, property tests, concurrency invariants,
source lints, and behavioral seams are all in place. Operations is
the only untested class. If operations integration needs a decision
that will not be made now, **stop here**. The codebase is in a
clean, well-tested, documented state. A future session can pick up
Option A with the decision already in hand.

## Session 17 in one line

Closed the highest-value remaining behavioral gap (ACP
`session/cancel`) by first reading the engine tests to confirm what
was already covered, then building two tests — the bridge dispatch
and both arms of the stop-reason mapping — that together pin the
whole path.

## L20 — never overwrite `wr1.sh` in place; write `tmp` + `mv`

Session 17 ended with a hung terminal. Diagnosis: `wr1.sh` was
overwritten non-atomically while a `watchexec` instance was
mid-execution of the previous script. Bash reads scripts
incrementally; replacing the file mid-read makes bash execute a mix
of old and new content, which can block on a half-read heredoc
forever.

The hang is indistinguishable from a slow test run until you check
`ps` — the screen shows test-binary output (`acp: loaded 76 skill
file(s)`) that came from the *previous* script still executing.

Discipline: whenever writing `wr1.sh` (or any watched script),
always:

    cat > wr1.sh.tmp << 'SCRIPT_EOF'
    ...content...
    SCRIPT_EOF
    chmod +x wr1.sh.tmp && mv wr1.sh.tmp wr1.sh

A `mv` is atomic on the same filesystem; the watcher sees one
complete file and executes it once. A direct `cat > wr1.sh` is not
atomic and races any mid-execution reader.

Recovery if it hangs: `Ctrl-C` the terminal, then
`pkill -TERM -f 'cargo nextest|cargo check|cargo build'`, `sleep 2`,
`pkill -KILL` anything left. Nothing is lost — the hang is a read
failure, not a write corruption; `git status` will show clean.

## Session 18 progress — operational integration, A1 (MCP)

**Commits this session so far:** `7e29e58`, `cf5a529`.

### Recovery first

A hang at the start of session 18 was diagnosed as L20 (overwriting
`wr1.sh` non-atomically while `watchexec` was mid-read). Recovery is
recorded in L20 itself; nothing was lost. The `wr1.sh` writer now
follows `cat > wr1.sh.tmp && mv`.

### Option A1 — real MCP filesystem server (`cf5a529`)

The first operational-integration test in the codebase. `kod-mcp` had
no `tests/` directory; the protocol tests in `src/client.rs` drive a
hand-written Python fake. This file drives the reference
`@modelcontextprotocol/server-filesystem` via `npx`, exactly as
`kod-provider-anthropic/tests/it/live.rs` and
`kod-tools/tests/it/sandbox_containment.rs` gate their live tests:
`#[ignore]`'d and guarded by an availability check.

Two tests, verified green on this host (npx present, server
downloaded in 3.5 s):

1. `real_filesystem_server_initialize_and_list_tools` — real
   `initialize` (name and version non-empty), real `tools/list`
   contains `read_file` and `list_directory`, every tool carries
   `inputSchema`.
2. `real_filesystem_server_reads_a_written_file` — real `tools/call`
   on both tools against a file the test wrote.

**Zero CI cost.** `npx` is not installed on any CI host; the tests are
`#[ignore]`'d, so `cargo nextest run --workspace` skips them. To run
manually:

    cargo nextest run -p kod-mcp --run-ignored only -E 'test(real_filesystem)'

(Syntax note: `--run-ignored` takes an argument `default|only|all`;
bare `--run-ignored` is a nextest usage error. Cost 1 round.)

### What A1 does NOT do

- No CI wiring. There is still no `.github/`; that is the A2 option
  the original handoff described, and it is a from-scratch workflow,
  not an edit.
- No test against a real language server. `kod-lsp`'s live-test
  surface was partially explored — `client.rs` has `binary_on_path`
  but the specific `#[ignore]`d test body was not read. That is the
  next A1 candidate if it turns out to exist in a form worth
  following.
- No multi-client daemon test. `serve_roundtrip.rs` covers single
  client + shutdown; multiple concurrent clients on the unix socket
  is untested.

### What to do next

The original three options are unchanged in shape; only A1 for MCP is
newly done. Options ranked by cost:

1. **Read `kod-lsp`'s live test** (`grep -B3 '#\[ignore'` or `grep
   binary_on_path` in `crates/kod-lsp/src/client.rs`) and decide
   whether to follow it with a real `rust-analyzer` test in the same
   `#[ignore]` + availability-gated shape as `cf5a529`. Cost:
   exploration + ~150 lines, no CI change.
2. **`kod serve` multi-client test** — two `UnixStream` clients on
   one socket, one shutting down while the other is idle. Cost:
   ~80 lines, no new deps. `serve_roundtrip.rs` is the model.
3. **Stop here.** The arc has delivered: P2 fix + fixups + source
   lint + bug report (`v0.1.11`), five ACP/MCP seam tests, ACP cancel,
   and a real MCP server integration. Everything that needed code
   has code. Everything else is CI-policy.

## Correction (session 18) — LSP live is covered; the earlier claim was wrong

The "Session 18 progress" section above says, under "What A1 does
NOT do", that the LSP live-test surface was "partially explored" and
that reading `kod-lsp`'s live test was "the next A1 candidate." That
is **factually wrong**, and reading the file settled it:

`crates/kod-lsp/src/client.rs` has a full live integration test,
`test_rust_analyzer_did_change_clears_error`, that:

- spawns a real `rust-analyzer` via `LspClient::start`;
- writes a fixture crate with a type error (`didOpen`);
- asserts the error is reported;
- rewrites the file with the fix (`didChange`);
- asserts the error clears — which specifically catches the
  "second `didOpen` returns stale diagnostics" bug.

It is **not** `#[ignore]`'d. It uses the runtime-skip pattern:
`binary_on_path("rust-analyzer")` returns early with an `eprintln!`
if absent, and a **handshake probe** (`client.initialize()`) then
returns early if the binary is a rustup shim with no toolchain. That
is strictly better than `#[ignore]`: the test runs wherever a real
`rust-analyzer` exists, and skips gracefully everywhere else, with no
`--run-ignored` flag needed.

This is the same over-claim pattern the session-16 handoff recorded:
a gap was named from memory, and reading the code showed it covered.
The lesson stands — **read the test file before writing a gap into a
handoff, not after.** The LSP entry in "What to do next" (item 1) is
void; there is no LSP gap to close.

What is still genuinely untested: **two concurrent clients on the
`kod serve` unix socket.** `serve_roundtrip.rs` covers one client
(`shutdown`, `unknown method`, second-daemon-refuses) but never two
at once. The accept loop spawns per-connection, so the contract
"each client's request is answered independently" is the thing to
pin. That is the one remaining gap, and it is being tested next.

## Session 18 — arc complete

**Commits this session:** `7e29e58`, `cf5a529`, `e2713fd`, `c4b30ea`, `9b66120`.

The behavioral-integration arc is now complete. Every gap that was
named across sessions 16-18 has been closed, or shown to be
non-existent:

- **Session 16** — provider adapter seam, ACP `session/prompt`,
  MCP tool dispatch, ACP `session/request_permission`.
- **Session 17** — ACP `session/cancel` (dispatch + stop-reason
  mapping).
- **Session 18** — real `@modelcontextprotocol/server-filesystem`
  integration (A1, zero CI cost); **two concurrent clients on the
  `kod serve` unix socket** (`9b66120`); and a correction showing
  LSP live was already covered (the earlier claim was wrong).
- **Session 18 recovery** — L20 (`7e29e58`): `wr1.sh` is written
  atomically (tmp+mv), never in place. The session opened with a
  hung terminal caused by exactly this.

### Multi-client test (the last gap)

`serve_roundtrip.rs::two_clients_are_served_independently_then_shutdown_stops_the_daemon`
(`9b66120`) connects two clients, has each send a distinct
unknown-method request, and asserts each receives its own
`{type:"error"}` response keyed to its own id — the contract a
serialised-handler regression would break. Then one client shuts
the daemon down, the serve task returns, the socket file is removed.

The wire shape was learned from two failing rounds, not assumed:
`id` is a JSON **string** (not integer), and errors are
**`{type, data}`-discriminated** (not `{error: ...}`). Both are
pinned as strings in the final test. Recorded so the next session
does not re-derive.

### What is genuinely left

Nothing in the testable surface. Three categories remain, each a
decision rather than a code task:

1. **CI wiring** (A2). The `#[ignore]`d MCP test and the LSP live
   test both exist and run on a developer machine with the right
   deps installed. CI could run them: there is no `.github/`
   directory to modify — a workflow would be built from scratch.
   Cost: one workflow, dependency install, ~5 min/night. Not code.
2. **Operational assertions on a real editor session** — a real
   editor speaking ACP against the binary, a real daemon on a
   stable port. The pieces exist (`acp_handshake.rs` spawns the
   binary; `serve_io` drives the bridge), what is missing is the
   *editor* on the other end. Same decision as (1): CI cost.
3. **Release tag** (option B from the earlier handoff). Not
   recommended unless a consumer keys on tags — the post-release
   commits are test-only.

### The correct next move is a decision, not a commit

Sessions 16-18 built the behavioral-testing foundation. Session 19
should not write more tests until the CI-cost question is answered.
If the answer is "no CI change," the codebase is in a clean,
well-tested, documented state and the arc is done. If the answer is
"nightly job," the work is a `.github/workflows/` file, not a
`src/` or `tests/` change.

### Full-workspace verification (post-arc)

The last all-workspace gate was at `v0.1.11` release. Eight commits
landed after it (provider seam, serve_io, ACP session/approval/cancel,
MCP dispatch, real MCP server, multi-client serve, docs), each gated
per-crate only.

A final all-workspace run at HEAD `9171666` reported:
`4378 tests run: 4370 passed, 8 exec failed, 5 skipped`.

The 8 failures were **not test failures**. Each reported
`error spawning child process / Resource temporarily unavailable
(os error 35)` — **EAGAIN on fork**, the host pressure quirk the
session-16 environment notes record ("fork pressure produces 9+
false exec_failed"). The tests never executed.

Re-running the 8 at `--test-threads 1` (minimal fork pressure)
passed; the full `kod-core` suite at `--test-threads 1` also passed.
Interpretation: **no regression**, only host fork pressure under a
full-workspace run.

Lesson for the environment notes (**L21**): a full-workspace gate on
this host can end with exit 100 purely from `execfail` — nextest could
not spawn the test process. Before treating exit 100 as a regression,
read the failure list: `execfail` + `os error 35` is host pressure;
an assertion or panic is a real failure. Re-run the execfailed names
at `--test-threads 1` to distinguish.

L21 extension (session 18, panic-isolation commit): **the same fork
pressure also kills the *compile* step**, not just test execution.
A `cargo nextest run --workspace --test-threads 1` under load can
abort with `could not compile <crate> (lib test) due to 1 previous
error` where the "previous error" is `Resource temporarily
unavailable (os error 35)` emitted by the **linker** (`cc`).
Distinguishing the two: read past the surface "could not compile"
line — if it is followed by an `os error 35` from `cc`, the fix was
never the code. Re-verify the affected crate at `--jobs 1`, which
bypasses the fork. Do not treat that compile failure as a regression.

## Session 18 — fault-injection arc

Continues the same session, after the behavioral-integration arc was
declared complete. The previous claim ("nothing left in the testable
surface") was about *feature* coverage. A different axis — *what
happens when a component misbehaves* — had barely been exercised.
This arc starts there.

A pre-arc audit found: `catch_unwind` appears **exactly once** in
production code (`swarm_runner.rs:1630`, wrapping agent bodies); **no
chaos/fault-injection tests existed** in the repo.

### What landed

| Commit | What | Outcome |
|---|---|---|
| `1dbf8cc` | tool-registry panic isolation | **real bug fixed** |
| `530ef01` | openai malformed-SSE e2e | regression guard |
| `a33b58c` | anthropic malformed-SSE e2e (parity) | regression guard |

### The real bug: a panicking tool killed the turn

`ToolRegistry::execute_tool` awaited `Tool::execute` directly. The
engine collects a parallel round's tool futures with
`futures::future::join_all(...).await` **in its own task** — no spawn
boundary between the registry and the engine. A panic inside any one
tool therefore unwound through the registry and out of the engine,
killing the whole turn. A model invokes dozens of tools per round and
the tool surface is open (MCP servers, plugins); one bad tool was
enough.

Written failing-first: three tests in
`crates/kod-tools/tests/it/panic_isolation.rs` failed **3/3 by
panic** on the pre-fix tree, then passed after the fix. The fix wraps
each `Tool::execute` future in `AssertUnwindSafe::catch_unwind` and
converts the payload into `ToolResult::Error` naming the tool.

The three tests:
- `panicking_tool_returns_an_error_not_an_unwind`
- `panicking_tool_in_a_parallel_batch_does_not_kill_siblings`
  (mirrors the engine's `join_all`; sibling tools survive)
- `panicking_tool_error_names_the_tool` (the model must see its own
  call to correct it)

### The guards: malformed SSE mid-stream, both adapters

Same shape a truncated TCP write or a frame-mangling proxy produces:
one bad frame in the middle of an otherwise-valid SSE body.
`wire.rs` unit tests pin the parser on a single frame; nothing pinned
the caller-observable behavior. Both adapters **already drop the bad
frame** — the guards are the value, not a bugfix. Proven at the
caller level for both, so a future divergence between them (one
drops, one errors-after-commit) fails the tests.

### What this arc did NOT close (candidates for next)

From the pre-arc audit, still uncovered:

1. **MCP malformed JSON-RPC from a real server.** `client.rs:507`
   parses every line a server emits. There is a `fuzz` target
   (`mcp_line`) but **no deterministic test** that a garbage line
   mid-conversation leaves the request/response correlation intact.
   Same class as the SSE tests, different subsystem. Highest-value
   remaining fault-injection test.
2. **`DecisionWait::RecvClosed`** — the branch exists
   (`tool_dispatch.rs:838`, handled at `:516` and `:711`) but no test
   drops the oneshot sender mid-wait. Small concurrency fault.
3. **Per-tool timeout** — `execute_command` has its own; no general
   per-tool timeout exists at the registry. That is a feature, not a
   test.
4. **Memory embedder panic** — partially covered by
   `flush_returns_after_panicking_embed_task`; other paths (retrieval
   mid-turn) not audited.

### Lesson

**L22 — grep in `crates/` misses `fuzz/`.** The `fuzz` directory is
`exclude`d from the workspace (`Cargo.toml`: `exclude = ["fuzz"]`).
A call from `fuzz/fuzz_targets/` is invisible to a
`grep -rn ... crates/` search. `kod_mcp::parse_line_for_fuzz` looked
dead in this session's first audit — its only caller is
`fuzz/fuzz_targets/mcp_line.rs`. When a `pub fn` looks uncalled,
grep the **whole repo** (minus `target/`), and look inside `fuzz/`
explicitly. This is the same over-claim shape as the earlier
sessions' "read the tests before naming a gap," with a specific new
cause: an out-of-workspace call site.

## Session 18 — fault-injection arc, completed

Extends the earlier session-18 fault-injection notes. Total for the
arc: **three real bug fixes with failing-first tests, three
regression guards, one class-level source lint with a positive
control.** All committed under session 18; tree clean at `60b5f83`.

### The full commit list

| Commit | Kind | Outcome |
|---|---|---|
| `1dbf8cc` | fix | tool-registry panic killed the turn — **fixed** |
| `530ef01` | guard | openai malformed SSE mid-stream |
| `a33b58c` | guard | anthropic malformed SSE mid-stream (parity) |
| `31e962a` | guard | MCP garbage line + timeout recovery |
| `69c94aa` | fix | embedder panic on retrieval path — **fixed** |
| `fd84489` | fix | embedder panic in fuse_duplicates — **fixed** |
| `60b5f83` | guard | source lint: every foreground embed goes through `embed_safe` |

### The three real fixes

All three had the same shape: an `.await` on a fallible operation
(third-party code, plugin, MCP server) in the caller's task, with no
spawn boundary — so a panic unwound through the caller and killed
the turn or the maintenance pass. Each was **written failing-first**
(the pre-fix run panicked with the fixture's exact panic string), and
each fixed at the *invariant* level rather than the instance.

1. **Tool panic** (`1dbf8cc`): `ToolRegistry::execute_tool` awaited
   `Tool::execute` directly; the engine's parallel round is a
   `join_all` in the engine's own task. One bad tool took the whole
   turn down. Fix: `AssertUnwindSafe::catch_unwind` at the registry,
   converting the payload into `ToolResult::Error` naming the tool.
2. **Embedder panic on retrieval** (`69c94aa`): `retrieve_context` ->
   `retrieve_long_term_hybrid` -> `rebuild_index` -> `embed()` on
   every prompt. Fix: `embed_safe` helper; two call sites wrapped.
3. **Embedder panic in fuse_duplicates** (`fd84489`): the
   consolidation pass from `router.rs:527`. Same helper, third call
   site, plus the Arc-deref the other sites already used.

The background embed path was **already** isolated — the existing
`flush_returns_after_panicking_embed_task` guarded it. The asymmetry
(background guarded, foreground not) is what made the fix necessary.

### The class-level guard

`crates/kod-memory/tests/it/embed_safe_lint.rs` scans
`src/manager.rs` and fails if any `.embed(` call appears outside
`embed_safe` and outside `spawn_embed`. It also asserts each barrier
still contains its intended call site, so a refactor that moved the
call out of `embed_safe` fails the lint too. Shape mirrors
`kod-types::source_lints::truncate_helpers_name_matches_contract`
(session 16).

**Positive control:** the test script injects a bad `.embed(` call
outside both barriers, confirms the lint *fails with the intended
message*, restores, and confirms green. A source lint that only ever
passes is untested; this one is proven to have teeth. Recorded in the
commit message so a future reader knows the guard was validated
against a positive control, not just observed passing.

### The vacuous-pass traps (both cost a round)

**The retrieval-panic test passed on first write without exercising
the embed path.** `retrieve_long_term_hybrid` returns early when the
long-term store is empty (`if all_arc.is_empty() { return
Ok(Vec::new()); }`). The test passed by never reaching the code it
was meant to exercise. Fix: store one entry first.

**The fuse-panic test passed on first write too.** Two traps stacked:
1. `fuse_duplicates` skips any project group with fewer than two
   entries; two *identical* contents dedupe to one at the store
   layer, so the group had one and the embed never fired. Contents
   must differ.
2. The fuse sort falls back to a 1-second wall-clock when
   `last_retrieved_at_ms` is unset. Explicit timestamps remove that.

**Discipline that caught both:** the pre-fix script fails **closed**
— if the fixture's exact panic string is not in the pre-run output,
the script refuses to apply the fix. A guard that says "did the
failing-first actually fail first?" is what distinguishes a real fix
from a test that never ran.

### What this arc did NOT close (from the earlier list)

- **`DecisionWait::RecvClosed`** — ruled out. Both drain paths
  (`respond_to_approval`, `deny_all_pending_approvals`) send before
  removing, so the sender is never dropped without a send. The branch
  is defensive and unreachable in current code; a test would be
  synthetic.
- **Per-tool timeout** — not a test. `execute_command` has one; a
  general per-tool timeout is a feature, not a bug.
- **MCP malformed JSON-RPC** — closed as a guard (`31e962a`). Both
  the garbage-line-mid-conversation and the timeout-does-not-poison
  shapes were already correct; the guard pins them.

### When to look here again

If a future change adds a fourth `.embed(` call in `manager.rs`, the
source lint fails immediately — that is the invariant holding. If it
adds a fourth fallible-`.await` in the caller's task elsewhere (a
tool path, an MCP path, a retry path), the same class of bug can
reappear there. The search shape:

    grep -rn '\.await' crates/<crate>/src \
      | grep -vE 'spawn|join_all|buffer_unordered|spawn_blocking'

An `.await` that is not behind a spawn boundary and calls into
third-party code (a provider, an MCP server, a plugin, a database)
is a candidate for panic-isolation review. This arc fixed three of
those; the fourth is not yet known.

## Session 18 — the in_flight flag class (exhausted)

The compaction-summary fix (`d5544ca`) and the ACP `in_flight`
guard (`58345bb`) were both instances of one shape:

    insert(flag) -> spawn an await -> clear(flag)

The bug is that "clear" only runs on the task's normal-exit path.
A panic between insert and clear leaks the mark; every future
attempt for that key early-returns (insert returns false) and the
feature silently no-ops. The bug-hunt fixed the ACP instance; the
compaction instance was missed.

### The audit

Searched production code for the shape: any flag with a set-like
mutation paired with a clear-like one, cross-referenced against
existing `Drop` guards and `catch_unwind` wrappers.

    grep -rnE "in_flight|in_progress|busy|poisoned|rebuild_in_progress" \
      crates/*/src --include="*.rs"

Every hit classified:

| Flag | Where | How it is guarded |
|---|---|---|
| `in_flight` | `kod-core-serve/src/acp.rs` | **Fixed** `58345bb` |
| `summaries_in_flight` | `kod-core/src/engine/compaction.rs` | **Fixed** `d5544ca` |
| `rebuild_in_progress` | `kod-memory/src/manager.rs:393` | `RebuildGuard` Drop guard |
| `poisoned` | `kod-mcp/src/client.rs:87` | Store-only — by design, permanent |
| `is_running` | `kod-core/src/advisor_tools.rs:89` | Mirrored from engine control, not a leaked-mark flag |
| `SwarmSlotGuard` / `SwarmRunGuard` / `DispatchGuard` | `kod-core/src/engine/swarm.rs`, `swarm_runner.rs` | Drop guards |
| `ConcurrencyPermit` | `kod-provider/src/concurrency.rs:250` | Drop guard |

### Result: class exhausted

Every set/clear flag in production is either:
- Drop-guarded (the guard clears on any unwind, including panic), or
- intentionally permanent (`poisoned`), or
- fixed in this session.

No further instance of the class exists in production code today.
That is a *finding*, not a gap: the search shape above came back
clean against the current tree.

### When this could reappear

A future commit could add a fourth set/clear flag without a Drop
guard. The way to catch it:

    git grep -n "\.store(true" crates/*/src \
      | grep -v "/tests" \
      | cut -d: -f1 | sort -u

For each file that stores a `true`/`insert`s an id, check whether
the matching clear is Drop-guarded. A flag whose clear sits in the
normal-exit path *after* an `.await` on third-party code is a
candidate. This is a slower check than a source lint, but it works
across the shape (it catches the four instances above without
hard-coding any of them).

If a fifth instance is ever added and leaks, the failure shape is
the same: the feature silently no-ops after the first panicking
attempt, and the flag's set membership is the only visible symptom.

## Session 18 — byte/char display-site migration (complete)

Continuation of session 18 after the fault-injection arc. This
sub-arc migrated the eleven remaining display-preview sites that had
the pre-P2-fix shape, added a helper to make the fix uniform, and
closed the class with a workspace-wide source lint.

### The shape

    if s.len() > N {
        format!("{ellipsis}", truncate_chars(s, N))
    } else {
        owned
    }

`truncate_chars` is a char cap since session 16. The guard is a
byte-length check. On CJK input whose char count is at the cap but
whose byte count exceeds it, the guard fires, `truncate_chars`
returns the string unchanged, and the caller appends an ellipsis to
a string that was never truncated. Session 16 fixed seven sites in
`render.rs`; the same shape persisted elsewhere.

### The work

| Round | Commits | What |
|---|---|---|
| Helper | `9042592` | `strutil::truncate_with_ellipsis(s, N, ellipsis)` + 8 tests |
| 1 | `f9b6ab8`, `d9515dd` | kod-cli × 2, kod-core-state × 1 |
| 2 | `336bba3`, `5e81c4b`, `8b93eb9` | kod-core × 1, kod-provider-anthropic × 1, kod-tools × 1 |
| 3 | `65c8e2a` | kod-tui × 3 |
| 4 | `2450be1` | workspace source lint + positive control |

Fourteen call sites migrated across six crates (five in render.rs
across fc14303 + e02f958, nine across the six other-crate migration
commits). Six source-level
assertion files pin per-crate adoption; the workspace lint in
`kod-types::it::source_lints::truncate_display_sites_use_helper`
catches any future reintroduction.

### The design choices, worth keeping

**The helper removes the outer guard entirely.** `truncate_with_ellipsis`
compares the returned prefix's byte length to the input's — strictly
shorter iff truncation happened. Exact in any unit. The naive fix
(change the guard to `.chars().count()`) would work for the *call
sites* but leaves the same decision split across two places
(the guard, and the format). The helper collapses it.

**Each round is per-crate with its own gate.** A regression is
attributable to one crate. The alternative (one large commit) would
make a failed workspace gate ambiguous.

**The source-level assertions are honest coverage, not tests of
behavior.** These are private display-preview sites inside command
handlers. A behavior harness would cost more than the specificity it
adds; the helper's own unit tests (in `kod-types`) cover the
semantics; the source assertions cover the *adoption*.

**The lint has a positive control.** The commit script injected a
plausible bad shape at the end of `render.rs`, confirmed the lint
fails with the byte-guard message, restored, and re-confirmed green.
A guard that only ever passes is untested.

### What this sub-arc leaves un-done

- **No `.github/workflows/`** — the `#[ignore]`d MCP-filesystem test
  and the live LSP test still run only on a developer machine with
  the right deps. The lint runs in `cargo nextest --workspace`; the
  live tests do not.
- **No release tag** — the last is `v0.1.11`. Everything since is
  test-only or docs except the four panic-isolation fixes
  (`1dbf8cc`, `69c94aa`, `fd84489`, `d5544ca`) and the eleven
  display-site fixes (`f9b6ab8` … `65c8e2a`). The panic-isolation
  fixes are behavior changes: a panic that killed a turn or wedged a
  session now returns a tool error or falls through to
  keyword+recency. `v0.1.12` would be a legitimate release.

### Bug-hunt-markers audit (session 18)

Grepping for `P1`/`P2`/`bug-hunt` comments and checking which had
tests surfaced the ACP `in_flight` gap, now closed by `58345bb`.
That audit is repeatable:

    grep -rnE "// (P1|P2|bug-hunt)" crates/*/src --include="*.rs"

For each hit, check whether the fix it names has a test pinning the
observable behavior. Two that were checked and closed:

- ACP `in_flight` release-on-completion (`58345bb`) — the compaction
  sibling of this was fixed under fault-injection (`d5544ca`).
- The in_flight flag class — audited, exhausted, recorded under
  "in_flight flag class exhausted".
