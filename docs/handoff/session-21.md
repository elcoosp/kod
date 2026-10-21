# Handoff — kod, session 21

**Continues:** session 20 (`docs/handoff/session-20.txt` for the original
starting doc; `docs/handoff/session-20-detail.md` for the narrative and
the deferred `patch.rs` finding). This file supersedes `session-20.txt`
as the entry point: that doc's HEAD, commit count, and open-target list
are stale.

**Last tag:** `v0.1.15` (`612d26e`), on origin. **No new tag cut.**
**HEAD:** `1b58978` (`docs(handoff): record the green full-workspace gate`).
**Tree at handoff:** clean.
**Local vs origin:** 22 commits ahead of `origin/main` (`a37f8a4`).
Nothing pushed.
**Gate:** green at `1b0c5ba`, the code state under the docs commit —
5035/5035 passed, 1 leaky, 5 skipped, exit 0, 80 s, no `EAGAIN`.

## Step 0 — verify before touching anything

    git log --oneline -5
    git status --porcelain --untracked-files=all
    git describe --tags --abbrev=0
    git rev-list --count origin/main..HEAD

Expect HEAD at `1b58978` or later, tree clean, tag `v0.1.15`, **22
commits ahead**. If the tree is dirty, name the files before proceeding.
If HEAD has moved, read `git log --oneline a37f8a4..HEAD` before
trusting the rest of this file.

## The one thing to know first

**The gate is green and the tag is the only open decision.** The code is
verified end-to-end; nothing is half-landed; the `patch.rs` revert left
the tree sound. There is no fire to put out.

What there is: 22 unpushed commits and a tag that has not been cut. The
user's "don't push, continue testing" was in effect for all of session
20. Do not push and do not tag without an explicit instruction — tagging
fires the six-target release workflow on push.

## State

### Commits added in session 20 (7 code, 2 docs)

| Commit | Crate | Outcome |
|---|---|---|
| `f110082` | kod-types | SecretVault security + round-trip: 6 props, 3 controls. Harden. The handoff's determinism worry was resolved by reading the body: `Reverse(len)` over a stable sort cannot tie on two secrets where one is a substring of the other, because equal-length strings are never substrings of each other. |
| `3916f34` | kod-types | Secret scanners: 7 props. Harden. Pins that every scanned value is registered and does not survive `obfuscate`, and that `deobfuscate` restores the text. |
| `cb9564e` | kod-types | **Real bug.** `resolve_tool_name` stripped exactly one `functions.` prefix; a doubly-prefixed name resolved to a non-registered tool and `resolve(resolve(x)) != resolve(x)`. Seed committed with the fix (strip in a loop). |
| `ebeb5e3` | kod-core | Stream control markers: 7 props + an exhaustive 13-parser collision test. Harden. |
| `0b9c9c7` | kod-tools | **Real bug.** `has_top_level_operator` did not treat `\n` as a top-level separator, and `split_shell_words` did not split on it. Two-statement commands were misrouted. Not an injection hole (nothing spawns on intercept); a documented-contract violation. Seed committed with the fix. |
| `1b0c5ba` | kod-types | `floor_char_boundary` maximality + `truncate_with_ellipsis` contract: 5 props. Harden. |
| `63f4f26`, `1b58978` | docs | Session record and the gate result. |

Plus `d545771` and the nine commits before it from session 19.

### The two bug seeds on disk

- `crates/kod-types/proptest-regressions/aliases.txt` — `functions.functions.`
- `crates/kod-tools/proptest-regressions/bash_interceptor.txt` — `a\na`

Both are real-bug seeds and are committed with their fixes, per the arc's
rule. Do not delete them.

## What's open

### 1. The tag decision (blocked on the user)

Seven production fixes beyond `v0.1.15`: `tool_quota` saturation,
`WorkPool` FreshAgents item drop, `take_batch` empty `Some`,
`has_touched` poison gap, `judgment` colon drop, `resolve_tool_name`
double prefix, bash-interceptor newline. That count matches the release
cadence (`v0.1.13` three, `v0.1.14` three, `v0.1.15` six) and the gate is
green. Cutting `v0.1.16` and pushing is defensible on verification, not
just commit count — but it is the user's call because it fires a release
workflow.

Mechanical steps if the freeze lifts:

    git push origin main          # branch first
    git tag -a v0.1.16 -m "..."   # then the tag
    git push origin v0.1.16       # fires the release workflow

### 2. `kod-tools/src/patch.rs` — two confirmed defects, write-tests-first

Documented in full in `session-20-detail.md` under "Deferred". Summary:

- **Defect 1:** `apply_unified_diff` loses a trailing newline that a
  patch *adds* to a file that previously lacked one. Seed: `old = "A"`,
  `new = " \n"` → applied `" "`. The result's trailing-newline decision
  reads the *original*'s shape, never the patch's last emitted line.
- **Defect 2:** `"".split('\n')` yields `[""]`, so an all-context patch
  against an empty original emits `" \n"` instead of `""`.

