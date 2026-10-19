# Handoff — kod, session 18

**Continues:** session 17 (`docs/handoff/session-17.md`, now a mixed
record — see "Note on session-17.md" at the end).
**Last tag:** `v0.1.12` (`59e6951`).
**Last verified commit:** `45aec66` (full-workspace gate green at that
commit; see "State" below and the ten "Re-verification"
subsections in "What's open" #1). 4463/4463 on the first try; 5
skipped; no leaky. The commit fixes a **panic** in
`kod-swarm/src/brief.rs::parse_report` — a `}` before the first
`{` in LLM subagent text sliced `&trimmed[start..=end]` with
`start > end` and panicked mid-wave.
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

### 1. CLOSED — full-workspace gate at `a2eb27b` (`74d8ef7`); re-verified at `a23bce7`, `47cc41a`, `5a005a9`, `98e129b`, `90e8126`, `7f6cfd5`, `dac5844`, `1ce1bb6`, `31bd1a5`, and `45aec66`

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

**Re-verification at `45aec66` (after the `parse_report` panic fix).**
Ran the same command at `45aec66`:

    4463 tests run: 4463 passed, 5 skipped

Up from 4459 at `31bd1a5` — exactly the four new `kod-swarm`
properties. Clean first try: no execfail, no EAGAIN, no leaky.

**Re-verification at `31bd1a5` (after the MCP origin-parser fix).**
Ran the same command at `31bd1a5`:

    4459 tests run: 4459 passed, 5 skipped

Up from 4455 at `1ce1bb6` — exactly the four new `kod-mcp`
properties. Clean first try: no execfail, no EAGAIN, no leaky.

**Re-verification at `1ce1bb6` (after the `parse_reply` proptests).**
Ran the same command at `1ce1bb6`:

    4455 tests run: 4455 passed, 5 skipped

Up from 4450 at `dac5844` — exactly the five new `kod-memory`
properties. Clean first try: no execfail, no EAGAIN, no leaky.

**Re-verification at `dac5844` (after the Anthropic SSE overflow fix).**
Ran the same command at `dac5844`:

    4450 tests run: 4450 passed, 5 skipped

Up from 4445 at `7f6cfd5` — exactly the five new proptests in
`kod-provider-anthropic/wire.rs`. Clean first try: no execfail,
no EAGAIN, no leaky.

**Re-verification at `7f6cfd5` (after the streaming E2E test).**
First attempt at `7f6cfd5` failed at link time with
`errno=28 (No space left on device)`, then fork failures
cascaded. That is not a code failure (L40: the build cache had
grown to 45 GiB of physical store against a documented cap it
was not applying, and its auto-gc daemon was silently unable to
evict). `kache gc --max-age 3d` reclaimed 14 GiB and the
re-gate was clean:

    4445 tests run: 4445 passed (1 leaky), 5 skipped

Up from 4444 at `90e8126` — exactly the new streaming test. The
`leaky` annotation is a nextest resource-leak warning inside the
test, not a failure; the test passes.

**Re-verification at `90e8126` (after the `serve_io` extraction + E2E test).**
Ran the same command at `90e8126`:

    4444 tests run: 4444 passed, 5 skipped

Up from 4443 at `98e129b` — exactly the new E2E panic-guard test.
Clean on the first try this time: no execfail, no EAGAIN. The
prior gate at `98e129b` had one `execfail` (`os error 35`) that
reran green at `--test-threads 1`, recorded in the previous
subsection.

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

`main` at `45aec66`, twenty-five commits past `v0.1.12`:

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
| `ad05072` | docs(handoff): gate at `98e129b`; bug-hunt section; L37 |
| `b38b7c2` | refactor(serve): extract `serve_io`; log abnormal connection termination |
| `7252f61` | chore(deps): Cargo.lock companion for `b38b7c2` |
| `90e8126` | test(serve): E2E panic-guard through `serve_io` |
| `5f1f550` | docs(handoff): gate at `90e8126`; followups closed |
| `7f6cfd5` | test(serve): streaming E2E + finding: only `process` was wedged |
| `34fc35c` | docs(handoff): gate at `7f6cfd5`; scope correction + cache lesson |
| `dac5844` | fix(anthropic): saturating_add on SSE usage counters; proptests for `parse_sse_line` |
| `aa03e3c` | docs(handoff): gate at `dac5844`; SSE overflow fix + L41/L42 |
| `1ce1bb6` | test(memory): proptests for `parse_reply` (5 properties, 2 controls) |
| `82ee974` | docs(handoff): gate at `1ce1bb6`; `parse_reply` proptests + L43 |
| `31bd1a5` | fix(mcp): use `url::Url` for `Origin::parse`; proptests for the redirect policy |
| `5f03d2c` | docs(handoff): gate at `31bd1a5`; MCP origin fix + L44 |
| `45aec66` | fix(swarm): guard `start <= end` in `parse_report`; proptests for the report parser |

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

**Followups closed (b38b7c2, 90e8126).** Two items named in the
fix's own handoff were done in the same session, not deferred:

1. **`serve_io` extraction.** `handle_connection` was a concrete
   function taking `transport::Stream`; a duplex-driven test could
   not reach it. `b38b7c2` extracted the dispatch loop into
   `serve_io<R, W>(engine, reader, writer, shutdown)`, matching the
   shape `acp.rs::serve_io` already uses. `handle_connection` is
   now a five-line wrapper. No behavior change: the existing
   75/75 tests passed after the extraction.

2. **E2E test.** `90e8126` added
   `process_request_with_panicking_provider_gets_error_frame`: a
   `PanickingProvider` (panics in `generate_with_tools`) installed
   on a real engine, one `process` request driven through
   `serve_io` over a duplex pair, asserting an error frame for
   that request id arrives within 5s. On the pre-fix shape the
   caller would wait until its own timeout. This closes the
   honest-scope caveat the helper-only test carried; the helper
   test remains as the smaller, faster isolation of the fix.

Both positive controls confirmed: reverting the helper body to
`let _handle = tokio::spawn(fut); let _ = (out, id);` fails both
tests; restoring re-greens. The E2E failure is
"EOF on client side; the guard did not send a frame" — the exact
wedge the fix addresses, and the same symptom the original
analysis predicted from reading the code.

**L38, in the followup's own shape.** The honest-scope caveat said
a duplex test "would need the same `serve_io` extraction `acp.rs`
got". That was true when written. It was a ~40-line mechanical
refactor (substitute reader/writer for the concrete halves; keep
the body). The lesson: before scoping a test down, check whether
the enabling refactor is actually small. An honest caveat is
honest only about the code as it stands; a followup that reads
the extraction target and finds it mechanical should do the
extraction, not inherit the caveat.

**Companion-file miss, L29 again.** `b38b7c2` added three new
direct dev-deps to `kod-core-serve`; the `git add` listed two
files and `Cargo.lock` was left behind (`7252f61` commits it).
Same shape as `8b4e7f5` earlier in the session: staging a subset
of the change leaves the companion. The completion check is
"`git status --porcelain` is empty", not "the tests pass". The
final commit in this arc (`90e8126`) staged by directory
(`git add crates/kod-core-serve/ Cargo.lock`) instead of by file
name; that is the shape that would have avoided both misses.

**A correction to the fix's own scope (L39).** The `98e129b`
commit message said "three sites wired through a helper," which
reads as "three bugs of the same class fixed." Reading the code
again while writing the streaming test showed that is not true:

- `process`: calls `engine.process_for(...)` directly. Panic
  unwinds past the outer `match ... Err(e)` arm. That was the bug.

- `process_streaming`: `run_streaming` (line 763) already wraps
  `process_streaming_for` in `let call = tokio::spawn(...)` with
  `call.await.map_err(|e| KodError::Internal(format!("engine task
  panicked: {e}")))`. A panic in the engine returned `Err` from
  `run_streaming` and the outer dispatch arm sent an error frame.
  Not wedged.

- `swarm`: `run_swarm` has the identical inner-spawn + map_err
  shape. Not wedged.

The helper at those two sites is **defense-in-depth** (it would
catch a panic in `run_streaming` / `run_swarm`'s own body, not in
the engine they call), which is a legitimate reason to keep it,
but not the same reason as `process`. `7f6cfd5` adds the
streaming E2E test and the empirical evidence: reverting the
helper to the pre-fix shape fails `panicking_task_produces_an_error_frame`
and `process_request_with_panicking_provider_gets_error_frame`,
but `streaming_request_with_panicking_provider_gets_error_frame`
**passes** — exactly because `run_streaming` was already safe.

The uniform helper is not wrong. The commit message that implied
the same bug at three sites was.

**Third round: the SSE parser overflow (`dac5844`).** After the
serve fix and its followups closed, a survey of test density
across the workspace found `kod-provider-anthropic/src/wire.rs`:
2182 lines, 30 example tests, **zero proptests** — the largest
untested external-boundary parser in the tree. `parse_sse_line`
is the byte-level entry for every frame of the Anthropic streaming
API; a panic there kills the turn, same as the four fixes that
opened this arc.

Five proptests were written before any fix:

- `prop_parse_sse_line_never_panics` — boundary contract
- `prop_blocks_only_grow` — state monotone in `blocks.len`
- `prop_finished_is_sticky` — no second `Done` to a client
- `prop_stream_error_is_sticky` — TPD-01 mid-stream error not lost
- `prop_adversarial_message_start_does_not_overflow` — failing-first

The fifth was written to fail: `message_start` does `state.input_tokens
= base + cache_read + cache_creation`, three `u64`s cast to `usize`
and summed without a guard. Test/dev profile has `overflow-checks`
on by default, so wire values near `u64::MAX` panic on the addition.
The property was written and confirmed red before the fix landed.

**The fix** (`dac5844`): `saturating_add` at both addition sites
(`message_start` three-way, `message_delta` two-way). Saturating
is the correct semantic for token counters — a value at
`usize::MAX` already means the wire carried garbage; clamping
beats aborting the turn.

**Two independent positive controls** (L27): reverting only the
`message_start` fix fails the property; reverting only the
`message_delta` fix fails it too. The property exercises both
`u64::MAX` cases in one run, so a re-read of the failure message
tells which site tripped.

**A small detour on the way in (L41):** the first `#[derive(Debug)]`
insertion produced a duplicate-attribute error because
`AnthropicStreamState` and `BlockInfo` already carried
`#[derive(Default)]`. The correct edit is in place
(`#[derive(Default)]` -> `#[derive(Default, Debug)]`), not a second
attribute above it. proptest needs `Debug` on the strategy tuple
types for failure reporting; a struct with only `Default` will
not compile a proptest that takes it as an input.

**L42, in the survey's own shape.** The prior handoff closed item
#2 after three custom-parsing modules. The survey this round found
a fourth — `wire.rs` — with a real bug in it. "The proptest
surface is closed" is a claim about the *codebase*, not about the
last three targets. The next same-class target, named under
"Where to start" item 5, is `kod-memory/src/extract.rs`
`parse_reply` — parses LLM output, zero proptests.

**Fourth round: the memory extraction parser (`1ce1bb6`).** The
next target from the L42 survey: `kod-memory/src/extract.rs`
`parse_reply`, which parses the LLM's reply for an extraction
call. Prose that *may* contain one JSON array; 270 lines total;
five example tests; zero proptests.

Five properties were written:

- `prop_parse_reply_never_panics` — boundary contract
- `prop_returned_facts_are_well_formed` — every returned fact
  has non-empty trimmed content and `>= 12` bytes
- `prop_cap_holds` — `parse_reply(r, n).len() <= n`
- `prop_prefix_stable_across_caps` — for `n1 <= n2`,
  `parse_reply(r, n1)` is a prefix of `parse_reply(r, n2)`
- `prop_valid_reply_is_fully_parsed` — a well-formed reply with
  `k <= n` entries round-trips with `k` facts, in order,
  kind-preserved

The parser is defensive; all five passed on the first try once
the strategy was correct. Two checker bugs surfaced during the
control drill — the properties were fine, the strategy was not.

**Checker bug 1 (L23, second appearance this session).** The
first content strategy used `[a-zA-Z0-9 ]{12,40}` (spaces
allowed). `parse_reply` trims before the `>= 12` check and
before storing, so a 12-byte raw value with a leading space
became an 11-byte trimmed value and was *correctly* dropped —
then the expected-value comparison (against the untrimmed
string) failed on a correct parser. `parse_reply` was never at
fault. Fixed by `[a-zA-Z0-9]{12,40}` (no whitespace).

**Checker bug 2 (L43, new).** After the first fix, control B
(remove the `if content.len() < 12` filter) passed all five
properties. The cause: the strategy only generated 12-40-byte
content, so the filter **never fired** on the corpus — the
edge was present in the type domain but never sampled. This is
L35's companion, on the guard's own invariant: a strategy that
*can* generate the edge but almost never *does* is vacuous
against a control that targets that edge. Fixed by an
`arb_content` strategy that `prop_oneof!`-forces `"x"`,
`"hello!"`, and `"12345678901"` (11 bytes, one short) into
every 2-case window.

**Both controls now fail the intended property:**

- Reversing `arr.iter()` -> `arr.iter().rev()` fails
  `prop_valid_reply_is_fully_parsed` on the entry order.
- Removing the `if content.len() < 12` filter fails
  `prop_returned_facts_are_well_formed` on the `>= 12`
  invariant (the short-content branch now appears in the
  corpus).

The honest shape: five properties, two checker bugs, zero
parser bugs. The parser was correct; the test harness needed a
second revision before it could prove that. That ratio is the
same one the handoff has been naming since L23 — in four
proptest crates and ~20 rounds, most failures are in the check,
not the code.

**Fifth round: the MCP redirect origin parser (`31bd1a5`).** The
next target from the L42 survey: `kod-mcp/src/http_policy.rs`.
305 lines, 17 example tests, zero proptests. `Origin::parse` is a
**hand-rolled** URL parser whose result `decide_redirect` uses to
decide whether to forward the configured auth header across a
redirect. `reqwest` — the client actually following the redirect
— uses the WHATWG parser (the `url` crate). Any disagreement
between the two matters.

Four proptests, all taking `url::Url::parse` as the reference:

- `prop_never_panics` — boundary contract
- `prop_rejects_what_url_rejects` — over-acceptance is the
  unsafe direction
- `prop_agrees_with_url_crate` — for inputs both accept, the
  `(scheme, host, effective_port)` triples must match
- `prop_same_as_implies_url_same_origin` — if `same_as` is true,
  the WHATWG origins must be equal

**Two were red on the pre-fix code**, minimal cases in the
forced corpus:

- `https://example.com:abc/x` — non-numeric port treated as part
  of the host; WHATWG rejects.
- `https:///x` — empty host accepted; WHATWG rejects.
- `https://evil.com\@example.com/` — `\` not treated as `/` for
  special schemes; the two parsers disagree on the host
  (`evil.com` from WHATWG, `example.com` from the hand-rolled
  parser). `same_as` on this pair returns **true**, so
  `decide_redirect` would attach the configured auth header to
  a request the wire client resolves to `evil.com`.

**The fix:** delegate `Origin::parse` to `url::Url::parse`. The
same parser reqwest uses; any disagreement *is* the bug.
`Url::port()` already normalizes the scheme's default port to
`None`, so `effective_port()` is a no-op for this constructor;
the struct's `pub` fields are unchanged, and an external
`Origin { port: Some(443), .. }` still behaves correctly.
`url = "2"` moves from `[dev-dependencies]` to
`[dependencies]` on `kod-mcp` (already in the lockfile
transitively via reqwest, so the lock change is a per-package
list update).

**Two independent controls (L27):**

- Control A: revert `Origin::parse` to the hand-rolled version.
  `prop_agrees_with_url_crate` and `prop_rejects_what_url_rejects`
  both fail.
- Control B: `same_as` returns `true` unconditionally.
  `prop_same_as_implies_url_same_origin` fails.

**L44 — the pair-property lesson.** `prop_same_as_implies_url_same_origin`
**passed** on the pre-fix code, even though the underlying bug
(disagreement on `evil.com\@example.com`) was present. The
property draws `a` and `b` independently from the corpus; the
specific adversarial pair is one out of ~2^128 possible pairs
and was never sampled. The property is *semantically correct*
and *pragmatically vacuous* on the corpus, and its teeth are
proved by control B — the direct mutation of `same_as` — not
by the corpus. Pair properties need either a derived second
side (mutate `a` to get `b`), or a direct control. The control
is the proof; the corpus is not.

**Net for the round:** two real over-acceptances, one real
host-disagreement (the security-relevant one), four properties
with two controls, and a lesson about the *shape* of the
property rather than its strategy. That is the most
productive surface a single round has found in this arc.

**Sixth round: the swarm report parser (`45aec66`).** The next
target from the L42 survey: `kod-swarm/src/brief.rs::parse_report`,
which parses the LLM subagent's final text into a
`SubagentReport`. Model-authored prose that *may* contain a JSON
object; the parser tries a fenced block, then a `{...}` slice,
then falls back to the whole text as the summary.

**One real panic, in the second branch.** The slice
`&trimmed[start..=end]` had no `start <= end` guard. An input
with a `}` before the first `{` — e.g. `"a}b{c"` — produced
`&trimmed[3..=1]` = `&trimmed[3..2]` and panicked with
`byte range starts at 3 but ends at 2`. Reachable from model
text: a subagent's closing prose with a `}` before the JSON
block would panic the swarm runner mid-wave (call site:
`kod-core/src/swarm_runner.rs:1832`). Same class as the four
panic-isolation fixes that opened this arc, and the serve
panic-guard: a panic in a boundary parser kills the turn.

The minimal failing case was forced into the corpus with
`Just("a}b{c")` (L43), because a purely random `any::<String>()`
corpus almost never produces that exact shape. Proptest shrank
to the same input on the first run; no further reduction needed.

**The fix:** `&& start <= end` in the second branch's `if let`
chain. A `}` before the first `{` cannot be a well-formed JSON
object anyway, so skipping the branch is correct — the fallback
summary is what the caller gets for a non-JSON reply.

**Four proptests added:**

- `prop_parse_report_never_panics` — the failing-first red
- `prop_valid_report_round_trips` — a well-formed report
  serializes through `serde_json` and parses back identically
  (required adding `PartialEq, Eq` to `SubagentReport`;
  non-behavioural)
- `prop_fallback_preserves_trimmed_input` — with no `{` in the
  input, the fallback preserves the trimmed text in `summary`
  and defaults every other field
- `prop_fenced_block_strips_to_bare_json` — a fenced JSON block
  parses identically to the bare JSON

**Two independent controls (L27):**

- Control A: revert `&& start <= end`.
  `prop_parse_report_never_panics` fails on `"a}b{c"`.
- Control B: overwrite the parsed report with a `CONTROL`
  summary. `prop_valid_report_round_trips` fails.

**L45 — the relational-property lesson.** Control B was designed
to target the fenced-block stripping, and I expected
`prop_fenced_block_strips_to_bare_json` to catch it. It did not:
the mutation replaced the *result* of the JSON branch with a
fixed value, so both the bare parse and the fenced parse returned
the same fixed value, and the relational comparison passed. The
property that caught it was `prop_valid_report_round_trips`,
which compares against `serde_json::to_string` — something
outside the pair. A relational property ("A is identical to B")
needs a companion that pins *one* side to external truth; without
it, any mutation that affects both sides equally is invisible.

The honest shape: one real panic, four properties, two controls,
and a lesson about relational properties. That is the second
round in a row where the *control* found something the property
structure did not — L43 (strategy blind to edge), L44 (pair
vacuous on unsampled pair), L45 (relational blind to symmetric
mutation). The control drill keeps producing lessons that are
about the shape of the check, not the shape of the code.


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

## The lesson index (L20–L45)

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
| L38 | An honest-scope caveat is honest only about the current code. "Would need the same `serve_io` extraction `acp.rs` got" was correct on the day it was written and wrong a day later — the extraction was a ~40-line refactor with no behavior change. Before scoping a test down, check whether the enabling refactor is actually small. |
| L39 | A commit message that says "three sites wired through a helper" implies three bugs. Read the code at each site before claiming the fix is the same at each. In `serve.rs`, only `process` was wedged; `run_streaming` and `run_swarm` already had the inner-spawn guard. The uniform helper was defense-in-depth, not the fix for the same bug three times. |
| L40 | A build cache can silently fail in two ways at once. (a) kache's `kache stats` header reports logical-vs-physical sizes and a cap that does not match `~/.config/kache/config.toml`'s `max_cache_size`; `kache gc` with no args is a no-op if physical size is under that (wrong) cap. (b) `auto-gc.log` had hundreds of `skipping eviction ... database is locked` lines, and `gc_stats.json` showed `entries_evicted: 0` — the daemon's LRU was silently broken for at least a day. `kache gc --max-age 3d` reclaimed 14 GiB. Check `kache stats`, `auto-gc.log`, and `gc_stats.json` when a disk-full recurs. |
| L41 | proptest's failure reporting needs `Debug` on every type in the strategy tuple. A struct with only `#[derive(Default)]` needs `Debug` added alongside it — and the *first* `#[derive(Debug)]` insertion will collide with the existing derive unless you edit in place (`#[derive(Default)]` -> `#[derive(Default, Debug)]`), not add a second `#[derive(Debug)]` above it. |
| L42 | "The proptest surface is closed" is a claim about the *codebase*, not about the last three targets. The prior handoff closed item #2 after covering three custom-parsing modules; the survey this round found `parse_sse_line` in `kod-provider-anthropic` — a 2182-line external-boundary parser with 30 example tests and zero proptests, and a real overflow bug in it. Before declaring a surface done, list every parser on a crate boundary and check each one. |
| L43 | A proptest's strategy can be non-vacuous and still blind to a control. When the drill reverted the `>= 12` content-length filter and the property still passed, the cause was the strategy: it only generated 12–40-byte content, so the filter never fired. Force every edge the code branches on into the corpus with `prop_oneof![Just(edge), range]`. L35's companion — same failure shape, on the guard's own invariant. |
| L44 | A **pair** property can be semantically correct and pragmatically vacuous. `prop_same_as_implies_url_same_origin` draws two URLs independently; the specific adversarial pair (e.g. `evil\@example.com` vs `example.com`) is one out of 2^128 possible pairs and is never sampled. Pair properties need either a derived second side (mutate `a` to get `b`), or a direct control that forces `same_as` to return the wrong answer. Proving a pair property has teeth is the same shape as proving a unit property has teeth — the control is the proof, not the corpus. |
| L45 | A **relational** property ("A parses identically to B") is blind to any mutation that changes both sides identically. `prop_fenced_block_strips_to_bare_json` compares the fenced parse to the bare parse; a control that overwrote both with the same wrong value would pass it. The property that anchors to *external truth* — `prop_valid_report_round_trips` comparing against `serde_json::to_string` — is what caught the mutation. Relational properties need a companion that pins one side to something outside the pair. |

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
   at `45aec66`. Nothing this session started is unfinished.
   Known gaps: (a) kache's auto-gc daemon was not evicting (L40),
   check `kache stats` and `auto-gc.log` if disk-full recurs.
   (b) Remaining same-class parsers from the survey (L42), none
   with a real bug found yet: `kod-skills/parser.rs`,
   `kod-swarm/completion_report.rs`, `kod-types/effort.rs`.
   Each is a small read-then-write round; none is urgent.

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
classes, a test-surface class, two open items, six more bugs
found by extended hunting, and one security-relevant
disagreement: a panicking third party no longer kills a turn
(four fixes, each failing-first), a char-named truncator no
longer miscounts CJK (fourteen sites, one helper, one
workspace lint), doc-claimed contracts in five crates —
kod-schema-dialect, kod-error, kod-ast, kod-minimize, and
kod-config — are property-tested with positive controls, the
two previously-uncovered live tests run in CI on every push to
main/develop, a panicking spawned engine task in the NDJSON
serve adapter now sends an error frame instead of leaving the
caller to time out (same class as the ACP fix in aa21bf8), a
wire-input `usize` overflow in the Anthropic SSE parser
(`dac5844`) is now saturated, the memory extraction parser
(`parse_reply`, `1ce1bb6`) is pinned by five properties with
two controls that caught checker bugs (L23, L43), the MCP
redirect's `Origin::parse` (`31bd1a5`) now delegates to
`url::Url` — the hand-rolled parser accepted URLs reqwest
rejects, including one where the two parsers disagreed on the
host, which `same_as` would have reported as same-origin and
forwarded the auth header to the wrong host — and the swarm
report parser (`parse_report`, `45aec66`) now guards
`start <= end` before slicing, fixing a panic on LLM text with
a `}` before the first `{`. The honest-scope caveat from the
serve fix was closed by a `serve_io` extraction (`b38b7c2`)
and an end-to-end test (`90e8126`). A follow-up test
(`7f6cfd5`) verified the finding that only the `process` site
was ever wedged (L39). Twenty-six handoff lessons (L20–L45)
that are mostly about the *checker* being wrong, not the code;
two (L40) came from a build cache that failed silently, two
(L41, L42) came from a parser survey that found a real
overflow the prior "surface closed" claim missed, one (L43)
came from a control that ran blind to the edge the guard
branches on, one (L44) came from a **pair** property whose
adversarial pair was never sampled, and one (L45) came from a
**relational** property that changed both sides identically
under the control. The full workspace gate at `45aec66` runs
4463 tests, all green, including twenty-seven new properties
across seven modules (`kod-config/config.rs`,
`kod-config/policy.rs`, `kod-config/llm.rs`,
`kod-provider-anthropic/wire.rs`, `kod-memory/extract.rs`,
`kod-mcp/http_policy.rs`, `kod-swarm/brief.rs`).
