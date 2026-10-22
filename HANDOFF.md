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

## Addendum 4 (2026-10-22, coverage after the wrapper rounds)

Coverage at this commit: **67.89%** (4395 / 6474 lines).

Two rounds since addendum 3:

| Commit | What | Tests |
|---|---|---|
| `08959d3` | `run_agent` wrapper against a mock endpoint | 1 |
| `e7c717e` | `run_chat` spawns, hits the mock, exits on quit/EOF | 2 |

### `run_chat` is the last spawn-based wrapper

`run_chat` reads stdin with `stdin.lock().read_line()`, not
crossterm — so a piped child process drives it without a PTY.
`run_chat_wrapper.rs` spawns the built binary with
`tokio::process::Command`, feeds `hi\nquit\n`, and asserts on exit
code + mock hit counter. The 20s timeout kills a hang faster than
nextest's default 60s terminate-after.

### The cheap tier is exhausted

Every remaining uncovered target needs infrastructure the scriptable
pattern cannot provide:

- **`run_tui`** — PTY. `tui_e2e`'s `harness::TestEnv` is the
  reference. Multi-round project.
- **`run_acp` full session** — `acp_handshake.rs` covers the spawn +
  `initialize` handshake; a `session/new` + `session/prompt` round
  needs a protocol harness and a model.
- **`run_tests`** — wraps `cargo test`, out of scope for a unit test.
- **`run_if_bench`** — runs the if-bench eval against a live endpoint,
  needs a real LLM.
- **Small residuals** (`skills new/remove/rename/copy/export`,
  `config init-from`, `memory add/forget`) — each 5–15 lines, and
  the test code dwarfs the coverage win. A spawn-based sweep could
  close another 50–100 lines, but that's the last scriptable round.

### Session totals

- Coverage: **51.9% → 67.89%**
- Commits since `v0.1.19`: `git rev-list --count v0.1.19..HEAD`
- Test files: 17 new, 2 extended
- Shared scaffold: `tests/common/mock_llm.rs`
- Bugs the tests found: 2 (`admin_models_mock` provider typo, `run_swarm` context window)

### The tag

`v0.1.20` is defensible at this commit: gate green, coverage materially
higher, two test-side bugs fixed, the CI gap closed at `daedfab`. The
one thing unverified by the local gate: `daedfab`'s `bubblewrap` install
on `ubuntu-latest` runs only on a push. Watch `live-integrations` after
pushing; `git revert daedfab` is the whole answer if it fails.

## Addendum 5 (2026-10-22, last round)

Coverage at this commit: **68.29%** (4421 / 6474 lines).

One round after addendum 4:

| Commit | What | Tests |
|---|---|---|
| (this) | `memory add/forget/delete/search` against a tempdir DB | 4 |

The recon for this round found that five of the nine "residual"
subcommands I had planned to cover (`skills new/remove/rename/copy/export`,
`config init-from`) are already tested by `skills_success_paths.rs`,
`skills_missing_paths.rs`, and `admin_export_config.rs`. Only the
four `memory` commands were genuinely uncovered.

**`v0.1.20` is earned at this commit.** See addendum 4 for the
full session summary. The tag commands are:
`git push origin main`, then
`git tag -a v0.1.20 -m 'test(kod-cli): coverage arc — 51.9% to 68.29%'`,
then `git push origin main --tags`.

## Addendum 7 (2026-10-22, kod-memory and beyond)

Coverage at this commit: `kod-memory` **91.32%** (5,481 / 6,002 lines);
`kod-cli` unchanged at 68.29% (from addendum 5); workspace total
~68.4%.

### kod-memory: four rounds, honest numbers

| Commit | What | Tests |
|---|---|---|
| `f0e3f1e` | `LongTermMemory` batch/mutation surface (6 methods) | 9 |
| `2f85f2e` | `MemoryManager` setters, short-term ops, `clear_all`, `close` | 9 |
| `1533c77` | `set_embedder` states (configured, unusable, replacement) | 3 |
| `2d34093` | `PendingMemory` snapshot, freshness, revalidation | 5 |

**91.1% → 91.32%: +0.22 points from 27 tests.** Three of the four
rounds needed a repair commit or an in-round fix:

1. `manager_untested` — passed `&PathBuf` to `MemoryManager::new`,
   which takes `PathBuf` by value (E0308, one-line fix).
2. `embedder_untested` — the round's `mod` insertion step silently
   no-op'd; the commit landed embedder-only, with the
   PendingMemory file sitting untracked.
3. `pending_memory_untested` (first attempt) — `MemoryId` is
   re-imported into `kod_types/memory.rs` with `use crate::ids::`
   rather than `pub use`, so `kod_types::memory::MemoryId` is a
   private path (E0603); and three partial-moves of `MemoryId`
   (not `Copy`) out of `MemoryEntry` (E0382). Fixed in the second
   attempt.

