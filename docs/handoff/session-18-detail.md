# Session 18 — detail

The *detail* companion to `session-18.md`. Read the starting
document first; come here when you need the full story of a
lesson, a fix, or a gate run.

## Contents

- Gate re-verification history — every workspace gate from
  `a23bce7` through `fca3c8b`
- Bug hunt — sixteen rounds of bug finding, from the
  panicking serve task through the retry-classifier
  digit-substring fix (see the numbering note at the top of
  that section)
- What landed before `v0.1.12` in this arc
- Operational quirks (watcher, heredoc limits, Bash 3.2,
  kache disk pressure, `--` in `git commit -m`)
- The lesson index (L20–L52)
- What NOT to do — guardrails for the next session
- Note on `session-17.md` — mixed-record state

---


## Gate re-verification history

### Every gate run, `a23bce7` through `fca3c8b`

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

**Re-verification at `fca3c8b` (after the classify boundary fix).**
Ran the same command at `fca3c8b`:

    4503 tests run: 4503 passed, 5 skipped

Up from 4500 at `56fed90` — exactly the three new `kod-core-routing`
properties. Clean first try: no execfail, no EAGAIN, no leaky.

**Re-verification at `56fed90` (after the preflight-parser fix).**
Ran the same command at `56fed90`:

    4500 tests run: 4500 passed, 5 skipped

Up from 4495 at `767e280` — exactly the five new `kod-core-quality`
properties. Clean first try: no execfail, no EAGAIN, no leaky.

**Re-verification at `767e280` (after the risk-classifier fix).**
Ran the same command at `767e280`:

    4495 tests run: 4495 passed, 5 skipped

Clean first try *when no other cargo/rustc was running on the host*:
140s wall, 125s test time, no execfail, no EAGAIN, no leaky.

**Three failed attempts preceded it — the diagnosis was wrong.**
Attempt 1 was SIGKILLed (exit 137) at 458s during the criterion /
plotters / axum dev-deps compile; `target/` had just been deleted to
free disk, so the rebuild started from scratch. Attempt 2 (resumed
from 5 GiB of built `target/`) was SIGKILLed at 93s at the same
dev-deps. Attempt 3 hit a kache-served `kod_ast` exit — the killed
rustc had left a corrupt cache entry; a re-run cleared it.

The first pass diagnosed OOM on this 16 GiB host and fell back to
per-crate verification: **1476 tests across 5 crates** (kod-risk 74,
kod-config 246, kod-core-tools 84, kod-tools 441, kod-core 631),
all passing. That was correct but insufficient — it proved the
change's blast radius, not the whole workspace.

**The user reported the SIGKILLs were another script's fault, not
OOM.** A fourth attempt, with no other cargo/rustc running, passed
in 140s. The pass is the honest record. L51 names the lesson: a
SIGKILL during a build is not the build's fault until you check for
a concurrent process.

**Re-verification at `88ca5f7` (after the zero-position parser fix).**
Ran the same command at `88ca5f7`:

    4493 tests run: 4493 passed, 5 skipped

Up from 4488 at `016c43b` — exactly the five new `kod-tools`
tests. Clean first try: no execfail, no EAGAIN, no leaky.

**Commit message count note.** The `88ca5f7` message says
"444/444 kod-tools (439 prior + 5 new)"; the crate suite actually
ran 441 passed + 3 skipped = 444 total. The "prior" count in the
message was slightly off (440 not 439) — the gate's actual count
(4493) is correct. Minor; recorded here so a future count-audit
does not chase a ghost.

**Re-verification at `016c43b` (after the jitter-seed fix).**
Ran the same command at `016c43b`:

    4488 tests run: 4488 passed, 5 skipped

Clean first try: no execfail, no EAGAIN, no leaky.

**Count correction.** The `1231a4e` commit (whitespace boundary
before attribute names + 6 proptests) was gated at 4487 tests
and *failed* on one flake — `kod-provider retry::tests::jitter_is_centered_and_signed`.
That gate never got a clean pass. The `016c43b` commit fixed the
underlying bug and added 1 test, so this gate covers both rounds:
4488 = 4481 (`4600d40`) + 6 (`1231a4e`) + 1 (`016c43b`). The
"expected 4482" the gate script printed was my arithmetic error
— I forgot the six tests from the extract_attribute round. The
gate's actual count (4488) is correct and matches the test
progression; the "expected" warning is stale.

