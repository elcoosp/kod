# Handoff — kod, session 16

**Continues:** session 15 (`docs/handoff/session-15.md`).
**Last stable release:** `v0.1.11` (`98bbd1c`).
**Last verified commit:** `0effff4 test(serve): ACP session/request_permission approval round-trip`.
**Working tree state at handoff:** clean.

Session 16 did two things: it executed the P2 fix that session 15
diagnosed but could not land, and it opened the behavioral-integration
arc — the first end-to-end testing of seams between already-tested
layers. Both are complete; the remaining behavioral gaps are named
below with the evidence they were verified against.

## The P2 fix — `v0.1.11` (16 commits, tagged)

Session 15 found that `truncate_chars` capped by **bytes** under a
character-count name. Session 16 executed the handoff's 3-round plan,
and in doing so found that the plan's Round-A enumeration was
incomplete in two directions. The complete record is in
`docs/BUG_REPORT_TRUNCATE_CHARS.md` (182 lines); the summary:

- **Round A** (5 commits, additive, no behavior change): added
  `truncate_bytes`, migrated 6 byte-intent callers (`manager.rs`,
  `hooks.rs`, 3 in `router.rs`), retargeted the byte property test.
- **Round B** (3 commits, gated): flipped `strutil::truncate_chars`
  to char cap; the full-workspace gate caught 3 `kod-core` display
  tests asserting byte semantics; fixed in one commit. Added
  char-bounded property tests.
- **Class X** (Fixup-1, `44addb9`): `engine::render::truncate_chars`
  delegates to `strutil::truncate_chars`, so Round B's flip silently
  flipped four byte-intent `render`/`tool_dispatch` callers
  (`STRUCTURED_TOOL_MSG_CAP`, `per_field`, two `cap` sites) to a
  4x-over-CJK-bytes regression. **Missed by the handoff's Round-A
  list.**
- **Class Y** (Fixup-2, `46e0182`, `54c2218`): the handoff said
  `router::truncate_chars` and `context_engine::truncate_chars` were
  "already byte-correct, their callers are byte-budget." **False** at
  both sites: their only callers are char-count display budgets
  (`MAX_EXAMPLE_CHARS = 1500`, literals 80/200). These were the
  original P2 pattern, at sites the audit never inspected. Flipped.
- **Round C** (`022c139`): the handoff proposed a definitions-only
  source lint. Because Round B surfaced the call-site mismatch class,
  the shipped lint scans **both** directions: definition bodies and
  call-site budget names. It would have caught Class X and Class Y.
- **Bug report** (`568baee`): the session-15 report was truncated at
  13 lines. Completed to 182 lines with the full affected set, why
  example-based passes missed it, the multi-round fix plan with SHAs,
  and the do-not-do list.

**Full workspace at release:** 4364 passed, 3 skipped, 0 failed.

## Behavioral integration — 5 commits, post-release

The premise: each layer around the engine has tests, but the **joins
between them** do not. Session 16 built five seam tests. Every one is
no-model, no-network, deterministic.

| Commit | Seam | Gate |
|---|---|---|
| `6931cef` | engine <-> real provider adapter (text, request-shape, streaming) + 2 proptest seeds | kod-core 620/620 |
| `ac84e3e` | `serve_io(reader, writer)` extracted from `serve()` (parameterization, no behavior change) | acp_handshake green |
| `7b2fdb2` | ACP `session/prompt` full turn: initialize -> session/new -> streamed `agent_message_chunk` -> `{stopReason}` | kod-core-serve 68/68 |
| `6b00622` | engine <-> MCP tool dispatch: config -> install -> start() -> model tool call -> real fake server -> tool_results | kod-core 622/622 |
| `0effff4` | ACP `session/request_permission`: gated tool call -> approval request -> approve/deny -> file written/skipped -> `{stopReason}` | kod-core-serve 70/70 |

The `serve_io` refactor (`ac84e3e`) was the enabling change: `serve()`
read `tokio::io::stdin` and wrote `stdout` directly, so the ACP bridge
could only be driven by spawning the binary. `serve_io` takes any
`AsyncRead`/`AsyncWrite`; `serve()` is now a one-line wrapper. Same
loop, same dispatch, same shutdown semantics — `acp_handshake` stayed
green as proof.

