# kod — current handoff

**Supersedes:** the v0.1.16-era content that was on disk before 2026-10-22
(the file carried a v0.1.19 header over a v0.1.16 body; `0287c95` and `47.3%`
were both stale).

**Tag:** `v0.1.19` (`70838fd`), on origin.
**Branch:** `main`, 4 commits ahead of `origin/main` from this session, plus
this handoff commit.
**HEAD:** `1bb410e` (plus this commit).
**Tree:** clean.
**Gate:** full-workspace gate runs in the same script as this file; its
result is the summary of record. Crate-local greens at `1bb410e` were
achieved four times.
**`kod-cli` coverage:** 56.35% lines (3,648/6,474), up from 47.3% at
`v0.1.19`.

## Step 0 — verify before touching anything

    git log --oneline -8
    git status --porcelain --untracked-files=all
    git describe --tags --abbrev=0
    git rev-list --count origin/main..HEAD

Expect HEAD at `1bb410e` or later, clean tree, tag `v0.1.19`, **4 ahead**
(or more if further rounds landed).

## Session 2026-10-22 — four test-only commits

No production change. 19 tests across four files (two new, one extended
with two more tests, one extended with one more test).

| Commit | What | Tests |
|---|---|---|
| `bee9c6b` | `jev stats` populated, `skills edit`/`search`, `config edit` | 6 |
| `321a011` | `run_prompt_remote` daemon error/done frames via one-shot `UnixListener` | 4 |
| `073e69d` | `run_fixture_replay` bootstrap, `serve --stop` no-daemon, `sandbox check` | 5 |
| `1bb410e` | `swarm` merge-synthesis + 2-agent fan-out, `chat_remote` guard | 4 |

New files: `tests/it/jev_edit_search.rs`, `tests/it/prompt_remote_daemon.rs`,
`tests/it/fixture_replay_serve.rs`, `tests/it/chat_remote_guard.rs`.
Extended: `tests/it/swarm_cli_mock.rs` (two tests appended, no rewrite).

## Coverage curve — with the honest flattening

| Commit | Covered / 6,474 | Pct | Delta |
|---|---|---|---|
| `eacab9b` (pre-session) | 3,363 | 51.9% | — |
| `bee9c6b` | 3,435 | 53.06% | +1.16 |
| `321a011` | 3,539 | 54.66% | +1.60 |
| `073e69d` | 3,635 | 56.15% | +1.49 |
| `1bb410e` | 3,648 | 56.35% | **+0.20** |

The curve has flattened sharply. Rounds 1-3 closed 60-100 lines each;
round 4 closed 13. **Realistic next-round cost is 20-80 lines**, not
the ~200 the previous handoff projected.

## What the recon rounds established

- **`read_traces` is `pub`** at `kod-core-state/src/trace_writer.rs:55`,
  re-exported as `kod_core::read_traces` at `kod-core/src/lib.rs:38`.
  The trace struct is **`TurnTrace`** (not `TraceTurn`);
  `ToolCallTrace` and `ToolOutcomeKind` live in
  `kod-core-state/src/trace.rs`. A copyable construction is at
  `kod-core/tests/it/swarm_runner.rs:1563`.
- **`run_fixture_save` happy path** (read traces -> build
  `RoundFixture`s carrying `tool_calls`/`tool_results` -> `save_to`)
  is testable with the `KOD_FIXTURES_DIR` seam. ~40-60 lines.
- **`run_fixture_save_v2 --force`** success path is untested; the
  refuse-without-force arm is covered.
- **`run_fixture_replay` strict tail** (mismatched `request_hash`
  -> `Ok(1)` or `Err` per `strict`) is untested; needs a hand-written
  fixture with a wrong hash.
- **`run_chat_remote`** is an interactive REPL — only the
  `#[cfg(unix)]` nonexistent-socket guard is testable without stdin
  (now covered). The rest needs a PTY.
- **`swarm.rs`'s 374 missed lines** sit in the event-print `match`
  (worktree, retry, boundary-violation events) — needs a mock that
  fails once then succeeds, not a scripted happy path.
- **`mod.rs` dispatch (4,117 lines)** still has no clean seam.
  A 100% goal remains four sessions out.

## Next session, ranked by ROI

1. **`run_fixture_save` happy path + `save_v2 --force`** — one file,
   ~80 lines, closes ~50.
2. **`run_fixture_replay` divergence** — needs a bad-hash fixture,
   ~40 lines.
3. **`swarm` retry/boundary events** — needs a failing-then-succeeding
   mock; harder.
4. **`run_chat` PTY round** — `tui_e2e.rs`'s `harness::TestEnv` is the
   pattern. Largest target, opens a multi-round session.
5. **`run_serve` full daemon** — needs a shutdown handle; likely a
   production seam first.

## Verification policy (carried forward)

- **Production fixes** and shared-crate changes -> **full workspace
  gate**: `cargo nextest run --workspace --locked --no-fail-fast`
  plus `cargo test --doc --workspace --locked` (see
  `.github/workflows/ci.yml`).
- **Test-only rounds in `kod-cli`** -> `cargo check -p kod-cli --tests`
  plus `cargo nextest run -p kod-cli --test it <subset>`.
- **Before any tag** -> full workspace gate.

A crate-local proptest pass is **not** verification (different seeds
per process).

## The tag decision

The previous handoff named **6+ commits** as the cadence bar for
`v0.1.20`. This session delivered **4**. **Do not fire `v0.1.20` yet.**
Two more test-only rounds (the fixture-save round plus one more) reaches
6, then a full gate and tag.

The user's call, not this file's.

## Session-arc totals (since `v0.1.16`)

- **27 commits** (20 pre-session + 4 this session + this handoff)
- **19 test files**, ~134 tests
- **0 production changes this session**
- Coverage: `kod-cli` **29.9% -> 56.35%**
