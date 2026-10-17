# Handoff — kod codebase, session 12

## TL;DR

Continuation of session 11's bug-hunt arc, driven by an external
full-codebase audit report (78 findings). **29 of the 78 were fixed and
verified**; the rest are enumerated below with reasons. HEAD moved
v0.1.6 (`de557e6`) to `573fb79`. Tests: **4319 passing, 3 skipped**.

The audit's P0 security cluster — risk-gate bypasses, the
background-exec sandbox/env leak, and the `grep` read-protection leak —
is fixed. The remaining ~49 are correctness/perf/polish, several of
which need a design decision rather than a mechanical patch.

## Current repo state

| | |
|---|---|
| Repo | `/Users/adm/Documents/Repos/kod` |
| Branch | `main` |
| HEAD | `573fb79` |
| Working tree | clean |
| Clippy | 0 warnings (workspace, all-targets, `-D warnings`) |
| `cargo fmt --check` | clean |
| Tests | **4319 passing**, 3 skipped, 0 leaky |
| Tags | `v0.1.0` … `v0.1.6` |
| Disk | ~15 GiB free |
| Commits since v0.1.6 | 25 |

## Environment quirks (sessions 10–12)

1. **Six `watchexec` instances watch `wr1.sh`** (one per repo the
   operator runs). Every write of the script fires all six. Any script
   that must run once — a proof, a commit — needs an atomic `mkdir`
   lock; `flock(1)` is absent on macOS.
2. **Concurrent git is the sharpest hazard this session.** Six parallel
   runs committing at once corrupted `.git/index` (truncated it to 0
   bytes) and produced duplicate commits with wrong messages. Recovery:
   `rm -f .git/index.lock && git read-tree HEAD` — the object store and
   `HEAD` are intact, only the index is rebuilt. **Commit one file per
   invocation** with a lock-retry loop; never batch `git add`/`commit`
   across many files under the watcher.
3. **Big heredocs arrive truncated.** Long single-script file writes
   have failed repeatedly (session 11) — write long files in verified
   chunks or one moderate heredoc, and always check `wc -c` after.
4. The rest of session 10/11's quirks hold: `kache` wrapper, never
   `cargo clean` (use `rm -rf target`), stale `.git/index.lock`, no
   `timeout(1)`, `--no-fail-fast`, `--test it`.

## What was fixed (29, all verified in HEAD + green suite)

### Risk gate (P0/P1) — `kod-risk`
- **SEC-A-01** — tool aliases (`bash`, `exec`, …) bypassed the risk
  gate, per-tool policy and taint. Canonicalized at parse time
  (`resolve_tool_name`); the alias table moved to `kod-types` so
  `kod-provider`'s replay path shares it. **Guarded by a source lint**
  (`tool_calls_are_canonicalized_at_parse`).
- **SEC-A-02** — `run_in_background` spawned an unsandboxed shell with
  the full secret-bearing env and spooled output into the transcript.
  Now shaped like the foreground: secret strip + env policy + sandbox.
