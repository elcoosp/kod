# kod — current handoff

**Tag:** `v0.1.19`, on origin. 8 commits since `v0.1.18`.
**Branch:** `main`, level with `origin/main`.
**Gate:** green at the tagged commit (full workspace).
**Tree:** clean.
**kod-cli coverage:** 47.3% (3,057/6,461), up from 29.9% at `v0.1.16`.




Everything below is the state a session should start from. The
historical per-session notes are gone from the working tree; they are
still reachable from the `v0.1.16` tag and earlier commits if a
decision's reasoning needs revisiting.

## Step 0 — verify before touching anything

    git log --oneline -3
    git status --porcelain --untracked-files=all
    git describe --tags --abbrev=0
    git rev-list --count origin/main..HEAD

Expect HEAD at `0287c95` or later, clean tree, tag `v0.1.16` (or a
later tag), **0 ahead**.

## What v0.1.16 contains

Twelve production fixes since `v0.1.15` (`612d26e`), each with a
regression test:

| # | Crate | Fix |
|---|---|---|
| 1 | kod-core | `tool_quota::check` saturating soft-threshold arithmetic |
| 2 | kod-swarm | `WorkPool::dispatch` `FreshAgents` item drop |
| 3 | kod-swarm | `CleanseScheduler::take_batch` empty `Some` on a zero budget |
| 4 | kod-swarm | `FileTouchService::has_touched` poisoned-lock recovery |
| 5 | kod-provider | `Question::validate` `:` delimiter collision |
| 6 | kod-types | `resolve_tool_name` strips all `functions.` prefixes |
| 7 | kod-tools | bash interceptor: newline is a top-level separator |
| 8 | kod-tools | `apply_unified_diff` trailing-newline round-trip (three defects) |
| 9 | kod-core | `parse_subtasks` skips malformed items instead of aborting |
| 10 | kod-schema-dialect | `classify_rejection` case-insensitive construct match |
| 11 | kod-stats | `count_cues` word-boundary short cues; `total` saturates |
| 12 | kod-core-state | `pending_tool_calls` output sorted (was nondeterministic) |

Plus roughly thirty property/characterization rounds of tests across
fourteen crates.

## Testing techniques used, and what each found

| Technique | Applied to | Bugs found |
|---|---|---|
| Single-function properties | ~20 functions | 3 |
| Differential across implementations | duration formatters, location parsers | 0 |
| Metamorphic monotonicity | truncators | 0 |
| Model-based state machine | `SecretVault` | 0 |
| Serde round-trip | `SwarmConfig` | 0 |
| Integration with fixtures | `worktree.rs` | 0 |
| Write-tests-first over a read body | `pending_tool_calls` | **1** |

The pattern: rounds that **read the body carefully before drafting**
found bugs; mechanically applied techniques found none, because the
code was already sound in those dimensions.

## Verification policy

- **Production fixes** and changes to shared crates (`kod-types`,
  `kod-error`, `kod-config`, `kod-core-state`) → full workspace gate.
- **Test-only rounds in a leaf crate** → crate-local
  (`cargo nextest run -p <crate>`).
- Before any tag → full workspace gate.

A crate-local proptest pass is **not** verification: proptest is a
randomized search, so the crate run and the workspace run use
different seeds.

## Open items — decisions, not bugs

Six behaviours are characterized in tests as intentional. Each is a
format limit or a conservative choice, and each would need a **design
decision**, not a fix:

1. **`kod-config/instructions.rs`** — `::: whenfoo` (no space after
   `when`) is a no-op fence; `::: <other>` is silently dropped.
2. **`kod-provider/structured.rs`** — `extract_json` anchors on `{`
   or `[`, so a scalar in prose (`"the answer is 42"`) is not found.
   A bare scalar parses.
3. **`kod-telemetry/parse_headers`** — a comma in a header value is
   not expressible; the format has no quoting.
4. **`kod-provider-anthropic/parse_response`** — nested
   `cache_creation.ephemeral_1h_input_tokens` with no flat
   `cache_creation_input_tokens` yields `Some(0)`, not `None`.
5. **`kod-tools/check.rs`** — `parse_location_colon` rejects line/col
   0 and an empty file; `parse_location_colon_prefix` does neither.
   One guard was added to one parser and not the other.
6. **`kod-tools/check.rs`** — `parse_location_colon_prefix` treats a
   single-letter path (`a:1:1: err`) as a Windows drive letter and
   returns `None`.

## The one real target left

**`kod-lsp/src/client.rs`** — the JSON-RPC read loop. Needs a live
`rust-analyzer` child process and a fixture that stands one up. Not a
one-round script; a fresh session's opening move.

Every other crate's remaining untested surface is async read loops,
filesystem fixtures, or live-engine state. The pure-function survey
is exhausted.

## Lessons worth carrying

1. **Print the input before reasoning about a dependency's output.**
   Five `patch.rs` attempts guessed at what `similar` emits; the sixth
   printed it and the bug was visible in one line.
2. **Construct, don't filter.** A generator that constrains the
   interesting case with `prop_assume!` on a broad alphabet aborts on
   proptest's reject limit. Build the case directly.
3. **Bound any generated `usize` that drives a loop or allocation.**
   `any::<usize>()` in `take(count)` hung a test for 300 s.
4. **`grep FAIL` is not a failure guard.** nextest also reports
   `TIMEOUT`, `LEAK`, and tool-level `error:`. Use exit code plus
   `TIMEOUT` plus `panicked at`.
5. **`prop_assert!` returns `()`.** In a `match` arm that must yield a
   value, use `panic!`.
6. **A test should not expand a production type's API.** Compare via
   `toml::Value` or `Debug` rather than adding a `PartialEq` derive
   purely to serve a test.
7. **Compare the same representation on both sides.** A lone trailing
   `\r` is kept by `str::lines()` but stripped once a parser re-adds
   `\n`; comparing `lines()` to `lines()` fails where the code is
   correct.
8. **Never `rm` a seed file without checking `git ls-files` first.**
   Only untracked seeds belong to the current round.
