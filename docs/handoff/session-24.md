# Handoff — kod, session 24

**Continues:** session 23 (`docs/handoff/session-23.md`). This file is the
entry point.

**Last tag:** `v0.1.15` (`612d26e`), on origin. **No new tag cut.**
**HEAD:** `d77d579` (`test(kod-provider-anthropic): properties for
parse_response`).
**Tree:** clean.
**Local vs origin:** 50 commits ahead of `origin/main` (`a37f8a4`).
Nothing pushed.
**Gate:** green at `d77d579` — 5210/5210 passed, 5 skipped.

## Step 0 — verify

    git log --oneline -5
    git status --porcelain --untracked-files=all
    git describe --tags --abbrev=0
    git rev-list --count origin/main..HEAD

Expect HEAD at `d77d579` or later, clean tree, tag `v0.1.15`, **50
commits ahead**.

## Verification policy (changed this session)

Per the user's instruction: **do not run the full workspace gate on
every round.** Run it when:

- a `fix(...)` commit changes production code,
- a shared crate (`kod-types`, `kod-error`) is touched,
- the tree has accumulated several unchecked test-only commits,
- before any tag decision.

Test-only rounds in a leaf crate are verified with the **crate-local**
suite (`cargo nextest run -p <crate>`) and the handoff records which
commits are gate-verified and which are only crate-verified.

## Rounds added this session (after session 23's entry point)

| Commit | Crate | Verification | Outcome |
|---|---|---|---|
| `be41a7f` | kod-config | crate + gate | `parse_agents_md`: 6 props + 2 characterizations + 5 checks. F2b-9 (directive inside a code fence is literal), fence conditions. |
| `28c1a02` | kod-provider | crate + gate | `extract_json`: 5 props + 1 characterization. Object/array in prose round-trips; a **scalar in prose is not found** (no `{`/`[` anchor). |
| `644bb1c` | kod-telemetry | crate only | `parse_headers`: 7 props + 4 characterizations. `key=value` pairs, `=` in value kept, duplicates preserved, comma-in-value not expressible. |
| `d77d579` | kod-provider-anthropic | crate + gate | `parse_response`: 5 props + 1 characterization + 2 checks. Usage arithmetic (the cost contract), variant selection, 1h cache clamp. |

## Characterizations that need a decision, not a test

**1. `kod-config/instructions.rs` — `::: whenfoo` is a no-op fence.**
`strip_prefix("when")` matches the "when" prefix of `whenfoo`, and
`parse_condition("foo")` returns `Always`. A typo silently means "no
guard". `::: <other>` (any non-`when` directive) is silently **dropped**
from the output.

**2. `kod-provider/structured.rs` — a scalar in prose is not found.**
`extract_json` anchors on `{` or `[`. `extract_json("42")` is
`Some(42)`, but `extract_json("the answer is 42")` is `None`. Design,
not bug; a caller that needs a scalar from prose cannot use this.

**3. `kod-telemetry/parse_headers` — a comma in a value is not
expressible.** No quoting: `a=b,c` is two pairs. A caller needing a
comma in a header value must use a different mechanism.

**4. `kod-provider-anthropic/parse_response` — nested 1h with no flat
total is `Some(0)`.** When `cache_creation_input_tokens` is absent but
`cache_creation.ephemeral_1h_input_tokens` is present, the
`.min(cache_creation.unwrap_or(0))` clamp yields `Some(0)`, not `None`.

**5. `kod-stats/behavioral.rs` — substring cues fire inside common
words.** (From session 23, still open.) `"ugh"` matches inside
`enough`/`rough`/`tough`/`laugh`/`through`.

**6. `BehavioralSignals` — `merge` saturates, `total` does not.** (From
session 23, still open.)

## Lessons from sessions 23–24, condensed

1. **Construct, don't filter.** Every generator that constrained the
   interesting case with `prop_assume!` on a broad alphabet eventually
   aborted on proptest's reject limit. Build the case directly instead.
2. **Bound any generated `usize` that drives a loop or allocation.**
   The `stream_guard` timeout was `any::<usize>()` in `take(count)`.
3. **`grep FAIL` is not a failure guard.** nextest also reports
   `TIMEOUT`, `LEAK`, and tool-level `error:`. Use exit code plus
   `TIMEOUT` plus `panicked at`, never `FAIL` alone.
4. **Match the code's predicate.** `char::is_alphanumeric` is
   Unicode-aware; `[^a-zA-Z0-9]` is not.
5. **`prop_assert!` is a statement macro returning `()`.** In a `match`
   arm that must yield a value, use `panic!` — `prop_assert!` is only
   correct when every arm yields `()`.
6. **Compare the same representation on both sides.** A lone trailing
   `\r` is kept by `str::lines()` but stripped once a parser re-adds
   `\n`; comparing `lines()` to `lines()` fails on an input where the
   code is correct. Compare the reconstructed string instead.
7. **A user-facing alias and its canonical name are two namespaces.**
   `ext` is stored as `filetype`.

## Production fixes across sessions 20–24

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

Plus ~25 harden/characterization rounds of property tests.

## The one open decision

50 commits, ten production fixes, green gate at 5210/5210, tag `v0.1.15`
alone on origin. Cutting `v0.1.16` and pushing matches the release
cadence and is defensible on verification. It fires the six-target
release workflow, so it is the user's call.
