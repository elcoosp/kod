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

## Addendum 3 (2026-10-22, session close)

Coverage at the final commit: **66.25%** (4289 / 6474 lines).
Two further rounds after addendum 2:

| Commit | What | Tests |
|---|---|---|
| `473e47b` | refactor: extract shared `MockLlm` into `tests/common` | — |
| `578eafc` | `run_swarm` wrapper against a scripted mock endpoint | 1 |
| (this) | handoff addendum 3 | — |

### The mock HTTP scaffold is now shared, and it pays off

`tests/common/mock_llm.rs` is one axum server with `GET /v1/models`
and `POST /v1/chat/completions` (SSE + non-streaming, branched on the
request's `stream` field) plus a hit counter. Two callers:

- `kod_prompt_mock.rs` — `run_prompt`, `run_streaming_prompt`
- `run_swarm_wrapper.rs` — `run_swarm`

`kod_prompt_mock.rs` shrank from 287 to ~130 lines with the same four
tests; the `run_swarm` round that followed was ~110 lines of test
code and one fix (the default `KodConfig` context window is too small
for the swarm's tool schemas; set `ep.context_window = 32768`, the
same value `swarm_cli_mock.rs` and `agent_mock.rs` use).

Deliberately NOT extracted: `admin_models_mock` (its `/v1/models`
handler is configurable per test) and `tui_e2e/mock_llm` (PTY-specific
with `MockStep` scripting for tool calls). Their handlers have
different jobs.

### Two bugs the tests found

1. `admin_models_mock.rs` used `provider = "openai_compatible"`
   (underscore). The parser accepts `openai-compatible` (hyphen); with
   the typo the whole `[[llm.endpoints]]` block was dropped and the
   provider fell back to `localhost:11434`. The test passed anyway
   because whatever answered there returned a plausible response.
   Fixed at `476d73f`.
2. `run_swarm`'s default `KodConfig` context window (6144-char prompt
   budget) is too small for the swarm's tool schemas (~9700 chars).
   The `run_swarm` test hit this as `Invalid tool parameters: the
   request is too long for this model`. Fixed by setting
   `context_window = 32768` in the test config (`578eafc`).

### Remaining uncovered targets (each is a session opener)

- `run_chat` / `run_tui` — PTY. `tui_e2e.rs`'s `harness::TestEnv`
  is the reference; the `MockLlm` scaffold (now shared) is what the
  mock LLM endpoint should be.
- `run_acp` — stdin/stdout JSON-RPC framing. Needs a protocol harness.
- `run_agent` full path — mostly covered by `agent_mock.rs`; the
  wrapper itself is a 15-line function like `run_prompt` and could use
  the shared `MockLlm` the same way, ~30 lines.
- `run_tests` / `run_if_bench` — cargo/bench wrappers. Out of scope.

### Note on the last two amend fixes

Both `7990494` and `578eafc` were amended to include
`crates/kod-cli/tests/it/main.rs` — the `mod` declaration for the new
test file was written by a step that ran after `git add` had already
staged only the test file. Every codegen round should `git add` both
files unconditionally.

### Tag

`v0.1.20` is defensible at this commit: 20 commits since `v0.1.19`
(18 pre-session + 2 rounds beyond the first handoff), gate green,
coverage 51.9% → 66.25%, two test-side bugs found and fixed, the
mock HTTP scaffold extracted and shared. The user's call.