**A naive one-line fix was written and reverted** — `patch.ends_with('\n')`
is always true and appended a newline to every patch, regressing a
pre-existing production test (`test_patch_file_creates_a_new_file_from_dev_null_diff`).
The correct fix tracks per-line marker placement in `parse_unified_diff`
(a parallel `Vec<bool>`, or a field on `HunkLine`) and rewrites the
trailing-newline decision against the last *emitted* line. Defect 2 is a
separate `"" -> []` normalization.

**Open this first, in a fresh session, write-tests-first.** Verify with
the full `kod-tools` suite, not just `patch::` — that pre-existing test is
exactly what the filtered run would miss.

Ceiling to know: `similar` does not emit `\ No newline` for
`old = "", new = "a"`, so a render→apply round-trip property cannot hold
for that input. Any property demanding it is a test-shape mistake.

### 3. Remaining survey targets (none is the L42 shape)

- `kod-tools/src/check.rs` — 11 diagnostic parsers, already 3 proptests.
  Likely harden or a position off-by-one seed.
- `kod-tools/src/context.rs` — `matches_pattern_inner` is `globset`-backed,
  so only the `{pattern}/**` fallback and the `fail_closed` branch are
  kod-authored. Narrow round. `resolve_path` / `can_write` need
  `tempfile` fixtures.
- `kod-core-quality/src/preflight.rs` — `parse_loadavg` / `parse_meminfo`
  / `parse_pmset`, 3 parsers, 0 proptest.
- `kod-error` classifiers — **surveyed, no round.** `has_status_code` is
  already token-bounded with a test pinning `"5001"` / `"a500b"` false.
  The other two are prose-substring by documented design. The handoff's
  feared digit-substring bug does not exist there.

## Lessons from session 20 worth carrying

1. **One red seed is a bug; two red seeds in the same function, one on
   trivial input, means the diagnosis is not finished.** The `patch.rs`
   fix was written after two properties went red and regressed a
   pre-existing test. The full-crate run is what caught it.
2. **A test file next to the code is not the only place tests live.**
   `truncate_bytes` and `truncate_chars` had proptests in
   `crates/kod-types/tests/it/string_safety.rs`, so a workspace sweep
   that only looks beside the source reports a false zero. Check
   `tests/it/` before declaring a module uncovered.
3. **E0716 in a property is a test-shape mistake, not a code bug.** Bind
   the temporary to a `let` before comparing a returned slice against a
   `format!`. It bit twice in session 20 (`aliases.rs`, `strutil.rs`).
4. **A revert is a valid outcome.** The `patch.rs` fix was reverted with
   the finding recorded; the tree stayed green and the next session opens
   on a clean, documented target rather than a half-fixed file.

## Session 21 in one line

The gate is green at 5035/5035, the tree is clean, 22 commits wait for a
tag decision, and the one real remaining bug — `patch.rs`'s trailing
newline — is documented with a repro and a known-wrong fix, ready for a
write-tests-first round.

## Update — two more rounds and a survey correction

Commits after the session-21 entry point was written:

| Commit | Crate | Outcome |
|---|---|---|
| `e1c1cdf` | kod-core | `parse_plan_steps`: 6 props + 3 deterministic checks. Harden. First test of any kind for that function. Control pins the documented "first `[` to last `]`" limitation. |
| `af7a8b8` | kod-schema-dialect | `contains_word` / `classify_rejection`: 7 props + 2 checks + 1 characterization. Harden. |

### The survey's false zeros

Two files were reported as "0 proptest" by the workspace sweep and **both had them**:

- `crates/kod-types/src/strutil.rs` — covered in `crates/kod-types/tests/it/string_safety.rs`.
- `crates/kod-core-quality/src/preflight.rs` — 5 proptests in a second `#[cfg(test)]` module *in the same file*, past a first `mod tests` that the sweep stopped at.

**Rule:** never trust a sweep's "0 proptest" claim. Check `tests/it/`, and check for a *second* `#[cfg(test)]` block in the file. If a target is declared uncovered, grep the file for `proptest!` directly before writing anything.

### `recovery.rs` — one real limitation, characterized

`contains_word` walks bytes and treats every non-ASCII-alphanumeric, non-underscore, non-dollar byte as a boundary. Every UTF-8 continuation byte (`0x80..=0xBF`) satisfies that, so `"列挙enum"` matches `"enum"` even though `挙` is a word character. Reachability is very narrow; the fix is `char_indices()`. Pinned by `utf8_multibyte_before_keyword_is_currently_a_false_positive` with a doc comment saying the fix is correct and the test must be updated when it lands.

Also noted and deliberately **not** changed: `classify_rejection` does not lowercase, so an all-uppercase provider body misses. The sibling `contains_word` in `kod-core/src/router.rs` *does* lowercase. No documented contract settles it; left for a round that can decide rather than a round that pins.

### Gate status

Green at `1b0c5ba` (5035/5035) and again at `e1c1cdf` (5044/5044). `af7a8b8` is test-only in one crate, verified at 33/33 there, and is covered by the gate this record precedes.