## The over-claim pattern — read this before naming any gap

Three times in this session I named a coverage gap, and three times
reading the tests showed most of it was covered:

1. "The provider adapters are never exercised end-to-end." **False.**
   `httpmock` 0.8 was already a dev-dep of both provider crates;
   `stream_retry.rs` and `contracts.rs` drive the real adapters.
2. "MCP is the largest untouched surface." **False.**
   `kod-mcp/src/client.rs::tests` drives a real Python fake server
   through `initialize` / `tools/list` / `tools/call` / errors /
   SIGTERM; `mcp_adapters` has 10 more tests.
3. (For the first two, the actual gap was narrower each time: the
   *join* between two tested layers, not either layer.)

The discipline that should have come first: **grep the tests before
naming a gap.** The exploration scripts in this session's later half
do exactly that. Earlier ones did not.

## Remaining behavioral gaps (read-then-verified, not guessed)

Each entry below was confirmed by reading the test files, not by
assuming. The format is: what is uncovered, where the code is, what
a test would need.

### 1. ACP `session/cancel` (narrow)

The dispatch arm at `crates/kod-core-serve/src/acp.rs:289` handles a
`session/cancel` notification by calling `engine.request_cancel_for`.
No test drives it. A test needs a `MockProvider` script that emits a
long-running turn (a tool call that blocks, or a text stream with
delays), a `session/cancel` sent mid-turn, and an assertion that the
final `{stopReason}` is `cancelled` (the bridge maps a cancel to that
reason at `acp.rs:571`). Deterministic version: a scripted provider
whose second round never returns, plus cancel, plus a timeout
assertion. Medium effort.

### 2. ACP corrupt-batch deny-all path (narrow)

`acp.rs:606-616`: an approval batch whose JSON fails to parse
triggers `deny_all_pending_approvals()`. Driving it needs an engine
that emits a malformed `tool_approval_batch_marker` — the marker is
produced internally by `tool_dispatch.rs:679`, so a test would need a
fake engine or a fault-injection hook that does not exist. Lower
value than (1); the happy-path approve/deny is covered by
`0effff4`.

### 3. LSP live tests (policy question, not a gap)

`kod-lsp`'s live tests are `#[ignore]`d by design — they need
rust-analyzer installed. The `auto_lsp` path in `kod-core` runs
against a real fixture in CI. Whether to run the ignored set in a
nightly job is a CI policy decision, not missing coverage.

### 4. What is NOT a gap (verified covered)

- Provider adapter wire (both crates, `httpmock`).
- `kod-mcp` client protocol (fake Python server).
- `mcp_adapters` name composition + `ensure_started` error paths.
- Engine policy gates (`policy_gate.rs`).
- ACP `initialize` handshake (spawns the real binary).
- Session log, secret placeholders, compaction, fallback chain —
  each has its own `it` file.

Do not re-build any of these. The session-15 lesson L12 applies:
the tests prove the contracts they assert; read them before assuming
a contract is unasserted.

## Session-16 quirks (append to L9-L15)

**L16 — The file-watcher truncates scripts at inner triple-backticks.**
A heredoc that writes a Markdown file containing a fenced code block
(three backticks) is cut at the opening fence: the watcher treats it
as a message boundary. Cost 1 round (the session-15 bug report was
re-truncated). Fix: write Markdown fences as `~~~` — valid
CommonMark, renders identically, and contains no triple-backtick for
the stripper to match.

**L17 — `src/` proptests write seeds to a different directory than
`tests/` proptests.** A proptest inside `src/foo.rs` writes its
regression seed to `<crate>/proptest-regressions/foo.txt`; one inside
`tests/it/bar.rs` writes to `<crate>/tests/proptest-regressions/bar.txt`.
The two paths are not interchangeable: a seed at the `src/` path
guards nothing if the proptest compiles only in the `tests/` module.
Session 16 found the P2 reproducer (`s = "¡"`) stranded at
`crates/kod-types/proptest-regressions/strutil.txt` — a path where no
proptest compiles — and moved it into
`crates/kod-types/tests/proptest-regressions/string_safety.txt`
where it now re-runs. When adding a proptest, check where its seed
lands after the first failure.