**Re-verification at `4600d40` (after the section-extraction
properties).** Ran the same command at `4600d40`:

    4481 tests run: 4481 passed, 5 skipped

Up from 4476 at `af0301f` — exactly the five new `kod-skills`
properties. Clean first try: no execfail, no EAGAIN, no leaky.

**Re-verification at `af0301f` (after the `completion_report` fix).**
Ran the same command at `af0301f`:

    4476 tests run: 4476 passed, 5 skipped

Up from 4470 at `02de4e8` — exactly the six new `kod-swarm`
properties. Clean first try: no execfail, no EAGAIN, no leaky.

**Re-verification at `02de4e8` (after the SKILL.md proptests).**
Ran the same command at `02de4e8`:

    4470 tests run: 4470 passed, 5 skipped

Up from 4463 at `45aec66` — exactly the seven new `kod-skills`
properties. Clean first try: no execfail, no EAGAIN, no leaky.

**Count correction.** The `02de4e8` commit message says "89/89
kod-skills (82 prior + 7 new)". That count was wrong — I wrote
it from an earlier estimate. The crate suite actually ran
116/116 (109 prior + 7 new). The gate-level count above (4463 →
4470, +7) is correct and is the number that matters.

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


## Bug hunt — sixteen rounds, 9 production bugs

**Numbering note.** Rounds are numbered as they were written, not
renumbered after later edits. Two consequences:

- **First** and **Second** were never explicitly labelled — the
  section header above (originally "Bug hunt — the panicking
  serve task") stood in for round one, and the follow-up
  (`serve_io` extraction + E2E test) for round two. The narrative
  in that section covers both.

- **Twelfth** was a duplicate of **Tenth** (same commit `1231a4e`,
  same text) and was removed during the split. There is no round
  numbered 12 below; rounds go Third → … → Eleventh →
  Thirteenth → … → Sixteenth.

The commit hash is the load-bearing identifier, not the ordinal.

---

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

**Seventh round: the SKILL.md parser (`02de4e8`).** The next
target from the L42 survey: `kod-skills/src/parser.rs::parse_content`,
which splits YAML front matter from a markdown body, deserializes
the metadata, and extracts sections and `<example>` blocks. 464
lines, 15 example tests, zero proptests.

**No parser bug found.** The `split_front_matter` /
`extract_section` / `extract_examples` arithmetic was hand-checked
first: each slice is preceded by a `find` whose result bounds the
slice, so no `start > end` panic exists here (unlike
`parse_report` at `45aec66`). The seven properties are a
regression guard, not a fix.

**Two checker bugs caught during the write (L23 twice, same
shape, in one round).** Both are L46:

1. The test's `build` helper interpolated name/description
   unquoted into the front matter. A description of `" "` (one
   space) becomes YAML's `""` after normalization;
   `validate_metadata` correctly rejects it; the test's
   `expect("must parse")` panicked on five properties at once.
   Fixed by serializing through `serde_yaml::to_string`.

2. The example output strategy allowed whitespace-only strings
   (`"[a-zA-Z0-9 ]{1,40}"`). A value of `" "` gets trimmed to
   `""` by `extract_examples`'s `content.trim()`; the assertion
   compared `""` (actual) to `" "` (expected). Fixed by
   dropping spaces from the output strategy.

Neither was a parser bug; the parser correctly trims and
correctly rejects empty descriptions. Same root cause as the
`parse_reply` round (`1ce1bb6`) — the test input was not what
the assertion assumed. L46 names the rule: the strategy must not
produce a value the code's own trim/normalize step changes into
something the assertion treats as different.

**Two positive controls (L27):**

- Control A: `find("<example")` -> `find("<exampleXX")`.
  `prop_example_blocks_are_captured_in_order` fails.
- Control B: `starts_with("---")` -> `starts_with("__x__")`.
  Five of seven properties fail (front matter no longer found).

**Not covered, named honestly:** none of the seven properties
exercises `## Instructions` / `## Constraints` extraction. That
extractor is a small `find`-and-slice with no arithmetic hazard
and no security relevance; a future round can add a property if
it becomes worth the surface.

**Eighth round: the completion-report parser (`af0301f`).** The
next target from the L42 survey: `kod-swarm/src/completion_report.rs::parse`,
which reads the LLM subagent's final message as a tag-delimited
report (`<completion-report status=done>...`). Called from
`kod-core/src/swarm_runner.rs:1850` and `kod-core/src/overnight.rs:176`.

**One real bug, in `extract_status`.** The pre-fix code did
`open_line.find("status")` — a substring match. An attribute
like `xstatus=done`, `some=status=done`, or `my-status=done`
matched as if it were a real `status=` attribute, so an agent
whose tag carried a descriptive word ending in "status"
reported `Done` even though it never named a status. Same class
as the `parse_report` panic (`45aec66`): a boundary parser
accepting input it should reject. The failing-first property
(`prop_fake_status_attribute_is_unknown`) was written and run
against the pre-fix code and confirmed red on `xstatus=done`.

The fix: require a whitespace boundary before `status`.
Attributes are whitespace-separated; "preceded by whitespace"
is the property the parser actually wants. The loop keeps
searching past a non-boundary match, so a tag like
`<completion-report foo=xstatus=bar status=done>` still parses
correctly (the real attribute is found second).

**Six properties:** never-panics, valid-block round-trip,
absent-block fallback, real-status positive control,
fake-status failing-first, `had_block` matches the open prefix.

**The L46 drill, three times in one round.** The round-trip
property's strategies first admitted whitespace-only strings
for `summary`, `validation`, `f1`, `f2`. Each produced a
different manifestation of the same rule: `summary = " "` —
`parse` keeps the untrimmed string, the expected value
`.trim()`s to `""`; `validation = " "` — `parse` correctly
treats empty validation as `None`; `f1 = " "` — `parse`
correctly drops the empty followup. All four strategies now
exclude whitespace. Across the session, L46 has now fired six
times in three rounds.

**A verification-tool bug, on the way in (L47).** The
structural check for `summary in "[a-zA-Z0-9]{1,40}"` used
`grep -qE` with `\[a-zA-Z0-9\]{1,40}` — in ERE the `{1,40}`
binds to the closing `]`, not the character class, so the
pattern can never match. The patch was correct; the check was
wrong. Fixed by `grep -qF` (fixed string). Same family as L23
but distinct: the check *logic* was fine, the *pattern* was
wrong.

**Two positive controls:**

- Control A: revert the whitespace-boundary fix.
  `prop_fake_status_attribute_is_unknown` fails on
  `xstatus=done`.
- Control B: break the `followups:` key match (`followupsXX:`).
  `prop_valid_block_round_trips` fails.

Both restored cleanly; suite re-confirmed green. One real bug,
six properties, two controls, and two lessons (L46's third
occurrence and the new L47).

**Ninth round: SKILL.md section extraction (`4600d40`).** Not a
new target — the gap `02de4e8` named honestly. Five properties
that pin `extract_section` through `parse_content`'s public
outputs (`Skill::instructions`, `Skill::constraints`), since
`extract_section` is private:

- `prop_instructions_section_does_not_leak_past_next_header`
- `prop_no_instructions_section_falls_back_to_body`
- `prop_constraints_section_is_extracted`
- `prop_no_constraints_section_yields_none`
- `prop_both_sections_extract_independently`

**No parser bug found.** The hand-analysis held: `find(header)`
then `find("\n## ")`; both bound their slices, neither can
overshoot. This is a regression-guard round, not a fix round —
the same honest shape as `1ce1bb6` (kod-memory) and half of the
`02de4e8` round.

**Two positive controls:**

- Control A: `extract_section(&body, "## Instructions")` ->
  `extract_section(&body, "## InstructionsXX")`. The lookup
  never matches, so the whole body is the instructions and the
  `## Notes` line leaks in.
  `prop_instructions_section_does_not_leak_past_next_header`
  fails; `prop_both_sections_extract_independently` fails too
  (bonus — it also asserts the instructions match the input).
- Control B: `find("\n## ")` -> `find("\n##NEVER")`. Same
  two properties fail for the same reason (boundary never
  matches).

Both restored cleanly; suite re-confirmed green. No new lesson
this round — the properties are a straightforward regression
guard and the controls confirmed they have teeth on the first
try. That is the honest shape: a round that adds coverage
without finding a bug, and without inventing a lesson to
justify itself.



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


## The lesson index (L20–L52)

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
| L46 | A strategy that admits whitespace-only strings passes a property that trims. `parse_content`'s example extractor trims the output; a strategy of `"[a-zA-Z0-9 ]{1,40}"` produced `" "`, which trimmed to `""`, and the assert compared `""` to `" "`. The same round's `build` helper hit the same shape on the YAML side: an unquoted `" "` normalizes to `""`, `validate_metadata` rejects it, and the test's `expect("must parse")` panics. Rule: the strategy must not produce a value the code's own trim/normalize step changes into something the assertion treats as different. Six occurrences across three rounds (`1ce1bb6` ×1, `02de4e8` ×2, `af0301f` ×3) — the class is real and the checklist rule is cheap. |
| L47 | When verifying a literal string that contains regex metacharacters, use `grep -F` (fixed string), not `grep -E` with hand-escaped metacharacters. A pattern like `'[a-zA-Z0-9]{1,40}'` in ERE binds the `{1,40}` to the closing `]`, not the class — the escaped pattern looks correct and can never match. The patch was right; the check was wrong. Same family as L23 but distinct: the check *logic* was fine, the *pattern* was wrong. |
| L48 | A flake is a test whose *assumption* fails, not a test that is wrong. `jitter_is_centered_and_signed` asserts `min < 900 and max > 1100` across 1000 samples; that cannot hold if all 1000 samples are identical. The identical samples were the bug (`jitter_for` seeded only from wall-clock nanos, which does not advance on fast hardware), not the test. Reduce the flake to a deterministic test (`jitter_for(12345)` twice) and it becomes a failing-first property for the fix. A flake that reduces deterministically is a bug report, not noise. |
| L49 | `proptest!` requires every `#[test]` inside it to be `fn name(args in strategy)`. A bare `#[test]` with no `in` clause fails to parse, and the compile error is reported at the block's *first* item, not the offending one — an error message pointing at property #1 that is actually property #4. A bare `#[test]` belongs at module level; doc comments on items inside `proptest!` are line comments (`//`) in this version, not `///`. |
| L50 | A security classifier's escalation level is part of its contract. The `kod-risk` module doc says "never refuses; escalates" — `Confirm`, not `Catastrophic`, for a construct the classifier cannot statically inspect. Escalating to `Catastrophic` (refused outright, no path through) would violate the contract even though it looks safer. The fix for the subshell-after-separator bug used `Confirm` because the module contract demands it, not because `Catastrophic` would have failed a test. |
| L51 | A SIGKILL (exit 137) during `cargo nextest run` is not the build's fault until you check for a concurrent process on the host. Two `767e280` gate attempts died at ~90s with `broken pipe` from a killed `rustc`; the first pass diagnosed OOM on a 16 GiB host and fell back to per-crate verification (1476 tests across 5 crates). A fourth attempt — after the user reported the kill was another script — passed clean at 4495/4495 in 140s. Before concluding a build failure is OOM, run `pgrep -fl 'cargo\|rustc\|nextest'` and check for a concurrent process. The per-crate fallback was correct but insufficient: it proves the change's blast radius, not the whole workspace. |
| L52 | Before fixing a bug a property found, check the call sites: is the failing shape reachable from a real caller? A property can construct input that production never produces; fixing that case hardens an unreachable path and ships a change for no reason. The `classify` digit-substring fix passed the check (`engine::control` classifies `e.to_string()`, which can contain a request id, attempt counter, or token count in the same string as a status). If it had failed the check, the honest response would have been to record the property as a regression guard and move on. Reachability is one grep: `grep -rn 'Fn::name\|fn_name(' crates/ --include='*.rs'`. |
| L53 | A per-developer tool committed as a project invariant only surfaces at release time. The tracked `.cargo/config.toml` set `rustc-wrapper = "kache"`; cargo reads it from every clean checkout, including CI, where kache is not installed. Local release builds pass because the tool is on the developer machine — the failure is invisible until a tag push fires the release workflow. **Test the release workflow on a dry-run tag (`v0.0.0-test`) before a real release**, and keep machine-specific tooling in the user-level config (`~/.cargo/config.toml`), not the repo. The v0.1.13 first attempt failed on all six targets; the fix was to remove the wrapper from the repo and set `RUSTC_WRAPPER: ""` in both workflows. |

| L54 | `cargo fmt -p <crate>` reformats the whole crate, not just your edit. A single edit to `kod-types/effort.rs` + `cargo fmt -p kod-types` pulled two pre-existing drift hunks in `strutil.rs` and `tests/it/source_lints.rs` into the diff — both reproduced by `rustfmt --check` on `git show HEAD:<file>`. Left in one commit, the test commit carries unrelated layout churn and the review has to separate "what this change did" from "what the formatter had queued". Prove the drift predates the edit (`rustfmt --check` on the HEAD copy), then commit it separately: `chore(crate): apply pending rustfmt drift`. |

| L55 | An ordering contract needs a *triple*, not a pair. Two branches co-existing in one input only proves precedence if each half is first shown to reach its own branch on its own — otherwise the concatenation could classify as the winner because *both* halves would have, and the test passes under any order. This is L45's lesson applied to `if`-chain precedence: write `assert(classify(a) == X)`, `assert(classify(b) == Y)`, `assert(classify(a + "; " + b) == X)`. The first two lines are load-bearing, not setup. The `classify` ordering tests use this shape for context-window-before-auth/rate-limit, server-busy-before-timeout/auth, and auth-before-timeout (BUG-C-10). |

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
- **Do not tag for test-only commits.** The `v0.1.13` premise
  ("everything since `v0.1.12` is tests and docs") was true when
  written and false by the end of the session: ten `fix:` commits
  landed, nine real production bugs, three security-relevant. The
  rule is *do not tag commits whose only value is tests*; it is
  not *never tag this session's work*. `v0.1.13` is a real
  release.
- **Do not re-build a covered seam.** Three times this session a
  "gap" was named from memory and reading the tests showed it
  covered. Read the test file first.

**Tenth round: the SKILL.md attribute fix (`1231a4e`).** The gap
`4600d40` named honestly — `extract_attribute` — turned out to
have the same bug shape as `completion_report::extract_status`
(`af0301f`): a substring match. `<example data-input="wrong">`
produced `input = "wrong"`; `<example xinput="wrong">` did too;
`<example data-input="wrong" input="A">` returned `"wrong"`
instead of `"A"`. `SkillExample.input` flows into the router's
prompt, so a fake attribute landed in the model's input.

Fixed by requiring a whitespace boundary before the attribute
name in both quote branches (double and single), looping past
non-boundary matches. Six proptests; two of them failing-first.
Two positive controls: revert the fix (both failing-first
properties fire); break only the single-quote branch
(`prop_single_quote_form_is_honoured` fires, the double-quote
properties stay green, so the two branches are covered
independently).

**A structural-check failure, caught during the write (L23 again).**
The first verification counted `preceded_ok` expecting 2; the
actual count was 4 — two per branch (a `let` and an `if`). The
patch was right; the check was wrong.

**Eleventh round: the jitter seed (`016c43b`).** The `1231a4e`
gate failed on a test I had not touched —
`kod-provider retry::tests::jitter_is_centered_and_signed`,
`sample min must be below base (some -jitter); got 997`. 5/5
standalone re-runs passed. That looked like a flake; it was a
bug.

**The reduction (L48).** The test asserts `min < 900 and max >
1100` across 1000 `delay_for(1)` calls. Those conditions cannot
both hold if all 1000 samples are identical — the assertion
therefore encodes "the samples differ", which is exactly what
jitter is supposed to guarantee. Reducing to a deterministic
form (`jitter_for(12345)` twice → differ) made the bug
reproducible: two calls returned the same `f64`, every time.

**Root cause.** `jitter_for` seeded from `nanos ^ tid ^ pid`. Both
`tid` and `pid` are constant within a test run, and
`SystemTime::now()` (microsecond resolution on macOS) does not
advance between back-to-back calls — so a 1000-call burst saw
one nanos, one seed, one jitter. The 997 ms min was the sample
value; it happened to fall below 900, but the max could not
rise above 1100 because every sample was the same 997.

**Why it matters in production, not just in tests.** Jitter
exists to spread concurrent retries and avoid thundering herds.
If every retry on a fast host computes the same jitter, the
herd is back — silently. The bug was making the backoff
deterministic on fast hardware, which is the exact shape the
T2-H11 fix (`8f801fb`) tried to address.

**The fix.** Mix a global monotonic counter into the seed:
`nanos ^ tid ^ pid ^ seq.wrapping_mul(0x9E37_79B9_7F4A_7C15)`.
Global (not per-instance) so two `RetryPolicy` values that share
a nanos do not collide; the golden-ratio multiplier spreads
adjacent counters across the u64 bit-space. `delay_for` was
refactored to call `Self::jitter_for(nanos)` so the seed
derivation is testable without a clock dependency.

**Failing-first (L27) and two controls:**

- `jitter_for_identical_nanos_differs_per_call` written and run
  against the pre-fix code: RED, both calls `-0.19256941508501768`.
- Control A: keep `SEQ` but drop the mix from the XOR — test
  fails. The counter is load-bearing.
- Control B: `seq.wrapping_mul(0)` — test fails. The multiplier
  itself is load-bearing.

**Verification that the flake is gone.** `jitter_is_centered_and_signed`
re-run 10 times back-to-back: 10/10 pass. With 1000 distinct
seeds, the probability that all 1000 land below 900 or all land
above 1100 is ~10⁻¹⁵⁵. The test is deterministic in practice
now; if it ever fires again, the fix itself regressed.

The honest shape: a flake reduced to a deterministic failing-
first test is a bug report, not noise (L48). A flake that does
not reduce deterministically, and that only reproduces under
host pressure (L21's `execfail` + `os error 35`), is a different
animal — that one is environmental. The distinction is whether
the failure survives a from-scratch reduction.


**Thirteenth round: the compiler-output zero-position fix
(`88ca5f7`).** `kod-tools/src/check.rs` parses external tool
output (cargo, tsc, ruff, go, `rustc`). Eight `parse_*_line`
free functions, 29 example tests, zero proptests — an
external boundary where a panic takes down the whole diagnostic
pass.

**The bug.** `.parse::<u32>()` accepts `"0"`, so a diagnostic
line like `a.rs:0:0: error: x` produced
`Diagnostic { line: 0, column: 0 }`. No compiler emits position
0; a downstream consumer doing `line - 1` for 0-based indexing
would underflow. Two independent paths accept zero:
`parse_location_colon` (shared by rustc and ruff) and
`parse_tsc_line` (its own `(line,col)` split). Both fixed with
an explicit `if line == 0 || col == 0 { return None; }`.

**The finding method matters.** `prop_diagnostic_positions_are_one_based`
(`text in any::<String>()`) **passed** on the pre-fix code. In
256 cases it did not sample a well-formed-looking line with
`0` — `any::<String>()` produces a diagnostic-shaped string so
rarely that the shape is unreachable. The
`zero_position_line_is_rejected` **direct probe** — a plain
`#[test]` with hand-picked inputs — found the bug immediately.
The property was not vacuous by construction; it was vacuous
by corpus. That is the same lesson L48's flake reduction
taught, in the opposite direction: a hand-picked input that
reaches the shape beats a property whose strategy cannot.

**Two positive controls:**

- Control A: revert the `parse_location_colon` guard.
  `zero_position_line_is_rejected` fails.
- Control B: revert *only* the `parse_tsc_line` guard.
  `zero_position_tsc_line_is_rejected` fails; the rustc probe
  stays green, so the two paths are covered independently.

**Three checker bugs during the write (L23, L46, L49).**

1. Round-trip strategy initially included `note`/`help`
   severities. The parser scopes to `error`/`warning` via
   `split_at_severity`; note/help correctly return `None`.
   Strategy's "well-formed" was wrong; parser was right (L23).

2. The message alphabet allowed messages that are only colons.
   `parse_severity_and_code` does `trim_start_matches(':')`, so
   a `":"` message collapses to empty and is correctly
   rejected. First char must be non-colon (L46 — strategy must
   not produce a value the code's trim step empties out).

3. The bare `#[test] zero_position_line_is_rejected` was first
   written inside `proptest!`. `proptest!` requires every
   `#[test]` inside it to be `fn name(args in strategy)`; a
   bare test fails to parse, and the compile error is reported
   at the block's *first* item, not the offending one — the
   message "expected expression, found keyword `fn`" pointed at
   the first property while the real problem was a bare test
   later in the block (L49). Also: `///` doc comments inside
   `proptest!` are not consumed by the macro in this version —
   `//` line comments are.

**Disk pressure note.** The gate ran with 25 GiB free (down
from 81 GiB after the first kache cleanup this session); the
run completed normally, no execfail, but the next gate should
`rm -rf target` first if free space drops below ~20 GiB. L21's
fork-pressure signal (`execfail` + `os error 35`) has not
recurred since the kache cleanup; the disk-full failure earlier
in the session was a different signal (`errno=28`).


**Fourteenth round: the risk-classifier subshell bug (`767e280`).**
The last major untested surface was `kod-risk/src/classify.rs` —
1903 lines of security classification where `RiskLevel::Safe`
runs a command immediately and `Catastrophic` is refused
outright. Prior coverage: 59 example tests, three properties
(`never_panics`, `expansion_in_argument_is_not_safe`,
`bare_ampersand_splits_segments`).

**The bug.** `echo hi; (rm -rf ~)` classified **Safe**. The
subshell after `;` runs `rm -rf ~` in a child shell, but the
tokenizer glues `(rm` into one token, which is not in
`DESTRUCTIVE`, so no path check runs. The whole-command
leading-`(` heuristic fires only when the raw text *starts*
with `(`; a subshell after `;`, `&&`, `||`, or `|` escapes
entirely. A false `Safe` on a literal `rm -rf ~` is the
highest-severity bug class this crate has.

**Found by a property.** `prop_subshell_after_separator_is_not_safe`
(`prefix in "[a-z]{1,10}"`, `sep in [";", "&&", "||", "|"]`)
generates `{prefix}{sep}(rm -rf ~)` and asserts the verdict
is not `Safe`. On the pre-fix code it failed on the first case
(`a ; (rm -rf ~)`). The three existing properties all passed —
none exercised the subshell shape after a *separator*.

**The fix.** After `split_segments`, iterate over segments
once and push a `Confirm` finding for each whose first token
begins with `(`. The module contract is "never refuses to run
something it does not understand; it escalates", so `Confirm`
is the correct level — a subsequent user policy gate decides
whether `(rm -rf ~)` runs. Escalating to `Catastrophic` would
refuse the command outright with no path through (L50).

**A second property, added alongside as an over-escalation
control.** `prop_echo_without_subshell_stays_safe` asserts
that `echo <args>` stays `Safe` or `Low`. If a future fix for
the subshell property escalates every command, this catches
it. It passes on the fixed code.

**Two positive controls:**

- Control A: remove the per-segment `(` block.
  `prop_subshell_after_separator_is_not_safe` fails.
- Control B: change the condition from `starts_with('(')` to
  `starts_with(')')` — the check exists but no segment can
  match it. Same property fails.

**The gate: three interrupted attempts, one clean pass.** After
the fix, the full-workspace gate was attempted three times and
interrupted each time (two SIGKILLs at ~90s and one poisoned
kache entry). The first pass diagnosed OOM on this 16 GiB host
and fell back to per-crate verification (1476 tests across 5
crates, all passing). The user reported the SIGKILLs were
another script's fault; a fourth attempt with no other
cargo/rustc running passed clean at 4495/4495 in 140s.

**The first-pass diagnosis was wrong, and the correction is
recorded.** The per-crate fallback proved the change's blast
radius, not the whole workspace; the handoff's `767e280`
verification now records the fourth, passing gate. L51 is the
lesson: a SIGKILL during a build is not the build's fault until
you check for a concurrent process.

**Net for the round:** one real security bug, two proptests,
two positive controls, and a 4495/4495 full-workspace gate.


**Fifteenth round: the host-preflight parsers (`56fed90`).**
The next untested surface from the survey:
`kod-core-quality/src/preflight.rs` — 232 lines, three `pub`
parsers (`parse_meminfo`, `parse_loadavg`, `parse_pmset`), 9
example tests, zero proptests. All three run at startup to
decide whether the host is under pressure.

**Two real bugs, both in `pub` parsers:**

1. `parse_loadavg` accepted `NaN` and `±inf`. `f64::parse`
   accepts `"NaN"`, `"inf"`, `"-inf"`, `"Infinity"`. A
   `NaN` load average silently failed the `load > cores`
   check in `pressure_reasons` — any comparison with `NaN`
   is false — so an under-pressure host was reported as
   safe. Fixed with `.filter(|v| v.is_finite())`.

2. `parse_meminfo` panicked on an out-of-range kB value.
   `kb * 1024` overflowed under `overflow-checks` (debug and
   test profiles); a hostile or corrupt `/proc/meminfo` with
   a huge `MemTotal:` aborted the startup-time probe. Fixed
   with `kb.saturating_mul(1024)` — saturation at `u64::MAX`
   is the correct answer for a byte count that cannot fit.

**Five proptests added:**

- `prop_parsers_never_panic` — all three parsers on arbitrary input
- `meminfo_huge_value_does_not_overflow` — direct probe of
  out-of-range kB values
- `loadavg_is_finite` — property over arbitrary input
- `loadavg_nan_and_inf_are_rejected` — direct probe of
  `"NaN"`, `"inf"`, `"-inf"`, `"Infinity"`, `"INF"`, and
  the braced forms
- `loadavg_parses_a_finite_number` — positive control

**Two failing-first, both were RED on the pre-fix code:**

- `loadavg_nan_and_inf_are_rejected` failed on
  `parse_loadavg("NaN") = Some(NaN)`.
- `meminfo_huge_value_does_not_overflow` failed with `attempt
  to multiply with overflow` at preflight.rs:75.

**Two positive controls:**

- Control A: revert `saturating_mul` to `kb * 1024`.
  `meminfo_huge_value_does_not_overflow` fails.
- Control B: revert the `.filter(is_finite)`.
  `loadavg_nan_and_inf_are_rejected` fails.

Both restored cleanly; suite re-confirmed green.

**The shape.** Both bugs are the same class the arc has been
finding all session: a boundary parser accepting input it
should reject (like `Origin::parse` at `31bd1a5`,
`extract_status` at `af0301f`, `extract_attribute` at
`1231a4e`) or doing arithmetic on it without a guard (like
the SSE overflow at `dac5844`, the `parse_report` panic at
`45aec66`). The preflight parsers read OS-reported text, not
model-authored text, but the contract is identical: reject
what you cannot interpret, do not abort on it.


**Sixteenth round: the retry-classifier digit-substring bug
(`fca3c8b`).** The next untested surface from the survey:
`kod-core-routing/src/retry_strategy.rs::classify` — 11
`l.contains("...")` checks that map provider error text to a
retry class. Three of them are bare digit sequences (`"401"`,
`"403"`, `"429"`), a substring match where the needle is
supposed to be a whole token.

**The bug.** `classify("request id 1401 failed")` returned
`ProviderAuthError` because the string contains `"401"`. Same
for `"tokens: 12429"` → `TransportRateLimit`. `ProviderAuthError`
is terminal — it aborts the retry chain — so a transient
failure whose error text contains a request id, an attempt
counter, or a token count with those three digits in it becomes
a permanent failure. `TransportRateLimit` sleeps; a transient
failure whose text contains `429` in a token count burns the
wait window on a request that cannot succeed.

Same class as `extract_status` (`af0301f`), `extract_attribute`
(`1231a4e`), and `Origin::parse` (`31bd1a5`) — a substring match
where the needle is a whole token.

**Found by a property.** `prop_digit_substring_is_not_a_status`
generated the false-positive shapes and asserted no auth or
rate-limit classification. On the pre-fix code it failed on the
first case with `ProviderAuthError { detail: "request id 1401
failed" }`.

**The positive-direction property kept the fix honest.**
`prop_bounded_status_is_honoured` asserts that a real
`"http 401"`, `"(429)"`, `"http:401"` still classifies. A naive
fix like requiring whitespace before the digits would have
broken `"http:401"` and `"(429)"`. `contains_bounded` allows
any non-digit before and after, so both still match.

**Reachability check.** Before writing the fix, I checked the
call sites: `classify` is called from `engine::control` and
`engine::process` on `e.to_string()`. Provider error `Display`
output can include a request id (`req_1401`), an attempt
counter, or a token count in the same string as the status —
none of which are guaranteed to be non-digit-bounded. The
false positive is reachable. If it were not, the fix would
have been hardening an unreachable path and the honest response
would have been to say so in the commit message instead of
shipping a change. The reachability check is now part of the
drill; L52 records it.

**The fix.** A `contains_bounded(haystack, needle)` helper: the
needle matches only when bounded by non-digits (or at the
string's edge). Text needles (`"rate limit"`, `"unauthorized"`,
`"too many requests"`) stay on plain `contains` — only the
three bare-digit checks were touched.

**Two positive controls:**

- Control A: revert only the 401 boundary
  (`contains_bounded` → `contains`).
  `prop_digit_substring_is_not_a_status` fails.
- Control B: revert only the 429 boundary. Same property fails.

Both restored cleanly; suite re-confirmed green.



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