- **SEC-A-03/05** — backslash escapes (`\rm`), mixed-quote
  concatenation (`'r'm`), leading subshell `(`, and `$(…)`/backticks/
  `<(...)` as arguments. Raw-command scan escalates the expansion
  constructs; tokenizer handles `\` and strips quotes before matching.
- **SEC-A-06/07** — `rm -rf ~dev`/`~+` (unexpanded tilde) and
  `rm -rf <working_dir>` (equality vs the prefix shortcut).
- **SEC-A-08** — Landlock `git_readonly` claim made every command
  refuse on bwrap-less Linux. Degrade with a warning.
- **SEC-A-09** — `ln` classified only the link; a hardlink target under
  a credential store is now classified too.
- **SEC-A-11** — `run_git` inherited the full env (`git_commit` runs
  repo hooks). Secret strip + env policy.
- **SEC-A-14** — Landlock profile tempfile was 0666&~umask; set 0600.
- **SEC-A-15** — seatbelt deny list gained `~/.kube`.
- **SEC-A-16** — secret-env filter gained `PASSPHRASE` and `MYSQL_`.

### Providers — `kod-provider-anthropic`
- **SEC-B-01** — Anthropic client refused redirects (was forwarding
  `x-api-key` and the body cross-origin).
- **SEC-B-03** — `native_compact` had no request timeout; bounded.
- **TPD-01** — in-band SSE `error` events were dropped, so a truncated
  turn reported success. Recorded on the stream state; surfaced before
  delivering a partial turn.

### Engine — `kod-core`
- **BUG-C-01** — `record_turn_for`'s trim orphaned tool rows (provider
  400 on the next turn). Pair repair added.
- **BUG-C-02** — the goal loop was the third attempt loop with no
  `truncate_history_to`; retries duplicated tool rounds. Captured the
  history base; truncate per attempt.
- **BUG-C-03** — `cap_hook_output` panicked on a mid-char byte slice.
- **BUG-C-04** — `ShrinkHistory` drained without pair repair; uses the
  pair-safe `safe_cutoff`.
- **BUG-C-09** — checkpoint id validator rejected a >7-digit pid.
- **BUG-C-10** — `TurnFailure::classify` checked timeout before auth;
  a permanent 401 burned the retry chain.

### Tools — `kod-tools`
- **BUG-D-01** — `grep` ignored `[policy.read_protection]` (the
  default-on Redact leak). Mirrors `search.rs`: refuse/skip/redact.
- **BUG-D-02** — `resolve_path` one-level fallback broke nested
  new-file writes; walks to the nearest existing ancestor.
- **BUG-D-03** — conflict write sliced before the changed-file guard;
  `get()` cannot panic.
- **BUG-D-04** — edit hashlines trimmed payload trailing whitespace.
- **BUG-D-09** — `todo` advertised no `unblock` though it handles it.
- **BUG-D-11** — `which_rg` never found `rg.exe` on Windows.
- **BUG-D-12** — `--append` skipped fsync.
- **PERF-07** — edit hoists `lines().count()` out of the per-op loop.

## What was NOT fixed (~49) — with reasons

**Need a design decision or a shared-tokenizer refactor (do not
mechanical-patch):**
- **SEC-A-04b** — unknown wrappers (`nohup`, `watch`, `setsid`, …) hide
  the inner command. Needs an extended WRAPPERS table and a
  clean=false path for unknown programs; the audit's fix sketch is a
  starting point but the false-positive surface needs review.
- **SEC-A-10** — `.git` writes via shell redirect. `kod-risk/paths.rs`
  has no `.git` concept; add a Catastrophic rule mirroring
  `context.rs::can_write`.
- **SEC-A-12** — policy `binary_candidates` repeats the tokenizer
  blind spots. Share the `kod-risk` tokenizer/de-quoter.
- **SEC-A-17** — Jev downgrade allowlist includes build tools that run
  repo code (`build.rs`, `Makefile`).
- **SEC-A-19** — speculative reads pre-gate without `can_read`.
- **SEC-A-20** — `xd://` re-gate; the feature is accidentally dead
  (permissions never plumbed) and becomes a full bypass the moment
  someone wires them.
- **SEC-A-21** — bwrap ignores `tmp_rw`; landlock ro-set lacks `/dev`.
- **SEC-A-13** — Auto-sandbox fail-open is silent.

**Network / parsing:**
- **SEC-B-02** — `web_fetch` domain allow-list bypassed by redirect
  hops (thread `allowed_domains` into `fetch_with_safe_redirects`).
- **SEC-B-04** — unbounded success-body reads (`resp.json()`).
- **SEC-B-05** — `kod-ast` walkers recurse without a depth bound.
- **SEC-B-06** — fuzz targets exist but are never run in CI.

**Engine correctness:**
- **BUG-C-05** — serve leaves the turn running on client disconnect.
- **BUG-C-06** — off-track switch keeps stale partial tool state.
- **BUG-C-07** — `truncate_history_to` doesn't clear the gauge anchor.
- **BUG-C-08** — goal-loop turns never record cost/usage.
- **BUG-C-11** — fidelity scorer feeds a raw count into 0..1 thresholds.
- **BUG-C-12** — `parse_plan_steps` drops non-string elements.

