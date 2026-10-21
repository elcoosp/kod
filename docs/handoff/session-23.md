# Handoff — kod, session 23

**Continues:** session 22 (`docs/handoff/session-22.md`). This file is the
entry point.

**Last tag:** `v0.1.15` (`612d26e`), on origin. **No new tag cut.**
**HEAD:** `22d2926` (`test(kod-stats): properties and characterizations for
the behavioral signals`).
**Tree:** clean.
**Local vs origin:** 45 commits ahead of `origin/main` (`a37f8a4`).
Nothing pushed.
**Gate:** green at `22d2926` — 5176/5176 passed, 5 skipped.

## Step 0 — verify

    git log --oneline -5
    git status --porcelain --untracked-files=all
    git describe --tags --abbrev=0
    git rev-list --count origin/main..HEAD

Expect HEAD at `22d2926` or later, clean tree, tag `v0.1.15`, **45
commits ahead**.

## The one thing to know

**The pure-function survey is exhausted.** Every target checked is either
already property-covered or needs fixtures (`tempfile`, `git init`, a live
engine, an async read loop). There is no known open bug.

The tag is the only outstanding decision. It fires the six-target release
workflow on push, so it stays the user's call.

## Rounds added this session (after session 22's entry point)

| Commit | Crate | Outcome |
|---|---|---|
| `408015b` | kod-risk | `paths.rs` classifier: 7 props + 3 characterizations. Traversal cannot hide a protected path. |
| `71f8d8b` | kod-risk | `tokenize`: 7 props + 3 checks. Operators cannot glue to neighbours (`a\|b` splits). |
| `34f5655` | kod-tools | `edit_hashline::parse`: 7 props + 5 checks. BUG-D-04 payload whitespace, W5 second-header, append sentinel. |
| `9c27770` | kod-memory | `hygiene`: 7 props + 5 checks. No tag survives; unterminated block drops its tail. |
| `a9f191b` | kod-tools | `query_syntax::parse`: 8 props + 3 checks. BUG-D-08 negated constraints, `://` guard, `ext`→`filetype` alias. |
| `8709e9b` | kod-tools | `context` path-allow fail mode: 4 props + 2 checks. H-R16 fail-closed/fail-open pinned. |
| `2881916` | kod-provider | `stream_guard`: 7 props + 4 checks. Cycle soundness + both period thresholds; header-runaway threshold. |
| `22d2926` | kod-stats | `behavioral`: 5 props + characterizations. Signals decompose into the cue tables. |

One commit was **reverted** during the session: the first `stream_guard`
draft generated `count in any::<usize>()`, which made
`iter::repeat(..).take(count)` hang; the test timed out at 300 s and the
post-run guard — which grepped only for `FAIL` — did not see `TIMEOUT`, so
the bad commit landed. `git reset --hard HEAD~1` removed it and the
corrected round landed as `2881916`.

## Two characterizations that need a decision, not a test

**1. `kod-stats/behavioral.rs` — substring cues fire inside common words.**

`count_cues` is plain `str::matches`. `"ugh"` is an anguish cue, so
`enough`, `rough`, `tough`, `laugh`, `through` each score a point; `"ffs"`
fires inside `offs`/`staffs`. The module doc says "five lexical signals"
and promises no boundaries, so this is the design as documented. A
boundary-aware matcher would change the signal's meaning — a decision, not
a bug fix. Pinned by `ugh_cue_fires_inside_common_words` and
`ffs_cue_fires_inside_ffs_words`.

**2. `BehavioralSignals` — `merge` saturates, `total` does not.**

`merge` uses `saturating_add`; `total` uses plain `+`. Unreachable from
real text (a message would need billions of cue hits) but inconsistent.
Either both saturate or neither does.

## Lessons from sessions 22–23, condensed

1. **Construct, don't filter.** Three separate rounds aborted on
   proptest's global-reject limit because the generator drew from a broad
   alphabet and then `prop_assume!`-d the interesting case. Building the
   constrained input directly (interleaving canceling path fragments;
   listing a non-alphanumeric alphabet; repeating a generated period) is
   the fix every time.
2. **A test whose runtime depends on a generated `usize` must bound the
   generator.** `any::<usize>()` in a loop count is an unbounded
   allocation. The `stream_guard` hang was exactly this.
3. **`grep FAIL` is not a failure guard.** nextest also reports
   `TIMEOUT`, `LEAK`, and tool-level `error:`. The guard that would have
   caught the hang is a non-zero exit code **plus** `TIMEOUT` **plus**
   `panicked at` — never `FAIL` alone.
4. **Use the same predicate the code uses.** `char::is_alphanumeric` is
   Unicode-aware; `[^a-zA-Z0-9]` is not. A generator that excludes a
   class must exclude it with the real predicate.
5. **Bind the expected value before `prop_assert_eq!`.** Four E0716/E0507
   errors this session came from borrowing a temporary or moving out of a
   `Vec` index inside the macro. `let expected = ...; prop_assert_eq!(&x,
   &expected);`
6. **A user-facing alias and its canonical name are two namespaces.**
   `ext` is stored as `filetype`; a property that generates from one and
   asserts against the other is wrong, and the code is right.
7. **Crate-local green is not verification.** proptest is a randomized
   search; the workspace gate uses a different seed. A fix that goes
   green locally and red in the gate means "not verified."

## Production fixes across sessions 20–23

Ten, all on a green gate:

1. `tool_quota::check` saturating soft-threshold arithmetic
2. `WorkPool::dispatch` `FreshAgents` item drop
3. `CleanseScheduler::take_batch` empty `Some` on a zero budget
4. `FileTouchService::has_touched` poisoned-lock recovery
5. `Question::validate` `:` delimiter collision
6. `resolve_tool_name` double `functions.` prefix
7. bash-interceptor newline not treated as a top-level separator
8. `apply_unified_diff` trailing-newline round-trip (three defects)
9. `parse_subtasks` skip-instead-of-abort
10. `classify_rejection` case-insensitivity

Plus ~20 harden/characterization rounds of property tests.

## The one open decision

45 commits, ten production fixes, green gate at 5176/5176, tag `v0.1.15`
alone on origin. Cutting `v0.1.16` and pushing matches the release cadence
and is defensible on verification. It fires the six-target release
workflow, so it is the user's call.
