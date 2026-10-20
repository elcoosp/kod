# Handoff — kod, session 19

**Continues:** session 18 (`docs/handoff/session-18.md` for the
start; `docs/handoff/session-18-detail.md` for the narrative
through `330185a`).
**Last tag:** **`v0.1.14` (`0980469`)** — released in session 19.
**Last verified commit:** `330185a` (docs-only close of session
18 — no code change, no gate).
**Tree at handoff:** clean.
**Commits since the tag:** 41 (see `session-18-detail.md`,
"Post-extraction rounds").


## Step 0 — verify before touching anything

Three commands, reconcile against "State" below:

    git log --oneline -8
    git status --porcelain --untracked-files=no
    git describe --tags --abbrev=0

Expect HEAD at `330185a` or later, tree clean, tag `v0.1.13`.
If the tree is dirty, name the files before proceeding — do not
assume an in-progress change is yours.


## What session 18 left

Session 18 closed its property-testing cadence at `330185a`. The
last section of `session-18-detail.md` ("Post-extraction rounds")
has the full record: 16 commits after the file was extracted,
one new bug (`extract_citations` malformed ranges, `fc88fe6`),
two fixture bugs in my own property code (the `image_budget`
`media_type` field and the `:04`-vs-`:05` frame width that would
have made a survivor property vacuous), five pinned behaviours
recorded as controls rather than fixed, one correction to a
"real bug" claim that wasn't (`commit.rs:187` is inside
`validate`, not `format_message`), and a consolidated note on
the five counting errors scattered through the session.

The tag is still `v0.1.13` at `087d6e1`. Nothing since is
released. The tree is clean and every module touched since the
tag has a targeted `cargo nextest run -p <crate> --lib <module>`
on record — but no full-workspace gate has been run since
`06d97d0`.


## Where to start

### 1. Full-workspace gate — before anything else

The last full-workspace gate ran at `06d97d0`:

    4529 tests run: 4529 passed, 5 skipped

41 commits have landed since — most of them property tests, a
handful of `chore(deps)` / `chore(proptest)` / `chore(rustfmt)`
companions, and one `fix`. The per-module runs from session 18
are green; the whole-workspace interaction between those new
properties and the existing suite has **not** been verified.

Run it once before doing anything else:

    cargo nextest run --workspace --no-fail-fast

Expected total: 4529 + (new test count). The per-module runs from
session 18 suggest roughly 4580–4600, but nobody has checked the
total end to end. Do not predict the number; read it from the
`Summary` line.

The "stop running full gate ate every commit" instruction from
session 18 was about *per-commit* gating — the gate is expensive
and a targeted run is enough for a property round. A single
end-of-session gate is what that instruction was saving the cost
for. Run it once here.

**If red:** paste the full failure. Do not start a new round
until it's green. The session-18 lessons apply:

- L51 — a `SIGKILL` (exit 137) during `cargo nextest run` is
  not the build's fault until you check for a concurrent process
  on the host. Run `pgrep -fl 'cargo|rustc|nextest|llvm-cov'`
  first.
- L54 — a `Resource temporarily unavailable (os error 35)` at
  `fork` is the host process budget, not code. A concurrent
  `cargo llvm-cov` (session 18 saw ~2200 `llvm-cov` processes
  from a different repo, `glyim-v2`) is the shape. Wait for it
  to recede, then retry.
- L56–L57 — verify a test-count delta from the run's `Summary`
  line, not from arithmetic on a bullet list.

### 2. If the gate is green

The remaining work, in order of what's most useful per hour:

**a. `repomap.rs`** — `crates/kod-core-quality/src/repomap.rs`,
1300 lines. AST-based symbol and import extraction via
`tree-sitter`, plus a regex fallback (`extract_rust`). The natural
properties:

- extraction is deterministic;
- `extract_rust_via_ast` and `extract_rust` produce overlapping
  symbol sets on real Rust source (the fallback must not
  disagree with the AST path on simple files);
- `build_repo_map` on a fixture directory yields symbols whose
  file paths are all inside the root;
- `render` never exceeds `max_chars`.

Costs: on-disk fixture files (real `.rs`/`.py`/`.ts` source trees),
slow grammar compiles on first build. Two or three rounds, not
one. Start with a read-only survey of `extract_symbols_and_imports`,
`extract_rust_via_ast`, and the existing test fixtures — do not
write a property until you know what the extraction returns for a
hand-written fixture.