**L18 — Nextest output can be truncated in transmission.** Twice in
session 16 a script's output stopped mid-run (once mid full-workspace
gate, once mid-commit). The script had actually completed both. The
fix is the same as Step 0: before assuming the last visible line is
the state, run a verify script that reads `git log`, `git status`,
and re-runs the gate. Never trust a truncated stream's implied state.

**L19 — `set_mcp_host` is a setter, not a registrar.** The engine's
tool loop "only sees the adapters registered at `start()` time"
(doc on `set_mcp_host`, `engine/policy.rs:35`). An MCP host installed
after `start()` registers nothing. Any MCP test must call
`set_mcp_host` before `start()`. This is not obvious from the method
name and cost one diagnosis round.

## Environment quirks (unchanged from sessions 14-15)

- Six `watchexec` instances watch `wr1.sh`; every write fires all
  six. Use an atomic `mkdir` lock.
- Bash 3.2: no `declare -A`, no `mapfile`, no `${x^^}`.
- `grep -c X || echo 0` produces `"0\n0"` — use `grep -c X; true`.
- `tokio::fs` is not in the workspace feature set — use
  `spawn_blocking` for sync file IO.
- kache disk pressure can trigger EAGAIN on `rustc` fork; `rm -rf
  target` (never `cargo clean`, never delete the kache cache).
- Full workspace suite at `--test-threads 2` on this host.
- Long heredocs truncate at ~120-150 lines — write in verified
  chunks.
- `~~~` for Markdown fences in any file a heredoc writes (L16).

## What NOT to do

- **Do not re-build a covered seam.** Read the test file first. Three
  over-claims in session 16 were all corrected by reading.
- **Do not add a `proptest!` in `src/` for a test whose siblings are
  in `tests/it/`** unless you intend the seed at the `src/` path.
  See L17.
- **Do not write a Markdown file with triple-backtick fences via
  heredoc.** See L16.
- **Do not trust truncated command output.** See L18.
- **Do not install an MCP host after `start()`.** See L19.
- **Do not flip a `truncate_*` helper without running the call-site
  lint** (`source_lints::truncate_helpers_name_matches_contract`).
  It scans budgets, not just bodies — that is the class-level guard
  for the P2.
- **Do not rename a local `truncate_chars` to `truncate_bytes`** to
  "make it honest." The docstring carries the history; the lint
  pins the contract. Renaming loses both.

## Where to start (next session)

1. **Read this file, then `docs/BUG_REPORT_TRUNCATE_CHARS.md`.**
   Together they are the complete record; do not re-diagnose.
2. **If continuing behavioral integration:** the highest-value
   remaining gap is ACP `session/cancel` (above, gap 1). The harness
   exists (`serve_io` + the frame helpers in `acp_session.rs` /
   `acp_approval.rs`); the new work is a scripted provider that
   blocks, plus a mid-turn cancel frame. Everything else in the ACP
   bridge is covered by `0effff4` and `7b2fdb2`.
3. **If the arc is done:** the next class of work is not more tests.
   The static audit, bug hunt, property tests, concurrency
   invariants, source lints, and behavioral seams are all in place.
   What remains under-exercised is *operational*: a real editor
   session against a real MCP server (filesystem or github), a real
   language server, a long-running daemon. That is integration
   testing against live dependencies, and it needs a decision about
   CI cost before it needs code.

## Session 16 in one line

Executed the P2 `truncate_chars` fix end-to-end (16 commits, tagged
`v0.1.11`, including two classes of affected callers the handoff's
plan missed), then opened the behavioral-integration arc with five
seam tests across the provider adapter, ACP session/approval, and MCP
tool-dispatch boundaries — each verified green, each no-model and
deterministic — and recorded the "read the tests before naming a
gap" lesson that three over-claims cost.
