# Handoff — kod, session 22

**Continues:** session 21 (`docs/handoff/session-21.md`, which is long and
accurate but predates this session's fixes). This file is the entry point.

**Last tag:** `v0.1.15` (`612d26e`), on origin. **No new tag cut.**
**HEAD:** `b50cc4b` (`fix(kod-schema-dialect): match rejected constructs
case-insensitively`).
**Tree:** clean.
**Local vs origin:** 36 commits ahead of `origin/main` (`a37f8a4`).
Nothing pushed.
**Gate:** green at `b50cc4b` — 5086/5086 passed, 5 skipped.

## Step 0 — verify

    git log --oneline -5
    git status --porcelain --untracked-files=all
    git describe --tags --abbrev=0
    git rev-list --count origin/main..HEAD

Expect HEAD at `b50cc4b` or later, clean tree, tag `v0.1.15`, **36
commits ahead**.

## The one thing to know

**Everything from session 21's open-findings list is resolved.** There is
no known open bug. The tag is the only outstanding decision.

## What session 21 closed

| Finding | Resolution |
|---|---|
| `patch.rs` trailing newline | **Fixed** in `8e62f47` — three defects: empty-split phantom newline, trailing-newline decision read the original's shape, and the `\ No newline` marker is positional (applies to the line above; a parser that leaves it set across a body push makes the next line inherit a stale marker). The general round-trip property was **withdrawn** in `b04b7ea` — `similar` 2.7.0 emits an inconsistent header for whitespace-only boundary diffs (`@@ -1 +2 @@` with a two-line body) and `apply_unified_diff`'s W9 guard correctly rejects it. The property tested `similar`, not `kod`. |
| `parse_subtasks` batch abort | **Fixed** in `7783456` — a missing `name`/`description` now skips the item instead of aborting the batch. The pre-fix `?` made the caller fall back to N copies of the goal when one item was malformed; every other field already degraded gracefully, so aborting was the anomaly. |
| `parse_subtasks` `max = 0` | **Not a bug** — `swarm_runner.rs:276` does `max_agents.clamp(2, 8)`, so `max` is never 0 from production. Unreachable; no change. |
| `classify_rejection` case | **Fixed** in `b50cc4b` — the body is lowercased once and candidates are compared with `eq_ignore_ascii_case`; the canonical spelling is returned so the caller drops the exact schema key. Boundary predicate unchanged (`ENUMERATE` still does not match `enum`). |

## Production fixes across sessions 20–22

Ten, on a green gate:

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

Plus nine harden rounds of property tests and four characterization
suites.

## What is NOT done

**The pure-function survey is exhausted.** Every remaining untested
surface is `&mut self` state machines, async read loops, or needs
`tempfile` / `git init` / a live engine. Further harden rounds would pad
the commit count.

**A future `patch.rs` round-trip property** should feed only patches
`similar` emits consistently, or pin `similar` and file the upstream
bug. Not a kod bug; a dependency bug the property accidentally tested.

## Lessons from sessions 20–22, condensed

1. **When a fix fails twice, print the input instead of reasoning
   about it.** Five `patch.rs` attempts guessed at what `similar`
   emits; the sixth printed it and the bug was visible in one line.
2. **A crate-local proptest pass is not verification.** `8e62f47`
   passed 465/465 locally and failed the workspace gate at the same
   commit, because proptest is a randomized search and the gate uses a
   different seed. Gate-verify production changes.
3. **A test should not expand a production type's API.** `Subtask`
   does not derive `PartialEq`; the tests use `is_none()` /
   `is_empty()` rather than asking the struct to gain a trait.
4. **Check `tests/it/` and second `#[cfg(test)]` blocks before
   declaring a target uncovered.** Three false zeros in session 21.
5. **Don't force a dependency to satisfy a property.** If a test
   fails because a crate emits malformed output, the test is wrong —
   withdrawing it is a valid outcome.

## The one open decision

36 commits, ten production fixes, green gate at 5086/5086, tag
`v0.1.15` alone on origin. Cutting `v0.1.16` and pushing is defensible
on verification and matches the release cadence (`v0.1.13` three fixes,
`v0.1.14` three, `v0.1.15` six). It fires the six-target release
workflow, so it is the user's call.
