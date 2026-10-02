# Session handoff — kod (2026-10-02)

**Repo:** `/Users/adm/Documents/Repos/kod`
**Host:** macOS aarch64, rustc 1.98.1, edition 2024
**Branch:** `main`
**HEAD:** `e7a4a4b`

---

## 1. TL;DR

Two workstreams landed:

1. **Audit deferred MEDIUMs** — closed M-11, M-22, M-39, M-40, M-44,
   M-47, M-48; fixed a stale test the previous session left failing
   on `main`.
2. **Server-busy / HTTP 503 overload retry** — the parallel agent's
   in-progress work, committed one-file-per-commit at the user's
   request.

**Still deferred:** M-13 and M-16 (see section 4).

**HEAD is green:** `cargo check --workspace --tests` clean;
`cargo nextest run --workspace --lib` = 3547/3547.

---

## 2. Commits this session (on top of `2509a09`)

Audit fixes:

| Commit | Finding |
|---|---|
| `50389fe` | M-22 / F2d-7 — subtask worktrees keyed by capability, not pool index |
| `5767995` | M-47 / F2h-2 — one stdin reader for the chat REPL |
| `84fee6e` | M-48 / F2h-3 — Ctrl+C cancels the running turn |
| `7f57048` + `575d5cc` | M-40 / F2f-21 — artifacts per-holder + tests |
| `6a7e7f9` | M-39 / F2f-16 — re-validate the write parent before opening |
| `45164a6` | M-44 / F2g-7 — chat rows from the cached probe |
| `67cc3e6` | stale test: unmapped keys assert `Ignore` |

Server-busy / 503 overload (13 commits, one per file):

| Commit | File |
|---|---|
| `b63cec0` | error.rs — `KodError::ServerBusy` + body classifier |
| `5121d54` | retry_strategy.rs — `TurnFailure::TransportServerBusy` |
| `ec3ad8a` | provider/retry.rs — honors ServerBusy Retry-After |
| `bf71582` | provider-openai/provider.rs — 503 → typed ServerBusy |
| `3081c22` | engine/mod.rs — marker + 4 waits/turn |
| `ba729d7` | config/llm.rs — doc |
| `6e6a1a1` | tui/app/mod.rs — `GenPhase::ServerBusy` |
| `1cb7558` | tui/app/streaming.rs — phase guard |
| `8ff62bc` | tui/app/ui_state.rs — countdown label |
| `65cd320` | tui/event.rs — `Event::ServerBusy` |
| `e9a520a` | tui/main_loop.rs — handler + test |
| `80cf894` | cli/chat.rs — render wait |
| `e7a4a4b` | cli/prompt.rs — render wait |

---

## 3. BISECT CAVEAT — read before `git bisect`

Two intermediates in the server-busy range **do not compile**, because
an enum variant lands in one file before its match arm lands in
another:

- `6e6a1a1` (adds `GenPhase::ServerBusy`) — non-exhaustive match in
  `ui_state.rs`, not yet committed.
- `65cd320` (adds `Event::ServerBusy`) — non-exhaustive match in
  `main_loop.rs`, not yet committed.

`e9a520a` onward compiles. This is the inherent cost of
one-commit-per-file for an enum+handler feature; HEAD is correct.
If bisectability matters, squash `6e6a1a1..=e9a520a` (the 5 TUI
commits) into one — but that rewrites history and was not done.

---

## 4. Deferred MEDIUMs — M-13 and M-16 only

Both in `crates/kod-core/src/engine/mod.rs`, both the "transcript
integrity" refactor the audit groups together.

### M-13 · F2c-4 — mid-stream error discards the partial text/calls
`partial_error` is carried on `StreamRoundOutcome` (field at ~1550);
the caller (~10821) logs it but still `return Err(err)`s without
persisting `text`/`calls`. The half-fix is committed; the persist
step remains.

### M-16 · F2c-7 — failed attempts duplicate tool rounds
`run_streaming_loop` persists each round's `section.messages` to the
shared history (~10946) before the fallback chain picks a winner. A
retryable failure leaves the failed attempt's rounds in the
transcript; the next endpoint appends on top. Collected loop same
shape (~10546 / ~10607).

**Coupled.** Fix together behind one `attempt_round_messages` buffer:
- Replace the immediate `hist.write()` with an attempt-local buffer.
- After the chain picks the winner (~9838 / ~9336), merge once +
  `cap_transcript`.
- On a retryable failure before the merge, drop the buffer.
- For M-13: persist `outcome.text`, run complete `outcome.calls`,
  then surface `partial_error`.

---

## 5. Traps learned this session

1. **Per-file staging is not content-safe.** `staged == "$FILE"`
   checks the file *set*, not the *content*. A file-level stage swept
   the parallel agent's `Event::ServerBusy` edit into a test-only
   commit (`74c818f`); fixed by `git reset --soft` + an `awk`-filtered
   hunk patch. When two workstreams share a file, **stage hunks**.
2. **`2509a09` left a test failing on `main`.** `cargo check` missed
   it; only the full test run caught it. Always run the suite.
3. **No worktrees.** Verification uses plain `git checkout`/diffs in
   the main tree. A linked worktree + shared `CARGO_TARGET_DIR`
   hangs on kod-tui's dep graph and leaves a stray worktree behind.

---

## 6. Next prompt is probably

"do M-13 and M-16" — the last two deferred MEDIUMs, coupled, sketch
in section 4. Or "do the LOWs" — ~34 remain, each small.

**One-commit-per-file is the user's standing rule.** It produces
non-compiling intermediates for enum+handler features; that is
accepted, not a bug to fix by squashing.