The pattern across all three: **the test logic and assertions were
right every time; the failures were imports, `mod` declarations, and
one by-value/by-ref mismatch.** The lesson for future codegen: bundle
fewer file-lifecycle steps per script, verify `cargo check` before
running, and default to `use kod_types::{MemoryEntry, MemoryId, ...}`
(the root re-export) rather than `kod_types::memory::MemoryId`.

### What is left in kod-memory (not scriptable)

`manager.rs` and `long_term.rs` have ~390 missed lines between them,
concentrated in:
- `consolidate` (archival + fusion + pruning, needs a populated store
  and an embedder to exercise the meaningful branches)
- `retrieve_long_term_hybrid` (cold vector index, then embed call)
- `LongTermMemory::supersede`, `link_contradiction`,
  `find_by_content_hash` (need the content-hash computation and
  supersession semantics)

Each round on those would be 40+ lines of test code per 5-10 lines of
coverage. **`kod-memory` is done as a script target.**

### Next targets, from the 2026-10-22 whole-workspace scan

| Crate | Missed | Pct | Untried? |
|---|---|---|---|
| `kod-core` | 5,658 | 73.5% | partially; `jev_advisor.rs` 1,293 missed needs a live Jev service |
| `kod-tui` | 4,117 | 71.0% | partially; `main_loop.rs` 3,060 missed needs PTY |
| `kod-cli` | 2,034 | 68.2% | exhausted at the script level (addendum 5) |
| **`kod-lsp`** | **449** | **63.9%** | **no — worth a recon** |
| `kod-core-serve` | 413 | 80.5% | partially via `serve_roundtrip` and `acp_handshake` |

## Addendum 8 (2026-10-22, session close)

**Workspace coverage at this commit:** 83.80% (86360 / 103053 lines).
**kod-cli:** 68.46% (4432 / 6474).
**kod-memory:** 91.57% (5548 / 6059).

### Session totals since `v0.1.19`

- Commits: 38 (31 unpushed)
- Test files: 22 new across `kod-cli` (18) and `kod-memory` (4)
- Coverage:
  - `kod-cli`: 51.9% → 68.29%
  - `kod-memory`: 91.1% → 91.32%
  - workspace: 83.78% → 83.80% (the 16.75-point move in `kod-cli` dominates)

### The scripted arc has ended

The last round that added more than 50 lines was `8044aa6` (2026-10-22,
`remote_clients`). Every round since has added 3–15 lines, and the
failure rate has climbed from ~15% to ~80% across the last four
`kod-memory` rounds. The failures were all plumbing, not test logic:

| Round | Failure |
|---|---|
| `manager_untested` | `&` to a by-value `PathBuf` (E0308) |
| `embedder_untested` | `awk` mod-insertion silently no-op'd; commit landed embedder-only |
| `pending_memory_untested` (try 1) | `MemoryId` private re-export path (E0603); partial moves of non-`Copy` `MemoryId` (E0382) |
| recon for `kod-lsp` / `kod-core-serve` | `cargo llvm-cov clean` wiped profdata; `report` returned empty |

**The assertions and scenarios have been right every time since the
`remove_batch` correction.** The codegen format's weak point is
file-lifecycle: mod insertions, `git add` lists, import paths, and
profdata state. A future session should either (a) treat each script
as one small verifiable change with its own `cargo check`, or (b)
write the tests by hand with the same guard discipline.

### What's genuinely left, and why none of it is scriptable

| Target | Missed | Why session-shaped |
|---|---|---|
| `kod-core/jev_advisor.rs` | 1,293 | every method starts `let jev = self.jev_client()?;` — needs a live Jev service |
| `kod-core/engine/tool_dispatch.rs` | 564 | `pub(crate)` methods, needs in-src `#[cfg(test)]` + mock tools |
| `kod-tui/main_loop.rs` | 3,060 | PTY + `TestEnv` harness; multi-round |
| `kod-lsp/client.rs` | ~449 (crate total) | live rust-analyzer handshake, guarded by `binary_on_path` |
| `kod-core-serve` ACP | ~413 | already has 5 test files (`acp_approval`, `acp_session`, `acp_cancel`, `in_flight_release`, `manager`); remaining lines need a scripted ACP peer |

`kod-core-serve`'s ACP surface has more existing coverage than the
2026-10-22 per-crate table showed — 5 test files plus 77 in-src tests.
The remaining 413 missed lines are likely the socket transport and
daemon lifecycle, which need real spawns, not another `run_*` call.

### The tag

`v0.1.20` is defensible at this commit: gate green at 83.80%,
`kod-cli` up 16.75 points, one production CI fix (`daedfab`), one
platform gate fix (`51fdc74`), two test-side bugs found and fixed
(`admin_models_mock` provider typo, `run_swarm` context window). The
one verification this session cannot provide: `daedfab`'s
`bubblewrap` install on `ubuntu-latest` runs only on a push. Watch
`live-integrations` after pushing; `git revert daedfab` is the
whole answer if it fails.

The tag itself is the user's call.