**Tools / memory / swarm / skills:**
- **BUG-D-05** — conflict write is non-atomic, unlocked, unsynced.
- **BUG-D-06** — `WorkPool` tombstone orphans queued items;
  `take_batch` resurrects tombstoned slots.
- **BUG-D-07** — `SkillLoader` hot reload leaves a ghost on rename.
- **BUG-D-08** — `query_syntax` inverts negated operators (module is
  exported with zero callers today).
- **BUG-D-10** — `TodoTool` instances sharing a list mint duplicate ids.
- **BUG-D-13** — `MemoryManager::update` leaves a stale embedding.
- **BUG-D-14** — `fuse_duplicates` persists stale tag snapshots.
- **BUG-D-15** — `grep` reports `truncated: true` at exactly the cap.

**Performance (the biggest user-visible one first):**
- **PERF-01** — TUI re-materializes the whole transcript every frame;
  the markdown cache is a FIFO that full-thrashes past 128 messages.
- **PERF-02** — grep relevance reorder is O(n²) (the fix's index map is
  built but unused).
- **PERF-03** — engine image caches unbounded.
- **PERF-04** — walk-cache hit clones the full path list.
- **PERF-05** — sync `std::fs` inside async grep/search.
- **PERF-06** — memory retrieval deserializes the whole table per query.
- **PERF-08/09/10/11** — tool-def churn, regex per call, LSP
  `ensure_open` blocking, TTSR per-chunk lock.

**TUI / CI / deps / scripts:**
- **TPD-02** — quit cleanup wired to a never-emitted `Event::Quit`.
- **TPD-03** — no SIGTERM/SIGHUP handler (raw terminal left on kill).
- **TPD-04** — `/stats` cache rate double-counts cached tokens.
- **TPD-05** — fuzz targets never run in CI.
- **TPD-06** — `terminal-testlib` git dep pinned by `branch`, not `rev`.
- **TPD-07…13** — terminal partial-failure restore, panic-hook bracketed
  paste, stream retry inconsistencies, dual reqwest majors, unpinned CI
  tools, packaging-script drift, comments-only `.cargo/config.toml`.

## Where the next agent should pick up

1. **Cut v0.1.7.** 25 commits since v0.1.6, all fixes — the P0 security
   cluster plus correctness. By the session's standing rule this is due.
2. **PERF-01 (TUI render)** — the highest user-visible remaining item;
   a long session lags on every keystroke.
3. **SEC-B-02 / SEC-B-04** — the two real network findings (redirect
   allow-list bypass; unbounded body).
4. **BUG-C-05 / C-06 / C-08** — serve disconnect, off-track stale
   partials, goal-loop cost accounting.
5. **SEC-A-04b / A-10 / A-20** — the remaining gate bypasses. Each
   wants its own commit with a failing-test-first proof; do not batch.

## Gotchas

- **Commit one file per invocation under the watcher.** Concurrent
  commits corrupted `.git/index` this session. Recover with
  `rm -f .git/index.lock && git read-tree HEAD`.
- **Prove every regression test fails without the fix.** This session
  verified the alias source lint and the risk-behavior tests that way.
- **`tests/it` is a separate crate**; `#[cfg(test)]` helpers do not
  reach it — use `*_for_tests` accessors.
- **Source lints live in `crates/kod-types/tests/it/source_lints.rs`.**
  A lint that scans a window must bound it.
- **`cargo audit` / `cargo deny` change under you** (advisory-DB
  updates, not code changes).

## What NOT to do

- Do not `pkill watchexec` (the operator runs it across repos).
- Do not delete `~/Library/Caches/kache`.
- Do not run `cargo clean` — `rm -rf target` is the safe shape.
- Do not batch `git add`/`git commit` across many files under the
  watcher.
- Do not "fix" the audit's by-design narrowness: `rsync --delete`,
  `chmod`, `chown`, `git clean` classify Safe deliberately — the
  sandbox is the defense there, not the blast-radius gate.
- Do not add speculative fixes without a reproducer.
- Do not write one giant heredoc; verify file size after any long write.

---

**Where to start:** cut v0.1.7 (25 unreleased fixes), then PERF-01.