**b. `worktree.rs`** beyond `sanitize_slug` —
`crates/kod-core-quality/src/worktree.rs`, 1146 lines.
`WorktreeManager` operations: `detect`, `create`, `create_session`,
`merge_all`, `cleanup`. Every test needs
`tempfile::tempdir()` + `git init` + a commit. nextest runs tests
in parallel; parallel `git init` in parallel tempdirs is the L51
leak shape at scale. Real work, slow, flaky-prone. If attempted,
run with `--test-threads=1` for the git-heavy tests and accept the
runtime cost.

**c. `worktree_isolation_ownership.rs`** — 364 lines. Same
`tempfile` cost, plus `libc::kill`-based process-death simulation
(`reap_dead`). `#[cfg(unix)]` on the relevant tests either way.
Lower value per hour than (a) or (b).

**d. `run_structured`** — `crates/kod-provider/src/structured.rs`.
Async, needs a mock provider. `extract_json` already has
properties (session 18 covered the parse shape). `run_structured`
itself is the async wrapper whose contract is about retry budget
times parse outcome. Lower value per hour than (a)–(c); the
property would be about retry policy, not parsing.

**e. Stop.** The tag is unmoved, the tree is clean, the handoff
is current, and the remaining targets are all new-session shape.
This is a natural stopping point too.

### 3. If the gate is red and the cause is not environmental

Fix the failing test (or the code it exercises) as a single
focused commit, then re-run the whole-workspace gate before
proceeding. Do not stack a new property round on top of a red
gate — the next round's targeted run would be against a tree that
does not pass its own gate, and the diagnosis of any failure in
the new round becomes ambiguous.


## Round log

The narrative for each property round lives in
`docs/handoff/session-19-detail.md`. Read it when a specific
fix or lesson matters; the starting document stays short.

## State

| | |
|---|---|
| HEAD | `b457ac5` (`test(kod-core-state): property tests for DecisionLog`) |
| tag | **`v0.1.14` at `0980469`** (released in session 19) |
| previous tag | `v0.1.13` at `087d6e1` |
| tree | clean |
| `session-18.md` | 340 lines (the starting document) |
| `session-18-detail.md` | 1618 lines (the narrative) |
| `session-19-detail.md` | ~320 lines (R1–R6, the v0.1.14 release note) |
| commits since v0.1.13 | 53 |
| last full-workspace gate | `b457ac5` — `Summary [  55.056s] 4755 tests run: 4755 passed, 5 skipped` |

### Gate verification at session open

The first task in the starting document (the full-workspace gate
that had not been run since `06d97d0`) was run at `61536ba`, the
session-open commit:

    Summary [  49.501s] 4692 tests run: 4692 passed, 5 skipped

Clean first try: no failures, no timeouts, no leaks. Delta from
`06d97d0`: +163 tests — larger than the "~55" estimate in this
document's "What session 18 left" section. That estimate was
based on the property rounds' *own* counts; it missed the
`#[cfg(test)]` helpers in each module's proptest file, the
`control_*` functions that are separate tests, and the Cargo.toml
companion test count changes. The number that matters is the
`Summary` line, not the estimate.


## Session 19 in one line

Opened with a full-workspace gate (4720/4720 at `dde9d65`), ran
six property rounds (R1–R6) across three crates, released
`v0.1.14` at `0980469` with three production fixes carried in
from sessions 18 and 19, and closed with three clean
regression-guard rounds and one real bug (`R2`, Go block
imports). The cadence that worked for twenty-plus rounds — read
the body, write properties plus controls, targeted
`cargo nextest run -p <crate> --lib <module>`, fix only if a
property goes red on the expected class — is now exhausted on
pure-function targets. The remaining work in the workspace
(`worktree`, `worktree_isolation_ownership`, `run_structured`,
`repomap`'s filesystem and budget functions) needs `tempfile` +
`git init`, `libc::kill`, or async + mock-provider fixtures, and
is new-session shape, not one-round.

The session also spent real effort on something other than
properties: the counting/checking failures (L56–L58, plus the
R4 panic-probe misfire and the release-script tag-object
mismatch) were all in the scripts that *surrounded* the code —
derived numbers, anchored greps, `rev-parse` on an annotated
tag, a check whose result was printed and not used. None of
them was a code bug; all of them were the same family. The
pattern is stable enough that a session-20 opening should read
L56–L58 first and prefer the *run's own Summary line* over any
script-derived count.
