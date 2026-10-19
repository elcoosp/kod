# Handoff — kod, session 18

**Continues:** session 17 (`docs/handoff/session-17.md`, now a mixed
record — see "Note on session-17.md" at the end).
**Last tag:** `v0.1.12` (`59e6951`).
**Last verified commit:** `98e129b` (full-workspace gate green at that
commit; see "State" below and the four "Re-verification"
subsections in "What's open" #1). The gate at `98e129b` had one
`execfail` (`os error 35`); the execfailed test reran green at
`--test-threads 1`, confirming L21 host pressure, not a regression.
**Tree at handoff:** clean.

## Step 0 — verify before touching anything

Three commands, reconcile against "State" below:

    git log --oneline -8
    git status --porcelain --untracked-files=no
    git describe --tags --abbrev=0

Expect HEAD at `a2eb27b` or later, tree clean, tag `v0.1.12`.
If the tree is dirty, name the files before proceeding — do not
assume an in-progress change is yours.

## What's open, in order

### 1. CLOSED — full-workspace gate at `a2eb27b` (`74d8ef7`); re-verified at `a23bce7`, `47cc41a`, `5a005a9`, and `98e129b`

The four proptest crates (`195f30b`, `f665998`, `0006609`,
`b5704c3`) were each gated per-crate. The full-workspace run since
them — `CARGO_BUILD_JOBS=1 cargo nextest run --workspace
--test-threads 2` — passed at `a2eb27b`, recorded in `74d8ef7`.
The four-crate expansion did not perturb any other crate.

If you are reading this and HEAD has moved past `74d8ef7`: re-run
the gate before trusting the four-crate state, because a later
commit could have changed the interaction.

**`--no-fail-fast` is not optional on this host.** The first attempt
at `--test-threads 2` without it stopped at test 1902 of 4433 on an
EAGAIN execfail, leaving 2531 tests unrun — a partial run that is
not a verification. The complete command:

    CARGO_BUILD_JOBS=1 cargo nextest run --workspace --no-fail-fast --test-threads 2

`--test-threads 2` and `JOBS=1` are host requirements, not
preferences (L21). If the run ends nonzero, read the failure list
before interpreting: `execfail` + `os error 35` is EAGAIN fork
pressure (L21), not a regression. Re-run only the execfailed names
at `--test-threads 1` to distinguish.

**Re-verification at `a23bce7` (after the kod-config proptests).**
The gate at `74d8ef7` predates the two kod-config commits (`8b4e7f5`
and its companion `a23bce7`). Re-ran the same command at `a23bce7`:

    4436 tests run: 4436 passed, 5 skipped

The count went from 4433 to 4436 — exactly the three new kod-config
properties. No execfail, no EAGAIN, `--no-fail-fast` was on (the
whole suite ran). This is the last-verified state.

**Re-verification at `98e129b` (after the serve panic-guard fix).**
Ran the same command at `98e129b`:

    4443 tests run: 4442 passed, 1 exec failed, 5 skipped

One `execfail` (`Resource temporarily unavailable`, `os error 35`)
on `kod-risk paths::tests::etc_exact_is_catastrophic_and_below_it_too`.
Re-ran only that test at `--test-threads 1`: passed in 0.02s. L21
host fork pressure, not a regression. Total 4443/4443 green.

**Re-verification at `5a005a9` (after the `LlmConfig::validate` proptests).**
Ran the same command at `5a005a9`:

    4442 tests run: 4442 passed, 5 skipped

The count went from 4439 to 4442 — exactly the three new
`llm.rs` properties. No execfail, no EAGAIN. The `4f794fc` commit on
top is YAML-only; the code state at `5a005a9` is what is verified.

**Re-verification at `47cc41a` (after the `Policy::from_toml` proptests).**
Ran the same command at `47cc41a`:

    4439 tests run: 4439 passed, 5 skipped

The count went from 4436 to 4439 — exactly the three new `policy.rs`
properties. No execfail, no EAGAIN. This is the last-verified state.

### 2. CLOSED — `kod-config` parsers (three custom-parsing modules done)

Ten modules in `crates/kod-config/src/` parse TOML or JSON
(`config.rs`, `llm.rs`, `policy.rs`, `memory.rs`, `limits.rs`,
`swarm.rs`, `jev.rs`, `mcp.rs`, `retry.rs`, `skills.rs`). Each has
example tests; none has a proptest. The `load_from` /
per-section-recovery path in `config.rs` is the biggest single
contract: `recover!` keeps the sections that parsed and drops the
ones that did not, and the record in `docs/brainstorms/` names a
past bug where `[mcp]` and `[limits]` were silently dropped on a
partial parse failure.

The property shape is the one the four crates this session used:

- `parse(serialize(cfg)) == cfg` for generated `KodConfig` values
- recovery preserves every section whose TOML is individually valid
- `load_default` never panics on a malformed file

First exploration, not a guess:

    sed -n '/pub fn load_from/,+60p' crates/kod-config/src/config.rs
    grep -n 'recover' crates/kod-config/src/config.rs

Read the `recover!` macro before writing any property against it.
Budget 2-4 rounds; the checker-vs-code failure rate this session was
4 failures in ~15 rounds, all in the *check*, none in the code
(L23, L25, L26, L27).

### 3. CLOSED — live integration tests wired into CI

The previous text claimed `.github/` was absent and framed CI as an
open decision. That was stale: `.github/workflows/ci.yml` has existed
since `668e54b` and runs the full workspace test suite, provider
contracts, prompt goldens, MSRV, security audit, cargo-deny,
coverage, PR bloat gate, live Ollama, live Anthropic (skippable),
benchmarks, and a fuzz smoke.

The gap was narrower. Two tests were not being exercised:

- `kod-mcp` `real_filesystem` (`cf5a529`) — `#[ignore]`d, needs
  `npx`.
- The live LSP test in `kod-lsp/src/client.rs` — runs in the
  default suite but silently skips when `rust-analyzer` is not on
  `PATH`.

Both are now covered by the `live-integrations` job in `ci.yml`,
which installs the `rust-analyzer` rustup component (turning the
LSP test from a silent skip into a real assertion), asserts
node/npx/rust-analyzer versions up front, runs the MCP
`--run-ignored` set, and greps the LSP test's output for
`skipping:` — a silent skip in this job would be the fail-open
shape L25 records.

The decision: the project promises to exercise the real
integrations on every push to main/develop and every PR from the
same repo, and to skip cleanly on a fork PR (same shape
`live-model` and `live-anthropic` already use). Not nightly; every
push. If the cost becomes a problem, moving behind a schedule is a
one-line change.

### 4. `v0.1.13` — do not tag

Everything since `v0.1.12` is test-only or docs. A release tag for
test-only commits is a tag that means nothing. `v0.1.12` is the
correct last tag.

## State

`main` at `98e129b`, seventeen commits past `v0.1.12`:

| Commit | What |
|---|---|
| `e489e04` | L25 — a check that does not halt is not a check |
| `195f30b` | `kod-schema-dialect::sanitize` proptests (4) |
| `744d1cb` | L26 — grep for a code shape must exclude comments |
| `f665998` | `kod-error::provider_status` proptests (4) |
| `0006609` | `kod-ast::extract::symbols` proptests (4 + depth stress) |
| `eb92c8e` | L27 — a property test must exercise the code path it asserts on |
| `b5704c3` | `kod-minimize::plan::classify` proptests (5) |
| `a2eb27b` | L28 — `--` in a `git commit -m` message is a pathspec separator |
| `74d8ef7` | full-workspace gate green at `a2eb27b` |
| `0cd837e` | docs(handoff): session-18.md — the starting document for the next agent |
| `8b4e7f5` | test(config): proptests for KodConfig round-trip, recovery, load_default |
| `a23bce7` | chore(config): the proptest dev-dep that 8b4e7f5 left behind |
| `d279de0` | docs(handoff): full-workspace gate green at `a23bce7` (4436/4436) |
| `47cc41a` | test(config): proptests for `Policy::from_toml` (never-panics, round-trip, preset_explicit) |
| `12a1ec1` | docs(handoff): full-workspace gate green at `47cc41a` (4439/4439) |
| `5a005a9` | test(config): proptests for `LlmConfig::validate` (never-panics, postconditions, idempotent) |
| `4f794fc` | ci: add live-integrations job (rust-analyzer + npx) |
| `092bc4a` | docs(handoff): gate at `5a005a9`; items #2 and #3 closed |
| `98e129b` | fix(serve): send an error frame when a spawned engine task panics |

`v0.1.12` (`59e6951`) is the release that carries the two behavior
changes from this work: four panic-isolation fixes (a panic in
third-party code no longer kills a turn or wedges a session) and
fourteen CJK display-site fixes (no spurious ellipsis at the char
cap). Everything after is tests and documentation.

## Bug hunt — the panicking serve task (session 18, continuation)

After the two open items closed, a fresh hunt explored the panic-isolation
class — the same class as the four prior fixes (`1dbf8cc`, `69c94aa`,
`fd84489`, `d5544ca`). The reading found one gap: the NDJSON serve
adapter (`crates/kod-core-serve/src/serve.rs`) spawns
`engine.process_for`, `run_streaming`, and `run_swarm` directly in
`tokio::spawn`. A panic unwinds past the `match ... Err(e)` arm — that
arm handles `Result::Err`, not an unwind — so no error frame reaches
the caller, and it waits on its request id until its own timeout.

The sibling adapter `acp.rs` fixed exactly this shape in `aa21bf8` (P1:
a panicking `run_prompt` wedged the session on `-32002` forever). The
ACP fix used an inner `tokio::spawn` whose `JoinHandle::await` returns
`Err` on panic, so the cleanup runs on every path. Same class, one
adapter fixed, one not.

The fix (`98e129b`): a shared `spawn_panic_guarded` helper that takes
the future, the outbound writer channel, and the request id. On a
`JoinError` it sends a `kind: "error"` frame for that id and logs;
`is_panic` vs cancelled are distinguished in the frame message so a
cancelled task is not reported as a panic. Three sites wired through
the helper — `process` (line 471 pre-fix), `process_streaming` (line
504), `swarm` (line 608).

**Honest scope** (matching `in_flight_release.rs`): the end-to-end
serve path is not driven by the new test. `handle_connection` takes a
concrete `transport::Stream`, not a generic reader/writer, so a
duplex-driven test would need the same `serve_io` extraction `acp.rs`
got. That extraction is a separate change; named here so the next
session does not re-derive it. What the new test pins is the helper
the three sites share, which is what the fix is.

**Positive control** (L27): the test script temporarily reverts the
helper body to `let _handle = tokio::spawn(fut); let _ = (out, id);`,
confirms the test fails with "no error frame" (channel-close or
timeout, either means no frame), restores, and re-confirms green.

**A dishonest red, kept as a lesson (L37):** the first attempt at the
failing-first test spawned its *own* panicking future and asserted on
its own channel. That test would fail before the fix and pass after,
but only because the fix was inside the test body — not because
production code changed. The reverted stub left no trace; the lesson
is recorded because the shape is tempting and the check is easy to
write without noticing it is tautological.


## What landed before `v0.1.12` in this arc

Three sub-arcs, summarized so the next agent does not re-derive:

**Behavioral integration.** Five seam tests (provider adapter,
ACP `session/prompt`, ACP approval, ACP cancel, MCP tool dispatch),
`serve_io` extraction, a real MCP filesystem test, two concurrent
`serve` clients. Each closes a *join* between two already-tested
layers, not a layer itself.

**Fault injection.** Four real bugs, each failing-first: a
panicking tool killed the turn (`1dbf8cc`), a panicking embedder
killed retrieval (`69c94aa`) and consolidation (`fd84489`), a
panicking compaction-summary provider wedged the transcript forever
(`d5544ca`). Three guards on already-correct behavior (malformed SSE
mid-stream, both adapters; MCP garbage line + timeout). Two source
lints with positive controls (`embed_safe_lint`, and the in-flight
flag class audited as exhausted in `665a74d`).

**Byte/char display sites.** The session-16 P2 (byte cap under a
char name) had fourteen call sites still using the pre-fix shape.
A shared helper `strutil::truncate_with_ellipsis` (`9042592`), six
per-crate migration commits, and a workspace source lint
(`2450be1`) with a positive control.

**Proptest surface.** Four crates, 19 new property tests, each with
a positive control proving it fails on an injected regression:
`sanitize` (idempotency, no-mutation), `provider_status`
(recoverable ⊆ retryable), `symbols` (1-based lines, documented
kinds, no-panic), `classify` (false negatives impossible — the
doc's own claim, mechanized).

## Operational quirks — read before running anything

These cost rounds this session. They are not preferences.

**The file watcher.** Six `watchexec` instances watch `wr1.sh`;
every write fires all six. Write scripts atomically:

    cat > wr1.sh.tmp << 'SCRIPT'
    ...
    SCRIPT
    chmod +x wr1.sh.tmp && mv wr1.sh.tmp wr1.sh

A direct `cat > wr1.sh` races a mid-execution reader; bash reads
scripts incrementally, so a replaced file executes a mix of old and
new content and can hang on a half-read heredoc. If it hangs:
`Ctrl-C`, `pkill -TERM -f 'cargo nextest|cargo check|cargo build'`,
`sleep 2`, `pkill -KILL` anything left. Nothing is lost — the hang
is a read failure, not a write corruption. `git status` will be
clean.

**Heredocs truncate at roughly 120-150 lines on this host.** Write
long files in verified chunks: write, check `wc -l` and `tail -1`,
append, re-check. Never assume a long heredoc completed.

**Markdown files with triple-backtick fences cannot be written via
heredoc** — the watcher's fence stripper treats the inner fence as a
message boundary and truncates the script there. Use `~~~` for
Markdown fences (valid CommonMark, renders identically, no
triple-backtick for the stripper to match).

**Bash 3.2.** No `declare -A`, no `mapfile`, no `${x^^}`. No
`timeout` binary either (`gtimeout` from coreutils, or absent) — a
`command not found` swallowed by `2>/dev/null` looks exactly like
"no matches". Diagnose tool availability before relying on it.

**`set -uo pipefail` does not include `-e`.** A failed `cat >` or
`python3 -` does not abort the script. Every write must be followed
by `test -f`, and every verification must `exit 1` on mismatch.

**`grep -c X || echo 0` produces `"0\n0"`** — use `grep -c X; true`
or capture into a variable first.

**`tokio::fs` is not in the workspace feature set.** Use
`spawn_blocking` for sync file IO.

**kache disk pressure can trigger EAGAIN on `rustc` fork.** If the
build fails with `Resource temporarily unavailable`, `rm -rf target`
(never `cargo clean`, never delete the kache cache).

**Full-workspace suites need `--test-threads 2`, `JOBS=1`, and
`--no-fail-fast`.** At default parallelism, 9+ false `exec_failed`.
Without `--no-fail-fast`, a single execfail stops the run and leaves
the rest unverified.

**A ` -- ` in a `git commit -m` message is parsed by git as the
end-of-options separator**, not just by the shell. The message is
truncated at `--` and the trailing words become pathspecs. Any
message containing an em-dash, a `--flag` snippet, or a Markdown
horizontal rule needs `git commit -F <file>`.

## The lesson index (L20–L37)

Every one of these is from a real failure this session. Read the
corresponding section in `session-17.md` for the full story.

| L | One line |
|---|---|
| L20 | Never overwrite `wr1.sh` in place; write tmp + mv |
| L21 | Fork pressure hits tests *and* the linker; `execfail` + `os error 35` is host pressure, not regression. Read the failure list before interpreting exit code. |
| L22 | `fuzz/` is excluded from the workspace; a call from `fuzz/fuzz_targets/` is invisible to `grep crates/`. Grep the whole repo when a `pub fn` looks uncalled. |
| L23 | A guard test is code, and code has bugs. Read the assertion's arithmetic before concluding the *code under test* is wrong. |
| L24 | `cat > missing/path` fails silently without `-e`. `mkdir -p` first; `test -f` after. |
| L25 | A check that prints a warning and continues is fail-open. Every verification must `exit 1` on mismatch. |
| L26 | A grep for a code shape matches comments. Match `name(` (open paren) or strip `//` / `*` lines. For a committed lint, use `production_lines()`. |
| L27 | A property test over a `Vec` is vacuous if the strategy does not populate it. Add one input class that guarantees non-empty; the existing example tests are that class. |
| L28 | `--` in a `git commit -m` message is git's pathspec separator. Use `-F <file>`. |
| L29 | Staging a subset of the intended change leaves companion files behind. After a commit, verify `git status --porcelain` is empty, not just that the tests pass. |
| L30 | In `echo "outer "inner's" outer"`, the apostrophe outside the nested double quotes opens a `'` bash reads to EOF for. Escape it or use a variable. |
| L31 | A serialization round-trip that compares serialized strings cannot catch a field dropped from both directions. Assert on the field values on the way back, not only on the bytes. |
| L32 | `let _ = catch_unwind(...)` passes on a panicking implementation. The `catch_unwind` swallows the panic; the property has to assert `.is_ok()` (or `is_err()`). A check that does not halt is not a check. |
| L33 | A control's landing check must not count a string the file already contains in prose. Verify structure (attribute adjacent to the target field), not occurrence count. |
| L34 | A strategy that generates values the comparison method rejects is vacuous: the assert never runs, and the error is in `toml::to_string`, not in the code under test. Bound the strategy by the comparison method's domain. |
| L35 | A strategy whose *range* includes an edge is vacuous if the edge is never *sampled*. `0u64..=i64::MAX` has 2^63 values; P(0) per case ≈ 10⁻¹⁵ even at 10,000 cases. Force the edge with `prop_oneof![Just(v), range]`. |
| L36 | An item named "CI wiring — a decision" must be checked against `ls .github/` before being rewritten from memory. The handoff claimed `.github/` was absent; it was present and comprehensive since `668e54b`. |
| L37 | A failing-first test that spawns its own panicking future is tautological: the fix it proves cannot be in production code. Drive the failure through production code, or pin a helper and accept the honest-scope caveat the way `in_flight_release.rs` does. |

The pattern behind L23–L27: **the checker is code, and code has
bugs.** In four proptest crates and ~15 rounds, there were four
checker-vs-code failures and zero code failures. The proptests
themselves were correct from the first non-vacuous write. Writing
the test is easy; proving it has teeth is the hard part.

## What NOT to do

- **Do not interpret a nonzero workspace gate as a regression before
  reading the failure list.** `execfail` + `os error 35` is host
  pressure. Assertions and panics are real. This is L21 and it cost
  a round this session.
- **Do not write `wr1.sh` in place.** L20.
- **Do not put triple-backtick fences in a file written by heredoc.**
  Use `~~~`.
- **Do not trust a warning-only check.** If a verification can fail
  and does not exit, it is not a check. L25.
- **Do not conclude a property test has teeth because it passes.**
  Write the positive control: inject a plausible regression, confirm
  the property fails at the intended assertion, restore, re-confirm
  green. All four proptest crates this session did this; the one
  that did not (the first kod-ast draft) was vacuous and the control
  caught it. L27.
- **Do not `git commit -m` with `--` in the message.** L28.
- **Do not tag `v0.1.13` for test-only commits.** Everything since
  `v0.1.12` is tests and docs; a tag would be meaningless.
- **Do not re-build a covered seam.** Three times this session a
  "gap" was named from memory and reading the tests showed it
  covered. Read the test file first.

## Where to start

1. **Step 0** — verify tree, HEAD, tag.
2. **"What's open" #1** — if HEAD has moved past `74d8ef7`, re-run
   the full-workspace gate with `--no-fail-fast`.
3. **"What's open" #2** — CLOSED. `config.rs`, `policy.rs`, and
   `llm.rs` are property-tested. The other seven modules in
   kod-config are pure `#[serde(default)]` derives where the only
   contract is serde's own.
4. **"What's open" #3** — CLOSED. The `live-integrations` job in
   `ci.yml` runs the two previously-uncovered tests.
5. **Or stop.** The tree is clean, tagged, and verified end-to-end
   at `98e129b`. Nothing this session started is unfinished.

## Note on `session-17.md`

`session-17.md` is now 1100+ lines and is a **mixed record**: it
began as the session-17 handoff and accumulated eight appendices
from session 18 (behavioral integration, fault injection, byte/char
migration, proptest expansion, L20–L28). It is the *detail* file —
the place to read the full story of a lesson or a fix. This file is
the *starting* file — the place to read what is open and where to
go. If a future session finds the split unhelpful, the fix is to
extract session 18's appendices into `session-18-detail.md` and
leave `session-17.md` as a coherent session-17 record.

One concrete issue in that file: its "Full-workspace verification
at $(git rev-parse --short HEAD)" section contains literal
`$SUMMARY_LINE` and `$EPILOGUE` placeholders — the script that wrote
it used a quoted heredoc, so those never expanded. The verdict
(green at `a2eb27b`) is correct; the summary line the file shows is
a template, not the real 4433-test output. If a future session needs
a gate-record shape, copy the "Re-verification at `a23bce7`"
section in this file, not that one.

## Session 18 in one line

Started with a P2 fix plan from session 15 and closed three bug
classes, a test-surface class, two open items, and one more
bug found in a follow-up hunt: a panicking third party no longer
kills a turn (four fixes, each failing-first), a char-named
truncator no longer miscounts CJK (fourteen sites, one helper,
one workspace lint), doc-claimed contracts in five crates —
kod-schema-dialect, kod-error, kod-ast, kod-minimize, and
kod-config — are property-tested with positive controls, the
two previously-uncovered live tests run in CI on every push to
main/develop, and a panicking spawned engine task in the NDJSON
serve adapter now sends an error frame instead of leaving the
caller to time out (same class as the ACP fix in aa21bf8).
Eighteen handoff lessons (L20–L37) that are mostly about the
*checker* being wrong, not the code. The full workspace gate at
`98e129b` runs 4443 tests, all green, including nine new
properties across the three kod-config modules (`config.rs`,
`policy.rs`, `llm.rs`).
