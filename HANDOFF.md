# kod — current handoff

**Supersedes:** the version at `b89a76d`. That one was correct when
written but predates six test rounds, one CI fix, and one platform
gate fix that followed it.

**Tag:** `v0.1.19` (`70838fd`), on origin.
**Branch:** `main`, 19 commits ahead of `origin/main`, unpushed.
**HEAD:** `b22991d` (+ this handoff commit).
**Tree:** clean.
**Gate:** green at this commit (nextest workspace + doctests).
**`kod-cli` coverage:** 61.71% lines (3995/6474).

## Step 0 — verify before touching anything

    git log --oneline -8
    git status --porcelain --untracked-files=all
    git describe --tags --abbrev=0
    git rev-list --count origin/main..HEAD

Expect HEAD at `b22991d` or later, clean tree, tag `v0.1.19`,
19 ahead (or more if further rounds landed).

## Session 2026-10-22 — coverage arc

Coverage went from **51.9%** (`eacab9b`) to **61.71%** at
`b22991d`. Commits since `v0.1.19`, chronological:

| Commit | What |
|---|---|
| `bee9c6b` | jev stats populated, skills edit/search, config edit (6 tests) |
| `321a011` | run_prompt_remote daemon frames via one-shot UnixListener (4) |
| `073e69d` | run_fixture_replay bootstrap, serve --stop, sandbox check (5) |
| `1bb410e` | swarm merge-synthesis + 2-agent fan-out, chat_remote guard (4) |
| `b89a76d` | handoff rewrite (intermediate) |
| `daedfab` | CI: sandbox_containment run in live-integrations |
| `6d4a2bf` | fixture_save happy path and save_v2 --force (2) |
| `f61c9d5` | skills validate, checkpoint list/diff, trace replay error arms (6) |
| `8044aa6` | agent_remote and swarm_remote via one-shot UnixListener (8) |
| `51fdc74` | cfg(unix) gate for prompt_remote_daemon |
| `b22991d` | run_serve daemon round trip (start, stop, join) (1) |

**36 new tests.** Seven new test files; `swarm_cli_mock.rs` and
`config_remaining_paths.rs` extended.

## What got covered

- **Daemon clients**: `run_prompt_remote`, `run_agent_remote`,
  `run_swarm_remote` — done, error, and closed-stream frames against
  one-shot `UnixListener` fakes.
- **Daemon server**: `run_serve(false)` full bootstrap — `KodConfig::load_default`,
  `engine_from_config` with production install flags, `PolicyEngine::load`,
  `engine.start()`, `serve()`, graceful shutdown via `stop_daemon`.
  The DB is redirected via `[memory] long_term_db_path` — same seam
  `memory_manager_paths` uses.
- **Fixture pipeline**: save (from a `TurnTrace` written via `TraceWriter`),
  save_v2 force overwrite, replay bootstrap.
- **Observability**: jev stats populated, trace replay error arms.
- **Residual paths**: skills edit/search/validate(_strict), config edit,
  checkpoint list/diff.
- **Swarm**: merge-synthesis (single-agent, `merge_results=true`),
  two-subtask fan-out.

## The cheap tier is exhausted

Every remaining `run_*` falls into one of three buckets:

1. **Needs a PTY**: `run_chat`, `run_tui`.
   `tui_e2e.rs`'s `harness::TestEnv` is the pattern; multi-round session.
2. **Needs a real LLM endpoint**: `run_prompt`, `run_swarm`,
   `run_trace_replay` success. The `*_inner` variants already take
   `provider_override` and are tested; the wrappers do not.
3. **Out of scope**: `run_tests`, `run_if_bench` (cargo/bench wrappers),
   `run_config_show_merged` (14 lines).

`mod.rs`'s 4,117-line clap dispatch is the largest remaining single
target; each arm delegates to an already-tested `run_*`. A `--help`
snapshot per arm would close ~500 lines but is tedious.

## Verification policy

- Production fixes and shared-crate changes → full workspace gate
  (`cargo nextest run --workspace --locked --no-fail-fast` plus
  `cargo test --doc --workspace --locked`).
- Test-only rounds in `kod-cli` → `cargo check -p kod-cli --tests`
  plus `cargo nextest run -p kod-cli --test it <subset>`.
- Before any tag → full workspace gate.

## The tag decision

`v0.1.20` is **defensible** at this commit: 19
commits since `v0.1.19`, gate green, coverage **51.9% → 61.71%**,
one production CI fix (`daedfab`), one platform gate (`51fdc74`),
and this handoff accurate. The user's call.

## Session-arc totals (since `v0.1.16`)

- Coverage: `kod-cli` **29.9% → 61.71%**
