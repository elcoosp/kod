# Session handoff — kod (2026-10-03)

**Repo:** `/Users/adm/Documents/Repos/kod`  **Branch:** `main`  **HEAD:** `76e7b6a4`
**Host:** macOS aarch64, rustc 1.98.1, edition 2024

Full workspace lib suite: **3571/3571 green**. Remote in sync.

---

## 1. TL;DR

Two workstreams since the last handoff:

1. **Decomposed `crates/kod-core/src/engine/mod.rs`** from 19,275 lines
   to 3,187 across 36 files.
2. Confirmed the audit LOW/perf backlog state (see §3).

---

## 2. Engine decomposition (4 commits)

| Commit | Change |
|---|---|
| `f62e16df` | stream-marker protocol -> `engine/markers.rs` |
| `51dff848` | tool/turn/usage markers -> `markers.rs` |
| `fda4a414` | 17 test modules -> `engine/*_tests.rs` (-3,956 lines) |
| `76e7b6a4` | `impl KodEngine` -> 16 responsibility modules (-12,148) |

The 16 impl modules are named for what they hold: `compaction`,
`settings`, `plans`, `swarm`, `state`, `lsp`, `policy`, `jev`,
`routing`, `lifecycle`, `process`, `agent_loop`, `tool_dispatch`,
`memory`, `control`, `transcript`. Each is an `impl KodEngine` over
methods moved byte-for-byte; methods are `pub(crate)`, and
`use super::*` gives them the engine's private fields.

**What remains in `mod.rs` (3,187 lines):** the `KodEngine` struct
(~580 lines of fields), the small protocol types, `diag_key` /
`render_diagnostics` / `BaselineRefresher`, and the module
declarations. Further splitting is diminishing returns.

---

## 3. Audit backlog — what actually remains

### All MEDIUMs closed
The 59-finding MEDIUM table is exhausted. HIGHs and CRITICALs done in
earlier sessions.

### Perf LOWs — DONE
`1fdf0524` F2e-6 (safe_cutoff O(n log n)), `fe64219c` F2a-13 (regex
cache), `026d603e` F2g-9 (completion dir cache), `9943ed0c` F2b-11
(instruction cache), `96d3fc8e` F2c-11 (spawn_blocking read),
`c418f00a` F2c-9 (spool sweep).

### Structural LOWs — DONE
`b05f5f6e` F2c-8 + F2c-10 (approval-sender eviction, cancel all keys),
`b516d394`+`aa1309be` F2d-11 (failed dep gates dependents),
`cb5d705f`+`9b16bd65` F2i-8 (complete() retries 503),
`a0d80403` F2h-12 (round-robin least-loaded), `2899779e` F2h-15
(waiter Drop guard), `87b7d068` F2c-12 (secret-vector cache).

### Still open (verified by scan, not fixed)
| ID | File | Finding |
|---|---|---|
| F2d-9 | `swarm_runner.rs` | error-path `?` skips bus/subscriber cleanup |
| F2f-18 | `kod-tools/src/git.rs` | `Command::output()` buffers whole stdout before truncate |
| F2g-13 | `kod-tui/app/ui_state.rs` | raw `eprint!` bell/OSC while TUI owns terminal |
| F2g-15 | `kod-tui/event.rs` | input task parks after `stop()` |
| F2d-13 | `kod-core/repomap.rs` | per-match full-file line count |
| F2h-18 | `kod-cli/commands/observability.rs` | replay temp dirs leak on error |
| F2i-10 | `kod-memory/manager.rs` | whole redb table deserialized per op |
| F2b-9 | `kod-config/instructions.rs` | `parse_agents_md` ignores code fences |
| F2b-10 | `kod-config/policy.rs` | `git.history_protected` has no consumers |

### F2c-12 — PARTIAL (`87b7d068`)
Obfuscation cache done. The per-round `messages.clone()` remains;
removing it needs an `Arc`/`Cow` refactor of `CompletionRequest.messages`
across the provider crates. Documented, not attempted.

---

## 4. Oh-my-pi plan remaining

From `docs/REMAINING_FROM_OH_MY_PI.md`:

- **§14.4 disposable commit agent** — needs a live model + a custom
  tool surface. Design project.
- **§14.5 items** — deferred custom tools, scan-plan tamper evidence,
  OTLP GenAI semconv, model roles + chains, RPC event-stream hygiene,
  two-phase extension load. Each needs a subsystem that does not exist.
- **§7.7 item 8 `ToolChoice`** — abort policies + non-forcing pending
  invokers remain.
- **§8 in-process shell** — DECIDED-NO.
- **§7.7 item 5 fuzzy patch** — DECIDED-NO.

---

## 5. Traps learned this session

1. **Split by responsibility, never by line count.** A line-count
   splitter produced meaningless `engine_impl_NN` names. Naming the
   modules after what they hold is the whole value.
2. **Verify each split compiles + tests before the next.** Three
   split attempts failed (missing imports, method privacy, orphaned
   doc comments) before the responsibility split landed.
3. **`pub(crate)` for moved methods; leave test fns alone.** The
   visibility regex also hit `#[test]` fns — harmless but wrong.
4. **Rewrite unpublished commits.** `engine_impl_NN` was rewritten
   out of history with `reset --soft` because it was never pushed.
5. **The parallel agent races `.git/index.lock`.** Retry, do not
   delete the lock.

---

## 6. Environment

```bash
ulimit -n 65536
export CARGO_BUILD_JOBS=6
```

Without these, `cargo check` fails with a fake EAGAIN. No `timeout`
binary on macOS — background + `kill` for a bound.

---

## 7. Next

Pick any from §3 "Still open" (9 items, each self-contained) or §4
(design projects). The F2i-10 memory-table deserialize is the
biggest perf win still on the table.
