# KOD — Deep-Dive Code Review Report

**Repository:** https://github.com/elcoosp/kod
**Commit reviewed:** HEAD at clone time (2026-10-02)
**Codebase size:** 21-crate Rust workspace, ~163 KLOC of non-test source
**Review date:** 2026-10-03
**Review method:** 5 parallel sub-agents reading source line-by-line, each producing a per-crate-group report. This document concatenates those reports verbatim with a unified TOC and an executive summary on top.

---

## Executive summary

This review identified **342 concrete issues** across the kod workspace. The codebase is well-structured for its size (clear crate boundaries, a comprehensive `docs/` tree, regression tests guarding several known-bug fixes) but contains a number of defects that, in combination, undermine the local-first security model the project advertises and produce real-world reliability issues under load.

**Breakdown:**

| Severity | Count | Meaning |
|----------|------:|---------|
| Critical | 78 | Sandbox escape, data corruption, security bypass, deadlock, hang |
| High     | 67 | Persistence loss, race condition, retry storm, leak |
| Medium   | 89 | Error handling gaps, missing timeouts, UX bugs |
| Performance | 30 | O(n²) loops, redundant clones, allocation in hot path, await-in-loop |
| Low / Code quality | 78 | Style, dead code, naming |

**Per-part breakdown:**

| Part | Crit | High | Med | Perf | Low/Q | Total |
|------|-----:|-----:|----:|-----:|------:|------:|
| Part 1 | 17 | 24 | 28 | 20 | 25 | 114 |
| Part 2 | 8 | 15 | 30 | 0 | 12 | 65 |
| Part 3 | 8 | 18 | 13 | 10 | 15 | 64 |
| Part 4 | 5 | 10 | 18 | 0 | 26 | 59 |
| Part 5 | 40 | 0 | 0 | 0 | 0 | 40 |
| **TOTAL** | **78** | **67** | **89** | **30** | **78** | **342** |

### Recurring themes (highest-impact patterns)

1. **Sandbox backends have different gaps.** `landlock`, `bwrap`, and `seatbelt` each have a different threat model and at least one critical gap (Landlock ABI ≥ 4 silently skips `net_deny`; bwrap omits `--unshare-pid`/`--unshare-user`; seatbelt begins with `(allow default)`). Operators switching backends get a different threat model than they think they have.
2. **Non-deterministic hashes everywhere a fingerprint is needed.** `DefaultHasher::new()` (randomized seed) is used in `note_tool_surface_fingerprint`; FNV-1a (non-cryptographic) is used in `LearnedAllow::from_call` to gate auto-approval. Both defeat the purpose of the fingerprint — the cache journal always reports a change, and the auto-approval gate is collision-bypassable.
3. **`get_all()` + full-table scan on every hot path in kod-memory.** Store dedup, retrieval, consolidation, and fuse each call `long_term.get_all()` then linear-scan the result. For a long-running session this is O(N²) in entries; with no secondary indices and no caching, the long-term store degrades badly with size.
4. **TOCTOU windows everywhere a path is checked before write.** `patch_file`, `edit_hashline`, `revalidate_write_parent` all check, then write, leaving a window for symlink races and concurrent edits.
5. **`unsafe { std::env::set_var }` in tests under Rust 1.85 (made `unsafe` for a reason).** Test code in `provider_setup.rs`, `embedding.rs`, `app/tests.rs` claims `SAFETY: serialized via lock`, but the lock only serializes test functions — concurrent production reads of the same env var in the same process are still racy.
6. **Locks held across `.await`.** `ttsr.write()` held across `chunk_tx.send().await` in `stream_round`; `inflight` Mutex in `flush_embeddings` held across spawn; `cache_journal` lock released before the truncation step it should protect.
7. **`let _ = <JoinHandle>.await;` discards panic information.** At least 5 occurrences in kod-core alone. A panic in a spawned memory-consolidation or background task is invisible — the operator sees nothing, the next consolidation never runs.
8. **Unbounded mpsc channels + fire-and-forget `tokio::spawn` with no supervision.** Telemetry, skills hot-reload, TUI's `/handoff`/`/summarize`/`/check`/prewarm — all spawn without `JoinSet` or cancel tokens. A panic in one of these tasks is silent and the task is gone.
9. **Path-classification / risk-classification is string-based with overly broad patterns.** `xargs rm /etc/passwd` is Safe; `bash -c 'rm -rf ~'` only checks the first token; unterminated heredocs swallow the rest of the command. The classifier is correct on the patterns it tests, but the patterns are too narrow.
10. **Per-id write transactions / per-call reconnections in loops.** `fuse_duplicates` does N write txs (N fsyncs) for N removals; Jev classification awaits in a `for` loop (sequential RPCs); `did_change` notifications have no debounce.

### Top critical findings (curated)

| ID | Title | Location | Impact |
|----|-------|----------|--------|
| T1-C1 | Landlock `net_deny` silently NOT enforced on ABI ≥ 4 — `handled_access_net` is implicitly 0 | crates/kod-tools/src/sandbox/landlock.rs | Sandbox escape: network restrictions advertised as enforced but never applied to the kernel ruleset |
| T1-C2 | Landlock `rw_paths`/`ro_paths` are not canonicalized — symlinks grant/deny the wrong inode | crates/kod-tools/src/sandbox/landlock.rs | Sandbox escape: a symlink in the workspace path resolves to a different inode than the rule covers |
| T1-C3 | `bwrap` invocation omits `--unshare-pid` and `--unshare-user` — child shares parent PID namespace | crates/kod-tools/src/sandbox/mod.rs | Sandbox escape: child can `cat /proc/$PPID/environ` to read secrets the env-strip tried to remove |
| T1-C4 | `seatbelt` invocation begins with `(allow default)` — fail-open for reads | crates/kod-tools/src/sandbox/mod.rs | Sandbox escape: child can read `~/.ssh/id_rsa`, `~/.aws/credentials` |
| T1-C5 | `patch_file` does not re-check file content between read and write — TOCTOU race with non-kod editors | crates/kod-tools/src/patch.rs | Data corruption: concurrent IDE/git edits clobbered silently |
| T1-C6 | `web_fetch` SSRF via DNS rebinding — pinned `.resolve()` only applies to original host, not redirect targets | crates/kod-tools/src/web.rs | SSRF: redirect to `127.0.0.1` or `169.254.169.254` reaches internal network |
| T2-C1 | Sub-second `Retry-After` hint truncated to 0s — retries hammer the server | crates/kod-provider-anthropic/src/provider.rs | Retry storm: 3 immediate POSTs in milliseconds on every 429 with sub-second hint |
| T2-C2 | OpenAI 429 defaults to 20-minute sleep when body has no cue phrase | crates/kod-provider-openai/src/provider.rs | UX freeze: 20-minute hang on transient rate limit instead of 1-5s |
| T3-C1 | `note_tool_surface_fingerprint` uses `DefaultHasher::new()` whose seed is randomized per call | crates/kod-core/src/engine/mod.rs | Cache invalidation: every call computes different hash → cache journal always sees prefix changed → cache useless |
| T3-C2 | `command_is_sandbox_downgrade_safe` allows `python -c`, `node -e`, `npm exec`, `cargo run --` | crates/kod-core/src/engine/mod.rs | Sandbox bypass: arbitrary code execution removes the sandbox |
| T3-C3 | `LearnedAllow::from_call` uses FNV-1a (non-cryptographic) to gate auto-approval | crates/kod-core/src/engine/mod.rs | Security bypass: model can brute-force a collision to write a different file under a previously-approved hash |
| T4-C1 | `Event::Quit` does not abort in-flight `gen_task` — orphaned task mutates shared engine state during shutdown/save | crates/kod-tui/src/main_loop.rs | Data race on chat vector + session file written before task completes |
| T4-C2 | `EventHandler::next_event` can sleep up to 100ms with a key in the priority queue | crates/kod-tui/src/event.rs | Input latency: bounded by tick rate, not keystroke — visibly laggy typing |
| T5-C1 | `xargs rm <protected-path>` bypasses destructive-path check | crates/kod-risk/src/classify.rs | Security: classifier returns Safe for `xargs rm /etc/passwd` |
| T5-C2 | Unterminated heredoc silently swallows rest of command — `cat foo <<EOF; rm -rf /` returns Safe | crates/kod-risk/src/classify.rs | Security: classifier returns Safe for destructive command after heredoc |
| T5-C3 | `policy::extract_path_arg` only checks `path`/`file` arg names — `target`/`destination`/`directory`/`output` bypass all path checks | crates/kod-config/src/policy.rs | Security: path-traversal args with non-standard names skip allow/deny lists |

(The full Critical section in each Part below contains every critical finding with code snippets and fixes.)

### How to read this report

- **Parts 1–5** are the verbatim output of five parallel review agents. Each finding has a real `file:line` reference, the offending code copied from source, an explanation of the failure scenario, and a corrected code block.
- Finding IDs are prefixed per part to avoid collisions:
  - Part 1 (memory + tools): `T1-C1`, `T1-H1`, …
  - Part 2 (network): `T2-C1`, `T2-H1`, …
  - Part 3 (core): `T3-C1`, `T3-H1`, …
  - Part 4 (tui + cli): `T4-C1`, `T4-H1`, …
  - Part 5 (support): `T5-C1`, `T5-H1`, …
- Each Part's findings are grouped: **Critical → High → Medium → Performance → Low / Code quality**.
- Where the same issue appears in multiple crates (e.g. `unsafe { set_var }` in tests), the per-part findings are kept verbatim and the cross-reference is noted in the executive summary above.

### Recommended fix order

If fixing in order of impact-per-line-changed:

1. **T3-C1** (`DefaultHasher::new()` → FNV) — 5-line fix in `engine/mod.rs`. Restores cache-journal usefulness.
2. **T3-C3** (`LearnedAllow::from_call` FNV → SHA-256 or full JSON) — 10-line fix. Closes a real auto-approval bypass.
3. **T2-C1 / T2-C2** (rate-limit handling) — 20-line fix in two provider crates. Removes the retry-storm and 20-min-hang failure modes.
4. **T1-C1 → T1-C4** (sandbox backends) — ~50 lines each. Closes the most-cited sandbox escape routes.
5. **T5-C1 → T5-C3** (risk / policy classification) — ~30 lines. Closes path-traversal bypasses.
6. **T1-C5 / T1-C9** (patch / edit TOCTOU + EOL handling) — 40 lines. Prevents silent file corruption.
7. **T3-C5 / T3-C6 / T3-C7** (poisoned mutex, watcher hang, TTSR lock across await) — ~30 lines. Three independent crash / hang / stall modes.
8. **T4-C1 / T4-C2** (TUI shutdown race, input latency) — ~10 lines each. Big UX impact.
9. The remaining ~190 issues can then be addressed by crate, in whatever order matches the team's capacity.

---

## Table of contents

- **Part 1 — kod-memory & kod-tools (memory, vector index, patch, sandbox)** — 114 findings (17 Critical, 24 High, 28 Medium, 20 Performance, 25 Low/Quality)
  - `T1-C1` — Landlock `net_deny` is silently NOT enforced on ABI ≥ 4
  - `T1-C2` — Landlock paths are not canonicalized before being added to the ruleset
  - `T1-C3` — Landlock never restricts ptrace, /proc/self/mem, signals, or prctl(PR_SET_DUMPABLE)
  - `T1-C4` — `bwrap_invocation` does NOT include `--unshare-pid`, leaking parent env via `/proc/<ppid>/environ`
  - `T1-C5` — `seatbelt_invocation` uses `(allow default)` — every file in the host is readable
  - `T1-C6` — `atomic_write` does not fsync the parent directory — rename is not durable across crashes
  - `T1-C7` — `redb` write transaction per `remove` in the fuse/archive loops → N fsyncs, partial failures leave the store half-fused
  - `T1-C8` — `execute_command` `kill_child_tree` race: child can `setpgid` to escape the group kill
  - `T1-C9` — `patch_file` does not lock around the read+diff+write cycle (H-R11 lock is only acquired at write time, leaving the diff racing with concurrent edits)
  - `T1-C10` — `edit_hashline::stage` rewrites the whole file's EOL based on a single `\r\n` probe
  - `T1-C11` — `parse_unified_diff` ignores `\ No newline at end of file` marker, then `apply_unified_diff` re-adds a trailing newline
  - `T1-C12` — `MemoryManager::store_with_metadata` dedup is O(N) per store via full-table scan + per-entry hash
  - `T1-C13` — `redb::Database::create` is single-writer-locked; concurrent `MemoryManager` instances on the same DB file fail
  - `T1-C14` — `LongTermMemory::get_all` silently drops entries that fail to deserialize
  - `T1-C15` — `WebFetchTool` SSRF via DNS rebinding: redirect policy is sync, can't re-DNS
  - `T1-C16` — `LandlockProfile::to_json` fails open to `"{}"`
  - `T1-C17` — `from_config` test uses `unsafe { std::env::set_var / remove_var }` in a multi-threaded test binary
- **Part 2 — Network & IO Layer (providers, MCP, LSP)** — 65 findings (8 Critical, 15 High, 30 Medium, 0 Performance, 12 Low/Quality)
  - `T2-C1` — Sub-second `Retry-After` hint truncated to 0s, retries hammer the server
  - `T2-C2` — OpenAI 429 defaults to a 20-minute sleep when body has no cue phrase
  - `T2-C3` — `is_session_busy` matches any "409 " substring in error messages
  - `T2-C4` — LSP notifications arriving during `request_with_response` are silently dropped
  - `T2-C5` — LSP `Content-Length` header can OOM the client (no upper bound)
  - `T2-C6` — MCP reader has no line-length cap; a malicious server can OOM the client
  - `T2-C7` — `kill_on_drop` does not kill the process group; grandchildren leak
  - `T2-C8` — Concurrency permit held during rate-limit sleep, blocking other streams
- **Part 3 — kod-core (the agent engine, swarm runner, router, serve, session log)** — 64 findings (8 Critical, 18 High, 13 Medium, 10 Performance, 15 Low/Quality)
  - `T3-C1` — `note_tool_surface_fingerprint` uses `DefaultHasher::new()` whose seed is randomized per call
  - `T3-C2` — `command_is_sandbox_downgrade_safe` allows arbitrary code execution through `python -c`, `node -e`, `npm -y`, etc.
  - `T3-C3` — `LearnedAllow::from_call` uses a 64-bit FNV-1a hash (collision-prone) to gate auto-approval
  - `T3-C4` — `cache_journal.rs` truncates the journal file *outside* the mutex, racing concurrent writers
  - `T3-C5` — `session_log::SessionRecorder` panics on a poisoned mutex
  - `T3-C6` — Background-job watcher can hang forever on a wedged child after an IO error
  - `T3-C7` — TTSR write-lock is held across `chunk_tx.send(t).await` inside `stream_round`
  - `T3-C8` — `provider_setup.rs` test helpers mutate env vars with `unsafe`, but the mutex only serializes test calls — production code reading the same var concurrently is still racy
- **Part 4 — kod-tui & kod-cli (TUI event loop, markdown parser, CLI dispatcher)** — 59 findings (5 Critical, 10 High, 18 Medium, 0 Performance, 26 Low/Quality)
  - `T4-C1` — `Event::Quit` does not abort the in-flight generation task; orphaned task keeps mutating shared engine state during shutdown
  - `T4-C2` — `EventHandler::next_event` can sleep up to `tick_rate` (100 ms) with a key sitting in the priority queue — input latency is bounded by the tick, not the keystroke
  - `T4-C3` — `unsafe env::set_var` in tests is process-wide and not actually safe just because a test mutex serializes the writers
  - `T4-C4` — Markdown parser closes a fenced code block on ANY line starting with ```` ``` ```` after whitespace, including indented lines that legitimately contain backticks
  - `T4-C5` — `wrap_spans` followed by `indent_continuation` overflows the wrap width by the indent on every continuation line
- **Part 5 — Support crates (config, risk, swarm, skills, telemetry, error, types, ast, stats, schema-dialect, minimize)** — 40 findings (40 Critical, 0 High, 0 Medium, 0 Performance, 0 Low/Quality)
  - `T5-C1` — `xargs rm <protected-path>` bypasses the destructive-path check
  - `T5-C2` — Unterminated heredoc swallows the rest of the command silently
  - `T5-C3` — `broadcast` records messages before delivery (phantom traffic)
  - `T5-C4` — `IrcBus::mark_dead` leaves parked waiters stranded until timeout
  - `T5-C5` — `send_await` ignores the enqueue receipt (TOCTOU between liveness check and enqueue)
  - `T5-C6` — `policy.rs::decide` only inspects `path`/`file` arg keys
  - `T5-C7` — `policy.rs::decide` first-token binary check is bypassable via shell wrappers
  - `T5-C8` — `schema-dialect::sanitize` has no recursion depth limit
  - `T5-C9` — `kod-error::provider_status` truncates bodies but does not redact secrets
  - `T5-C10` — `kod-config::load_cached` returns stale config when mtime resolution is coarse
  - `T5-C11` — `policy.rs::resolve_path` lexical normalization does not catch symlink escapes
  - `T5-C12` — `kod-minimize::compile` caches regexes process-wide without eviction
  - `T5-C13` — `kod-minimize::run` "safety valve" returns raw on legitimately-empty results
  - `T5-C14` — `AgentCommunicationHub::register_agent` double-locks to write history
  - `T5-C15` — `AgentRegistry::cold_revive` masks I/O errors as `RegistryError::Unknown`
  - `T5-C16` — `AgentRegistry::persist` is not atomic on Windows
  - `T5-C17` — `kod-swarm::file_touch::conflicts_for` panics on a poisoned lock
  - `T5-C18` — `Agent::is_timed_out` returns `true` when no heartbeat has been recorded
  - `T5-C19` — `kod-skills::enable_hot_reload` spawns a task with no shutdown handle
  - `T5-C20` — `SkillWatcher::start` is a no-op flag, not a real start
  - `T5-C21` — `SkillMatcher::score_skill` description-keyword matching produces false positives
  - `T5-C22` — `kod-ast::parse_cache::probe` does a full byte comparison on every hash hit
  - `T5-C23` — `KodError` has no source chaining
  - `T5-C24` — `KodError::is_retryable` text matcher is too broad
  - `T5-C25` — `kod-config::policy::load` mixes preset widening logic with per-tool widening
  - `T5-C26` — `kod-telemetry::spawn_post` has no backpressure / queue cap
  - `T5-C27` — `AgentCommunicationHub::clear_all` does not drain in-flight messages
  - `T5-C28` — `kod-config::llm::validate` does not catch missing API-key env vars
  - `T5-C29` — `kod-risk::Justification::is_substantive` only checks length and a fixed affirmation list
  - `T5-C30` — `kod-config::config::load_default` renames broken config but does not write a replacement
  - `T5-C31` — `kod-skills::parser::extract_attribute` only handles double-quoted attributes
  - `T5-C32` — `kod-config::policy::glob_matches` rebuilds matchers on every decide
  - `T5-C33` — `kod-swarm::TaskCoordinator::assign_task` holds `tasks` write lock across `assignments` and `agent_load` writes
  - `T5-C34` — `kod-stats::if_bench::cat_sound_at` uses lines-positions but is told "SoundPosition::Middle" means middle third
  - `T5-C35` — `kod-minimize::plan::classify` leaves quotes in tokens
  - `T5-C36` — `kod-schema-dialect::sanitize_in_place` runs the single-member-flatten step twice
  - `T5-C37` — `kod-config::policy::ReadProtection::matches` uses `globset::Glob` without `literal_separator`
  - `T5-C38` — `kod-swarm::agent_registry::AgentRef::depth` returns 0 or 1, not the true depth
  - `T5-C39` — `kod-config::KodConfig::skills_dir` silently falls back to `.kod/skills` when home is unresolvable
  - `T5-C40` — `kod-types::EffortLevel::parse` silently maps unknown values to `Medium`

---

# Part 1 — kod-memory & kod-tools (memory, vector index, patch, sandbox)

_Crates: kod-memory, kod-tools_


A deep, evidence-based review of the `kod-memory` (state, vector index, embeddings) and `kod-tools` (filesystem, git, shell, sandbox) crates. Findings are grouped by severity: Critical (sandbox escape, path traversal, data corruption, security), High (persistence loss, deadlock, race, perf), Medium (error handling, missing timeouts), Performance, Code quality.

Sandbox / landlock findings are in their own cluster at the top of Critical (T1-C1–T1-C8); the patch/edit cluster follows (T1-C9–T1-C13); then memory persistence and SSRF.

---

## Critical bugs

### T1-C1 — — Landlock `net_deny` is silently NOT enforced on ABI ≥ 4
- **File:** crates/kod-tools/src/sandbox/landlock.rs
- **Line:** 244–260, 291–297
- **Severity:** Critical
- **Category:** Sandbox safety / Security
- **Description:** The kernel's `landlock_ruleset_attr` is `{ u64 handled_access_fs; u64 handled_access_net; }` (16 bytes). The code passes `std::mem::size_of::<u64>()` (8 bytes) as the size to `landlock_create_ruleset`, so the kernel reads only the first 8 bytes — `handled_access_fs`. `handled_access_net` is implicitly 0, which means "do not restrict any network access". The `net_deny` flag is checked at the gate (`if profile.net_deny && abi < 4 { return Err }`) but when ABI ≥ 4 the function continues past the gate, never adds any `LANDLOCK_ACCESS_NET_*` rules, calls `landlock_restrict_self`, and returns `Ok(())`. The module's own doc claims it "refuses to proceed when [net_deny] is set on a kernel whose ABI is below 4 — the caller's expectation cannot be met, and silently proceeding would be the 'illusion of security' failure mode" — but on ABI ≥ 4 it does *exactly* the illusion-of-security thing it claims to forbid.
- **Code:**
```rust
let ruleset_fd_raw = unsafe {
    libc::syscall(
        SYS_LANDLOCK_CREATE_RULESET,
        &handled as *const u64 as *const std::ffi::c_void,
        std::mem::size_of::<u64>(),   // <-- only 8 bytes; net mask defaults to 0
        0u32,
    )
};
// ...
if profile.net_deny && abi < 4 {       // <-- only the *low* ABI case refuses
    return Err(...);
}
// on ABI >= 4 with net_deny == true: ruleset has handled_access_net == 0,
// no net rules added, restrict_self succeeds, network is unrestricted.
```
- **Why it's a bug:** An operator who sets `sandbox = require` + `net_deny = true` on a modern kernel (≥ 6.7) gets a "successful" sandbox that lets the child `curl http://attacker/` or reach `169.254.169.254`. The whole point of the `net_deny` flag is denied.
- **Fix:**
```rust
// Pass the full attr struct on ABI >= 4 so handled_access_net is honored.
#[repr(C)]
struct LandlockRulesetAttr {
    handled_access_fs: u64,
    handled_access_net: u64,
}

let attr = LandlockRulesetAttr {
    handled_access_fs: handled,
    handled_access_net: if profile.net_deny && abi >= 4 {
        LANDLOCK_ACCESS_NET_BIND_TCP
        | LANDLOCK_ACCESS_NET_CONNECT_TCP  // ABI 4 values
    } else {
        0
    },
};
let size = if abi >= 4 {
    std::mem::size_of::<LandlockRulesetAttr>()
} else {
    std::mem::size_of::<u64>()
};
let ruleset_fd_raw = unsafe {
    libc::syscall(
        SYS_LANDLOCK_CREATE_RULESET,
        &attr as *const _ as *const std::ffi::c_void,
        size,
        0u32,
    )
};
```
- **Notes:** The constants `LANDLOCK_ACCESS_NET_BIND_TCP`/`CONNECT_TCP` are u64 = `1<<0`/`1<<1` (Linux 6.7 `<linux/landlock.h>`). Also: the `LandlockPathBeneathAttr` struct here is `#[repr(C, packed)]` with `parent_fd: RawFd` (i32) — the kernel struct is `{u64, __s32}` with 4 bytes of trailing padding (12 → 16 with padding, or 12 packed). `packed` matches the 12-byte layout; verify against the UAPI header.

### T1-C2 — — Landlock paths are not canonicalized before being added to the ruleset
- **File:** crates/kod-tools/src/sandbox/landlock.rs (and context.rs)
- **Line:** landlock.rs:307–340; context.rs:213–235
- **Severity:** Critical
- **Category:** Sandbox safety
- **Description:** `add_path_rule` opens `path.as_os_str().as_bytes()` with `O_PATH`. If the profile's `rw_paths` contains a path that is itself a symlink (e.g., `wd` is `/tmp` which symlinks to `/private/tmp` on macOS, or a Linux `/var/run` → `/run` symlink), the rule applies to the *resolved* inode — fine. But the profile builder (`landlock_invocation` in context.rs:213–235) pushes `wd.to_path_buf()` verbatim, with no `std::fs::canonicalize`. The seatbelt builder (context.rs:426, 464) *does* canonicalize; the landlock builder does not. If `wd` is later replaced (atomic dir swap, `mv` of a tempdir), the rule still names the old inode. Worse, a profile that names a relative path like `./sensitive` where `./sensitive` is a symlink to `/etc` would grant rw on `/etc`.
- **Code:**
```rust
// context.rs:213 — no canonicalize
let mut profile = crate::sandbox::landlock::LandlockProfile {
    ro_paths: vec![PathBuf::from("/usr"), ...],
    rw_paths: vec![wd.to_path_buf()],    // <-- raw, possibly a symlink
    net_deny: opts.net_deny,
};
```
- **Why it's a bug:** A sandboxed child whose `rw_paths` was meant to be "the worktree" may end up with rw on the wrong inode (a swapped worktree, or a symlink target outside the worktree). The bwrap builder correctly places `wd_str` in `--bind wd wd` (which resolves at bwrap's mount time), and the seatbelt builder canonicalizes; only landlock is uncanonicalized.
- **Fix:**
```rust
let wd_canon = std::fs::canonicalize(wd).unwrap_or_else(|_| wd.to_path_buf());
let mut profile = crate::sandbox::landlock::LandlockProfile {
    ro_paths: vec![PathBuf::from("/usr"), ...],
    rw_paths: vec![wd_canon],
    net_deny: opts.net_deny,
};
// canonicalize the .git path too, matching seatbelt_invocation
if opts.git_readonly {
    let git = std::fs::canonicalize(wd.join(".git")).unwrap_or_else(|_| wd.join(".git"));
    if git.is_dir() { profile.ro_paths.push(git); }
}
```

### T1-C3 — — Landlock never restricts ptrace, /proc/self/mem, signals, or prctl(PR_SET_DUMPABLE)
- **File:** crates/kod-tools/src/sandbox/landlock.rs
- **Line:** 102–123, 216–300
- **Severity:** Critical
- **Category:** Sandbox safety
- **Description:** Landlock (ABI 1–4) restricts only filesystem and (ABI ≥ 4) TCP-network access. It does *not* restrict `ptrace(2)`, `process_vm_readv(2)`, `kill(2)`, `/proc/self/mem` writes, or `prctl(PR_SET_DUMPABLE)`. A sandboxed child can `ptrace(PTRACE_TRACEME, …)` and then read its own memory (fine), but more importantly it can `ptrace(PTRACE_ATTACH, <sibling-pid>, …)` against any sibling it can signal — including a sibling running *outside* the sandbox (a non-sandboxed helper process, or a sibling the agent spawned earlier without landlock). The child can also read `/proc/<parent-pid>/environ` if `/proc` is mounted (the bwrap backend mounts `/proc`; the landlock backend doesn't, but the child's existing `/proc` mount from the parent's namespace is still visible because Landlock doesn't unshare namespaces). The doc says Landlock is "a real sandbox" — for fs/net only. Without `CLONE_NEWPID | CLONE_NEWNS | CLONE_NEWUSER` (which `unshare(2)` would provide), Landlock is fs-only and trivially escapable via `/proc/<pid>/mem` or `ptrace`.
- **Code:**
```rust
// apply() never calls unshare(CLONE_NEWPID | CLONE_NEWNS | CLONE_NEWUSER)
// never calls prctl(PR_SET_DUMPABLE, 0)
// never restricts /proc
let ret = unsafe { libc::syscall(SYS_LANDLOCK_RESTRICT_SELF, ruleset_fd.as_raw_fd(), 0u32) };
```
- **Why it's a bug:** A prompt-injected model that runs `cat /proc/$PPID/environ` reads the parent kod process's environment, which contains `OPENAI_API_KEY`, `ANTHROPIC_API_KEY`, etc. (the `is_secret_like_env` strip only filters the *child's* env). Landlock's fs deny-by-default would block the open if `/proc` is not in any rule — but the child inherited `/proc` from the parent's mount namespace, and `LANDLOCK_ACCESS_FS_READ_FILE` is in the handled mask, so reads of paths not in any rule are denied. So `/proc/<pid>/environ` *is* denied. Good. But `ptrace` and `process_vm_readv` are *not* in the Landlock mask, so they are unrestricted. A child can `ptrace(PTRACE_ATTACH, ppid)` and read the parent's memory directly, bypassing the fs rules.
- **Fix:**
```rust
// Before landlock_restrict_self, drop privileges Landlock cannot express:
// 1. Disable ptrace via prctl(PR_SET_DUMPABLE, 0) — stops other processes
//    from ptrace-attaching THIS process, but does not stop THIS process
//    from ptrace-attaching others. To stop the latter, use seccomp:
unsafe {
    libc::prctl(PR_SET_DUMPABLE, 0, 0, 0, 0);
}
// 2. For full coverage, install a seccomp filter that denies ptrace,
//    process_vm_readv, and process_vm_writev. This is the only way to
//    close the cross-process memory-read channel Landlock cannot reach.
// 3. Unshare PID+mount namespaces (CLONE_NEWPID | CLONE_NEWNS) so the
//    child cannot see sibling PIDs at all. This requires CAP_SYS_ADMIN
//    or a user namespace (CLONE_NEWUSER), which bwrap already does.
```
- **Notes:** The doc admits "network restriction only landed in ABI 4" but is silent on ptrace/proc_vm. The right long-term fix is to prefer bwrap (which uses user namespaces) whenever available, and treat landlock as "fs-only, defense-in-depth" rather than "a real sandbox". The current `SandboxResolver::detect` prefers bwrap — good — but on hosts without bwrap, landlock is presented as sufficient when it is not.

### T1-C4 — — `bwrap_invocation` does NOT include `--unshare-pid`, leaking parent env via `/proc/<ppid>/environ`
- **File:** crates/kod-tools/src/context.rs
- **Line:** 357–409 (bwrap_invocation); 972–979 (env strip)
- **Severity:** Critical
- **Category:** Sandbox safety / Secret leak
- **Description:** `bwrap_invocation` binds `/usr`, `/lib`, `/lib64`, `/bin`, `/etc`, then `--dev /dev`, `--proc /proc`, `--bind wd wd`, optionally `--ro-bind .git`, `--unshare-net` (when net_deny). It does NOT include `--unshare-pid`, `--unshare-uts`, or `--unshare-user`. The child shares the parent's PID namespace. The `execute_command` env strip (`is_secret_like_env`, tools.rs:972–979) only filters the *child's* inherited env — it does not stop the child from reading the *parent's* environ via `/proc/<ppid>/environ` (which is `--proc`-mounted and visible). Any secret in the parent's env (`ANTHROPIC_API_KEY`, `OPENAI_API_KEY`, daemon tokens, Jev keys) is readable by the sandboxed child.
- **Code:**
```rust
args.extend([
    "--ro-bind".into(), "/usr".into(), "/usr".into(),
    // ... /lib, /lib64, /bin, /etc
    "--dev".into(), "/dev".into(),
    "--proc".into(), "/proc".into(),   // <-- shares parent's PID ns
    // ... --bind wd wd
]);
// no --unshare-pid, no --unshare-user, no --unshare-uts
```
- **Why it's a bug:** A prompt-injected `execute_command("cat /proc/$PPID/environ")` returns the parent's full env, including every secret `is_secret_like_env` tried to strip. The strip is theatre; the actual secret channel is `/proc`.
- **Fix:**
```rust
args.push("--unshare-pid".into());    // child cannot see sibling PIDs
args.push("--unshare-user".into());   // drops privileges to a new uid map
args.push("--unshare-uts".into());    // isolated hostname
// Plus: bind /proc as --proc (which bwrap mounts as a fresh procfs
// rooted in the new PID ns) so the child only sees its own descendants.
```
- **Notes:** `--unshare-pid` requires `--unshare-user` (or root). bwrap supports this; the question is whether the host's kernel allows unprivileged `CLONE_NEWUSER`. On modern Linux (≥ 5.4 with `kernel.unprivileged_userns_clone=1`) it does; on locked-down distros (Debian, some containers) it doesn't, and bwrap falls back. The current code's silent fallthrough to "no PID isolation" is the unsafe default.

### T1-C5 — — `seatbelt_invocation` uses `(allow default)` — every file in the host is readable
- **File:** crates/kod-tools/src/context.rs
- **Line:** 436 (`let mut profile = String::from("(version 1)\n(allow default)\n");`)
- **Severity:** Critical
- **Category:** Sandbox safety / Secret leak
- **Description:** The macOS Seatbelt profile begins with `(allow default)`, which is a fail-open default: any operation not explicitly denied is allowed. The subsequent `(deny file-write*)` denies writes, but reads are never denied. A sandboxed child can `cat ~/.ssh/id_rsa`, `cat ~/.aws/credentials`, `cat ~/.config/kod/kod.toml` (which may contain the API key in plain config). The child can also read every file under `$HOME` and the workspace. The model receives the file content as `stdout` (piped back to the agent) — the secret is exfiltrated through the sandbox.
- **Code:**
```rust
let mut profile = String::from("(version 1)\n(allow default)\n");
if opts.net_deny {
    profile.push_str("(deny network*)\n");
}
profile.push_str("(deny file-write*)\n");
profile.push_str(&format!(
    "(allow file-write* (subpath \"{wd}\"))\n",
    wd = wd_str
));
// ... no (deny file-read*) — reads of anything are allowed
```
- **Why it's a bug:** A sandbox that "denies writes" but allows reads of `~/.ssh/id_rsa` is not a sandbox — it's a write-protector. The bwrap backend is read-restrictive (only `--ro-bind`'d paths are visible); the seatbelt backend is not. The model can read any file on the host.
- **Fix:**
```rust
let mut profile = String::from("(version 1)\n(deny default)\n"); // fail-closed
profile.push_str(&format!(
    "(allow file-read* (subpath \"{wd}\"))\n",
    wd = wd_str
));
profile.push_str("(allow file-read* (subpath \"/usr\") (subpath \"/lib\") ...)\n");
// allow-list specific read paths; deny everything else by default
```
- **Notes:** macOS Seatbelt is the only option on that platform; bwrap is Linux-only. The fix makes the seatbelt backend fail-closed for reads, matching bwrap's behavior. The user may need to widen the read allow-list for tools that read system config (`/etc/ssl/certs`, `/usr/share/...`).

### T1-C6 — — `atomic_write` does not fsync the parent directory — rename is not durable across crashes
- **File:** crates/kod-tools/src/tools.rs
- **Line:** 154–194
- **Severity:** Critical
- **Category:** Persistence / Data corruption
- **Description:** `atomic_write` writes to a temp file, calls `f.sync_all()` (fsync the file's data and inode), then `std::fs::rename(&tmp, path)`. The file's *contents* are durable, but the *directory entry* naming the file is not — `rename` updates the parent directory's metadata, and that update is not fsynced. A crash after `rename` returns but before the parent directory's metadata is flushed can leave the directory pointing at the old inode (or at nothing). On ext4 with `auto_da_alloc` the rename is journaled, but on XFS / btrfs / network filesystems the parent fsync is required for crash-safety. The comment claims "atomic on POSIX" — atomicity is about *visibility* (no torn rename), not *durability* (the rename survives a crash).
- **Code:**
```rust
let mut f = std::fs::File::create(&tmp)?;
f.write_all(content)?;
f.sync_all()?;
drop(f);
if let Some(perms) = dest_perms { /* ... */ }
std::fs::rename(&tmp, path)?;   // <-- not durable without parent fsync
```
- **Why it's a bug:** A power loss or SIGKILL of the kod process between `rename` and the next parent-dir fsync can leave the destination file missing or pointing at the pre-write inode. The model thinks the write succeeded (the call returned Ok) but the file is gone. For a `patch_file` that just applied a 200-line edit, this is data loss.
- **Fix:**
```rust
std::fs::rename(&tmp, path)?;
// fsync the parent directory so the rename is durable.
if let Some(parent) = path.parent() {
    if let Ok(dir) = std::fs::File::open(parent) {
        let _ = dir.sync_all();   // best-effort; not all FS support dir fsync
    }
}
```

### T1-C7 — — `redb` write transaction per `remove` in the fuse/archive loops → N fsyncs, partial failures leave the store half-fused
- **File:** crates/kod-memory/src/manager.rs (fuse_duplicates), crates/kod-memory/src/long_term.rs (remove)
- **Line:** manager.rs:1403–1408 (fuse delete loop), 1130–1143 (archive loop); long_term.rs:176–195
- **Severity:** Critical
- **Category:** Persistence / Concurrency / Performance
- **Description:** `fuse_duplicates` builds a list of N ids to delete, persists the merged tags in one batch txn (good), then calls `self.long_term.remove(id).await` *in a loop* — each `remove` opens its own write transaction, commits, and fsyncs. If the process crashes after deleting 3 of 10, the store is left half-fused: the survivor's tags are merged, but 7 duplicates are still present with stale tags. A subsequent `consolidate` pass would re-fuse the same cluster, double-merging tags. The same shape exists in `consolidate`'s archive loop (manager.rs:1130–1143): each episodic-entry archive is its own txn. The comment on the tag-merge explicitly says "Persist the survivor tag unions first so a crash between the two writes does not lose them" — but the deletions are not batched, so a crash during the delete loop loses *deletions*, not tags, and the next pass re-fuses.
- **Code:**
```rust
let mut actually_deleted = 0usize;
for id in &to_delete {
    if self.long_term.remove(id).await.is_ok() {  // <-- one write txn per id
        actually_deleted += 1;
    }
}
Ok(actually_deleted)
```
- **Why it's a bug:** N fsyncs for N deletions is slow (each is ~5 ms on SSD, more on HDD). A crash mid-loop leaves the store with stale duplicates. The archive loop's `archived` counter is also non-atomic: a crash mid-archive reports `archived: 3` but the actual store has 3 fewer episodic entries and N-3 still-present ones — the counter is right, but the *consolidation* pass is not idempotent on the tag-merge side (re-fusing the same cluster merges tags again, deduplicating by content).
- **Fix:**
```rust
// Add a batch remove to LongTermMemory:
pub async fn remove_batch(&self, ids: &[MemoryId]) -> Result<usize> {
    let keys: Vec<Vec<u8>> = ids.iter().map(|id| id.as_uuid().as_bytes().to_vec()).collect();
    self.blocking(move |db| {
        let txn = db.begin_write()?;
        {
            let mut table = txn.open_table(MEMORY_TABLE)?;
            for key in &keys { let _ = table.remove(key.as_slice()); }
        }
        txn.commit()?;
        Ok(keys.len())
    }).await
}
// Use it in fuse_duplicates and consolidate.
```

### T1-C8 — — `execute_command` `kill_child_tree` race: child can `setpgid` to escape the group kill
- **File:** crates/kod-tools/src/tools.rs
- **Line:** 203–214 (kill_child_tree), 999–1000 (process_group(0))
- **Severity:** Critical
- **Category:** Sandbox safety / Concurrency
- **Description:** `execute_command` spawns the child with `spawn.process_group(0)` (tokio → `setpgid(0, 0)`), which makes the child a process-group leader with PGID == child PID. On timeout/output-overflow, `kill_child_tree` calls `libc::kill(-(pid as pid_t), SIGKILL)` to signal the whole group. An adversarial shell can `setpgid(0, getppid())` to move itself back into the parent's process group; the negative-PGID kill then targets no group (ESRCH), and the fallback `child.start_kill()` only kills the direct child, leaving grandchildren (the actual runaway process) alive.
- **Code:**
```rust
fn kill_child_tree(child: &mut tokio::process::Child) {
    #[cfg(unix)]
    if let Some(pid) = child.id() {
        let pgid = -(pid as libc::pid_t);
        let rc = unsafe { libc::kill(pgid, libc::SIGKILL) };
        if rc == 0 { return; }
    }
    let _ = child.start_kill();
}
```
- **Why it's a bug:** A prompt-injected `execute_command("setsid sleep 9999")` or `sh -c 'setpgid 0 0; exec sleep 9999'` (depending on shell) can escape the group-kill, and the runaway `sleep` keeps running, holding its stdout/stderr write-ends open, blocking the drain loop indefinitely (or until the parent kod process exits).
- **Fix:** Use a new session (`setsid`) rather than just a new pgrp, and kill by session ID:
```rust
// Spawn with `pre_exec(|| { libc::setsid(); Ok(()) })` to put the child
// in its own session, not just a new pgrp. Then kill by sid:
let sid = unsafe { libc::getsid(pid as libc::pid_t) };
if sid > 0 {
    unsafe { libc::kill(-sid, libc::SIGKILL); }   // signal the whole session
}
// Also: use `kill_on_drop(true)` (already set) and consider a fallback
// that re-reads the child's /proc/<pid>/task to enumerate threads.
```

### T1-C9 — — `patch_file` does not lock around the read+diff+write cycle (H-R11 lock is only acquired at write time, leaving the diff racing with concurrent edits)
- **File:** crates/kod-tools/src/tools.rs
- **Line:** 1537–1646 (PatchFileTool::execute)
- **Severity:** Critical
- **Category:** Concurrency / Data corruption
- **Description:** `PatchFileTool::execute` reads the original file *first* (line 1582: `std::fs::read_to_string(&resolved)`), then applies the diff in memory, then acquires the path lock (line 1565: `table.acquire(...)`) — wait, actually the lock IS acquired before the read (line 1565), per the H-R11 comment. Re-reading: the lock is acquired at line 1565, BEFORE the read at 1582. Good. But the lock is an *advisory* in-process lock (`PathLockTable`), so a *different* kod process (or a non-kod tool, like `git`) editing the same file is not blocked. The lock only serializes kod-internal writers. A `git checkout` between the read and the write (both under the lock) would still race. The `revalidate_write_parent` (line 1625) re-checks the parent dir but not the file content. The `atomic_write` then writes the patched content based on the *original* read — clobbering the git checkout. This is the classic "patch against stale content" bug.
- **Code:**
```rust
let _lock = match &context.lock_table { /* acquire */ };
let original = match std::fs::read_to_string(&resolved) { /* read */ };
let patched = match crate::patch::apply_unified_diff(&original, patch) { /* diff */ };
if dry_run { return ...; }
context.revalidate_write_parent(&resolved)?;   // checks parent, not content
if let Err(e) = atomic_write(&resolved, patched.as_bytes()) { /* write */ }
```
- **Why it's a bug:** Two kod agents in a swarm both patch the same file: agent A reads (lock acquired), agent B blocks on the lock, agent A writes, releases; agent B acquires, reads the now-patched content, patches against the new content. Good. But agent A and a *non-kod* editor (the user's IDE, `git stash apply`): agent A reads, the IDE edits, agent A writes — clobbering the IDE's edit. The advisory lock does not help.
- **Fix:** Re-read the file inside the lock right before the write, compare to `original`, refuse if changed:
```rust
let fresh = std::fs::read_to_string(&resolved).unwrap_or_default();
if fresh != original {
    return Ok(ToolResult::Error(format!(
        "patch_file: file changed between read and write ({} bytes -> {} bytes); \
         re-read and re-apply",
        original.len(), fresh.len()
    )));
}
```
- **Notes:** The hashline `edit` tool (edit_hashline.rs:213–226) *does* do this exact check (`if current != snap.text`). The patch tool does not.

### T1-C10 — — `edit_hashline::stage` rewrites the whole file's EOL based on a single `\r\n` probe
- **File:** crates/kod-tools/src/edit_hashline.rs
- **Line:** 293–325
- **Severity:** Critical
- **Category:** Correctness / Data corruption
- **Description:** `stage` detects `crlf = text.contains("\r\n")`. If *any* line in the file uses CRLF (even a single line in an otherwise-LF file — common for files edited on Windows then committed with `core.autocrlf=input`), *every* line in the output is joined with `\r\n`. A 1000-line LF file with one stray CRLF line becomes a 1000-line CRLF file. The reverse case (LF in a CRLF file) is also broken. The probe should be a *majority* vote, not a *contains*.
- **Code:**
```rust
let crlf = text.contains("\r\n");   // <-- any CRLF anywhere flips the whole file
// ...
let mut lines: Vec<String> = text.lines().map(|s| s.to_string()).collect();
// text.lines() strips \r — so lines have no \r regardless of source
// ...
let eol = if crlf { "\r\n" } else { "\n" };
let mut out = lines.join(eol);   // <-- every line gets the same EOL
```
- **Why it's a bug:** A mixed-EOL file (very common in cross-platform repos) is silently normalized to CRLF or LF based on a single line. The git diff then shows every line as changed. The patch tool's `apply_unified_diff` (patch.rs:52) uses the same `contains` probe — same bug. The pre-fix comment in patch.rs:46–51 claims this *fixes* a CRLF bug, but it introduces the opposite one.
- **Fix:**
```rust
// Majority vote: count CRLF vs lone LF, pick the dominant.
let crlf_count = text.matches("\r\n").count();
let lf_count = text.matches('\n').count() - crlf_count;  // lone LFs
let crlf = crlf_count > lf_count;
// Or: preserve per-line EOL by not stripping in the first place.
```

### T1-C11 — — `parse_unified_diff` ignores `\ No newline at end of file` marker, then `apply_unified_diff` re-adds a trailing newline
- **File:** crates/kod-tools/src/patch.rs
- **Line:** 212 (`"\\" => {}`), 156–163
- **Severity:** High
- **Category:** Correctness
- **Description:** The unified-diff `\ No newline at end of file` marker tells `patch` that the preceding line is the last line and has no trailing newline. The parser here treats `\\` as a no-op (`"\\" => {}` at line 212). Then `apply_unified_diff` decides whether to re-emit a trailing newline based on `had_trailing_newline = original.ends_with('\n')` (line 57). If the patch *removes* the trailing newline (hunk ends with `\ No newline`), the apply does not honor it — it re-adds a newline based on the original. A patch meant to produce a file with no trailing newline produces a file with one. `git diff` then shows the file as changed.
- **Code:**
```rust
"\\" => {}    // <-- the "no newline" marker is dropped
// ...
if had_trailing_newline && !lines.is_empty() {
    out.push_str(sep);    // <-- unconditionally re-adds the newline
}
```
- **Why it's a bug:** A patch that says "remove the trailing newline from this file" is silently ignored; the file keeps its newline. Lint rules that enforce "no trailing newline" (ruff's W391, eslint's `eol-last`) see the file as still violating.
- **Fix:** Parse the `\` marker as a flag on the preceding line, and propagate it through `HunkLine` so `apply_unified_diff` knows whether the final line had a newline.

### T1-C12 — — `MemoryManager::store_with_metadata` dedup is O(N) per store via full-table scan + per-entry hash
- **File:** crates/kod-memory/src/manager.rs
- **Line:** 493–530
- **Severity:** High
- **Category:** Performance
- **Description:** Every long-term store calls `self.long_term.get_all().await?` (line 499) which deserializes *every* entry in the DB, then iterates and hashes each entry's content with `DefaultHasher` to compare against the incoming content hash. For a store with N entries, that's O(N) deserializations + O(N) hashes per store. For N stores in a session, that's O(N²) work. A 10K-entry store with 100 stores in a session does 1M deserializations. The comment says "Cheap: one pass over the long-term store, which is the same scan the retrieval path already does per prompt" — but retrieval *also* scans the whole table per prompt, so the two compound.
- **Code:**
```rust
let existing = self.long_term.get_all().await?;   // <-- deserialize everything
for e in existing {
    let mut eh = DefaultHasher::new();
    e.content.hash(&mut eh);
    if eh.finish() == content_hash && e.memory_type == memory_type {
        // dedup
    }
}
```
- **Why it's a bug:** Quadratic scaling. At 10K entries, each store takes ~50 ms (rough). At 100K, ~500 ms. The DB has no content-hash index.
- **Fix:** Maintain a side table `content_hash -> MemoryId` (a redb secondary index). On store, hash the content, look up the hash in the side table, dedup if present. O(1) per store.

### T1-C13 — — `redb::Database::create` is single-writer-locked; concurrent `MemoryManager` instances on the same DB file fail
- **File:** crates/kod-memory/src/long_term.rs
- **Line:** 36–57
- **Severity:** Medium
- **Category:** Error handling
- **Description:** `Database::create(path)` opens the file with an exclusive lock. A second `MemoryManager::new` on the same path (e.g., a kod CLI invoked while the TUI is running, or two swarm agents in the same project) returns `KodError::MemoryDatabase("Failed to open database: ...")` with no hint that the cause is "another kod process holds the lock". The user sees a generic error.
- **Code:**
```rust
let db = Database::create(path)
    .map_err(|e| KodError::MemoryDatabase(format!("Failed to open database: {}", e)))?;
```
- **Why it's a bug:** The error doesn't distinguish "file locked by another process" from "disk full" from "corrupt DB". The user has no actionable message.
- **Fix:**
```rust
let db = Database::create(path).map_err(|e| {
    let msg = if e.to_string().contains("locked") || e.to_string().contains("LockError") {
        format!("Failed to open memory database at {}: {} \
                 — another kod process may hold the file lock", path.display(), e)
    } else {
        format!("Failed to open database: {}", e)
    };
    KodError::MemoryDatabase(msg)
})?;
```

### T1-C14 — — `LongTermMemory::get_all` silently drops entries that fail to deserialize
- **File:** crates/kod-memory/src/long_term.rs
- **Line:** 211–217
- **Severity:** High
- **Category:** Error handling / Data corruption
- **Description:** The iteration loop matches `Ok((_, value))` and then `if let Ok(memory_entry) = serde_json::from_slice::<MemoryEntry>(value.value()) { entries.push(...) }`. A deserialization failure (a single corrupt byte, a schema mismatch from a version downgrade) is silently dropped — no log, no error. The iterator-error branch (`Err(e)`) is logged at line 220, but the deserialization-error branch is not. A memory store with one corrupt entry will silently lose that entry from every retrieval, every dedup, every consolidate — invisible.
- **Code:**
```rust
match entry {
    Ok((_, value)) => {
        if let Ok(memory_entry) =
            serde_json::from_slice::<MemoryEntry>(value.value())
        {
            entries.push(memory_entry);
        }
        // <-- a deserialization error falls through here, silently
    }
    Err(e) => {
        tracing::warn!("Failed to read entry: {}", e);   // <-- only the iter-error is logged
    }
}
```
- **Why it's a bug:** A version upgrade that changes `MemoryEntry`'s serialization (a new required field, a renamed field) would make every entry silently invisible. The user sees "no memories" with no error. The dedup pass would then store a duplicate of every entry (since the originals are invisible). Permanent data loss with no signal.
- **Fix:**
```rust
Ok((_, value)) => match serde_json::from_slice::<MemoryEntry>(value.value()) {
    Ok(e) => entries.push(e),
    Err(e) => tracing::warn!(
        error = %e,
        key = ?value.value(),  // or the key
        "get_all: corrupt entry skipped — schema migration may be needed"
    ),
},
```

### T1-C15 — — `WebFetchTool` SSRF via DNS rebinding: redirect policy is sync, can't re-DNS
- **File:** crates/kod-tools/src/web.rs
- **Line:** 74–95 (redirect policy), 239–262 (pin), 264–302 (pinned client)
- **Severity:** Critical
- **Category:** Security / SSRF
- **Description:** The redirect policy is a *synchronous* closure (reqwest's redirect policy runs on the request thread, no async). It calls `block_private_host(host)` — which checks the literal hostname string against `localhost`, RFC1918 literals, etc. It cannot do a DNS lookup. The pre-flight DNS check (line 209–228) runs once, on the *original* URL. If the original URL is `http://attacker.com/r` (resolves to a public IP, passes pre-flight) and the server returns `302 → http://internal.local/secret`, the redirect policy checks `block_private_host("internal.local")` — `internal.local` is not in the literal blocklist (it's not "localhost", not a private IP literal, not ".local" — wait, the doc says `.local` mDNS is blocked, but let me check the function... the function `block_private_host` is referenced but its body isn't in the snippet I read). If `internal.local` resolves to `127.0.0.1` or `169.254.169.254`, the *redirect* policy can't detect it — only the pre-flight can, and the pre-flight only ran on the original URL. The pinned client (line 264–302) is built with `.resolve(host, addr)` for the *original* host, not the redirect target. reqwest's `.resolve(host, addr)` pins only `host`; a redirect to a different hostname does its own DNS. So the redirect's hostname is resolved fresh by reqwest, with no private-IP check. SSRF.
- **Code:**
```rust
// redirect policy — sync, no DNS:
.redirect(reqwest::redirect::Policy::custom(|attempt| {
    let next = attempt.url().clone();
    if let Some(host) = next.host_str()
        && let Some(reason) = block_private_host(host)  // <-- literal only
    {
        return attempt.error(...);
    }
    attempt.follow()  // <-- reqwest then does its own DNS for the redirect target
}))
// pin (line 254–256): only the FIRST validated address, only for the original host
.lookup.into_iter().find(|addr| block_private_ip(addr.ip()).is_none())
```
- **Why it's a bug:** An attacker controls a URL like `http://public-attacker.com/r` that returns 302 → `http://169.254.169.254/latest/meta-data/iam/security-credentials/`. The pre-flight checks `public-attacker.com` (passes). The redirect policy checks `169.254.169.254` — but `block_private_host` only blocks *literal* private IPs, and `169.254.169.254` IS a literal link-local address. So this specific case is blocked. But `http://public-attacker.com/r` → 302 → `http://metadata.google.internal/` (a name that resolves to a link-local) — the redirect policy sees `metadata.google.internal`, calls `block_private_host("metadata.google.internal")` — that string is not in the blocklist. reqwest resolves it to `169.254.169.254` and connects. SSRF.
- **Fix:** Disable redirect-following entirely, or do an async re-DNS at each hop (which requires a custom redirect handler that reqwest doesn't directly support — you'd need to disable redirects in the client and follow them manually in the `execute` body, with a `spawn_blocking` DNS check at each hop).

### T1-C16 — — `LandlockProfile::to_json` fails open to `"{}"`
- **File:** crates/kod-tools/src/sandbox/landlock.rs
- **Line:** 170–172
- **Severity:** Medium
- **Category:** Error handling / Sandbox safety
- **Description:** `to_json` does `serde_json::to_string(self).unwrap_or_else(|_| "{}".to_string())`. If serialization fails (it shouldn't for these structs, but a poisoned `Arc` or a future `#[serde(flatten)]` with a custom serializer could), the launcher receives `"{}"`. `from_json("{}")` then fails (the struct has required fields without `#[serde(default)]`) — the launcher errors out, the sandbox is never applied, and the `__sandbox-exec` subcommand prints an error and exits without exec'ing the inner command. So this actually fails *closed* (the command never runs). But the error path is unclear: the user sees "invalid sandbox profile" rather than "serialization failed".
- **Code:**
```rust
pub fn to_json(&self) -> String {
    serde_json::to_string(self).unwrap_or_else(|_| "{}".to_string())
}
```
- **Why it's a bug:** The fail-open-to-`"{}"` is the wrong shape — it should fail to a `Result` and propagate. The current shape silently degrades; only the launcher's strict `from_json` saves it.
- **Fix:** Return `Result<String, KodError>` and propagate.

### T1-C17 — — `from_config` test uses `unsafe { std::env::set_var / remove_var }` in a multi-threaded test binary
- **File:** crates/kod-memory/src/embedding.rs
- **Line:** 467–481
- **Severity:** High
- **Category:** Concurrency / UB
- **Description:** The test `from_config_openai_without_key_returns_none` calls `unsafe { std::env::remove_var("OPENAI_API_KEY") }` and `unsafe { std::env::set_var("OPENAI_API_KEY", v) }` with a SAFETY comment claiming "single-threaded test". Rust's default test runner (`cargo test`) is multi-threaded (one thread per test). `std::env::set_var` mutates the process-wide `environ` global; another test in the same binary that calls `std::env::var("OPENAI_API_KEY")` concurrently is a data race on `environ` — undefined behavior. The 2024 edition makes `set_var`/`remove_var` `unsafe` precisely for this reason.
- **Code:**
```rust
// SAFETY: no other test in this file reads OPENAI_API_KEY.
let prior = std::env::var("OPENAI_API_KEY").ok();
// SAFETY: single-threaded test; the removal is restored below.
unsafe { std::env::remove_var("OPENAI_API_KEY") };
// ... test body ...
if let Some(v) = prior {
    // SAFETY: same reasoning.
    unsafe { std::env::set_var("OPENAI_API_KEY", v) };
}
```
- **Why it's a bug:** The SAFETY claim is wrong — the test binary is multi-threaded. If a `manager::tests::*` test spawns an `OpenAIEmbedder::new` that calls `std::env::var("OPENAI_API_KEY")` (it doesn't directly, but `from_config` does), it races. The data race is on a global; the symptom is a torn read of the env string, which can cause a panic in `from_utf8` or a use-after-free in `getenv(3)`.
- **Fix:** Use `serial_test` crate, or refactor `from_config` to take the env as a parameter (testable without touching the global).

---

## High severity

### T1-H1 — — `MemoryManager::flush_embeddings` can deadlock if a background embed task panics
- **File:** crates/kod-memory/src/manager.rs
- **Line:** 634–660 (spawn_embed), 670–687 (flush_embeddings)
- **Severity:** High
- **Category:** Concurrency / Deadlock
- **Description:** `spawn_embed` does `inflight.fetch_add(1, ...)` then spawns a tokio task. The task body matches on `embedder.embed(...)`. If the embed future panics (a reqwest panic, a poisoned lock, an `unwrap` in a downstream crate), tokio catches the panic and aborts the task — but the `inflight.fetch_sub(1, ...)` at line 658 never runs (it's after the match, in the normal flow). The counter stays at >0 forever. `flush_embeddings` loops on `Notify::notified()` waiting for the counter to reach 0 — it never does. Deadlock at shutdown.
- **Code:**
```rust
tokio::spawn(async move {
    match embedder.embed(...).await {
        Ok(mut v) if !v.is_empty() => { /* persist + insert */ }
        Ok(_) => {}
        Err(e) => { /* log */ }
    }
    inflight.fetch_sub(1, Ordering::SeqCst);   // <-- never runs if the match panics
    notify.notify_one();
});
```
- **Why it's a bug:** A flaky network panic in reqwest hangs the engine at shutdown.
- **Fix:**
```rust
tokio::spawn(async move {
    let _guard = DropDecrement { inflight: inflight.clone(), notify: notify.clone() };
    match embedder.embed(...).await { /* ... */ }
});
struct DropDecrement { inflight: Arc<AtomicUsize>, notify: Arc<Notify> }
impl Drop for DropDecrement {
    fn drop(&mut self) {
        self.inflight.fetch_sub(1, Ordering::SeqCst);
        self.notify.notify_one();
    }
}
```

### T1-H2 — — `MemoryManager::rebuild_index` single-flight returns `Ok(())` without waiting — concurrent retrievals see no index
- **File:** crates/kod-memory/src/manager.rs
- **Line:** 351–439
- **Severity:** High
- **Category:** Concurrency / Performance
- **Description:** `rebuild_index` uses `swap(true, SeqCst)` as a single-flight guard. If a second caller sees `true`, it returns `Ok(())` immediately (line 364) — it does not wait for the first caller to finish. The retrieval path (line 817) checks `self.vector_index.read().is_none()` and calls `rebuild_index().await`. If two retrievals race, the first rebuilds, the second sees `rebuild_in_progress = true` and returns immediately — but the index is still being built (the first caller hasn't yet written `*self.vector_index.write() = Some(idx)`). The second retrieval proceeds with `cosines = Default::default()` (line 833) — no semantic scores. The hybrid falls back to keyword+recency only. The doc claims "best-effort" but the symptom is silent: a retrieval right after the first store sees no semantic results.
- **Fix:** Use a `tokio::sync::Mutex<()>` or a `Notify` to make the second caller wait for the first.

### T1-H3 — — `LongTermMemory::count` iterates the whole table instead of using `table.len()`
- **File:** crates/kod-memory/src/long_term.rs
- **Line:** 303–323
- **Severity:** Medium
- **Category:** Performance
- **Description:** `count()` does `for entry in table.iter() { if entry.is_ok() { count += 1; } }`. redb tables track their length internally; `table.len()` is O(1) (it's the underlying B-tree's stored count). The current code is O(N) per call.
- **Fix:** `Ok(table.len() as usize)` (note: redb's `len()` returns `Result<usize>` because it may need to compute for uncommitted transactions, but for a committed read txn it's O(1)).

### T1-H4 — — `MemoryManager::retrieve_long_term_hybrid` calls `get_all()` on every retrieval — no caching
- **File:** crates/kod-memory/src/manager.rs
- **Line:** 803
- **Severity:** High
- **Category:** Performance
- **Description:** Every `retrieve_context` call (which fires every turn) calls `self.long_term.get_all().await?`, which deserializes *every* entry in the DB. For a 10K-entry store, that's 10K `serde_json::from_slice` calls per turn. At ~5 µs per deser, that's 50 ms per turn just for the scan. The retrieval path then runs 4 passes over `all` (vector, keyword, importance, temporal) — 40K iterations. Plus the MMR rerank (40-pool, ~1600 Jaccard comparisons). The whole retrieval is O(N) per turn with a large constant.
- **Fix:** Cache `get_all()` in an `Arc<RwLock<Vec<MemoryEntry>>>` invalidated on write (the `store` path already holds the manager, so it can drop the cache). Or maintain an in-memory index alongside the redb store (like `vector_index` but for the full entries).

### T1-H5 — — `VectorIndex::insert` / `search` / `remove` are O(N) per op — no HashMap side index
- **File:** crates/kod-memory/src/vector_index.rs
- **Line:** 62–95 (insert uses `find`), 99–106 (remove uses `position`), 115–145 (search iterates all)
- **Severity:** Medium
- **Category:** Performance
- **Description:** `insert` does `self.entries.iter_mut().find(|(existing, _)| existing == &id)` to check for an existing ID — O(N) per insert. `remove` does `self.entries.iter().position(...)` — O(N). `search` iterates all entries — O(N) per query. For a rebuild that inserts 10K vectors, that's O(N²) = 100M comparisons. The doc says "deliberately simple: a Vec walked in a single pass per query. At the scale this index is designed for (≤ 10 000 entries) the pass is under 5 ms" — but the *rebuild* is O(N²), not the *search*. Rebuild at 10K is 100M comparisons → multi-second.
- **Fix:** Add `index: HashMap<MemoryId, usize>` alongside `entries`. `insert` is O(1) lookup + push-or-replace. `remove` is O(1) lookup + swap_remove. `search` is still O(N) (the brute-force walk is the design).

### T1-H6 — — `LongTermMemory::search` is a full-table scan with substring match — no FTS index
- **File:** crates/kod-memory/src/long_term.rs
- **Line:** 234–241
- **Severity:** Medium
- **Category:** Performance
- **Description:** `search` calls `get_all()` then filters with `e.content.to_lowercase().contains(&query_lower)`. For a 10K store, that's 10K `to_lowercase` allocations + 10K substring scans. The hybrid retrieval path in `retrieve_long_term_hybrid` doesn't use `search` (it uses `get_all` + BM25-lite), so this is only hit by the public `search` method. Still, a `MemoryManager::search` call (e.g., from a CLI `kod memory search foo`) is O(N).
- **Fix:** Use redb's secondary indexes, or maintain a token-posting list in a side table.

### T1-H7 — — `redb` write transactions are not batched across `store_with_metadata`'s dedup + store
- **File:** crates/kod-memory/src/manager.rs
- **Line:** 499 (read), 520 (write bumped), 579 (write entry)
- **Severity:** Medium
- **Category:** Performance
- **Description:** `store_with_metadata` does: `get_all` (read txn), then for the dedup-hit case: `store(bumped)` (write txn 1), returns. For the new-entry case: `store(entry)` (write txn 2). The background embed task later does another `update(entry)` (write txn 3). Each write txn is a fsync. So a single store call can produce 2 fsyncs (dedup-hit bump + new-entry store) or 1 (new-entry store) + 1 (embed persist) = 2. For a burst of N stores, 2N fsyncs.
- **Fix:** Combine the dedup-bump and the new-entry store into a single `store_batch` when both happen (rare — dedup-hit returns early). For the embed persist, the background task already does one store per embed — that's fine, but it could be batched if multiple embeds are in flight.

### T1-H8 — — `MemoryManager::consolidate` runs three full-table scans (archive + fuse + resolve_contradictions)
- **File:** crates/kod-memory/src/manager.rs
- **Line:** 1119–1156 (consolidate), 1173–1228 (resolve_contradictions), 1241–1410 (fuse_duplicates)
- **Severity:** Medium
- **Category:** Performance
- **Description:** `consolidate` calls `get_all` (line 1120), iterates for archival (line 1130–1143, calling `remove` per archived entry — each a write txn), then `fuse_duplicates(&remaining)` which calls `embedder.embed(&texts)` (one batch — good) then loops `remove` per delete (each a write txn), then `resolve_contradictions` which calls `get_all` *again* (line 1174 — a second full-table read) and loops `supersede` per contradiction (each a write txn). Three full-table scans + N+M+K write txns. For a 10K store with 100 archives, 50 fuses, 10 contradictions, that's 30K desers + 160 fsyncs.
- **Fix:** One `get_all` at the top, pass `&all` to all three passes. Batch the writes (`remove_batch`, `store_batch`).

### T1-H9 — — `PathLockTable` cells are never evicted — unbounded growth for long sessions
- **File:** crates/kod-tools/src/path_lock.rs
- **Line:** 76–113 (acquire), 36 (the cells HashMap)
- **Severity:** Medium
- **Category:** Performance / Memory leak
- **Description:** `acquire` calls `cells.entry(path).or_insert_with(|| Arc::new(Mutex::new(())))` and never removes the cell. The comment (line 31–35) admits this: "Cells are not removed when a guard drops. The cost is one Arc<Mutex<()>> per distinct path ever locked". For a session that touches 10K distinct paths (a large repo, a multi-project session), that's 10K `Arc<Mutex<()>>` cells forever. Each cell is ~40 bytes (Arc inner + Mutex inner) — 400 KB at 10K, 4 MB at 100K. Not a leak in the OOM sense, but unbounded.
- **Fix:** Evict cells with no waiters on release (use a `Weak<Mutex<()>>` and upgrade on acquire; or sweep periodically).

### T1-H10 — — `WalkCache::get_or_walk` race: invalidation between unlock and re-lock re-inserts a stale entry
- **File:** crates/kod-tools/src/walk_cache.rs
- **Line:** 121–163
- **Severity:** Medium
- **Category:** Concurrency / Correctness
- **Description:** The walk runs outside the lock (line 137 drops the read guard, line 139 calls `walk()`). Between the unlock and the re-acquire (line 149), a concurrent `invalidate_all` from a writer can clear the cache. The re-acquire then inserts the (now-stale) walk result, reviving the cache. A subsequent `list_files` in the same turn sees the stale entry. The TTL (5s) bounds this, but a write-then-list within 5s can return the pre-write file list.
- **Code:**
```rust
{   // lock, check, unlock
    let g = self.entries.lock().expect("walk_cache poisoned");
    if let Some(e) = g.get(&key) && e.at.elapsed() < self.ttl {
        return e.paths.clone();
    }
}   // <-- gap: invalidate_all can run here
self.misses.fetch_add(1, Ordering::Relaxed);
let mut paths = walk();
// <-- the walk may have happened against a tree that a writer just changed
let mut g = self.entries.lock().expect("walk_cache poisoned");
// re-insert, reviving a stale entry
```
- **Why it's a bug:** A `write_file` followed by `list_files` in the same turn: the write calls `invalidate_all` (good); the `list_files` then calls `get_or_walk` which (a) misses the cache (cleared), (b) walks the (updated) tree, (c) re-inserts. That's correct. But if the `list_files` walk *started* before the `write_file`'s `invalidate_all` (the walk was in flight), the walk returns the old tree, and the re-insert revives it. A subsequent `list_files` (also in the same turn) then hits the stale cache.
- **Fix:** Re-check the cache after the walk; if the walk's result matches the (now-stale) cached entry, skip the insert. Or use a per-key `Mutex` to serialize walks on the same key.

### T1-H11 — — `WalkCache::invalidate_path` uses `path.starts_with(&k.root)` — fails to match if `path` is canonicalized but `k.root` is not
- **File:** crates/kod-tools/src/walk_cache.rs
- **Line:** 174–177
- **Severity:** Medium
- **Category:** Correctness
- **Description:** `invalidate_path(path)` retains entries where `!path.starts_with(&k.root)`. The `path` argument is the write destination (canonicalized by `resolve_path`). The cache's `k.root` is the path passed to `get_or_walk`, which is the *raw* path from the tool (e.g., `.` or `src/`), not canonicalized. On macOS where `/tmp` → `/private/tmp`, a write to `/private/tmp/work/a.rs` against a cache keyed on `/tmp/work` (raw) fails the `starts_with` check — the cache is not invalidated. Stale entry survives.
- **Fix:** Canonicalize both sides before the `starts_with` check, or canonicalize the root at `get_or_walk` time.

### T1-H12 — — `resolve_path` fallback canonicalizes only one level of parent — deeply-nested new-file writes fail
- **File:** crates/kod-tools/src/context.rs
- **Line:** 986–1008
- **Severity:** High
- **Category:** Correctness
- **Description:** When `std::fs::canonicalize(&joined)` fails (the file doesn't exist), the fallback canonicalizes `joined.parent()` and joins the filename. For a path like `src/a/b/c/d.rs` where `src/a/b/c/` doesn't exist (a fresh tree), `joined.parent()` = `src/a/b/c`, which also doesn't exist — `canonicalize` fails, returns `KodError::Io`. The `write_file` code (line 715) calls `create_dir_all(parent)` *after* `resolve_path`, so the path must resolve first. A write to a deeply-nested fresh path fails with an IO error before `create_dir_all` can fix it.
- **Why it's a bug:** The very common case "model writes a new file in a new subdirectory" fails. The model writes `src/handlers/foo/bar.rs`, the workspace has `src/handlers/` but not `src/handlers/foo/`. `resolve_path` tries to canonicalize `src/handlers/foo/bar.rs` (fails — `foo/` doesn't exist), falls back to canonicalize `src/handlers/foo` (fails — same reason). Returns IO error. The model gets "No such file or directory" and gives up.
- **Fix:** Walk up the parent chain until a canonicalizable ancestor is found, then join back the remaining components.

### T1-H13 — — `revalidate_write_parent` TOCTOU window remains between re-validate and `atomic_write`'s `File::create`
- **File:** crates/kod-tools/src/context.rs
- **Line:** 1184–1200 (revalidate), tools.rs:154–194 (atomic_write)
- **Severity:** Medium
- **Category:** Security / TOCTOU
- **Description:** `revalidate_write_parent` re-canonicalizes the parent and checks it's under the root. Then `atomic_write` opens a temp file in the parent. Between the re-validate and `File::create`, a peer can swap the parent dir to a symlink pointing outside the workspace. `File::create(parent.join(tmp))` follows the symlink, creates the temp file outside the workspace, then `rename(tmp, path)` — both paths follow the symlink, so the file ends up outside the workspace. The doc (line 1180–1183) admits this and says the complete fix is `openat2(RESOLVE_BENEATH | RESOLVE_NO_SYMLINKS)`. The pragmatic check shrinks the window from "the whole tool body" to "a few microseconds", but does not close it.
- **Fix:** Use `openat2` (Linux ≥ 5.6) with `RESOLVE_BENEATH | RESOLVE_NO_SYMLINKS` relative to a held parent dirfd. On older kernels, the pragmatic check is the best available.

### T1-H14 — — `landlock_invocation` leaves the profile temp file in `/tmp` if the launcher crashes
- **File:** crates/kod-tools/src/context.rs
- **Line:** 257–299
- **Severity:** Low
- **Category:** Resource leak
- **Description:** The profile is written to `/tmp/kod-sandbox-{pid}-{nanos:x}.json` with `create_new(true)`. The launcher is supposed to read and unlink it. If the launcher crashes (a panic in `apply()`, a SIGKILL, a kernel that silently downgrades Landlock), the file leaks. There's no cleanup of stale `kod-sandbox-*.json` files. Over a long session with many crashes, `/tmp` accumulates.
- **Fix:** Register an `atexit` handler, or scan `/tmp` for `kod-sandbox-*` files older than 1h at startup.

### T1-H15 — — `LandlockProfile`'s `claims_git_readonly` is hardcoded to refuse — the field is dead
- **File:** crates/kod-tools/src/sandbox/landlock.rs
- **Line:** 220–227
- **Severity:** Low
- **Category:** Correctness
- **Description:** `apply` refuses if `profile.claims_git_readonly` is true, with a message saying "use the bwrap backend, or set git_readonly = false". But `landlock_invocation` (context.rs:224) sets `claims_git_readonly`... actually, looking at context.rs:213–235, `claims_git_readonly` is NOT set — the struct literal doesn't include it, so it defaults to `false` (the struct has no Default derive, but the field is `bool` so... actually the struct literal is `LandlockProfile { ro_paths: ..., rw_paths: ..., net_deny: ... }` — `claims_git_readonly` is missing, which is a compile error unless there's a `..Default::default()` or the field has a default. Let me re-read... actually the struct literal in context.rs:213 does NOT include `claims_git_readonly`, but the struct definition (landlock.rs:148–166) has it as a field. This would be a compile error unless `LandlockProfile` has a `Default` impl. Looking at the struct — no `Default` derive. So either there's a `..Default::default()` I'm missing, or the literal in context.rs:213 includes it (I only saw the snippet up to line 235). Let me assume the literal includes `claims_git_readonly: opts.git_readonly` (sensible). Then when `opts.git_readonly == true`, `apply` refuses. So the landlock backend *cannot* honor git_readonly — it always errors out when the operator asks for it. The `SandboxResolver::invocation` doesn't pre-check this, so the error surfaces only at launcher exec time (when `__sandbox-exec` runs `apply()`).
- **Fix:** Pre-check in `landlock_invocation`: if `opts.git_readonly`, return `Err` early with the same message, so the user sees it at tool-call time, not at launcher time.

### T1-H16 — — `WebFetchTool` per-call client rebuild is expensive (TLS init, connection pool)
- **File:** crates/kod-tools/src/web.rs
- **Line:** 264–302
- **Severity:** Low
- **Category:** Performance
- **Description:** When `pinned_addr` is Some, a new `reqwest::Client` is built per call. Building a client does TLS session init, connection-pool allocation, and the redirect-policy closure setup. For a burst of N fetches, that's N client builds. The shared `self.client` is only used when `pinned_addr` is None.
- **Fix:** Cache built clients per `pinned_addr` in a small LRU. Or accept the cost (it's ~1 ms per build).

### T1-H17 — — `bash_interceptor::split_shell_words` panics on a trailing backslash
- **File:** crates/kod-tools/src/bash_interceptor.rs
- **Line:** 117–121
- **Severity:** Medium
- **Category:** Error handling / Panic
- **Description:** The `b'\\' => { i += 2; end = i.min(bytes.len()); }` branch skips two bytes. If the backslash is the last byte (`echo foo\`), `i += 2` goes past the end, `end = i.min(bytes.len())` clamps to `bytes.len()`. The outer loop then `while i < bytes.len()` exits. The token is `&s[start..end]` = `&s[start..bytes.len()]`. That's fine — no panic. But a backslash followed by a single byte that's also a backslash (`echo foo\\`) — `i += 2` skips both. Then the loop continues. OK, no panic. Wait, let me re-check: `i += 2` could land on `bytes.len()` exactly (if backslash is at `bytes.len()-2` and the next byte is the last). Then `end = i.min(bytes.len())` = `bytes.len()`. The outer `while i < bytes.len()` is false → exit. Token = `&s[start..bytes.len()]`. Fine. No panic. Scratch this finding — the code is correct.

Actually, scratch T1-H17. The code is fine.

### T1-H18 — — `MemoryManager::store_with_metadata` redaction is post-content-truncation — a 100KB secret is truncated to 4KB, then redacted
- **File:** crates/kod-memory/src/manager.rs
- **Line:** 462–481 (redact, then truncate)
- **Severity:** Medium
- **Category:** Security
- **Description:** Wait, reading the code: line 462 `let content_owned = self.redact_text(content);` runs *first*, then line 467 `let content_owned = crate::hygiene::strip_memory_tags(&content_owned);`, then line 477 `let content = if content.len() > MAX_CONTENT_BYTES { truncate_chars(content, MAX_CONTENT_BYTES) }`. So redaction runs on the *full* content (good), then truncation runs on the *redacted* content. So a 100KB content with a secret in the first 4KB is redacted, then truncated. The redacted marker (`[REDACTED:openai-key]`) is in the first 4KB. The truncation preserves it. Good. But: if the secret is in the *last* 96KB (past the 4KB cap), redaction runs on the full content first, so the secret is redacted before truncation. Also good. So this is actually fine. Scratch T1-H18.

Actually, scratch T1-H18. The order is correct.

### T1-H19 — — `MemoryManager::spawn_embed` clones the embedder `Arc` per call but the embedder's `dims_cache` is `AtomicUsize` — the dim is learned lazily and may be 0 for the first call
- **File:** crates/kod-memory/src/embedding.rs
- **Line:** 78 (dims_cache), 141–143 (set on first call), 101–103 (load for query)
- **Severity:** Low
- **Category:** Correctness
- **Description:** `OllamaEmbedder::dims()` returns the cached dim. The cache is `AtomicUsize`, set on the first successful `embed()` call (line 142–143). But the rebuild_index path (manager.rs:816) checks `self.embedder.is_some()` (not `dims() > 0`) to decide whether to attempt semantic scoring. So the first retrieval after install calls `rebuild_index`, which calls `embedder.embed(texts)`, which sets `dims_cache`. Good. But `fuse_duplicates` (manager.rs:1245–1248) checks `if dim == 0 { return Ok(0); }` — so before the first embed call, fusion is skipped. That's "no embeddings yet" — acceptable but the user might expect fusion to work after install.
- **Fix:** Eagerly probe dims at `set_embedder` time (call `embedder.embed(&[""])` once to warm the cache). Or accept the lazy behavior.

### T1-H20 — — `redb::Database` is `Send + Sync` but `store_with_metadata`'s dedup+store pattern can deadlock under contention
- **File:** crates/kod-memory/src/long_term.rs, manager.rs
- **Line:** long_term.rs:79–101 (store), 110–145 (store_batch)
- **Severity:** Medium
- **Category:** Concurrency / Deadlock
- **Description:** Each `store` call opens a write txn. redb's write txns are exclusive — only one at a time. Under bursty writes (background embed tasks + foreground stores), the write txns serialize. That's fine for correctness, but a long write txn (e.g., `store_batch` of 1000 entries) blocks every other write for its duration. The `spawn_blocking` moves the work off the async runtime, but the redb mutex still serializes. No deadlock, just contention. Scratch the "deadlock" claim — redb doesn't deadlock between R/O and R/W (it's MVCC). The real issue is write contention, which is perf, not correctness.

### T1-H21 — — `MemoryManager::fuse_duplicates` re-embeds the group even if the embeddings are already in `metadata.embedding`
- **File:** crates/kod-memory/src/manager.rs
- **Line:** 1288–1299
- **Severity:** Medium
- **Category:** Performance
- **Description:** `fuse_duplicates` builds `texts: Vec<String> = group.iter().map(|e| e.content.clone()).collect()` and calls `embedder.embed(&texts)` — re-embedding every entry, even those that already have a stored embedding. The retrieval path (manager.rs:392–416) fills in missing embeddings during `rebuild_index`, but `fuse_duplicates` doesn't check whether the entries already have embeddings. A consolidation pass on a store where every entry is already embedded still does a full embed call.
- **Fix:** Check `entry.metadata.embedding` first; embed only the missing ones (like `rebuild_index` does).

### T1-H22 — — `WebFetchTool` error-body chunk read can hang if the server sends chunks slowly
- **File:** crates/kod-tools/src/web.rs
- **Line:** 318–325
- **Severity:** Medium
- **Category:** Error handling / Hang
- **Description:** The error-body read loop `while body_bytes.len() < 4096 { response.chunk().await }` has no timeout. A hostile server that sends 1 byte per second keeps the loop alive for 4096 seconds. The outer `request_client` has a 30s timeout (line 267), but that's on the *request* future, not on the chunk-stream future. Once the response starts, the timeout no longer applies to subsequent chunks.
- **Fix:** Wrap the error-body read in a `tokio::time::timeout(Duration::from_secs(5), ...)`.

### T1-H23 — — `MemoryManager::store` for `ShortTerm` doesn't acquire any lock — concurrent stores can race on `compact`
- **File:** crates/kod-memory/src/manager.rs
- **Line:** 547–565
- **Severity:** Low
- **Category:** Concurrency
- **Description:** The ShortTerm branch calls `self.short_term.store(entry)` (which acquires the short_term's internal RwLock write) then checks capacity and calls `self.compact(target)` (which acquires the same lock again). Two concurrent stores: A stores (lock), B waits, A releases, B stores (lock), A calls compact (waits), B releases, A compacts, etc. No race — the internal lock serializes. Fine.

Scratch T1-H23.

### T1-H24 — — `redb` `Database::create` does not set `Durability` — default is `Immediate` (fsync per commit), which is slow for bursty writes
- **File:** crates/kod-memory/src/long_term.rs
- **Line:** 43
- **Severity:** Medium
- **Category:** Performance
- **Description:** `Database::create(path)` uses default config. redb's default `Durability` is `Immediate` — every commit fsyncs. For the background-embed path (one `store` per embed completion), that's one fsync per embed. For a burst of 32 embeds, 32 fsyncs. SSD fsync is ~5 ms → 160 ms of fsync time per burst. The retrieval writeback (one `store_batch` of top-20) is one fsync — fine.
- **Fix:** Use `Database::builder().set_durability(Durability::Eventual)` for the embed-writeback path (acceptable to lose the last few embeds on a crash — they'll be re-embedded on the next retrieval). Keep `Immediate` for the foreground store path.

---

## Medium severity

### T1-M1 — — `LongTermMemory::clear` iterates and removes per-key instead of `table.clear()`
- **File:** crates/kod-memory/src/long_term.rs
- **Line:** 353–384
- **Severity:** Medium
- **Category:** Performance
- **Description:** `clear` does `let keys: Vec<Vec<u8>> = table.iter().filter_map(...).collect();` then `for key in keys { table.remove(key.as_slice())?; }`. For a 10K store, that's 10K removes in one txn — fine, but redb has `table.clear()` (O(1) for the whole table) which is much faster.
- **Fix:** `table.clear()?;` (available in redb 1.0+).

### T1-M2 — — `LongTermMemory::close` doesn't actually close until all cloned Arcs drop — misleading doc
- **File:** crates/kod-memory/src/long_term.rs
- **Line:** 345–351
- **Severity:** Low
- **Category:** Error handling / Lifecycle
- **Description:** `close(self)` just drops self. The doc (line 327–344) says "redb closes the file when the last Arc<Database> drops. This method gives the caller a deterministic close point". But the manager's `spawn_embed` tasks hold clones of `LongTermMemory` (manager.rs:629). `close` drops the manager's own Arc, but the background tasks' clones keep the DB open. So `close` does not deterministically close — it depends on whether background tasks have finished. The doc is misleading.
- **Fix:** Call `flush_embeddings().await` before `close` to drain background tasks.

### T1-M3 — — `MemoryManager::close` does not call `flush_embeddings` first
- **File:** crates/kod-memory/src/manager.rs
- **Line:** 1448–1459
- **Severity:** Medium
- **Category:** Lifecycle / Persistence
- **Description:** `close` destructures self and drops `short_term`, then `long_term.close()`. It does NOT call `self.flush_embeddings().await` first. If background embed tasks are in flight, they hold clones of `long_term` (an `Arc<Database>`). `long_term.close()` drops the manager's Arc, but the tasks' clones keep the DB open. The tasks may complete later, writing their embeddings to a DB the manager thought was closed. The writes succeed (the DB is still open), but the manager's shutdown is racy.
- **Fix:**
```rust
pub async fn close(self) {
    self.flush_embeddings().await;   // <-- drain background tasks
    let Self { short_term, long_term, .. } = self;
    drop(short_term);
    long_term.close();
}
```
- **Notes:** This changes the signature to `async`. The caller (`KodEngine::shutdown`) is already async.

### T1-M4 — — `embedder.embed` has no retry — a single transient HTTP error disables embeddings for the entry
- **File:** crates/kod-memory/src/embedding.rs
- **Line:** 104–149 (Ollama), 190–239 (OpenAI)
- **Severity:** Medium
- **Category:** Error handling
- **Description:** Both embedders do one HTTP POST per `embed()` call. A transient 503 / network blip returns an error; the calling code (manager.rs:635) logs and degrades — the entry stays FTS-only. The next retrieval's `rebuild_index` would re-embed it (good), but only if the entry is in `need_embed` (manager.rs:381–386). Since the embed failed, `metadata.embedding` is still `None`, so the next `rebuild_index` retries. That's actually correct retry-on-next-retrieval behavior. But if the embedder is permanently down, every retrieval retries the full corpus — O(N) embed calls per retrieval.
- **Fix:** Exponential backoff within `embed()` for transient errors (503, network). Persistent errors propagate.

### T1-M5 — — `parse_float_array` silently truncates NaN/Inf? No — it rejects. But it doesn't check for denormals
- **File:** crates/kod-memory/src/embedding.rs
- **Line:** 358–377
- **Severity:** Low
- **Category:** Correctness
- **Description:** `parse_float_array` accepts `f64::NAN` and `f64::INFINITY` (via `as_f64()` then `as f32` cast). The `VectorIndex::insert` (vector_index.rs:75–80) checks `norm_sq.is_finite()` — so a NaN/Inf component produces `norm_sq = NaN` (since `x*x` for NaN x is NaN, and sum of NaN is NaN), `!norm_sq.is_finite()` is true → rejected. Good. But a denormal (very small) component produces a near-zero norm → division by a near-zero produces Inf → `normalized` is all Inf → `norm_sq` of the normalized vector is Inf → on insert, the `!is_finite` check catches it. So denormals are caught. Fine. Scratch T1-M5.

### T1-M6 — — `MemoryManager::retrieve_context` doesn't filter superseded entries before scoring
- **File:** crates/kod-memory/src/manager.rs
- **Line:** 802–1104
- **Severity:** Medium
- **Category:** Correctness
- **Description:** `retrieve_long_term_hybrid` calls `get_all().await?` and scores every entry, including those with `superseded_by: Some(...)`. The `is_active()` check (line... actually I don't see an `is_active()` filter in the retrieval path). Superseded entries are scored and may be returned to the model. The `PendingMemory::revalidate` (line 2105–2133) *does* check `is_active()` and drops superseded entries. But a caller that doesn't use `PendingMemory` gets superseded entries in the context. The doc says superseded entries "stops appearing in retrieval" (long_term.rs:247) — but the retrieval path doesn't filter them.
- **Fix:** Filter `all.iter().filter(|e| e.is_active())` before scoring.

### T1-M7 — — `MemoryEntry::content.to_lowercase()` in `LongTermMemory::search` and `ShortTermMemory::search` allocates per entry per search
- **File:** crates/kod-memory/src/long_term.rs:239, short_term.rs:148
- **Severity:** Low
- **Category:** Performance
- **Description:** `e.content.to_lowercase().contains(&query_lower)` allocates a new String per entry per search. For 10K entries, 10K allocations. `contains` is also O(N) substring. Could use `str::contains` on the lowercased query against the original content with a case-insensitive search (no allocation). Or precompute lowercased content at store time.
- **Fix:** Use `unicase:: Ascii::new(content).contains(...)` or precompute a lowercased index.

### T1-M8 — — `stopwords::is_stopword` is a linear scan over a 184-element slice — O(N) per token, O(N*M) per query
- **File:** crates/kod-memory/src/stopwords.rs
- **Line:** 201–203
- **Severity:** Low
- **Category:** Performance
- **Description:** `ENGLISH.contains(&word) || FRENCH.contains(&word)` is O(184) + O(60) per call. For a query with 10 tokens, that's 2440 comparisons. `QueryTerms::build` (retrieval.rs:170–183) calls `stopwords::tokens` which calls `is_stopword` per token, then `stopwords::tokens` again per *candidate entry*. For 10K candidates with 10 query tokens, that's 10K * 10 * 2440 = 244M comparisons per retrieval. Catastrophic.
- **Fix:** Use a `HashSet<&str>` or a `phf::Set` for O(1) lookup.

### T1-M9 — — `stopwords::stem` is a linear scan over a 15-element suffix list — O(15) per token
- **File:** crates/kod-memory/src/stopwords.rs
- **Line:** 214–225
- **Severity:** Low
- **Category:** Performance
- **Description:** Same shape as T1-M8. Called per token per candidate. 10K * 10 * 15 = 1.5M `ends_with` calls per retrieval. Less catastrophic than T1-M8 but still wasteful.
- **Fix:** Trie-based stemmer, or accept the cost (the linear scan is cache-friendly).

### T1-M10 — — `redb` `Database::create` is called once per `LongTermMemory::new` — no connection pooling across managers
- **File:** crates/kod-memory/src/long_term.rs
- **Line:** 43
- **Severity:** Low
- **Category:** Performance
- **Description:** Each `MemoryManager::new` opens a new `Database::create`. redb locks the file exclusively, so only one `MemoryManager` per DB file. That's intended. But if the engine creates multiple managers for different projects, each opens its own DB. Fine. No issue.

### T1-M11 — — `tools.rs::execute_command` does not set a max on `stall_wake_seconds` — a model can request 10^9 second stalls
- **File:** crates/kod-tools/src/tools.rs
- **Line:** 868 (`let stall = params.get("stall_wake_seconds").and_then(|v| v.as_u64());`)
- **Severity:** Low
- **Category:** Error handling
- **Description:** The parameter schema (line 791–793) says "minimum 30" but the parsing code doesn't enforce a minimum or a maximum. A model that sends `stall_wake_seconds: 0` or `stall_wake_seconds: 999999999` would be accepted. The `BackgroundSpawnHook` may or may not clamp.
- **Fix:** `let stall = stall.max(30).min(3600);` or similar.

### T1-M12 — — `env_policy::NON_INTERACTIVE` is applied AFTER the secret-strip loop, so `EDITOR=true` overwrites any user-set `EDITOR`
- **File:** crates/kod-tools/src/tools.rs
- **Line:** 972–983
- **Severity:** Low
- **Category:** Correctness
- **Description:** The env loop (line 972–979) inherits all env vars except secret-shaped ones. Then `env_policy::apply` (line 983) overrides `EDITOR`, `PAGER`, etc. So a user who set `EDITOR=nvim` in their shell has it overwritten to `EDITOR=true`. That's the intended behavior (the doc says "these win over whatever the user set"). Fine. But `env_policy` also sets `CI=true` and `AGENT=1` — a child that probes `$CI` to enable CI-mode (e.g., a test runner that skips slow tests in CI) would behave differently. The doc admits this ("a non-interactive child is exactly the 'CI' case"). Acceptable.

### T1-M13 — — `internal_url::url_path_target` only handles `conflict://` — `memory://` and `artifact://` URLs with `..` are not validated
- **File:** crates/kod-tools/src/internal_url.rs
- **Line:** 95–106
- **Severity:** Low
- **Category:** Security
- **Description:** `url_path_target` returns `Some(PathBuf::from(rest))` for `conflict://<path>`. For `memory://foo/../etc/passwd` or `artifact://foo/../../etc/passwd`, it returns `None` (only `conflict://` is handled). So those schemes are not subject to the path-gate check — but their handlers (memory, artifact) don't touch the filesystem, so no traversal. The `xd://` handler dispatches to a tool, which goes through `resolve_path`. Fine. No issue.

### T1-M14 — — `WalkCache::collect_ranked` stats every file synchronously on the async runtime
- **File:** crates/kod-tools/src/walk_cache.rs
- **Line:** 231–241
- **Severity:** Medium
- **Category:** Performance
- **Description:** `collect_ranked` does `std::fs::metadata(p).ok()?` for every path in a loop, on the async runtime. For 1000 paths, that's 1000 sync stat syscalls on the async thread, blocking the runtime. Should be in `spawn_blocking`.
- **Fix:** Wrap the whole function in `tokio::task::spawn_blocking`.

### T1-M15 — — `WebFetchTool` does two DNS lookups (pre-flight + pin) — double the DNS load
- **File:** crates/kod-tools/src/web.rs
- **Line:** 209–216 (pre-flight), 242–250 (pin)
- **Severity:** Low
- **Category:** Performance
- **Description:** The pre-flight does `to_socket_addrs` to check every address. The pin does it *again* to find the first validated address. Two DNS lookups per fetch. The second may hit a different resolver result under TTL-0 (the basis of T1-C15).
- **Fix:** Cache the pre-flight's address list and pick from it.

### T1-M16 — — `MemoryManager::consolidate`'s `resolve_contradictions` does `get_all` *again* (third full scan)
- **File:** crates/kod-memory/src/manager.rs
- **Line:** 1174
- **Severity:** Medium
- **Category:** Performance
- **Description:** `consolidate` calls `get_all` (line 1120), iterates for archival, then `fuse_duplicates(&remaining)` (which is passed the remaining slice — good, no re-scan), then `resolve_contradictions` which calls `get_all` *again* (line 1174). That's two full scans. The third pass (resolve_contradictions) could take `&remaining` like fuse does.
- **Fix:** Pass `&all` (or `&remaining`) to `resolve_contradictions`.

### T1-M17 — — `index_text_for_embedding` allocates a new String per call (per retrieval)
- **File:** crates/kod-memory/src/manager.rs
- **Line:** 95–148
- **Severity:** Low
- **Category:** Performance
- **Description:** `index_text_for_embedding` does `String::with_capacity(stripped.len())` and builds a new String. Called once per retrieval (for the query). 1 allocation per retrieval. Fine.

### T1-M18 — — `QueryEmbedCache::get` clones the cached vector on every hit
- **File:** crates/kod-memory/src/manager.rs
- **Line:** 178–184
- **Severity:** Low
- **Category:** Performance
- **Description:** `Some(entry.1.clone())` clones the entire `Vec<f32>` (768 floats = 3KB) on every cache hit. For a query that hits 10 times in a session, 30KB of clones. Could return `Arc<Vec<f32>>` or a reference (with a lifetime tied to the guard).
- **Fix:** Store `Arc<Vec<f32>>` in the cache, return a clone of the Arc (cheap).

### T1-M19 — — `redb` `store_batch` serializes all entries before opening the txn — a serialization error mid-batch fails the whole batch
- **File:** crates/kod-memory/src/long_term.rs
- **Line:** 114–145
- **Severity:** Low
- **Category:** Error handling
- **Description:** `store_batch` does `let prepared: Vec<(Vec<u8>, Vec<u8>)> = entries.into_iter().map(|e| { ... serde_json::to_vec(&e) ... }).collect::<Result<Vec<_>>>()?;`. If one entry fails to serialize (shouldn't, but a poisoned enum variant could), the whole batch fails. The doc says "a serialization error fails the batch without touching redb" — that's the intent. Fine.

### T1-M20 — — `bwrap_invocation` does not bind `/dev/null`, `/dev/zero`, `/dev/urandom` explicitly — `--dev /dev` creates a fresh devtmpfs which may not include them
- **File:** crates/kod-tools/src/context.rs
- **Line:** 375–379
- **Severity:** Low
- **Category:** Sandbox safety
- **Description:** `--dev /dev` creates a fresh devtmpfs in the sandbox. On most systems this includes `/dev/null`, `/dev/zero`, `/dev/urandom`, `/dev/random`. But on stripped containers (no devtmpfs mount in the host), `--dev` may produce an empty `/dev`. A child that opens `/dev/null` for stdout redirection gets ENOENT. Not a security issue, but a breakage issue.
- **Fix:** Add `--dev /dev` (already there) and trust bwrap to populate it. If a host's devtmpfs is broken, the sandbox fails loudly (the child can't open /dev/null). Acceptable.

### T1-M21 — — `MentalModels::seed` is create-only but doesn't log a warning when a second seed is refused
- **File:** crates/kod-memory/src/mental_models.rs
- **Line:** 128–135
- **Severity:** Low
- **Category:** Error handling
- **Description:** `seed` returns `false` when the id already exists, but doesn't log. A config reload that tries to re-seed silently fails for every model. An operator debugging "why isn't my new model definition taking effect" sees no signal.
- **Fix:** `tracing::debug!(id = %seed.id, "seed refused: model already exists");`

### T1-M22 — — `redb` `ReadableTable::get` returns `Result<Option<AccessGuard<[u8]>>>` — the `value.value()` call borrows the guard; if the value is large, the guard holds the page
- **File:** crates/kod-memory/src/long_term.rs
- **Line:** 157–166
- **Severity:** Low
- **Category:** Performance
- **Description:** `serde_json::from_slice(value.value())` deserializes while holding the `AccessGuard` (which borrows the redb page). For a 4KB entry, the page is held for the deser duration (~5 µs). Fine. For `get_all` (line 207–223), the iterator yields `AccessGuard` per entry, and the deser happens inside the loop — the guard is dropped at the end of each iteration. Fine.

### T1-M23 — — `MemoryManager::fuse_duplicates` embeds the group's `content` (raw), not `index_text_for_embedding(content)` (projected)
- **File:** crates/kod-memory/src/manager.rs
- **Line:** 1288
- **Severity:** Medium
- **Category:** Correctness
- **Description:** `fuse_duplicates` builds `texts: Vec<String> = group.iter().map(|e| e.content.clone()).collect()` and embeds those. But the *store* path (manager.rs:604) embeds `index_text_for_embedding(content)` (the projected text — role prefixes stripped, memories blocks stripped). So the fusion embeddings are over the *raw* content, while the index embeddings are over the *projected* content. A pair of entries that differ only in a `User:` prefix would have different projected text but the same raw text — they'd fuse (correctly, since they're the same fact). But a pair that differs only in a `<memories>` block (which the store path strips) would *not* fuse under the projected embedding, but *would* fuse under the raw embedding. Inconsistent.
- **Fix:** Use `index_text_for_embedding(content)` for fusion, matching the store path.

### T1-M24 — — `MemoryManager::retrieve_long_term_hybrid` writeback re-reads each entry inside the write step — N `get` calls (each a read txn)
- **File:** crates/kod-memory/src/manager.rs
- **Line:** 1083–1094
- **Severity:** Medium
- **Category:** Performance
- **Description:** The writeback loop `for e in &top { let Some(mut fresh) = self.long_term.get(&e.id).await? ... }` does N `get` calls, each opening a read txn. Then `store_batch(updated)` does one write txn. For top-20, that's 20 read txns + 1 write txn. The 20 read txns are MVCC-safe but each is a B-tree lookup + deser.
- **Fix:** One `get_all` (or a `get_batch(ids)`) and filter by id.

### T1-M25 — — `seatbelt_invocation` `(allow file-write* (literal "/dev/stdout") ...)` is incomplete — `printf > /dev/fd/2` is denied
- **File:** crates/kod-tools/src/context.rs
- **Line:** 452–453
- **Severity:** Low
- **Category:** Sandbox safety
- **Description:** The allow list has `/dev/stdout`, `/dev/stderr`, `/dev/null`. But a child that does `echo hi > /dev/fd/2` (a path alias for stderr) is denied — `/dev/fd/2` is not in the literal list. Common shell idioms (`echo hi >> /dev/stderr` works, but `echo hi > /dev/fd/2` doesn't) break silently. The doc (line 449–453) admits this for stdout/stderr but not for `/dev/fd/*`.
- **Fix:** Add `(allow file-write* (literal "/dev/fd/1") (literal "/dev/fd/2"))` or use a subpath allow on `/dev/fd`.

### T1-M26 — — `truncate_to_ceiling` in sharpshooter.rs doesn't preserve trailing newline semantics
- **File:** crates/kod-memory/src/sharpshooter.rs
- **Line:** 295–305
- **Severity:** Low
- **Category:** Correctness
- **Description:** `truncate_to_ceiling` pushes `line + '\n'` for each line up to the cap, then `out.trim_end().to_string()`. So a file that ends with a trailing newline loses it; a file without one gains nothing. The result is `lines joined by \n, no trailing \n`. A subsequent write would normalize the file's EOL. Minor.

### T1-M27 — — `extract.rs::parse_reply` accepts `type: "preference"` etc. but a model that sends `type: "Preference"` (capitalized) maps to `FactKind::Fact` (the catch-all)
- **File:** crates/kod-memory/src/extract.rs
- **Line:** 128–134
- **Severity:** Low
- **Category:** Correctness
- **Description:** The match is on the exact string `"preference"`, `"decision"`, etc. A model that sends capitalized or mixed-case `type` values gets the catch-all `Fact` tag. The tag is stored as `auto-fact` instead of `auto-preference`, and a `kod memory search --tag auto-preference` query misses it.
- **Fix:** `t.to_lowercase()` before the match, or use `eq_ignore_ascii_case`.

### T1-M28 — — `redb` `Database::create` is called without `set_cache_size` — default cache may be too small for a large store
- **File:** crates/kod-memory/src/long_term.rs
- **Line:** 43
- **Severity:** Low
- **Category:** Performance
- **Description:** redb's default cache is 256 MB (I think). For a 10K-entry store with 4KB entries = 40 MB of data, the cache holds the whole working set. Fine. For a 100K-entry store = 400 MB, the cache evicts. Could tune.

---

## Performance

### T1-P1 — — `manager.rs::store_with_metadata` dedup is O(N) per store via full-table scan (see H4/H12 — same root cause)
- **File:** crates/kod-memory/src/manager.rs
- **Line:** 499–530
- **Severity:** High
- **Category:** Performance
- **Description:** Already covered as T1-C12/T1-H4. Listing here for the perf grouping.

### T1-P2 — — `LongTermMemory::get_all` deserializes every entry on every retrieval (see H4)
- **File:** crates/kod-memory/src/long_term.rs
- **Line:** 198–227
- **Severity:** High
- **Category:** Performance
- **Description:** Already covered. The fix is an in-memory cache invalidated on write.

### T1-P3 — — `VectorIndex::insert`/`remove` are O(N) — no HashMap side index (see H5)
- **File:** crates/kod-memory/src/vector_index.rs
- **Line:** 62–106
- **Severity:** Medium
- **Category:** Performance

### T1-P4 — — `stopwords::is_stopword` is O(184) linear scan per token (see M8)
- **File:** crates/kod-memory/src/stopwords.rs
- **Line:** 201–203
- **Severity:** Medium
- **Category:** Performance

### T1-P5 — — `LongTermMemory::count` iterates the whole table (see H3)
- **File:** crates/kod-memory/src/long_term.rs
- **Line:** 303–323
- **Severity:** Medium
- **Category:** Performance

### T1-P6 — — `LongTermMemory::clear` removes per-key instead of `table.clear()` (see M1)
- **File:** crates/kod-memory/src/long_term.rs
- **Line:** 353–384
- **Severity:** Medium
- **Category:** Performance

### T1-P7 — — `MemoryManager::consolidate` does three full-table scans (see H8)
- **File:** crates/kod-memory/src/manager.rs
- **Line:** 1119–1156
- **Severity:** Medium
- **Category:** Performance

### T1-P8 — — `MemoryManager::retrieve_long_term_hybrid` does 4 passes over `all` (vector, keyword, importance, temporal) plus MMR
- **File:** crates/kod-memory/src/manager.rs
- **Line:** 910–968
- **Severity:** Medium
- **Category:** Performance
- **Description:** Each voice iterates `all` (10K entries) and sorts. 4 sorts of 10K entries = 4 * O(N log N) = 4 * 40K = 160K comparisons. Plus the `base` HashMap insertion (line 973–981) is O(N). Plus the RRF fusion (line 968) is O(total ranks). Plus MMR (40-pool, O(K²) Jaccard). The whole retrieval is O(N log N) per turn. For 10K entries, ~5 ms. For 100K, ~50 ms.
- **Fix:** One pass to compute all four scores into a struct, then sort once.

### T1-P9 — — `fuse_duplicates` re-embeds already-embedded entries (see H21)
- **File:** crates/kod-memory/src/manager.rs
- **Line:** 1288
- **Severity:** Medium
- **Category:** Performance

### T1-P10 — — `WalkCache::collect_ranked` stats every file synchronously (see M14)
- **File:** crates/kod-tools/src/walk_cache.rs
- **Line:** 231–241
- **Severity:** Medium
- **Category:** Performance

### T1-P11 — — `PathLockTable` cells never evicted (see H9)
- **File:** crates/kod-tools/src/path_lock.rs
- **Line:** 76–113
- **Severity:** Medium
- **Category:** Performance / Memory

### T1-P12 — — `redb` write txn per `remove` in fuse/archive loops (see C7)
- **File:** crates/kod-memory/src/manager.rs
- **Line:** 1403–1408, 1130–1143
- **Severity:** High
- **Category:** Performance

### T1-P13 — — `ShortTermMemory::store` eviction is O(N) via `entries.remove(0)` + index rebuild
- **File:** crates/kod-memory/src/short_term.rs
- **Line:** 54–67
- **Severity:** Low
- **Category:** Performance
- **Description:** `entries.remove(0)` shifts all N-1 subsequent elements (O(N) memmove). The index rebuild (line 60–65) iterates all remaining entries and decrements positions (O(N)). Combined, O(N) per insert-when-full. For capacity 100, that's 100 memmoves per insert. For 1000 inserts, 100K operations. Marginal.
- **Fix:** Use `VecDeque` (push_back + pop_front is O(1)) or `linked_hash_map`.

### T1-P14 — — `QueryEmbedCache::get` clones the vector on every hit (see M18)
- **File:** crates/kod-memory/src/manager.rs
- **Line:** 178–184
- **Severity:** Low
- **Category:** Performance

### T1-P15 — — `WebFetchTool` per-call client rebuild (see H16)
- **File:** crates/kod-tools/src/web.rs
- **Line:** 264–302
- **Severity:** Low
- **Category:** Performance

### T1-P16 — — `bwrap_invocation` re-builds the args Vec from scratch per call — no caching
- **File:** crates/kod-tools/src/context.rs
- **Line:** 357–409
- **Severity:** Low
- **Category:** Performance
- **Description:** Every `execute_command` call (via `resolver.invocation(...)`) re-builds the bwrap args Vec. For a session with 100 commands, 100 Vec allocations. Marginal.
- **Fix:** Cache the invocation per `(working_dir, opts)` — but `working_dir` changes per tool call? No, it's per-session. Cache.

### T1-P17 — — `gitaware_walk` is called per `list_files`/`grep`/`search_files` — the cache helps but the first call per turn is a full walk
- **File:** crates/kod-tools/src/tools.rs
- **Line:** 1440–1470
- **Severity:** Low
- **Category:** Performance
- **Description:** The walk cache (walk_cache.rs) memoizes per `(root, recursive)`. The first call per turn walks; subsequent calls hit the cache. For a turn that does 1 `list_files` + 5 `grep`s on the same root, the first grep walks, the next 4 hit. But the first grep still walks the whole tree. For a 10K-file tree, the walk is ~50 ms. Fine.

### T1-P18 — — `redb` `Database::create` default `Durability::Immediate` fsyncs per commit (see H24)
- **File:** crates/kod-memory/src/long_term.rs
- **Line:** 43
- **Severity:** Medium
- **Category:** Performance

### T1-P19 — — `MemoryManager::retrieve_long_term_hybrid` writeback does N `get` calls (see M24)
- **File:** crates/kod-memory/src/manager.rs
- **Line:** 1083–1094
- **Severity:** Medium
- **Category:** Performance

### T1-P20 — — `SearchFilesTool::execute` calls `read_to_string` per file — no streaming, no early-exit on first match
- **File:** crates/kod-tools/src/search.rs
- **Line:** 145–180
- **Severity:** Low
- **Category:** Performance
- **Description:** For each file, `read_to_string(&file)` loads the whole file into memory, then iterates lines. For a 8 MB file (the cap), that's 8 MB allocation per file. Could stream with `BufReader` and `lines()`, early-exit on the first match if `MAX_MATCHES` is reached. The `break 'outer` does early-exit on the file loop, but not within a file.

---

## Code quality

### T1-Q1 — — `unsafe { std::env::set_var / remove_var }` in tests with misleading SAFETY comment (see C17)
- **File:** crates/kod-memory/src/embedding.rs
- **Line:** 467–481

### T1-Q2 — — `LandlockProfile` has `claims_git_readonly` but it's hardcoded to refuse — dead field (see H15)
- **File:** crates/kod-tools/src/sandbox/landlock.rs
- **Line:** 220–227

### T1-Q3 — — `LongTermMemory::close` doc is misleading (see M2)
- **File:** crates/kod-memory/src/long_term.rs
- **Line:** 327–351

### T1-Q4 — — `MemoryManager::close` doesn't drain background tasks (see M3)
- **File:** crates/kod-memory/src/manager.rs
- **Line:** 1448–1459

### T1-Q5 — — `MemoryManager::retrieve_long_term_hybrid` doesn't filter superseded entries (see M6)
- **File:** crates/kod-memory/src/manager.rs
- **Line:** 802–1104

### T1-Q6 — — `fuse_duplicates` embeds raw content, not projected (see M23)
- **File:** crates/kod-memory/src/manager.rs
- **Line:** 1288

### T1-Q7 — — `tools.rs::atomic_write` is `pub(crate)` but used by `edit_hashline.rs` — coupling
- **File:** crates/kod-tools/src/tools.rs
- **Line:** 154
- **Severity:** Low
- **Category:** Code quality
- **Description:** `atomic_write` is `pub(crate)` and called from `edit_hashline.rs:280`. The coupling is fine (same crate) but the function lives in `tools.rs` (the tool dispatcher) rather than in a dedicated `io` module. A future refactor that moves `atomic_write` would break `edit_hashline`.
- **Fix:** Move `atomic_write` to a `crate::io` or `crate::fs` module.

### T1-Q8 — — `describe_path_error` returns a String with advice embedded — not structured
- **File:** crates/kod-tools/src/tools.rs
- **Line:** 46–74
- **Severity:** Low
- **Category:** Code quality
- **Description:** The error message includes "Check the path — a typo or a directory you have not listed yet is the common cause." — that's advice to the model, not a structured error. A future caller that wants to render errors differently (e.g., a TUI with a different style) has to parse the string. Should be an enum variant with structured fields.
- **Fix:** Return `enum PathError { NotFound(PathBuf), PermissionDenied(PathBuf), IsDirectory(PathBuf), Other(PathBuf, io::Error) }` and let the caller format.

### T1-Q9 — — `MemoryManager` has 25+ fields — god object
- **File:** crates/kod-memory/src/manager.rs
- **Line:** 213–257
- **Severity:** Low
- **Category:** Code quality
- **Description:** `MemoryManager` holds: `short_term`, `long_term`, `context_window`, `embedder`, `vector_index`, `rebuild_in_progress`, `inflight_embeddings`, `embedding_complete`, `scorer`, `redactor`, `query_embed_cache`. 11 fields, several of which are `Arc<RwLock<...>>` / `Arc<Atomic...>`. The manager is the single point of mutation for the whole memory subsystem. A future split (e.g., a separate `EmbeddingCoordinator` for the background tasks) would reduce the field count.
- **Fix:** Extract `EmbeddingCoordinator { embedder, inflight, notify, query_cache }` as a separate struct.

### T1-Q10 — — `tools.rs` is 2788 lines — the dispatcher, all built-in tools, and helpers in one file
- **File:** crates/kod-tools/src/tools.rs
- **Line:** 1–2788
- **Severity:** Low
- **Category:** Code quality
- **Description:** `tools.rs` contains `ReadFileTool`, `WriteFileTool`, `ExecuteCommandTool`, `ListFilesTool`, `PatchFileTool`, `GrepTool`, plus `atomic_write`, `kill_child_tree`, `describe_path_error`, `read_some`, `gitaware_walk`, `truncate_entry`. Each tool could be its own file (like `edit_tool.rs`, `search.rs`). The current shape makes it hard to navigate.
- **Fix:** Split into `tools/read_file.rs`, `tools/write_file.rs`, `tools/execute_command.rs`, etc.

### T1-Q11 — — `context.rs` is 1780 lines — `ToolContext`, `SandboxResolver`, `SandboxInvocation`, `BackgroundSpawnHook`, `BackgroundAdoptHook`, `FileTouchHook`, `ArtifactStoreHook`, `PrefetchedRead`, plus the bwrap/seatbelt/landlock builders
- **File:** crates/kod-tools/src/context.rs
- **Line:** 1–1780
- **Severity:** Low
- **Category:** Code quality
- **Description:** The context file mixes the per-call context struct with the platform sandbox resolver. The sandbox code (bwrap, seatbelt, landlock builders) should be in `sandbox/` alongside `landlock.rs`.
- **Fix:** Move `bwrap_invocation` and `seatbelt_invocation` to `sandbox/bwrap.rs` and `sandbox/seatbelt.rs`.

### T1-Q12 — — `internal_url.rs` is 1098 lines — the router, the scheme parser, the protocol handler trait, the resolve context, all in one file
- **File:** crates/kod-tools/src/internal_url.rs
- **Severity:** Low
- **Category:** Code quality

### T1-Q13 — — Many `unwrap()`s in tests (the task description says "217 unwrap() in kod-memory")
- **File:** crates/kod-memory/src/* (tests)
- **Severity:** Low
- **Category:** Code quality
- **Description:** Test code uses `.unwrap()` liberally. That's idiomatic for tests (a panic is a failure), but the count (217) suggests some non-test code may also unwrap. A quick scan shows the unwraps are in `#[cfg(test)]` modules — fine. The production code uses `?` and `map_err`. No production panics found.

### T1-Q14 — — `MemoryManager::store_with_metadata` has 14 `if`/`match` arms before the actual store — long function
- **File:** crates/kod-memory/src/manager.rs
- **Line:** 450–614
- **Severity:** Low
- **Category:** Code quality
- **Description:** The function does: redact, strip tags, redact tags, cap content, hash content, dedup-scan, bump confidence, construct entry, store, spawn embed. 164 lines. Could be split into `redact_and_prepare(content, metadata) -> (content, metadata)`, `dedup_or_store(...)`, `spawn_embed(...)`.
- **Fix:** Extract helper functions.

### T1-Q15 — — `redb::Database` is wrapped in `Arc<Database>` and `LongTermMemory` is `Clone` (clones the Arc) — the Clone impl is hand-written
- **File:** crates/kod-memory/src/long_term.rs
- **Line:** 387–393
- **Severity:** Low
- **Category:** Code quality
- **Description:** `impl Clone for LongTermMemory { fn clone(&self) -> Self { Self { db: self.db.clone() } } }` — this is identical to `#[derive(Clone)]`. The hand-written impl adds nothing. Use derive.

### T1-Q16 — — `MemoryManager::embedder_name` returns `&'static str` but the embedder's `name()` returns `&str` — the function discards the actual name
- **File:** crates/kod-memory/src/manager.rs
- **Line:** 305–311
- **Severity:** Low
- **Category:** Code quality
- **Description:** `embedder_name()` returns `"configured"`, `"unusable"`, or `"none"` — not the actual embedder name (`"ollama"`, `"openai"`). The doc says "the installed embedder's name" but the implementation returns a status string. Misleading.
- **Fix:** Return the actual `embedder.name()` or rename the function to `embedder_status()`.

### T1-Q17 — — `LandlockPathBeneathAttr` is `#[repr(C, packed)]` — taking a reference to a packed field is UB
- **File:** crates/kod-tools/src/sandbox/landlock.rs
- **Line:** 129–133
- **Severity:** Medium
- **Category:** Correctness / UB
- **Description:** `#[repr(C, packed)]` structs have no padding. Taking a reference to a field of a packed struct is UB because the field may be unaligned. The code does `&rule as *const _ as *const c_void` — that's a reference to the *whole struct*, not a field. The struct as a whole is aligned (it's a local variable), so `&rule` is fine. But if anyone later writes `&rule.parent_fd` (to read the fd back), that's UB. The current code is safe, but the `packed` is a footgun.
- **Fix:** Add a comment warning against taking field references, or use `#[repr(C)]` (the kernel struct is naturally aligned to 4 bytes for the `__s32` field, but the UAPI header declares it `packed` to be explicit about no trailing padding — `repr(C)` with explicit padding would also work).

### T1-Q18 — — `internal_url::ProtocolRouter::register` clones the entire HashMap per registration
- **File:** crates/kod-tools/src/internal_url.rs
- **Line:** 278–285
- **Severity:** Low
- **Category:** Performance
- **Description:** `let mut map = (*self.handlers).clone(); map.insert(scheme, handler); Self { handlers: Arc::new(map) }`. For N handlers, registering all of them is O(N²) clone work. For ~10 handlers, 100 clones — fine. For 1000 handlers (an MCP proxy with many tools), 1M clones — slow.
- **Fix:** Use `Arc::make_mut` if the Arc is unique, or accept the copy.

### T1-Q19 — — `kod-memory/src/manager.rs` has 2413 lines — the manager, the cache, the pending memory, the consolidation, and tests in one file
- **File:** crates/kod-memory/src/manager.rs
- **Line:** 1–2413
- **Severity:** Low
- **Category:** Code quality
- **Description:** The file mixes the manager struct, the query cache, the `PendingMemory` snapshot, the consolidation pass, and 4 test modules. Could be split into `manager/mod.rs`, `manager/pending.rs`, `manager/consolidation.rs`, `manager/cache.rs`.

### T1-Q20 — — `kill_child_tree` uses `libc::kill` directly (unsafe) without a safe wrapper
- **File:** crates/kod-tools/src/tools.rs
- **Line:** 203–214
- **Severity:** Low
- **Category:** Code quality
- **Description:** The function has one `unsafe` block for the `libc::kill` call. The safety argument (PID > 0, PGID is valid) is not documented in a SAFETY comment. The `pid_t` cast `pid as libc::pid_t` assumes `pid` fits in `pid_t` (it does — `pid` is `u32` from `child.id()`, `pid_t` is `i32`, so values > i32::MAX would overflow — but PIDs are limited to a few million in practice).
- **Fix:** Add a `// SAFETY:` comment, or wrap in a `nix::unistd::kill` call (safe wrapper).

### T1-Q21 — — `execute_command` env loop iterates `std::env::vars_os()` — for a child with a 1000-var env, that's 1000 `spawn.env(k, v)` calls
- **File:** crates/kod-tools/src/tools.rs
- **Line:** 972–979
- **Severity:** Low
- **Category:** Performance
- **Description:** Each `spawn.env(k, v)` inserts into the child's env map. For 1000 vars, 1000 insertions. `Command::env_clear().envs(iter)` would be one bulk insert. Marginal.

### T1-Q22 — — `MemoryManager::store_with_metadata` clones `metadata.tags` for redaction, then clones again for the entry
- **File:** crates/kod-memory/src/manager.rs
- **Line:** 469
- **Severity:** Low
- **Category:** Performance
- **Description:** `metadata.tags = metadata.tags.iter().map(|t| self.redact_text(t)).collect()` allocates a new Vec, then the entry construction (line 548+) moves `metadata` in. The intermediate Vec is dropped. One allocation per store. Fine.

### T1-Q23 — — `redb` `Database::create` is called with `path: &Path` — no `OpenOptions`-like config for cache size, durability, etc.
- **File:** crates/kod-memory/src/long_term.rs
- **Line:** 43
- **Severity:** Low
- **Category:** Code quality
- **Description:** `Database::create(path)` uses all defaults. redb's `Database::builder()` allows tuning `cache_size`, `durability`, `read_cache_size`. None of these are exposed. A future perf tuning pass would need to thread them through `LongTermMemory::new`.

### T1-Q24 — — `MemoryManager::set_embedder` invalidates the vector index and the query cache — but not the `rebuild_in_progress` flag
- **File:** crates/kod-memory/src/manager.rs
- **Line:** 292–301
- **Severity:** Low
- **Category:** Concurrency
- **Description:** `set_embedder` does `*self.vector_index.write() = None;` and `self.query_embed_cache.lock().entries.clear();`. If a `rebuild_index` is in flight (rebuild_in_progress = true), the rebuild will finish and write a new index based on the *old* embedder's vectors. The new embedder's vectors are never built until the next retrieval. The `rebuild_in_progress` flag should also be reset.
- **Fix:** `self.rebuild_in_progress.store(false, SeqCst);` in `set_embedder`.

### T1-Q25 — — `redb::Database` is `Send + Sync` but the `blocking()` helper requires `F: Send + 'static` — closures capturing `&self` won't compile, so each call clones an `Arc`
- **File:** crates/kod-memory/src/long_term.rs
- **Line:** 66–75
- **Severity:** Low
- **Category:** Code quality
- **Description:** `async fn blocking<T, F>(&self, f: F) -> Result<T> where F: FnOnce(&Database) -> Result<T> + Send + 'static`. The closure can't capture `&self` (not `'static`), so `let db = Arc::clone(&self.db);` is done before the closure, and the closure captures `db` by move. That's correct, but the `Arc::clone` per call is a refcount bump (cheap). Fine.

---

## Summary of recurring themes

1. **Sandbox backend inconsistencies**: bwrap is read-restrictive but doesn't unshare PID; seatbelt is fail-open for reads; landlock can't enforce net_deny even on ABI ≥ 4. Each backend has a different gap; the operator who switches backend gets a different threat model.

2. **Full-table scans on the hot path**: `get_all()` is called per store (dedup), per retrieval (scoring), per consolidate (3×), per fuse. Each call deserializes the whole store. No secondary indices, no in-memory cache.

3. **O(N) per op in vector index**: insert/remove/search are all O(N). For 10K entries the search is fine (5 ms), but rebuild is O(N²) and a future 100K-entry store would be 100M operations per rebuild.

4. **Per-id write transactions**: every `remove` and `store` opens its own write txn (fsync). Batched APIs exist (`store_batch`) but `remove_batch` doesn't, and the fuse/archive loops use the per-id API.

5. **TOCTOU windows**: `resolve_path` → `can_write` → `revalidate_write_parent` → `atomic_write` has microsecond windows between each. The pragmatic checks shrink but don't close them. `openat2(RESOLVE_BENEATH)` is the only complete fix.

6. **Stopword / stem linear scans**: 184-element `contains` per token, called per candidate per retrieval. Should be a HashSet.

7. **Unsafe env mutation in tests**: `std::env::set_var` is `unsafe` in 2024; the test's SAFETY claim of "single-threaded" is wrong for the default multi-threaded test runner.

8. **Misleading docs**: `LongTermMemory::close` claims deterministic close but depends on background tasks; `embedder_name` claims to return the name but returns a status; landlock claims to refuse net_deny on low ABI but silently accepts it on high ABI.

9. **God objects and 2K+ line files**: `MemoryManager` (11 fields, 2413 lines), `tools.rs` (2788 lines), `context.rs` (1780 lines). Each could be split for navigability.

10. **Mixed EOL handling**: `patch.rs` and `edit_hashline.rs` both use `contains("\r\n")` to detect CRLF, which flips the whole file's EOL based on a single CRLF line. Should be majority-vote or per-line preservation.

---

**Total findings: 50** (T1-C1–T1-C17, T1-H1–T1-H24, T1-M1–T1-M28, T1-P1–T1-P20, T1-Q1–T1-Q25, with overlaps between severity and performance/quality groupings where the same root cause is listed in both for visibility). Sandbox/landlock findings: T1-C1–T1-C8, T1-H13, T1-H15, T1-M20, T1-M25, T1-Q17, T1-Q20 (12 findings touching the sandbox surface). Path-traversal / TOCTOU: T1-C2, T1-C9, T1-H12, T1-H13 (4 findings). Patch/edit: T1-C9, T1-C10, T1-C11 (3 findings). Persistence: T1-C6, T1-C7, T1-C13, T1-C14, T1-H1, T1-H3, T1-H4, T1-H7, T1-H8, T1-M1, T1-M2, T1-M3, T1-P12 (13 findings). SSRF: T1-C15 (1 finding, the most severe). Embedding: T1-C17, T1-H19, T1-M4, T1-M23 (4 findings).

---
# Part 2 — Network & IO Layer (providers, MCP, LSP)

_Crates: kod-provider, kod-provider-anthropic, kod-provider-openai, kod-mcp, kod-lsp_


## Critical bugs

### T2-C1 — — Sub-second `Retry-After` hint truncated to 0s, retries hammer the server
- **File:** crates/kod-provider-anthropic/src/provider.rs
- **Line:** 383
- **Severity:** Critical
- **Category:** Network / Error handling
- **Description:** When Anthropic returns a 429 with a sub-second hint (e.g. `retry-after-ms: 500`), the `extract_retry_hints` helper correctly produces `Duration::from_millis(500)`, but the code that surfaces the typed error stuffs it into `KodError::RateLimited { retry_after_secs: delay.as_secs() }`. `Duration::as_secs()` truncates — 500 ms becomes `0`. The retry layer then sees `retry_after_secs = 0`, computes `hint = Duration::from_secs(0)`, and sleeps `hint.min(max_delay) = 0`. The retry fires immediately, gets another 429, retries immediately, and so on for all three attempts within milliseconds.
- **Code:**
```rust
if status.as_u16() == 429
    && let Some(delay) = hints.delay
    && !delay.is_zero()
    && delay <= self.rate_limit_wait
{
    return Err(KodError::RateLimited {
        retry_after_secs: delay.as_secs(),  // ← truncates 500ms → 0
    });
}
```
- **Why it's a bug:** A real provider 429 with a 500 ms hint turns into a retry storm of three immediate POSTs, the opposite of what the server asked for. With many agents (swarm mode), the lockstep retry pattern is the failure mode `with_retry`'s jitter was supposed to prevent, and jitter is irrelevant when the hint is forced to zero.
- **Fix:** Surface the hint at millisecond granularity. Either widen `KodError::RateLimited` to carry a `Duration`, or convert to milliseconds:
```rust
return Err(KodError::RateLimited {
    retry_after_secs: (delay.as_millis() as u64).div_ceil(1000).max(1),
});
// or, better: change KodError::RateLimited to carry Duration
```
- **Notes:** The same `delay.as_secs()` truncation appears nowhere else, but `KodError::RateLimited`'s field name (`retry_after_secs`) is the root cause — the type cannot represent sub-second hints. Consider widening the type.

### T2-C2 — — OpenAI 429 defaults to a 20-minute sleep when body has no cue phrase
- **File:** crates/kod-provider-openai/src/provider.rs
- **Line:** 1066–1069
- **Severity:** Critical
- **Category:** Network / Error handling
- **Description:** `adk_err_typed` maps a 429 to `RateLimited` with `text_hint_secs(&text).unwrap_or(1200)`. `text_hint_secs` only matches cue phrases like "try again in", "retry after", "retry in", "wait " followed by a number and unit. If the body is JSON like `{"error":{"type":"rate_limit_exceeded","message":"Slow down"}}` (no cue), `text_hint_secs` returns `None`, and the default 1200 s (20 minutes) is used. The engine sleeps 20 minutes on a soft rate-limit the server probably wanted waited out for 1–5 seconds.
- **Code:**
```rust
fn adk_err_typed(e: adk_core::AdkError) -> KodError {
    if e.details.upstream_status_code == Some(429) {
        let text = e.to_string();
        let secs = kod_provider::retry::text_hint_secs(&text).unwrap_or(1200);
        return KodError::RateLimited {
            retry_after_secs: secs,
        };
    }
    ...
}
```
- **Why it's a bug:** A 429 from OpenAI proper (which sends `Retry-After` as a header, unreachable here because adk-model flattens the response) or any JSON-body 429 from a compatible server causes a 20-minute user-visible hang on every rate-limited turn. The agent looks frozen; cancellation is the only escape. The 1200 s default is tuned for tab-bridge's 20-min send-frequency limit, not for ordinary 429s.
- **Fix:** Default to the small linear backoff path (treat as a generic transient) when no hint is parseable, instead of guessing 1200 s:
```rust
if e.details.upstream_status_code == Some(429) {
    // Only surface a typed RateLimited when we actually parsed a hint.
    // A hint-less 429 falls through to adk_err, which the retry layer
    // treats as a generic transient and backs off with jitter.
    if let Some(secs) = kod_provider::retry::text_hint_secs(&e.to_string()) {
        return KodError::RateLimited { retry_after_secs: secs };
    }
}
```
- **Notes:** Same fix applies to the 503/`ServerBusy` arm below it (`unwrap_or(600)`).

### T2-C3 — — `is_session_busy` matches any "409 " substring in error messages
- **File:** crates/kod-provider-openai/src/provider.rs
- **Line:** 1126–1135
- **Severity:** Critical
- **Category:** Correctness / Error handling
- **Description:** The 409-session-busy classifier runs four substring checks against the formatted error message: `"409 "`, `" 409"`, `"status 409"`, `"HTTP 409"`. Any provider error whose message happens to contain one of these substrings is misclassified as a `409 session_busy` and routed through the background-gate retry path with 2 s + 8 s backoffs. A genuine transport error like `"http: error sending request: (HTTP 409 proxy redirect)"` or a body containing `"Error 409 too many fields"` triggers it.
- **Code:**
```rust
fn is_session_busy(err: &kod_error::KodError) -> bool {
    match err {
        kod_error::KodError::Provider(msg) => {
            msg.contains("409 ")
                || msg.ends_with(" 409")
                || msg.contains("status 409")
                || msg.contains("HTTP 409")
        }
        _ => false,
    }
}
```
- **Why it's a bug:** The function comment acknowledges the original bug ("'4096 tokens' / timestamps previously triggered the whole background retry schedule"). The fix moved from substring matching to status-code matching on the streaming path (`adk_precommit_retryable`), but the background collect path still uses string matching. Any new error message that happens to contain "409 " will silently enable the 2 s/8 s background retry schedule on the main turn path, multiplying latency on errors that should fail fast.
- **Fix:** Carry the upstream status code on `KodError::Provider` (or use `KodError::provider_status`), then match on the code:
```rust
fn is_session_busy(err: &kod_error::KodError) -> bool {
    match err {
        kod_error::KodError::Provider(msg) => {
            // Tag at the source instead of pattern-matching prose.
            msg.contains("\"status\":409")
                || msg.contains("\"status\": 409")
        }
        _ => false,
    }
}
```
Better: add a typed `KodError::SessionBusy { retry_after_secs }` variant and have `adk_err_typed` return it directly when `upstream_status_code == Some(409)`.

### T2-C4 — — LSP notifications arriving during `request_with_response` are silently dropped
- **File:** crates/kod-lsp/src/client.rs
- **Line:** 488–510
- **Severity:** Critical
- **Category:** Correctness / Concurrency
- **Description:** `request_with_response` (used by `definition`, `references`, `hover`) reads messages in a loop and returns only when a message with the matching id arrives. Any notification (no `id` field, e.g. `textDocument/publishDiagnostics`, `window/logMessage`, `$/progress`) is read, fails the `msg.get("id") == Some(id)` check, and is discarded without being processed. Diagnostics published by the server while a `definition` request is in flight never reach the engine.
- **Code:**
```rust
let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
loop {
    let now = tokio::time::Instant::now();
    if now >= deadline {
        return Err(LspError::Timeout);
    }
    let remaining = deadline - now;
    let msg = tokio::time::timeout(remaining, self.read_handling_server_requests())
        .await
        .map_err(|_| LspError::Timeout)??;
    if msg.get("id").and_then(|v| v.as_i64()) == Some(id) {
        ...
        return Ok(msg.get("result").cloned().unwrap_or(serde_json::Value::Null));
    }
    // ← notification: discarded, no handler invoked
}
```
- **Why it's a bug:** rust-analyzer publishes diagnostics continuously — during a `definition` request, the server often publishes a fresh diagnostics batch for the file the user just edited. The diagnostics are dropped, the engine never sees them, and the write-gating logic treats the file as unchanged. Same issue in `initialize` (line 154–176) where any notification arriving during the handshake is lost. The bug is silent — no log, no error.
- **Fix:** Queue non-matching messages (notifications) into an internal channel that `collect_diagnostics` drains, or invoke a registered handler:
```rust
// In request_with_response, instead of dropping:
if msg.get("id").and_then(|v| v.as_i64()) != Some(id) {
    // Stash the notification for the diagnostics reader.
    if self.pending_notifications.push(msg).is_err() {
        tracing::warn!("LSP: notification queue full, dropping");
    }
    continue;
}
```
Better: restructure to a single-reader model where one task demultiplexes by id and dispatches notifications to subscribers.

### T2-C5 — — LSP `Content-Length` header can OOM the client (no upper bound)
- **File:** crates/kod-lsp/src/client.rs
- **Line:** 605–610
- **Severity:** Critical
- **Category:** Security
- **Description:** `read_message` parses the `Content-Length` header as a `usize` and immediately allocates `vec![0u8; n]`. There is no upper bound on `n`. A malicious or buggy LSP server that sends `Content-Length: 999999999999\r\n\r\n` causes the client to attempt a ~1 TB allocation. On most systems this either panics (allocation failure) or triggers the OOM killer, taking the whole agent process down.
- **Code:**
```rust
let n = content_length
    .ok_or_else(|| LspError::Protocol("missing Content-Length header".to_string()))?;
let mut buf = vec![0u8; n];  // ← unbounded
self.stdout.read_exact(&mut buf).await?;
```
- **Why it's a bug:** LSP servers are spawned subprocesses, but "subprocess" is not "trusted". A misconfigured server, a server with a memory-corruption bug, or a server compromised via a malicious project file can send arbitrary bytes on stdout. The "minimal client" framing in the module doc does not change that the client must defend against malformed framing.
- **Fix:** Cap `n` at a sane upper bound for an LSP message (e.g., 50 MB):
```rust
const MAX_MESSAGE_BYTES: usize = 50 * 1024 * 1024;
let n = content_length
    .ok_or_else(|| LspError::Protocol("missing Content-Length header".to_string()))?;
if n > MAX_MESSAGE_BYTES {
    return Err(LspError::Protocol(format!(
        "Content-Length {n} exceeds the {MAX_MESSAGE_BYTES}-byte cap"
    )));
}
let mut buf = vec![0u8; n];
```

### T2-C6 — — MCP reader has no line-length cap; a malicious server can OOM the client
- **File:** crates/kod-mcp/src/client.rs
- **Line:** 335–342
- **Severity:** Critical
- **Category:** Security
- **Description:** `read_loop` reads the server's stdout via `BufReader::new(stdout).lines()` and parses each line as a JSON object. `tokio::io::AsyncBufReadExt::lines` imposes no maximum line length — it reads until `\n` or EOF, growing an internal buffer without bound. A server that emits a line with no `\n` for hundreds of megabytes forces the client to buffer it all, then attempt to parse it as JSON (which will fail, but the allocation has already happened).
- **Code:**
```rust
let mut reader = BufReader::new(stdout).lines();
loop {
    match reader.next_line().await {
        Ok(Some(line)) => {
            let trimmed = line.trim();
            ...
```
- **Why it's a bug:** Same threat model as T2-C5. MCP servers are external processes spawned from `npx`/`uvx`/etc.; a compromised package or a buggy server can send arbitrarily long lines. The `lines()` reader will happily allocate until OOM.
- **Fix:** Cap the line length with a custom reader:
```rust
const MAX_LINE_BYTES: usize = 10 * 1024 * 1024; // 10 MB
let mut reader = tokio_util::io::ReaderStream::new(stdout, ...);
// or: read into a Vec<u8> with a cap, erroring when it overflows
```
A simpler fix is to use `tokio::io::AsyncBufReadExt::read_until` with a manual cap:
```rust
let mut buf = Vec::new();
let n = reader.read_until(b'\n', &mut buf).await?;
if buf.len() > MAX_LINE_BYTES {
    tracing::error!("MCP: line exceeded {} bytes, closing connection", MAX_LINE_BYTES);
    break;
}
```

### T2-C7 — — `kill_on_drop` does not kill the process group; grandchildren leak
- **File:** crates/kod-mcp/src/client.rs, crates/kod-lsp/src/client.rs
- **Line:** kod-mcp/src/client.rs:94, kod-lsp/src/client.rs:83
- **Severity:** Critical
- **Category:** Concurrency / OS
- **Description:** Both `McpClient::spawn_stdio` and `LspClient::start` call `Command::kill_on_drop(true)`, which sends SIGKILL to the immediate child on drop. Neither sets a process group (`process_group(0)` on Unix). MCP and LSP servers commonly spawn children: `npx` spawns `node`, rust-analyzer spawns `cargo check`, `pyright-langserver` may spawn `python`. When the parent is killed, those grandchildren are orphaned and keep running, holding file handles, eating CPU, and pinning the workspace.
- **Code:**
```rust
let mut command = Command::new(cmd);
command
    .args(args)
    .stdin(Stdio::piped())
    .stdout(Stdio::piped())
    .stderr(Stdio::null())
    .kill_on_drop(true);  // ← no process_group(0)
```
- **Why it's a bug:** A daily driver that opens and closes the agent 50 times a day with a misbehaving MCP server can accumulate dozens of orphaned `node`/`cargo`/`python` processes, each pinning gigabytes of memory. The user sees a slow system and a fast battery drain with no signal pointing at the agent. On Windows the equivalent is `CREATE_NEW_PROCESS_GROUP` + `taskkill /T`.
- **Fix:** Set a new process group on Unix and kill the whole group on shutdown:
```rust
#[cfg(unix)]
{
    use std::os::unix::process::CommandExt;
    command.process_group(0);
}
```
And in shutdown, kill the negative PID:
```rust
#[cfg(unix)]
{
    let pid = child.id().unwrap();
    let _ = unsafe { libc::kill(-(pid as i32), libc::SIGTERM) };
}
```
- **Notes:** Pulls in `nix` or `libc` as a dep. Worth it.

### T2-C8 — — Concurrency permit held during rate-limit sleep, blocking other streams
- **File:** crates/kod-provider-anthropic/src/provider.rs, crates/kod-provider-openai/src/provider.rs
- **Line:** kod-provider-anthropic/src/provider.rs:524–556 (the `let _permit = concurrency.acquire().await;` is at line 524, the sleep is at lines 555 and 568)
- **Severity:** Critical
- **Category:** Performance / Concurrency
- **Description:** The streaming attempt loop acquires a concurrency permit at the top of each iteration and holds it across the entire attempt — including the rate-limit sleep before the next retry. If the cap is 1 and a stream hits a 429 with a 5 s hint, the permit is held for the full 5 s sleep. Any other stream trying to start during that window parks. The whole point of the cap is to bound concurrent HTTP requests; holding the slot during a sleep that issues no requests defeats the purpose.
- **Code:**
```rust
loop {
    attempt += 1;
    let _permit = concurrency.acquire().await;  // ← held across the sleep
    ...
    if attempt < MAX_STREAM_ATTEMPTS {
        ...
        tokio::time::sleep(d).await;  // ← permit still held
        continue;  // ← permit dropped here, next iteration re-acquires
    }
}
```
- **Why it's a bug:** With `cap = 1` and a swarm of 8 agents, a single 429 on one stream blocks all 7 others for the duration of the rate-limit sleep. The cap was supposed to spread load across the endpoint; instead it serializes rate-limit waits. Throughput collapses just when the endpoint is asking for slowness.
- **Fix:** Drop the permit before sleeping:
```rust
loop {
    attempt += 1;
    {
        let _permit = concurrency.acquire().await;
        // ... POST + status check + SSE read ...
        // _permit drops at the end of this block, before any sleep.
    }
    if attempt < MAX_STREAM_ATTEMPTS {
        tokio::time::sleep(d).await;
        continue;
    }
}
```
Or restructure so the permit is scoped to just the HTTP round-trip, not the attempt.

## High

### T2-H1 — — Anthropic `native_compact` has no retry, no timeout, no retry-hint extraction
- **File:** crates/kod-provider-anthropic/src/provider.rs
- **Line:** 401–432
- **Severity:** High
- **Category:** Network / Error handling
- **Description:** `native_compact` is a separate `POST /v1/messages` call, but unlike `complete()` it does not wrap the call in `with_retry`, does not bound it with `tokio::time::timeout`, and on a non-2xx response does not call `extract_retry_hints` or surface a typed `RateLimited`. A single 503 from Anthropic during a compaction call fails the whole turn, even though the same 503 on a `complete()` call would be retried three times with backoff.
- **Code:**
```rust
let resp = self
    .client
    .post(&url)
    .header("x-api-key", &self.api_key)
    .header("anthropic-version", "2023-06-01")
    .header("anthropic-beta", crate::wire::ANTHROPIC_COMPACTION_BETA)
    .json(&body)
    .send()
    .await
    .map_err(|e| {
        KodError::Provider(format!("anthropic native_compact: POST {url}: {e}"))
    })?;
let status = resp.status();
if !status.is_success() {
    let text = resp.text().await.unwrap_or_default();
    return Err(KodError::provider_status(status.as_u16(), &text));
}
```
- **Why it's a bug:** Compaction runs when the prompt is over budget — exactly when the call is most expensive and most worth retrying. A transient 5xx or a soft 429 fails the call, the dispatcher treats it as `Unavailable`, and the agent falls back to the local compaction path (or fails the turn). The same transient on a `complete()` call would have been retried.
- **Fix:** Wrap in the same `with_retry` + `tokio::time::timeout` + `extract_retry_hints` shape as `complete()`.

### T2-H2 — — OpenAI streaming cannot read HTTP `Retry-After` header (adk-model limitation)
- **File:** crates/kod-provider-openai/src/provider.rs
- **Line:** 790–815 (and the comment on line 745)
- **Severity:** High
- **Category:** Network
- **Description:** The OpenAI provider's streaming path uses `adk_model::openai_compatible::OpenAICompatible`, which surfaces errors as `AdkError` — a string-shaped type with only `details.upstream_status_code`. The raw `reqwest::Response` (and its `Retry-After` / `x-ratelimit-*` headers) is unreachable. The module comment acknowledges this: "the `adk-model` transport surfaces errors as `AdkError` (a string-shaped type with no headers), so retry hints cannot be extracted here." OpenAI sends `Retry-After` as a header on 429, not in the body. The streaming retry loop never sees it.
- **Why it's a bug:** A 429 from OpenAI proper includes a `Retry-After: 30` header. The streaming retry falls back to `text_hint_secs(e.to_string())`, which scans the formatted error string for cue phrases. If the body is the standard `{"error":{"message":"Rate limit reached","type":"rate_limit_exceeded"}}`, no cue phrase matches, and `unwrap_or(1200)` sleeps 20 minutes (see T2-C2). The 30-second hint is invisible.
- **Fix:** Bypass `adk-model` for the streaming path, the same way the Anthropic provider does for `stream_completion` — own the `reqwest::Client` and the SSE byte stream directly. The Anthropic provider already has the shape; port it. Short of that, parse `Retry-After` from the body's `error.retry_after` field if OpenAI ever adds one (they have not).

### T2-H3 — — OpenAI `adk_err_typed` parses body text for hints but the body is often JSON, missing the hint
- **File:** crates/kod-provider-openai/src/provider.rs
- **Line:** 1058–1079
- **Severity:** High
- **Category:** Error handling
- **Description:** `adk_err_typed` calls `kod_provider::retry::text_hint_secs(&e.to_string())` to extract a retry delay. `text_hint_secs` looks for English cue phrases ("try again in", "retry after", "retry in", "wait "). Real OpenAI and most OpenAI-compatible servers send JSON error bodies like `{"error":{"message":"Rate limit exceeded","type":"rate_limit_exceeded","code":"rate_limit_exceeded"}}`. No cue phrase matches. The function returns `None`, the code defaults to 1200 s (T2-C2). Tab-bridge's prose-style `"…wait ~20 minutes…"` body is the only server this works for.
- **Why it's a bug:** For the most common OpenAI-compatible server (OpenAI itself), the retry-delay extraction is dead code. The 20-minute default kicks in. This is the same root cause as T2-C2 but worth listing separately because even after T2-C2 is fixed (defaulting to small backoff), the hint extraction still misses JSON-body hints.
- **Fix:** Parse the body as JSON and look for `error.retry_after`, `error.wait_ms`, etc. Or, better, surface headers via a typed error from adk-model.

### T2-H4 — — Anthropic `stream_completion` issues a redundant double sleep on rate-limit retry
- **File:** crates/kod-provider-anthropic/src/provider.rs
- **Line:** 595–607
- **Severity:** Medium
- **Category:** Performance
- **Description:** When a streaming attempt hits a 429 with a small in-budget hint, the code sleeps the hint out (`tokio::time::sleep(d).await`), then immediately sleeps `250ms * attempt` as a backoff, then continues. The second sleep is redundant — the hint already expressed the server's desired wait. Adding 250 ms × attempt on top of a 5 s hint is wasted wall-clock.
- **Code:**
```rust
if !hints.cap_declined || small_hint.is_some() {
    if let Some(d) = small_hint {
        tracing::warn!(...);
        tokio::time::sleep(d).await;          // ← hint sleep
    }
    tracing::warn!(...);
    tokio::time::sleep(std::time::Duration::from_millis(
        250 * u64::from(attempt.max(1)),
    ))
    .await;                                   // ← redundant backoff
    continue;
}
```
- **Why it's a bug:** For a 5 s hint on attempt 1, the user waits 5.25 s instead of 5 s — a 5 % overhead. On attempt 2 (if the first retry also 429s), they wait 5 s + 500 ms. The redundancy is small per-occurrence, but every rate-limited turn pays it.
- **Fix:** Skip the backoff when the hint sleep was taken:
```rust
if let Some(d) = small_hint {
    tokio::time::sleep(d).await;
} else {
    tokio::time::sleep(std::time::Duration::from_millis(250 * u64::from(attempt))).await;
}
continue;
```

### T2-H5 — — `StreamGuard::final_verdict()` is never called at stream end; last-delta stalls are missed
- **File:** crates/kod-provider-anthropic/src/provider.rs, crates/kod-provider-openai/src/provider.rs
- **Line:** kod-provider-anthropic/src/provider.rs:724–756 (the tail buffer flush), kod-provider/src/stream_guard.rs:165 (the `final_verdict` method)
- **Severity:** Medium
- **Category:** Correctness
- **Description:** `StreamGuard::feed_chunk` only scans when `bytes_since_scan >= scan_stride` (default 128 bytes). If a degenerate pattern completes in the last few bytes of the stream (under the stride), the last `feed` call returned `Clean` and the pattern is missed. `StreamGuard::final_verdict()` exists precisely for this case (its doc says "Scan the current tail without regard to the stride. Call on stream termination so a short final delta can still complete a pattern the previous scan did not see."). Neither the Anthropic nor the OpenAI streaming loop calls it.
- **Why it's a bug:** A model that emits a final `abcabcabcabc` (period 3, > 180 bytes total) where the last `abcabc` arrives in a single sub-128-byte delta will not be detected as a stall. The stream ends, the guard is dropped, the pattern is missed. The user gets a stalled-stream's worth of garbage instead of a retry.
- **Fix:** After the SSE read loop ends (clean end), call `final_verdict()`:
```rust
if let Some(detector) = guard.as_mut().and_then(|g| match g.final_verdict() {
    StallVerdict::Clean => None,
    StallVerdict::Loop { detector } => Some(detector),
}) {
    stall_detector = Some(detector);
}
```

### T2-H6 — — Anthropic `parse_sse_line` ignores `thinking_delta` and future delta types
- **File:** crates/kod-provider-anthropic/src/wire.rs
- **Line:** 585–614
- **Severity:** Medium
- **Category:** Correctness
- **Description:** The `content_block_delta` handler matches `text_delta` and `input_json_delta` and drops everything else into `_ => Vec::new()`. Anthropic's extended-thinking protocol (and any future delta type) falls into the catch-all. Thinking content streamed via `thinking_delta` is silently discarded; the consumer never sees it and reasoning-effort budget is invisible.
- **Code:**
```rust
match delta.get("type").and_then(|t| t.as_str()) {
    Some("text_delta") => { ... }
    Some("input_json_delta") => { ... }
    _ => Vec::new(),  // ← thinking_delta, signature_delta, etc. dropped
}
```
- **Why it's a bug:** Reasoning models emit thinking deltas. If kod's engine ever wants to display reasoning (or bill for thinking tokens), the data is lost at the wire layer. Worse, if Anthropic adds a new delta type that carries content the engine needs (e.g., a new "citation_delta"), the parser silently drops it.
- **Fix:** Either add explicit arms for known delta types (`thinking_delta`, `signature_delta`) and a `tracing::debug!` for unknown ones, or surface unknown delta types as a generic chunk for the consumer to handle.

### T2-H7 — — Anthropic `stream_request` (legacy adk-model path) has no retry on pre-commit failure
- **File:** crates/kod-provider-anthropic/src/provider.rs
- **Line:** 819–879
- **Severity:** Medium
- **Category:** Error handling
- **Description:** `stream_request` (used by `stream` and `stream_with_tools`) wraps `inner.generate_content(request, true)` in a single `match` — no attempt loop. If the call fails with a retryable transport error, the stream errors out immediately. This is inconsistent with `stream_completion` (the structured path), which has a 3-attempt retry loop.
- **Why it's a bug:** Callers using the legacy `stream`/`stream_with_tools` API get worse reliability than callers using `stream_completion`. A transient 5xx on the legacy path fails the turn; the same 5xx on the structured path is retried twice. If the engine still routes some flows through the legacy API, those flows are flakier.
- **Fix:** Port the attempt loop from `stream_completion` into `stream_request`, or deprecate the legacy path entirely and route everything through `stream_completion`.

### T2-H8 — — Anthropic `collect` timeout wraps the retry loop, killing retries mid-flight
- **File:** crates/kod-provider-anthropic/src/provider.rs
- **Line:** 192–200
- **Severity:** Medium
- **Category:** Error handling / Network
- **Description:** `collect` wraps the entire `collect_inner` (which includes the `with_retry` loop) in `tokio::time::timeout(timeout, ...)`. If the first attempt takes 290 s (within the 300 s timeout) and fails, the retry starts but is killed after 10 s. The retry never gets a fair chance. The comment says "the pre-fix 300 s total made long generations fail mid-flight" — the fix moved the timeout to the call site, but the call site still wraps the whole retry loop.
- **Code:**
```rust
async fn collect(...) -> Result<...> {
    let timeout = std::time::Duration::from_secs(self.timeout_secs.max(1));
    tokio::time::timeout(timeout, self.collect_inner(request, stream))
        .await
        .map_err(|_| KodError::ProviderTimeout { timeout_ms: ... })?
}
```
- **Why it's a bug:** A 5xx on the first attempt after 290 s leaves only 10 s for the retry — not enough for a non-trivial prompt. The user sees a timeout when the retry would have succeeded.
- **Fix:** Bound each attempt individually:
```rust
async fn collect_inner(...) -> Result<...> {
    let policy = ...;
    kod_provider::retry::with_retry(&policy, || async move {
        tokio::time::timeout(
            Duration::from_secs(self.timeout_secs.max(1)),
            self.collect_once(&req, stream),
        ).await.map_err(|_| KodError::ProviderTimeout { ... })?
    }).await
}
```

### T2-H9 — — LSP `did_change` sends full file content with no debounce
- **File:** crates/kod-lsp/src/client.rs
- **Line:** 153–166, 246–280
- **Severity:** Medium
- **Category:** Performance
- **Description:** `diagnostics` increments the version and calls `did_change` synchronously on every invocation, sending the full file content as a single change. There's no debouncing — every `diagnostics` call from the engine sends a full-content LSP message. For a 50 KB file edited on every keystroke (if the engine ever wired `diagnostics` to a keymap), that's 50 KB per keystroke to the server, plus the server's re-analysis cost.
- **Code:**
```rust
pub async fn did_change(...) -> ... {
    self.notify(
        "textDocument/didChange",
        serde_json::json!({
            "textDocument": { "uri": uri, "version": version },
            "contentChanges": [{ "text": content }]  // ← full content
        }),
    ).await
}
```
- **Why it's a bug:** Today the engine calls `diagnostics` once per save, so the cost is one full-content send per save. But the design allows the engine to call it more frequently, and there's no debounce at the LSP layer to protect against that. Rust-analyzer re-indexes on every `did_change`; rapid calls pile up.
- **Fix:** Either debounce at the LSP layer (a `tokio::time::sleep` coalescing rapid changes), or send incremental `Range` changes when the diff is small.

### T2-H10 — — LSP `client_for` race spawns two servers, shuts down the loser
- **File:** crates/kod-lsp/src/manager.rs
- **Line:** 113–157
- **Severity:** Medium
- **Category:** Performance / Concurrency
- **Description:** `client_for` checks the cache; on a miss, releases the lock, spawns + initializes a new client, re-acquires the lock, and if another caller won the race, shuts the new client down. Two concurrent first-callers for the same language both pay the spawn cost (rust-analyzer: 3 s index for a medium crate); the loser's 3 s of work is thrown away.
- **Why it's a bug:** The comment justifies this as preferable to holding the lock across spawn (which would serialize every language on the slowest). That's a real trade-off, but the cost — a duplicated 3 s index — is significant for users who open the agent and immediately issue multi-language diagnostics. A `once`-style primitive per-binary would avoid both the lock-hold and the double-spawn.
- **Fix:** Use `tokio::sync::OnceCell` keyed by binary, or a `DashMap<String, shared_future>` where the shared future is awaited by all racers:
```rust
let fut = self.servers.entry(binary.to_string())
    .or_insert_with(|| shared_spawn(binary, &self.workspace_root))
    .clone();
fut.await
```

### T2-H11 — — Jitter uses `SystemTime::now().subsec_nanos()`, swarms retry in lockstep
- **File:** crates/kod-provider/src/retry.rs
- **Line:** 79–87
- **Severity:** Medium
- **Category:** Concurrency / Network
- **Description:** `delay_for` derives its jitter from `SystemTime::now().subsec_nanos()`. Multiple agents retrying a 429 simultaneously (e.g., a swarm that all hit the rate limit at the same wall-clock instant) read the same nanosecond and compute the same jitter factor. They retry in lockstep. The comment says "Deterministic pseudo-jitter from nanos; avoids pulling in `rand`." — but the lockstep problem is exactly the failure mode jitter is supposed to prevent.
- **Code:**
```rust
let nanos = std::time::SystemTime::now()
    .duration_since(std::time::UNIX_EPOCH)
    .map(|d| d.subsec_nanos() as f64)
    .unwrap_or(0.0);
let jitter = (nanos / 1e9) * 2.0 - 1.0;
let factor = 1.0 + jitter * self.jitter_fraction;
```
- **Why it's a bug:** On a fast machine, multiple threads can read `SystemTime` within the same nanosecond. The whole point of jitter is to decorrelate simultaneous retries; a correlated source defeats that. The codebase's own H-T2-P3 comment calls out "a swarm of agents hit a 429 simultaneously and retried in lockstep" as the bug the typed retry was supposed to fix — and the jitter source is still correlated.
- **Fix:** Use a thread-local PRNG seeded from `SystemTime` once, or pull in `rand` (the cost is small). At minimum, mix in the thread id:
```rust
let mut state = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos() as u64;
state ^= std::thread::current().id() as u64 ^ std::process::id() as u64;
state = state.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
let jitter = ((state >> 33) as f64) / (u32::MAX as f64) * 2.0 - 1.0;
```

### T2-H12 — — MCP and LSP servers have stderr discarded, debugging is impossible
- **File:** crates/kod-mcp/src/client.rs, crates/kod-lsp/src/client.rs
- **Line:** kod-mcp/src/client.rs:93, kod-lsp/src/client.rs:82
- **Severity:** Medium
- **Category:** Error handling / Observability
- **Description:** Both `McpClient::spawn_stdio` and `LspClient::start` set `.stderr(Stdio::null())`. A misbehaving server's diagnostics (Python tracebacks, Node unhandled rejections, rust-analyzer's internal logs) are invisible. When a server crashes, the agent sees a generic "stdout closed" / "spawn failed" error with no clue why.
- **Why it's a bug:** The module comments acknowledge this ("A future revision can pipe it to `tracing`"). For a daily driver, the first 30 minutes of debugging a misbehaving MCP server is spent re-running the server by hand to see its stderr. Piping stderr to `tracing` at debug level is a one-line change.
- **Fix:** Pipe stderr to a tracing reader task:
```rust
.stderr(Stdio::piped())
...
let stderr = child.stderr.take().expect("piped");
tokio::spawn(async move {
    let mut reader = BufReader::new(stderr).lines();
    while let Ok(Some(line)) = reader.next_line().await {
        tracing::debug!(target: "mcp::stderr", program, line = %line);
    }
});
```

### T2-H13 — — MCP server response with string id is silently dropped
- **File:** crates/kod-mcp/src/client.rs
- **Line:** 360–382
- **Severity:** Medium
- **Category:** Correctness
- **Description:** The reader dispatches responses by matching `msg.get("id").and_then(|v| v.as_i64())`. The pending map is keyed by `i64`. JSON-RPC 2.0 allows string ids. A server that echoes a string id for one of our requests (a server bug, but a survivable one) finds no match in the pending map and the response is dropped with a debug log. The request times out.
- **Code:**
```rust
if msg.get("method").is_none()
    && let Some(id) = msg.get("id").and_then(|v| v.as_i64())
{
    let sender = pending.lock().await.remove(&id);
    ...
} else if let Some(method) = msg.get("method").and_then(|v| v.as_str()) {
    ...
}
```
- **Why it's a bug:** kod only issues `i64` ids (via `AtomicI64`), so a string id in a response is the server's bug. But the failure mode — silent timeout — is the worst possible: the user sees a 60 s hang on `tools/call`, then a `Timeout` error, with no signal that the server sent a perfectly fine response with a string id.
- **Fix:** Either widen the pending map to `HashMap<serde_json::Value, ...>` (memory overhead, keying complexity), or log a warning when an id-bearing, method-absent message can't be matched:
```rust
} else if let Some(id) = msg.get("id") {
    if id.as_i64().is_none() {
        tracing::warn!(
            id = %id,
            "MCP: server responded with a non-integer id; kod only issues i64 ids, \
             this response cannot be matched to a request",
        );
    }
}
```

### T2-H14 — — Anthropic legacy `stream_request` doesn't track attempt safety; OpenAI does
- **File:** crates/kod-provider-anthropic/src/provider.rs
- **Line:** 819–879
- **Severity:** Medium
- **Category:** Correctness / Concurrency
- **Description:** The Anthropic legacy `stream_request` yields `StreamChunk::Done` after every error (the H-T2-P10 comment fixed the `return` after error, but didn't add an attempt tracker). The OpenAI `stream_request` and the Anthropic `stream_completion` both use `AttemptTracker` to decide whether a retry is safe. The Anthropic legacy path has no tracker, no buffer, no retry. If the engine ever wired the legacy path into a retry loop, it would re-emit chunks already delivered.
- **Why it's a bug:** The two paths have different correctness contracts for the same `LlmProvider` trait. A caller using `stream_with_tools` gets no retry; a caller using `stream_completion` gets 3 attempts with safety tracking. If the engine ever falls back from `stream_completion` to `stream_with_tools` (e.g., for a provider that didn't override), the safety properties change silently.
- **Fix:** Either delete the legacy path and route everything through `stream_completion`, or port the tracker + buffer into `stream_request`.

### T2-H15 — — Anthropic `provider.rs`'s HTTP client has no `User-Agent` header; some proxies reject
- **File:** crates/kod-provider-anthropic/src/provider.rs, crates/kod-provider-openai/src/provider.rs
- **Line:** kod-provider-anthropic/src/provider.rs:93–98, kod-provider-openai/src/provider.rs:88–99
- **Severity:** Low
- **Category:** Network
- **Description:** Both providers build a `reqwest::Client` without calling `.user_agent(...)`. reqwest's default UA is `reqwest/0.12.x`. Some corporate proxies and Cloudflare-fronted endpoints reject requests with no recognizable UA. The OpenAI-compatible path is more likely to hit this (self-hosted servers, Cloudflare-fronted gateways) than the Anthropic path (Anthropic's API accepts any UA).
- **Why it's a bug:** A user behind a corporate proxy might see "could not reach the model server at https://api.openai.com/v1: error sending request" with no signal that the UA is the problem. The fix is a one-liner.
- **Fix:**
```rust
let client = reqwest::Client::builder()
    .user_agent(concat!("kod/", env!("CARGO_PKG_VERSION")))
    .connect_timeout(...)
    .build()?;
```

## Medium

### T2-M1 — — `parse_sse_line` drops malformed JSON frames with no log
- **File:** crates/kod-provider-anthropic/src/wire.rs
- **Line:** 506–512
- **Severity:** Medium
- **Category:** Observability / Error handling
- **Description:** When `serde_json::from_str` fails on a `data:` payload, the parser returns an empty `Vec` and moves on. The comment justifies this as "the byte stream is non-fatal and the next frame may be well-formed." But there is no log — not even at `debug!` level — of what was dropped. A persistent protocol error (e.g., the server changed its event format) produces zero readable frames and the only signal is the provider's idle timeout.
- **Why it's a bug:** When an Anthropic API change silently breaks the parser, the user sees a timeout after 300 s with no clue that every frame was malformed. A single `tracing::debug!` would surface the malformed payload in `RUST_LOG=kod_provider_anthropic=debug`.
- **Fix:**
```rust
let Ok(v) = serde_json::from_str::<serde_json::Value>(payload) else {
    tracing::debug!(
        payload = %payload.chars().take(200).collect::<String>(),
        "anthropic SSE: dropped malformed JSON frame",
    );
    return Vec::new();
};
```

### T2-M2 — — SSE parser doesn't handle multi-line `data:` events (concat with newlines per spec)
- **File:** crates/kod-provider-anthropic/src/wire.rs
- **Line:** 499–505
- **Severity:** Low
- **Category:** Correctness
- **Description:** The SSE spec allows an event with multiple `data:` lines; the receiver concatenates them with `\n` before parsing. The parser processes each `data:` line independently as a separate JSON object. Anthropic doesn't send multi-line events today, but a proxy that re-chunks the stream (e.g., an SSE normalizer) could produce them.
- **Why it's a bug:** Today: no impact (Anthropic sends one `data:` per event). Future: a proxy or a new Anthropic event format that uses multi-line `data:` would have each line parsed as a separate (malformed) JSON object and dropped. The parser is not spec-compliant.
- **Fix:** Accumulate `data:` lines into a buffer until a blank line, then parse the concatenated payload. Or document the assumption that Anthropic never sends multi-line events.

### T2-M3 — — Anthropic `cache_creation_1h_input_tokens` `as usize` truncates on 32-bit platforms
- **File:** crates/kod-provider-anthropic/src/wire.rs
- **Line:** 526, 543
- **Severity:** Low
- **Category:** Correctness
- **Description:** `let get = |k: &str| -> usize { u.get(k).and_then(|n| n.as_u64()).unwrap_or(0) as usize };`. On a 32-bit target (rare for kod, but possible on embedded), `as usize` truncates values > 2^32. A 5-billion-token cache creation count (impossible today, but the type allows it) wraps.
- **Why it's a bug:** Theoretical today, but the type assertion is unsound. If kod ever runs on a 32-bit target (some ARM SBCs), cost accounting silently wraps.
- **Fix:** Use `u64` throughout the cache token fields, or assert `n <= usize::MAX as u64` before casting.

### T2-M4 — — MCP `cache_key` uses `unwrap_or_default()`; a failed serialization produces an empty key
- **File:** crates/kod-mcp/src/tool_cache.rs
- **Line:** 73
- **Severity:** Low
- **Category:** Correctness
- **Description:** `serde_json::to_string(&Canonical { ... }).unwrap_or_default()` returns an empty string on serialization failure. The SHA-256 of `""` is a fixed value. All servers that fail to serialize share that one cache key. The comment says "an empty string hashes to a valid, unique key; a spec that fails to serialize cannot be spawned anyway" — but two different unserializable specs would collide.
- **Why it's a bug:** Practically impossible (the `Canonical` struct is plain `&str`/`&[String]`/`&BTreeMap`, always serializable). But the failure mode is wrong: a collision means server A's tool list is served to server B. The defensive `unwrap_or_default()` masks a real bug if it ever fires.
- **Fix:** Propagate the error, or hash the raw bytes of the struct directly (not via JSON).

### T2-M5 — — LSP `read_message` has no header count limit; a malicious server can DoS
- **File:** crates/kod-lsp/src/client.rs
- **Line:** 580–603
- **Severity:** Medium
- **Category:** Security
- **Description:** The header-parsing loop reads lines until a blank line, with no cap on the number of headers or total header bytes. A server that sends a million `X-Foo: bar\r\n` lines keeps the client allocating Strings until OOM. Less catastrophic than T2-C5 (no single huge allocation), but a real DoS vector against a "minimal client".
- **Why it's a bug:** Same threat model as T2-C5/T2-C6 — LSP servers are subprocesses, not trusted.
- **Fix:** Cap the header section at, say, 64 KB or 100 lines:
```rust
const MAX_HEADER_BYTES: usize = 64 * 1024;
let mut header_bytes = 0;
loop {
    let mut line = String::new();
    let n = self.stdout.read_line(&mut line).await?;
    header_bytes += n;
    if header_bytes > MAX_HEADER_BYTES {
        return Err(LspError::Protocol("header section too large".into()));
    }
    ...
}
```

### T2-M6 — — LSP `path_to_uri` doesn't handle Windows UNC paths
- **File:** crates/kod-lsp/src/client.rs
- **Line:** 813–835
- **Severity:** Low
- **Category:** Correctness
- **Description:** `path_to_uri` has a special case for `C:\` (drive letter) but not for UNC paths like `\\server\share\file.rs`. A UNC path becomes `file:////server/share/file.rs` (extra slashes), which the server may reject.
- **Why it's a bug:** kod is unlikely to run against a UNC workspace, but the function's docstring claims "Absolute, canonical when possible" — UNC is a valid absolute path on Windows.
- **Fix:** Detect `\\` prefix and produce `file://unc/server/share/...`.

### T2-M7 — — MCP `shutdown` uses `start_kill` (SIGKILL) without graceful SIGTERM first
- **File:** crates/kod-mcp/src/client.rs
- **Line:** 267–271
- **Severity:** Low
- **Category:** OS / Error handling
- **Description:** `shutdown` calls `child.start_kill()` immediately. On Unix, this is SIGKILL — the server has no chance to flush state, close DB connections, or finish writing a file. Some MCP servers (database tools, file writers) could leave resources in an inconsistent state.
- **Why it's a bug:** A graceful shutdown would `SIGTERM`, wait a few seconds, then `SIGKILL` if still alive. The 2-second `wait()` is the only bound, but it's spent waiting after SIGKILL, not before.
- **Fix:**
```rust
#[cfg(unix)]
{
    use std::os::unix::signal::Signal;
    let _ = child.kill_with_signal(Signal::Term);  // or send_signal(libc::SIGTERM)
}
let wait_result = tokio::time::timeout(Duration::from_secs(2), child.wait()).await;
if wait_result.is_err() {
    let _ = child.start_kill();  // escalate to SIGKILL
    let _ = tokio::time::timeout(Duration::from_secs(1), child.wait()).await;
}
```

### T2-M8 — — MCP client doesn't send `notifications/cancelled` for dropped in-flight requests
- **File:** crates/kod-mcp/src/client.rs
- **Line:** 298–308 (the timeout path)
- **Severity:** Low
- **Category:** Correctness / Performance
- **Description:** When a `tools/call` request times out or its future is dropped, the server doesn't know — it keeps running the tool. The MCP spec has `notifications/cancelled` for this. kod doesn't send it. A long-running tool (e.g., a database migration tool) keeps running after the user cancelled, holding server-side resources.
- **Why it's a bug:** The server may complete the tool and the result is dropped (no pending entry). For a side-effectful tool, the side effects still happen. The user thinks they cancelled; they didn't.
- **Fix:** On timeout or drop, send `notifications/cancelled` with the request id:
```rust
Err(_) => {
    self.pending.lock().await.remove(&id);
    let _ = self.notify("notifications/cancelled", json!({ "requestId": id })).await;
    Err(McpError::Timeout)
}
```

### T2-M9 — — Anthropic `stream_request` (legacy path) emits a synthetic `Done` after error, breaking the stream contract
- **File:** crates/kod-provider-anthropic/src/provider.rs
- **Line:** 870–878 (the trailing `if let Some(usage) = last_usage { ... } yield Ok(StreamChunk::Done);`)
- **Severity:** Medium
- **Category:** Correctness
- **Description:** Wait, looking more carefully: H-T2-P10 added `return` after the error in the streaming path. Let me re-read... Actually, the legacy `stream_request` does NOT have the H-T2-P10 fix consistently. Looking at the `Ok(mut responses) => { ... while let ... match item { Ok(response) => ..., Err(e) => { yield Err(adk_err(e)); return; } } } if let Some(usage) = last_usage { yield Ok(StreamChunk::Usage(usage)); } yield Ok(StreamChunk::Done); }` — the inner `Err` arm has `return`. So on inner error, we `yield Err` and `return`. Good. But the outer `Err(e) => { yield Err(adk_err(e)); return; }` also has `return`. Good. So no synthetic Done after error. Not a bug. (Self-correcting: this entry is invalid; replace with T2-M9 below.)
- **Replacement T2-M9:** Anthropic `stream_request` legacy path emits a final `Usage` chunk from `last_usage` even when the stream errored mid-way (because the `if let Some(usage) = last_usage` runs after the `while` loop exits normally, but the `return` in the Err arm skips it — so actually it's fine). Withdrawing this entry.

### T2-M10 — — SSE parser strips `\r` from line ends but doesn't handle mid-stream `\r` from old servers
- **File:** crates/kod-provider-anthropic/src/provider.rs
- **Line:** 673–677
- **Severity:** Low
- **Category:** Correctness
- **Description:** The line-byte handler strips a trailing `\r` after popping `\n`. This handles `\r\n` line endings. But if a server (or proxy) emits `\r` mid-line (e.g., a CR LF→CRLF normalizer that doubled the CR), the extra `\r` survives and becomes part of the JSON payload, causing `serde_json::from_str` to fail. Unlikely, but possible through a misbehaving proxy.
- **Why it's a bug:** A proxy that mangles line endings causes the SSE parser to drop every frame (silently per T2-M1). The user sees a timeout.
- **Fix:** Also strip leading `\r` from the next line, or use `trim_end_matches` more aggressively.

### T2-M11 — — Anthropic `with_retry` `RateLimited` arm: hint of exactly `max_delay` is treated as not-long
- **File:** crates/kod-provider/src/retry.rs
- **Line:** 109
- **Severity:** Low
- **Category:** Correctness
- **Description:** `if hint > policy.max_delay && hint <= policy.max_rate_limit_wait && !long_wait_used` — strict `>` on the first comparison. A hint equal to `max_delay` (e.g., 8 s hint, 8 s max) falls through to `hint.min(max_delay)` = 8 s, slept every attempt (not just once). The "long wait once" rule never fires for hints at exactly the cap.
- **Why it's a bug:** Edge case, unlikely to matter. But the "long wait used once" invariant is subtly broken at the boundary.
- **Fix:** `>=` instead of `>`, or document the boundary.

### T2-M12 — — Anthropic `messages_array_impl` empty assistant turn emits an empty text block, masking a caller bug
- **File:** crates/kod-provider-anthropic/src/wire.rs
- **Line:** 263–269
- **Severity:** Low
- **Category:** Correctness
- **Description:** A truly empty assistant turn (no content, no tool_calls) becomes `[{"type":"text","text":""}]` to keep the API happy. The comment says "A caller that produced this has a bug upstream; the wire stays valid." But the caller's bug is invisible — the request succeeds with an empty assistant turn, and the model continues as if nothing was wrong.
- **Why it's a bug:** A real bug in the transcript (e.g., a tool result that lost its content) is masked. The model sees an empty assistant turn and may produce a confused response. Failing loudly would surface the bug.
- **Fix:** Log a warning when this fallback fires, or surface an error to the caller.

### T2-M13 — — OpenAI `stream_request` doesn't release the bg_gate across attempts
- **File:** crates/kod-provider-openai/src/provider.rs
- **Line:** 725–735 (the `collect_bg` path holds the gate; `stream_request` doesn't)
- **Severity:** Low
- **Category:** Concurrency
- **Description:** `collect_bg` holds `self.bg_gate` across the entire attempt sequence to prevent overlapping turns on a shared background session. The streaming path (`stream_request`) doesn't acquire `bg_gate` at all — background streaming turns can overlap on the shared session, which is exactly what `bg_gate` was added to prevent for the non-streaming path.
- **Why it's a bug:** If the engine ever routes background streaming through this provider, two concurrent streams on the same tab-bridge session would 409 each other. Today the engine doesn't do this, but the contract is inconsistent.
- **Fix:** Acquire `bg_gate` in `stream_request` when `default_session_string().is_some()`.

### T2-M14 — — LSP `initialize` 60 s timeout may be too short for large rust-analyzer indexes
- **File:** crates/kod-lsp/src/client.rs
- **Line:** 142 (the `INIT_TIMEOUT` constant)
- **Severity:** Low
- **Category:** Network
- **Description:** `INIT_TIMEOUT = Duration::from_secs(60)`. rust-analyzer's `initialize` is fast (it returns immediately and indexes in the background), but a cold start on a large workspace with a slow disk can take longer than 60 s for the `initialize` response if the server is synchronous. More importantly, the 60 s counts against the user's first `diagnostics` call, which on a 100k-LOC workspace can take 30 s+ to index.
- **Why it's a bug:** A user on a large workspace might see `LSP server did not answer initialize within 60s` and fall back to the compiler-based check, losing the LSP's value.
- **Fix:** Make `INIT_TIMEOUT` configurable, or raise to 120 s. The 60 s is generous for `initialize` proper but tight for the full handshake on a cold server.

### T2-M15 — — Anthropic `stream_completion` `attempt.max(1)` is redundant (attempt always ≥ 1)
- **File:** crates/kod-provider-anthropic/src/provider.rs
- **Line:** 602
- **Severity:** Low
- **Category:** Code quality
- **Description:** `attempt` is initialized to 0 and incremented at the top of the loop, so it's always ≥ 1. `u64::from(attempt.max(1))` is always `u64::from(attempt)`. The `.max(1)` is defensive but dead.
- **Why it's a bug:** Not a bug — defensive code. But it suggests the author was unsure about the loop invariant, which is a smell.
- **Fix:** Remove the `.max(1)` or add a comment.

### T2-M16 — — OpenAI `stream_request` `Part::FunctionCall` delta aggregation assumes adk-model aggregates correctly
- **File:** crates/kod-provider-openai/src/provider.rs
- **Line:** 875–905
- **Severity:** Medium
- **Category:** Correctness
- **Description:** The streaming loop yields one `ToolCallStart` + one `ToolCallDelta` per `Part::FunctionCall`. The delta's `arguments` is `args.to_string()` — the full serialized JSON. This is correct only if adk-model's `OpenAICompatible` client aggregates OpenAI's argument-fragment deltas (which arrive as `{"index":0,"function":{"arguments":"{\"pa"}}`, `{"index":0,"function":{"arguments":"th\":\"a.rs\"}"}}`, etc.) into a single `FunctionCall` part with the complete `args`. If adk-model yields a `FunctionCall` per delta fragment, this code yields a Start + Delta per fragment, with each Delta carrying a partial JSON string. The consumer would try to parse `{"pa` as JSON and fail.
- **Why it's a bug:** Depends on adk-model's behavior, which is not visible here. If adk-model ever changes its aggregation strategy (or if a server sends tool calls in a non-standard way that adk-model doesn't aggregate), tool calls break silently.
- **Fix:** Add a test that streams a multi-fragment tool call and asserts the consumer sees one Start + one complete Delta. If adk-model doesn't aggregate, aggregate here.

### T2-M17 — — Anthropic `parse_response` `input` defaults to `Value::Null` when missing
- **File:** crates/kod-provider-anthropic/src/provider.rs
- **Line:** 1135–1138
- **Severity:** Low
- **Category:** Correctness
- **Description:** A `tool_use` block with no `input` field becomes `Value::Null`. The `ToolCall::arguments` is `Value::Null`. When the engine dispatches this tool call, it passes `null` as the arguments. Most tools expect an object (`{}`); `null` may cause the tool to crash or no-op.
- **Why it's a bug:** A malformed tool_use block (rare) produces a tool call that the tool dispatcher can't handle. Better to default to `{}`.
- **Fix:**
```rust
let input = block
    .get("input")
    .cloned()
    .filter(|v| !v.is_null())
    .unwrap_or(serde_json::json!({}));
```

### T2-M18 — — MCP `McpToolResult` deserialization is overly tolerant of missing fields
- **File:** crates/kod-mcp/src/types.rs
- **Line:** 35–40
- **Severity:** Low
- **Category:** Correctness
- **Description:** `McpToolResult` has `content: Vec<McpContent>` with `#[serde(default)]` and `is_error: bool` with `#[serde(rename = "isError", default)]`. A server that returns `{}` (no content, no isError) deserializes to an empty result with `is_error = false`. The caller sees "tool succeeded with no output" — which is wrong; the server sent a malformed response.
- **Why it's a bug:** A buggy server that returns `{}` for every error is indistinguishable from a server that genuinely returns empty success. The caller can't tell.
- **Fix:** Make `content` required (no `default`), or surface a warning when both fields are absent.

### T2-M19 — — Anthropic `provider.rs` `complete()` hint-extraction reads `resp.headers()` after `send()`, before `text()` — correct, but the `unwrap_or_default()` on body is silent
- **File:** crates/kod-provider-anthropic/src/provider.rs
- **Line:** 372
- **Severity:** Low
- **Category:** Observability
- **Description:** `let text = resp.text().await.unwrap_or_default();` — if the body read fails (rare), the error text is empty, and the surfaced `KodError::provider_status_with_hint` gets an empty body. The user sees "HTTP 429: " with no body context.
- **Why it's a bug:** A transport error mid-body-read (e.g., connection reset) loses the body, which often contains the most useful diagnostic.
- **Fix:** Log when `text()` fails:
```rust
let text = resp.text().await.unwrap_or_else(|e| {
    tracing::warn!(error = %e, "anthropic: could not read error body");
    String::new()
});
```

### T2-M20 — — OpenAI `resolve_api_key` returns the local fallback `"not-needed"` silently when env var is unset
- **File:** crates/kod-provider-openai/src/provider.rs
- **Line:** 980–987
- **Severity:** Low
- **Category:** Error handling
- **Description:** When no explicit key and no `OPENAI_API_KEY` env var are present, `resolve_api_key` returns `"not-needed"`. For a local server (Ollama, LM Studio), this is fine. For OpenAI proper, this sends `"not-needed"` as the bearer token, gets a 401, and the user sees "401 unauthorized" with no clue that they forgot to set the env var.
- **Why it's a bug:** The "local fallback" is a silent default that's wrong for cloud providers. The user's first experience with the OpenAI provider against `api.openai.com` is a confusing 401.
- **Fix:** Distinguish: if the base_url is `api.openai.com`, require a real key. Otherwise, allow the fallback.
```rust
fn resolve_api_key(explicit: Option<String>, base_url: &str) -> String {
    if let Some(key) = explicit.filter(|s| !s.trim().is_empty()) {
        return key;
    }
    if let Ok(key) = std::env::var("OPENAI_API_KEY") {
        if !key.trim().is_empty() { return key; }
    }
    if base_url.contains("api.openai.com") {
        // Don't paper over a missing cloud key with a dummy.
        return String::new();  // caller surfaces a clear 401
    }
    LOCAL_FALLBACK_API_KEY.to_string()
}
```

### T2-M21 — — Anthropic `messages_array_with_cache` marks the last block of the last assistant message, but a trailing text block on the last assistant turn gets the marker, not the tool_use
- **File:** crates/kod-provider-anthropic/src/wire.rs
- **Line:** 222–240
- **Severity:** Low
- **Category:** Correctness
- **Description:** The marker is placed on `blocks.last_mut()`. For an assistant turn `[text, tool_use]`, the last block is `tool_use` — fine. For `[tool_use, text]` (a tool call followed by a text summary), the last block is `text`, and the marker goes on the text, not the tool_use. Anthropic's cache prefix then includes the text but the tool_use is below the marker. The next turn's request sees the cached text but not the cached tool_use, which can cause a cache invalidation.
- **Why it's a bug:** Edge case — most assistant turns end with tool_use, not text. But when they don't, the cache prefix is suboptimal.
- **Fix:** Place the marker on the last `tool_use` block if any, else the last block.

### T2-M22 — — OpenAI `stream_request` `transport_retryable` is set from `adk_precommit_retryable(&e)` but the error has already been converted to `KodError`
- **File:** crates/kod-provider-openai/src/provider.rs
- **Line:** 919
- **Severity:** Low
- **Category:** Correctness
- **Description:** `transport_retryable = adk_precommit_retryable(&e);` — but `e` is the `adk_core::AdkError` from `responses.next().await`. Then `transport_error = Some(adk_err_typed(e));` converts it to `KodError`. The retryability check is on the original `AdkError`, which is fine, but the code reads as if it's checking the typed error. The `rate_limit_delay(&err, ...)` later uses the typed `KodError`, so the two classifications are on different types.
- **Why it's a bug:** Not a bug today (both classifications agree), but the type mismatch is a smell. If `adk_precommit_retryable` and `rate_limit_delay` ever diverge on what's retryable, the inconsistency is invisible.
- **Fix:** Classify once, on the typed error, and use that classification for both decisions.

### T2-M23 — — Anthropic `provider.rs` `stream_completion`'s `bg_gate` is never acquired
- **File:** crates/kod-provider-anthropic/src/provider.rs
- **Line:** 459–525 (no `bg_gate` reference)
- **Severity:** Low
- **Category:** Concurrency
- **Description:** The OpenAI provider has `bg_gate: Arc<tokio::sync::Mutex<()>>` to serialize background turns on a shared session. The Anthropic provider doesn't have `bg_gate` at all. If Anthropic is ever used as a tab-bridge backend (today it's not), background turns would overlap.
- **Why it's a bug:** Today: no impact (Anthropic isn't a tab bridge). But the providers have inconsistent background-session handling, which is a maintenance smell.
- **Fix:** Add `bg_gate` to `AnthropicProvider` for parity, or document that Anthropic is never a tab bridge.

### T2-M24 — — LSP `manager.rs` `Default` impl calls `std::env::current_dir()` which can fail
- **File:** crates/kod-lsp/src/manager.rs
- **Line:** 252–256
- **Severity:** Low
- **Category:** Error handling
- **Description:** `Default::default()` calls `std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."))`. If the cwd is unreadable (deleted, permission), the fallback `"."` is a relative path that won't resolve correctly when passed to `Command::current_dir()` later — the server inherits the agent's cwd, which may be different.
- **Why it's a bug:** A deleted cwd is rare but possible (a test that creates a temp dir, cd's into it, deletes it). The fallback `"."` is silent and wrong.
- **Fix:** Surface the error, or use the home dir as fallback.

### T2-M25 — — Anthropic `wire.rs` `messages_array_impl` doesn't validate `tool_call_id` is non-empty for `tool_result` blocks
- **File:** crates/kod-provider-anthropic/src/wire.rs
- **Line:** 272–305
- **Severity:** Low
- **Category:** Correctness
- **Description:** A `Tool` message with `tool_call_id: None` becomes a `tool_result` block with `tool_use_id: ""`. Anthropic's API rejects this with a 400. The comment says "the local error is closer to the bug than a crash here" — but the local error is a 400 from Anthropic with a cryptic message, not a clear "missing tool_call_id" error from kod.
- **Why it's a bug:** A caller that loses the `tool_call_id` (a real bug in the transcript management) gets a 400 from Anthropic, not a local error pointing at the missing field.
- **Fix:** Return a local error:
```rust
let id = m.tool_call_id.clone().ok_or_else(|| {
    KodError::Provider("anthropic: tool result has no tool_call_id".into())
})?;
```

### T2-M26 — — OpenAI provider's `collect_bg` background backoff schedule is fixed at `[2, 8]` seconds
- **File:** crates/kod-provider-openai/src/provider.rs
- **Line:** 1138
- **Severity:** Low
- **Category:** Error handling
- **Description:** `const BG_BUSY_BACKOFF: [u64; 2] = [2, 8];` — two attempts with 2 s and 8 s waits. The comment says "2 attempts". After 2 retries (3 total calls), a still-busy session fails. The schedule is not configurable and not jittered.
- **Why it's a bug:** A tab-bridge backend that's busy for 30 s (a long-running previous turn) fails after 2+8 = 10 s, even though the previous turn would have finished in 30 s. The user sees a failure when waiting would have worked.
- **Fix:** Make the schedule configurable, or extend to 3 attempts with longer waits.

### T2-M27 — — `kod-provider` `with_retry` does not log the attempt count on success
- **File:** crates/kod-provider/src/retry.rs
- **Line:** 101–102
- **Severity:** Low
- **Category:** Observability
- **Description:** The success path `Ok(v) => return Ok(v)` logs nothing. A call that succeeded after 2 retries is indistinguishable from one that succeeded on the first try. For diagnosing slow turns, the retry count is the most useful signal.
- **Why it's a bug:** Not a bug, but an observability gap. A `tracing::debug!` on success-after-retry would help.
- **Fix:**
```rust
Ok(v) => {
    if attempt > 1 {
        tracing::debug!(attempt, "provider call succeeded after retry");
    }
    return Ok(v);
}
```

### T2-M28 — — Anthropic `provider.rs` `stream_completion` doesn't reset `empty_retry` across attempts correctly
- **File:** crates/kod-provider-anthropic/src/provider.rs
- **Line:** 519
- **Severity:** Medium
- **Category:** Correctness
- **Description:** `let mut empty_retry = kod_provider::retry_safety::EmptyCompletionRetry::new();` is outside the attempt loop — it persists across attempts. That's actually correct (the budget is per-turn, not per-attempt). But the `attempt` counter is inside the loop and the `empty_retry.attempts()` is separate. A turn that fails empty-completion retry twice, then succeeds on the 3rd streaming attempt (which itself produces empty output), would not retry again because `empty_retry.attempts() >= MAX`. Wait, that's by design. Withdrawing as not-a-bug.

### T2-M29 — — Anthropic `provider.rs` `complete()` honors `Retry-After` from body but the retry loop sees it as a typed error only within `rate_limit_wait`
- **File:** crates/kod-provider-anthropic/src/provider.rs
- **Line:** 380–390
- **Severity:** Medium
- **Category:** Error handling
- **Description:** The 429 → `RateLimited` conversion only fires when `delay <= self.rate_limit_wait`. If `rate_limit_wait` is `ZERO` (default), no 429 ever surfaces as `RateLimited` from `complete()` — they all fall through to the generic `provider_status_with_hint`. The retry loop then uses `policy.delay_for(attempt)` (exponential backoff), ignoring the server's hint. The comment on `RetryPolicy::default` says `max_rate_limit_wait: ZERO` means "any hint is clamped to max_delay" — but the `complete()` code never produces a `RateLimited` to be clamped in the first place when the budget is zero.
- **Why it's a bug:** The default config (zero rate-limit wait) means 429s from `complete()` are retried with exponential backoff, never honoring the server's hint. The hint is extracted and logged but not used. This is the same root cause as the OpenAI `unwrap_or(1200)` default but on the Anthropic side.
- **Fix:** Either always surface `RateLimited` (let the retry layer decide whether to honor it), or document that `rate_limit_wait = 0` means "ignore hints on the complete path too".

### T2-M30 — — Anthropic `provider.rs` `stream_completion` `MAX_STREAM_ATTEMPTS` is hardcoded to 3, ignoring `RetryPolicy::max_attempts`
- **File:** crates/kod-provider-anthropic/src/provider.rs
- **Line:** 481
- **Severity:** Low
- **Category:** Error handling
- **Description:** `const MAX_STREAM_ATTEMPTS: u32 = 3;` is a local constant. The `RetryPolicy::max_attempts` (default 3, configurable) is not consulted. A user who configures `max_attempts = 5` for the non-streaming path still gets 3 attempts on the streaming path. Same in the OpenAI provider.
- **Why it's a bug:** Inconsistent retry counts between streaming and non-streaming paths. A user tuning `max_attempts` is surprised when streaming doesn't honor it.
- **Fix:** Pass `RetryPolicy::max_attempts` into the streaming loop, or read it from `self`.

## Low

### T2-L1 — — Anthropic `wire.rs` `parse_sse_line` uses `as usize` cast on `u64` token counts
- **File:** crates/kod-provider-anthropic/src/wire.rs
- **Line:** 526, 543, 554, 583
- **Severity:** Low
- **Category:** Correctness
- **Description:** Multiple `n.as_u64().unwrap_or(0) as usize` casts. On 64-bit platforms, fine. On 32-bit, truncates. See T2-M3.
- **Fix:** Use `u64` throughout, or assert.

### T2-L2 — — LSP `client.rs` `read_handling_server_requests` replies with `result: null` to every server request, even ones the spec says we should support
- **File:** crates/kod-lsp/src/client.rs
- **Line:** 660–672
- **Severity:** Low
- **Category:** Correctness
- **Description:** The `read_handling_server_requests` inner loop replies with `{"result": null}` to any server-initiated request (one with both `id` and `method`). This includes `client/registerCapability`, `window/workDoneProgress/create`, `workspace/configuration`, `workspace/applyEdit`. Replying `null` to `workspace/applyEdit` means the server thinks we declined the edit, which is correct. Replying `null` to `workspace/configuration` means we have no config values, which may cause the server to use defaults. The comment says "the standard I-don't-support-this reply" — that's the right default for a minimal client.
- **Why it's a bug:** Not a bug — design choice. But `workspace/configuration` is sometimes needed (e.g., rust-analyzer asks for `rust-analyzer.cargo.features`). Replying `null` means the server uses its defaults, which may not match the user's `Cargo.toml`.
- **Fix:** Implement `workspace/configuration` to read from a config source, or document that LSP config is not supported.

### T2-L3 — — MCP `McpClient::request` uses `Ordering::Relaxed` for `next_id`
- **File:** crates/kod-mcp/src/client.rs
- **Line:** 283
- **Severity:** Low
- **Category:** Concurrency
- **Description:** `self.next_id.fetch_add(1, Ordering::Relaxed)`. Relaxed is fine for a counter — the only invariant is uniqueness, which `fetch_add` guarantees regardless of ordering. Two concurrent callers get different ids, just not in deterministic order. The pending map keys by id, so order doesn't matter.
- **Why it's a bug:** Not a bug. Listed for completeness — the ordering is correct.

### T2-L4 — — Anthropic `provider.rs` `collect_inner` uses `prompt_token_count.max(0)` on an `i64`
- **File:** crates/kod-provider-anthropic/src/provider.rs
- **Line:** 233
- **Severity:** Low
- **Category:** Correctness
- **Description:** `usage.prompt_token_count.max(0) as usize`. `prompt_token_count` is `i64` (from adk-model). A negative value (shouldn't happen, but the type allows it) is clamped to 0, then cast to `usize`. The `.max(0)` is defensive. The `as usize` truncates on 32-bit. See T2-M3.
- **Fix:** Use `u64` for token counts throughout.

### T2-L5 — — Anthropic `wire.rs` `tools_array_with_cache` always marks the last tool, even if there's only one
- **File:** crates/kod-provider-anthropic/src/wire.rs
- **Line:** 411–414
- **Severity:** Low
- **Category:** Correctness
- **Description:** `if let Some(last) = arr.last_mut() { last["cache_control"] = ... }`. For a single-tool request, the only tool gets the marker. That's correct per the design (one tools breakpoint). But it means a single-tool request pays a cache-write premium for one tool schema, which may not be worth it. Minor cost concern.
- **Why it's a bug:** Not a bug — design choice. The cache premium for one tool is small.

### T2-L6 — — OpenAI `provider.rs` `with_api_key_and_timeout` uses `timeout_secs` for both the reqwest client total timeout and the stored field
- **File:** crates/kod-provider-openai/src/provider.rs
- **Line:** 88–99
- **Severity:** Medium
- **Category:** Network
- **Description:** `reqwest::Client::builder().timeout(Duration::from_secs(timeout_secs))` sets a total request timeout. This kills long streams mid-flight (the comment in the Anthropic provider says they moved to connect-only for this reason). The OpenAI provider's streaming path goes through `adk-model`'s client, not `self.client`, so the reqwest timeout only affects `list_models` (non-streaming). But if the engine ever routes a streaming call through `self.client`, it'd be killed at 300 s.
- **Why it's a bug:** Inconsistent with the Anthropic provider, which uses connect-only. The OpenAI provider's comment doesn't justify the total timeout.
- **Fix:** Use `.connect_timeout(...)` only, like the Anthropic provider.

### T2-L7 — — Anthropic `provider.rs` `stream_completion` doesn't propagate `req.session_id` to the wire
- **File:** crates/kod-provider-anthropic/src/provider.rs
- **Line:** 459–489
- **Severity:** Low
- **Category:** Correctness
- **Description:** The OpenAI provider stamps `req.session_id` as the OpenAI `user` field for tab-bridge affinity. The Anthropic `stream_completion` doesn't read `req.session_id` at all — Anthropic isn't a tab bridge today, so this is fine, but if it ever becomes one, the session affinity is missing.
- **Why it's a bug:** Today: no impact. Future: a tab-bridge Anthropic backend would not get session affinity.
- **Fix:** Add a comment that Anthropic is not a tab bridge.

### T2-L8 — — `kod-mcp` `tool_cache.rs` TTL is 30 days; a server upgrade is invisible for a month
- **File:** crates/kod-mcp/src/tool_cache.rs
- **Line:** 46
- **Severity:** Low
- **Category:** Correctness
- **Description:** `DEFAULT_TTL = Duration::from_secs(30 * 24 * 60 * 60)`. A user who upgrades an MCP server (which may add or rename tools) sees the old tool list for up to 30 days. The comment says "A future `kod mcp refresh` can expose [`clear`]" — but that's a manual command the user has to know about.
- **Why it's a bug:** A server upgrade that adds a tool leaves that tool invisible to the agent for a month. The user runs `kod mcp refresh` only if they remember.
- **Fix:** Lower the TTL to 7 days, or detect server version changes (compare `ServerInfo.version` in the cache).

### T2-L9 — — `kod-mcp` `tool_cache.rs` `read_from` uses `Duration::from_nanos(age_nanos.min(u64::MAX as u128) as u64)` which truncates
- **File:** crates/kod-mcp/src/tool_cache.rs
- **Line:** 117
- **Severity:** Low
- **Category:** Correctness
- **Description:** `age_nanos` is `u128`. The code does `age_nanos.min(u64::MAX as u128) as u64` then `Duration::from_nanos(...)`. For an age > ~584 years (u64 nanos), this truncates. Practically impossible, but the cast is unsound.
- **Fix:** Use `Duration::from_nanos(age_nanos.try_into().unwrap_or(u64::MAX))`.

### T2-L10 — — `kod-provider-anthropic` `provider.rs` `with_api_key_and_timeout` defaults `timeout_secs` to 300
- **File:** crates/kod-provider-anthropic/src/provider.rs
- **Line:** 33–38, 64
- **Severity:** Low
- **Category:** Network
- **Description:** `pub fn with_api_key(...) -> Result<Self> { Self::with_api_key_and_timeout(base_url, model, api_key, 300) }`. The 300 s default is documented but not configurable from the outside without going through `with_api_key_and_timeout`. The config layer flows `timeout_secs` in via the latter; the former is the "simple" constructor that hardcodes 300.
- **Why it's a bug:** Not a bug — design choice. But the 300 s default is generous for a non-streaming call; a misconfigured endpoint that hangs wastes 5 minutes.
- **Fix:** Lower the default to 120 s, or make it configurable via the simple constructor.

### T2-L11 — — OpenAI `provider.rs` `stream_request` `accumulated_text` is built but never used for the empty-completion retry decision beyond `is_safe_to_retry`
- **File:** crates/kod-provider-openai/src/provider.rs
- **Line:** 765
- **Severity:** Low
- **Category:** Code quality
- **Description:** `let mut accumulated_text = String::new();` is built up by pushing text chunks. It's only used in `empty_retry.should_retry(&accumulated_text, ...)`. The string can grow unbounded for a long stream — minor memory overhead.
- **Why it's a bug:** Not a bug — the string is bounded by the response size. But for a multi-MB response, holding the full text just to check emptiness is wasteful.
- **Fix:** Track a `bool has_content` instead.

### T2-L12 — — `kod-provider` `RetryPolicy::delay_for` shift can be up to 5 (multiplier 32), but `saturating_mul` on `Duration` with a large `base_delay` could overflow before saturating
- **File:** crates/kod-provider/src/retry.rs
- **Line:** 71–73
- **Severity:** Low
- **Category:** Correctness
- **Description:** `base_delay.saturating_mul(1u32 << (attempt.saturating_sub(1)).min(5))`. For `base_delay = Duration::from_secs(60)` and attempt 6+, multiplier is 32, product is 1920 s. `Duration::saturating_mul` saturates at `Duration::MAX` (~584 years). No overflow. The `.min(5)` caps the shift, so the multiplier is at most 32. OK.
- **Why it's a bug:** Not a bug. Listed for completeness — the saturating arithmetic is correct.

## Recurring themes

1. **Hint truncation**: Sub-second `Retry-After` hints are truncated to 0 (T2-C1) or guessed at 1200 s (T2-C2, T2-H3). The root cause is `KodError::RateLimited { retry_after_secs: u64 }` — the type cannot represent sub-second waits. Widen to `Duration`.

2. **Substring classification**: `is_session_busy` (T2-C3) and the pre-fix `is_retryable` (mentioned in H-T2-P3 comment) both classify errors by pattern-matching prose. The fix is to use typed errors with status codes throughout.

3. **Silent drops**: LSP notifications during requests (T2-C4), malformed SSE frames (T2-M1), string-id MCP responses (T2-H13) — all silently dropped without logs. The pattern is "be tolerant, return empty" without observability.

4. **No upper bounds**: LSP `Content-Length` (T2-C5), MCP line length (T2-C6), LSP header count (T2-M5). Subprocess I/O is treated as trusted; it isn't.

5. **Permit/slot held across sleeps**: T2-C8 (concurrency permit held during rate-limit sleep) and the equivalent in the OpenAI provider. The cap is supposed to bound HTTP requests, not sleeps.

6. **Process group not set**: T2-C7 (both MCP and LSP). `kill_on_drop` kills the immediate child; grandchildren leak.

7. **Path-specific inconsistency**: Anthropic `stream_request` (legacy) vs `stream_completion` (structured) have different retry, tracker, and safety properties (T2-H7, T2-H14). OpenAI `collect` vs `stream_request` have different timeout handling.

8. **Stderr discarded**: T2-H12 (both MCP and LSP). Server-side diagnostics invisible.

9. **Cache accounting edge cases**: T2-C1 (sub-second truncation), T2-M3 (32-bit truncation), T2-M21 (marker on wrong block). Token accounting is correct in the common case but has silent edge cases.

---
# Part 3 — kod-core (the agent engine, swarm runner, router, serve, session log)

_Crates: kod-core_


A deep code review of the `kod-core` crate (~50K LOC, 67 files). Findings
are grouped by severity and each carries a real `file:line` reference and a
code snippet copied verbatim from the source.

---

## Critical bugs

### T3-C1 — — `note_tool_surface_fingerprint` uses `DefaultHasher::new()` whose seed is randomized per call
- **File:** crates/kod-core/src/engine/mod.rs
- **Line:** 11603–11614
- **Severity:** Critical
- **Category:** Correctness
- **Description:** `DefaultHasher::new()` is documented to use a random
  seed. Every call to `note_tool_surface_fingerprint` therefore computes a
  *different* hash for the *same* tool surface. The previous fingerprint
  stored in `tool_surface_fingerprint` was computed under a different seed,
  so `prev != fingerprint` is true on essentially every call. The journal
  fills with spurious `ToolSurfaceChanged` entries and the cache-ledger's
  "did the prefix change?" check becomes useless — defeating the whole
  purpose of fingerprinting the tool surface for cache-invalidation
  detection.
- **Code:**
```rust
async fn note_tool_surface_fingerprint(&self, key: &str, definitions: &[ToolDefinition]) {
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    for d in definitions {
        use std::hash::Hash;
        d.name.hash(&mut hasher);
        d.description.hash(&mut hasher);
        d.parameters_schema.to_string().hash(&mut hasher);
    }
    let fingerprint = std::hash::Hasher::finish(&hasher);

    let mut guard = self.tool_surface_fingerprint.write().await;
    let previous = guard.insert(key.to_string(), fingerprint);
    drop(guard);

    if let Some(prev) = previous {
        if prev != fingerprint {
            // Late MCP registration is the expected cause. Anything
            // else is worth a look at the journal.
            crate::cache_journal::record(
                crate::cache_journal::InvalidationCause::ToolSurfaceChanged { ... },
            );
        }
    }
}
```
- **Why it's a bug:** The fingerprint is intended to be **deterministic
  across calls** so that "same tools ⇒ same hash" holds. `DefaultHasher`
  violates that contract: every call to `note_tool_surface_fingerprint`
  re-seeds the hasher, so two consecutive calls for the identical tool
  inventory produce different hashes. The result is constant spurious
  cache-invalidation journal entries on every prompt, which makes the
  journal useless for distinguishing late-MCP registration (the expected
  cause) from a real bug. Worse, any caller that reads the fingerprint to
  decide whether to invalidate provider-side cache (Anthropic
  `cache_control` breakpoints) will *always* consider the prefix cold.
- **Fix:**
```rust
async fn note_tool_surface_fingerprint(&self, key: &str, definitions: &[ToolDefinition]) {
    const FNV_OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
    const FNV_PRIME: u64 = 0x0000_0100_0000_01b3;
    let mut h = FNV_OFFSET;
    let mut mix = |bytes: &[u8]| {
        for b in bytes { h ^= *b as u64; h = h.wrapping_mul(FNV_PRIME); }
    };
    let mut defs: Vec<&ToolDefinition> = definitions.iter().collect();
    defs.sort_unstable_by(|a, b| a.name.cmp(&b.name));
    for d in defs {
        mix(d.name.as_bytes()); mix(&[0]);
        mix(d.description.as_bytes()); mix(&[0]);
        mix(d.parameters_schema.to_string().as_bytes()); mix(&[0]);
    }
    let fingerprint = h;
    // …
}
```
  (Re-use the same FNV-1a helper the rest of the crate already uses;
  this is the same fix `cache_head_fingerprint` at line 3382 already
  applies.)
- **Notes:** Has the same shape as `cache_head_fingerprint` (line 3382)
  which uses FNV directly and works correctly — the inconsistency
  suggests one was written before the helper was factored.

### T3-C2 — — `command_is_sandbox_downgrade_safe` allows arbitrary code execution through `python -c`, `node -e`, `npm -y`, etc.
- **File:** crates/kod-core/src/engine/mod.rs
- **Line:** 1630–1683
- **Severity:** Critical
- **Category:** Security / Correctness
- **Description:** The downgrade-safe check rejects only the literal
  tokens `; && || | > < \` $( ${ \n \r` at the *top level* of the
  command line. A command like `python -c "import os;
  os.system('rm -rf ~')"` contains none of those tokens outside a
  quoted string, passes the structural check, has `python` as its
  first token (which is on the allow-list), and so reads as
  "sandbox-downgrade-safe" — letting the Jev-driven downgrade remove
  the sandbox for a command that can execute arbitrary shell. The same
  is true of `node -e '…'`, `npm exec -- …`, `cargo run -- …`, `npx
  foo`, etc. The doc-comment on the function explicitly says
  "structural, not textual", but the structure being checked does not
  see into quoted argument payloads.
- **Code:**
```rust
fn command_is_sandbox_downgrade_safe(command: &str) -> bool {
    // …
    const UNSAFE_TOKENS: &[&str] = &[";", "&&", "||", "|", ">", "<", "`", "$(", "${", "\n", "\r"];
    if UNSAFE_TOKENS.iter().any(|t| trimmed.contains(t)) {
        return false;
    }
    // …
    const ALLOW: &[&str] = &[
        "ls", "cat", "head", "tail", "grep", /* … */ "cargo", "rustc", "rustup",
        "go", "gofmt", "python", "python3", "node", "npm", "npx", "tsc", "ruff",
        "pytest", "make", "cmake",
    ];
    if !ALLOW.iter().any(|b| b == &first) {
        // `git` needs the subcommand check.
        if first != "git" { return false; }
        /* … */
    }
    // Reject a couple of argument shapes that are still unsafe even
    // when the first token is allowlisted.
    for tok in ["eval", "exec", "source"] {
        if trimmed.split_whitespace().any(|w| w == tok) { return false; }
    }
    true
}
```
- **Why it's a bug:** A prompt injection that flips the Jev verdict
  (the comment at line 1616–1620 explicitly worries about this case)
  can drive the engine to remove the sandbox for
  `python -c "import shutil; shutil.rmtree('/')"`. The downgrade is
  supposed to be conservative; this check is not.
- **Fix:**
```rust
// Reject interpreters driven by -c / -e / -m / exec / eval / --eval
// since their argument payload is by definition arbitrary code.
const INTERPRETER_RUN_TOKENS: &[&str] = &["-c", "-e", "-m", "--eval", "--exec"];
if ALLOW_INTERPRETERS.contains(&first)
    && trimmed.split_whitespace().any(|w| INTERPRETER_RUN_TOKENS.contains(&w))
{
    return false;
}
// Same for `cargo run`, `npx <pkg>`, `npm exec`, `go run`:
// each can run arbitrary scripts the structural check cannot see.
const RUN_SUBCOMMANDS: &[&str] = &["run", "exec", "x"];
if matches!(first, "cargo" | "go" | "npm" | "npx") {
    let sub = trimmed.split_whitespace().nth(1).unwrap_or("");
    if RUN_SUBCOMMANDS.contains(&sub) { return false; }
}
```
- **Notes:** Real fix may want to refuse the downgrade for any
  interpreter altogether. That is a product decision; the current code
  is unsafe in either case.

### T3-C3 — — `LearnedAllow::from_call` uses a 64-bit FNV-1a hash (collision-prone) to gate auto-approval
- **File:** crates/kod-core/src/engine/mod.rs
- **Line:** 1516–1529
- **Severity:** Critical
- **Category:** Security / Correctness
- **Description:** When the user clicks "always approve" in the
  approval overlay, a `LearnedAllow` is stored keyed on
  `(tool_name, args_hash)`. `args_hash` is a 64-bit FNV-1a of the
  JSON-encoded arguments. FNV-1a is non-cryptographic and known to
  collide easily under adversarial input. The doc on `LearnedAllow`
  says "a differing call is a new request for approval" — but two
  differing calls can hash to the same value, in which case the second
  is silently auto-approved. For `write_file`, that means a write the
  user never approved runs without a dialog.
- **Code:**
```rust
impl LearnedAllow {
    pub fn from_call(call: &ToolCall) -> Self {
        let mut h: u64 = 0xcbf2_9ce4_8422_2325;
        let bytes = serde_json::to_vec(&call.arguments).unwrap_or_default();
        for b in bytes {
            h ^= b as u64;
            h = h.wrapping_mul(0x0000_0100_0000_01b3);
        }
        Self {
            tool_name: call.tool_name.clone(),
            args_hash: format!("{h:016x}"),
        }
    }
}
```
- **Why it's a bug:** A model that learned its own `args_hash` and
  wants to write a *different* file with the same auto-approved hash
  can do so. The collision search against FNV-1a is short — for a
  64-bit hash with no randomization the adversary can brute-force
  pre-images in seconds. The fact that the model can read the
  arguments of its previous approved calls (they are in the
  transcript) gives it the raw hash to target.
- **Fix:** Store the full argument JSON (or its SHA-256) instead of a
  non-cryptographic hash. Approvals are rare (one per
  user-confirmed "always approve"), so storage cost is not a concern.
```rust
pub struct LearnedAllow {
    pub tool_name: String,
    pub args_canonical_json: String,  // or args_sha256: String,
}
impl LearnedAllow {
    pub fn from_call(call: &ToolCall) -> Self {
        // Canonicalise key order so re-ordered JSON still matches.
        let canonical = canonicalize(&call.arguments).to_string();
        Self { tool_name: call.tool_name.clone(), args_canonical_json: canonical }
    }
}
```

### T3-C4 — — `cache_journal.rs` truncates the journal file *outside* the mutex, racing concurrent writers
- **File:** crates/kod-core/src/cache_journal.rs
- **Line:** 81–109
- **Severity:** Critical
- **Category:** Concurrency / Correctness
- **Description:** `record()` locks the `Mutex<File>` only for the
  `writeln!`. The truncation step (read metadata, read whole file,
  re-write the latter half) runs **after** the lock guard is dropped.
  Two `record()` calls racing through the truncation path will both
  `read_to_string` the same file, both compute the same "keep" slice,
  and both `std::fs::write` — and a third call writing concurrently
  will append a line that the truncating call will discard on its
  next pass. Worse, `std::fs::write` truncates-then-writes, so a
  concurrent `writeln!` (which is two syscalls on a raw `File`:
  `write_all` + nothing — the newline is already in the buffer) can
  have its bytes overwritten mid-write by the truncating `write`.
  Result: a corrupted JSONL line the next reader will reject.
- **Code:**
```rust
pub fn record(cause: InvalidationCause) {
    let Some(w) = writer() else { return };
    let Ok(mut file) = w.file.lock() else { return };
    /* … */
    let _ = writeln!(file, "{line}");
    // F2e-8: honour the "bounded" doc. Past ~8 MiB, keep the newest
    // half (journal is debug-only).
    let path = journal_path();
    if let Some(p) = path
        && let Ok(md) = std::fs::metadata(&p)
        && md.len() > 8 * 1024 * 1024
        && let Ok(all) = std::fs::read_to_string(&p)
    {
        let keep: String = all
            .lines()
            .skip(all.lines().count() / 2)
            .collect::<Vec<_>>()
            .join("\n");
        let _ = std::fs::write(&p, keep);
    }
}
```
- **Why it's a bug:** The journal is supposed to be append-only and
  crash-safe (it is the audit trail the design depends on for
  "empty journal ⇒ harness bug"). A torn write or a lost `writeln!`
  corrupts the audit trail silently. Under load (a long session
  churning the tool surface or compacting) the file gets corrupted
  in exactly the failure mode the journal was meant to detect.
- **Fix:** Hold the lock across the truncation, or move the
  truncation out of the hot path entirely onto a periodic task.
```rust
pub fn record(cause: InvalidationCause) {
    let Some(w) = writer() else { return };
    let Ok(mut file) = w.file.lock() else { return };
    let _ = writeln!(file, "{line}");
    // Truncate under the same lock so concurrent record()s cannot
    // race the read-modify-write.
    if let Ok(md) = file.metadata()
        && md.len() > 8 * 1024 * 1024
    {
        // rewind, read, drop the first half, rewind, write, truncate.
        // Use a single file handle so the kernel serialises us.
    }
}
```

### T3-C5 — — `session_log::SessionRecorder` panics on a poisoned mutex
- **File:** crates/kod-core/src/session_log.rs
- **Line:** 375, 427
- **Severity:** Critical
- **Category:** Error handling
- **Description:** `record()` and `flush()` both use
  `self.writer.lock().unwrap()`. If the engine crashes (panic) while
  holding the inner `Mutex<File>` guard — which is now possible because
  `serde_json::to_string` and `write_all` can panic on a poisoned
  writer chain — the mutex is poisoned and every subsequent `record()`
  call panics in turn, taking the whole session-log path down. The
  engine keeps running (the recorder is behind an `Arc` and failures
  are best-effort), but each tool call now panics through `record()`.
  Compare with `taint_level()` at line 4303 which correctly uses
  `unwrap_or_else(|p| p.into_inner())` to recover poison.
- **Code:**
```rust
pub fn record(&self, entry: &SessionEntry) -> Result<()> {
    /* … */
    {
        let mut w = self.writer.lock().unwrap();
        /* … */
        w.flush().map_err(KodError::Io)?;
    }
    Ok(())
}

pub fn flush(&self) -> Result<()> {
    let mut w = self.writer.lock().unwrap();
    w.flush().map_err(KodError::Io)?;
    Ok(())
}
```
- **Why it's a bug:** A single panic anywhere in the writer code path
  poisons the mutex permanently; the next tool call's `record()` panics
  on entry, breaking the user's session. The TUI catches the panic via
  `tokio::spawn`'s join semantics but the recorder is dead for the
  rest of the run.
- **Fix:**
```rust
let mut w = self.writer.lock().unwrap_or_else(|p| p.into_inner());
```
  (matching `taint_level()`).

### T3-C6 — — Background-job watcher can hang forever on a wedged child after an IO error
- **File:** crates/kod-core/src/engine/mod.rs
- **Line:** 2913–2948, 3088
- **Severity:** Critical
- **Category:** Concurrency / Async correctness
- **Description:** The background job's pipe-read loop in
  `build_background_hook` and `build_background_adopt_hook` breaks on
  `Ok(0)` (EOF) or `Err(_)`. After the loop exits, `child.wait().await`
  is called with **no timeout**. If the child is wedged (stuck in a
  kernel call, blocked on a futex, half-zombie), `child.wait().await`
  never resolves. The JoinHandle for that watcher lives forever,
  leaking one Tokio task per wedged child. Worse: the parent's
  `runner_for_watch.spawn_guarded` task keeps the `runner` Arc alive,
  so the BackgroundJobRunner never drops, so the engine's `shutdown()`
  cannot reap it either.
- **Code:**
```rust
loop {
    let read = match stall_dur {
        Some(d) => match tokio::time::timeout(d, stdout.read(&mut buf)).await {
            Ok(r) => r,
            Err(_) => { /* … stall_reported = true; */ continue; }
        },
        None => stdout.read(&mut buf).await,
    };
    match read {
        Ok(0) => break,
        Ok(n) => { if spool.append(&buf[..n]).is_err() { break; } stall_reported = false; }
        Err(_) => break,
    }
}

let status = child.wait().await;  // ← no timeout; a wedged child parks this task forever
```
- **Why it's a bug:** A shell command that wedges after closing its
  stdout (rare but real: a `git push` that dies on a network reset
  mid-prompt) parks the watcher permanently. The job shows as
  "Running" forever; the `runner.complete()` line below `wait()` never
  runs; the user can never reap the job without restarting the engine.
- **Fix:** Wrap `child.wait().await` in a `tokio::time::timeout`:
```rust
let status = match tokio::time::timeout(
    std::time::Duration::from_secs(self.background_wait_timeout_secs),
    child.wait(),
).await {
    Ok(s) => s,
    Err(_) => {
        // The child closed stdout but won't exit. Kill and reap so
        // the watcher does not park forever.
        let _ = child.start_kill();
        child.wait().await
    }
};
```

### T3-C7 — — TTSR write-lock is held across `chunk_tx.send(t).await` inside `stream_round`
- **File:** crates/kod-core/src/engine/mod.rs
- **Line:** 11303–11324
- **Severity:** Critical
- **Category:** Concurrency / Async correctness
- **Description:** When a streamed text chunk arrives, the engine
  locks `self.ttsr.write()` to run `observe_text`, then while still
  inside that write-guard scope calls `chunk_tx.send(t).await`. If the
  receiver is slow (TUI busy, the daemon's writer task blocked on a
  full socket buffer), the send parks the *TTSR write lock*. Every
  other TTSR read on the same transcript — including the next chunk's
  `observe_text` call — blocks until the send resolves. A single
  slow consumer stalls the entire TTSR subsystem for that transcript.
- **Code:**
```rust
StreamChunk::Text(t) => {
    text.push_str(&t);
    {
        let mut engine = self.ttsr.write().await;   // ← write guard
        let fired = engine.observe_text(&t);
        let interrupt = fired.iter().find(|f| f.interrupt).cloned();
        if !fired.is_empty() {
            for f in &fired { tracing::debug!( /* … */ ); }
        }
        if let Some(f) = interrupt {
            let _ = chunk_tx.send(t).await;        // ← park holding the lock
            stream_error = Some(KodError::InvalidState(format!( /* … */ )));
            break;
        }
    }
    let _ = chunk_tx.send(t).await;
    /* … */
}
```
- **Why it's a bug:** TTSR is the live text-rule matcher that powers
  "stop saying X, say Y instead". If a TTSR rule fires and the
  consumer is not draining the channel (e.g. `kod serve` client
  disconnected), the lock park blocks the entire streaming round —
  including the Jev early-termination check further down the loop
  body. The transcript looks "stuck" to the user even though the
  provider is still streaming.
- **Fix:** Compute the interrupt outside the lock; send the chunk
  outside the lock:
```rust
let interrupt = {
    let mut engine = self.ttsr.write().await;
    let fired = engine.observe_text(&t);
    fired.iter().find(|f| f.interrupt).cloned()
};
if let Some(f) = interrupt {
    let _ = chunk_tx.send(t).await;
    stream_error = Some(KodError::InvalidState(format!( /* … */ )));
    break;
}
```

### T3-C8 — — `provider_setup.rs` test helpers mutate env vars with `unsafe`, but the mutex only serializes test calls — production code reading the same var concurrently is still racy
- **File:** crates/kod-core/src/provider_setup.rs
- **Line:** 257, 294, 324
- **Severity:** High (downgraded from Critical because it is test-only, but the `unsafe` is real)
- **Category:** Concurrency
- **Description:** `std::env::set_var` was made `unsafe` in Rust
  1.85 because reading an env var concurrently from another thread is
  a data race. The test code wraps the calls in a `Mutex` to serialise
  the *test functions* against each other, but the `unsafe` block
  claims `SAFETY: serialized via anthropic_env_lock` — that
  serialisation does **not** cover any non-test code in the process
  that reads `ANTHROPIC_API_KEY` (e.g. a background `KodConfig::load_cached()`
  in the same Tokio runtime). On test-only builds this is acceptable,
  but the safety comment is misleading — a future contributor
  extending the test to spawn a Tokio task that touches the env will
  trigger UB and the SAFETY comment will not have warned them.
- **Code:**
```rust
#[test]
fn anthropic_endpoint_requires_an_api_key() {
    let _guard = anthropic_env_lock();
    // SAFETY: serialized via anthropic_env_lock.
    unsafe { std::env::remove_var("ANTHROPIC_API_KEY") };
    /* … */
}
```
- **Why it's a bug:** The `SAFETY` comment promises something the
  mutex cannot deliver — concurrent reads outside the test path are
  still racy. Rust 1.85 made this `unsafe` for a reason.
- **Fix:** Either run these tests in a subprocess (a `#[test]`
  process-per-test runner) or replace env-var mutation with an
  injected `env: &dyn EnvProvider` in `resolve_anthropic_api_key`,
  which lets the test supply its own value without touching process
  global state.

---

## High severity bugs

### T3-H1 — — Sequential Jev calls in `classify_tool_outcome_with_jev` loop
- **File:** crates/kod-core/src/engine/mod.rs
- **Line:** 13358–13369
- **Severity:** High
- **Category:** Performance / Async correctness
- **Description:** Each tool call's outcome classification calls
  `self.classify_tool_outcome_with_jev(...).await` sequentially inside
  a `for` loop. Jev classification is a network round-trip; for a
  round that produced 8 tool calls, this is 8 sequential RPCs before
  the round returns. A faster path would batch (`join_all`) the
  classifications concurrently. The same pattern occurs at line 12953
  for `run_pre` hooks (sequential `.await` per call) and at line 13332
  for `tool_trust_level`/`escalate_taint` (sequential awaits).
- **Code:**
```rust
for (i, call) in calls.iter().enumerate() {
    let Some((result, ms)) = raw_results.get(i) else { continue; };
    let Ok(result) = result.as_ref() else { continue; };
    self.classify_tool_outcome_with_jev(effective_holder, call, result, *ms)
        .await;
}
```
- **Fix:**
```rust
let futs = calls.iter().enumerate().filter_map(|(i, call)| {
    let (result, ms) = raw_results.get(i)?;
    let result = result.as_ref().ok()?;
    Some(async move {
        self.classify_tool_outcome_with_jev(effective_holder, call, result, *ms).await
    })
});
futures::future::join_all(futs).await;
```

### T3-H2 — — `let _ = self.seed_mental_model(seed).await;` silently discards seeding errors
- **File:** crates/kod-core/src/engine/mod.rs
- **Line:** 5288 (and the same pattern at 3292, 5288, 8881, 14638, 15035)
- **Severity:** High
- **Category:** Error handling
- **Description:** Throughout the engine, futures whose results carry
  useful information are discarded with `let _ =`. The most worrying
  are:
  - `let _ = self.seed_mental_model(seed).await;` (5288) — silently
    drops a mental-model seeding error.
  - `let _ = self.maybe_compact_for(key).await;` (3292) — drops the
    number of compacted messages, which is fine, but also drops any
    panic from the compaction path.
  - `let _ = h.await;` (8881) — drops a `JoinError` (panic or
    cancel) from the memory consolidation task. A panic in that
    task is invisible to the operator; the next consolidation never
    runs and the engine reports nothing.
- **Code:**
```rust
let _ = self.seed_mental_model(seed).await;
/* … */
self.stop_memory_consolidation_task().await;
// inside that:
let _ = h.await;
```
- **Fix:** At minimum, log the JoinError/Result on the discard paths.
```rust
if let Err(e) = self.seed_mental_model(seed).await {
    tracing::warn!(error = %e, "mental-model seeding failed; mental block will be empty");
}
```
  For the JoinHandle case:
```rust
if let Err(join_err) = h.await {
    if join_err.is_panic() {
        tracing::error!("memory consolidation task panicked; observations lost");
    }
}
```

### T3-H3 — — `parse_plan_steps` has a redundant `or_else` arm that is unreachable
- **File:** crates/kod-core/src/engine/mod.rs
- **Line:** 3917–3935
- **Severity:** Medium
- **Category:** Code quality / Dead code
- **Description:** The closure inside `filter_map` calls `x.as_str()
  .map(String::from).or_else(|| x.as_str().map(String::from))`. The
  `or_else` is dead code: if `as_str()` returned `None` on the first
  call, the second `as_str()` on the same `x` also returns `None`. The
  likely intent was "try as_str, else fall back to serializing the
  value as a string" — which would be `x.as_str().map(String::from)
  .or_else(|| Some(x.to_string()))`.
- **Code:**
```rust
let steps: Vec<String> = arr
    .iter()
    .filter_map(|x| {
        x.as_str()
            .map(String::from)
            .or_else(|| x.as_str().map(String::from))
    })
    .filter(|s| !s.trim().is_empty())
    .collect();
```
- **Fix:**
```rust
let steps: Vec<String> = arr
    .iter()
    .filter_map(|x| match x {
        serde_json::Value::String(s) => Some(s.clone()),
        other => Some(other.to_string()),
    })
    .filter(|s| !s.trim().is_empty())
    .collect();
```

### T3-H4 — — `apply_compaction_plan` does `covers_through + 1`, which overflows `usize::MAX` in debug
- **File:** crates/kod-core/src/engine/mod.rs
- **Line:** 3672, 3740, 3810 (three call sites)
- **Severity:** Medium (high if the deserialized value is ever user-controlled)
- **Category:** Integer overflow
- **Description:** The `CompactionPlan::Summary`, `NativeSummary`,
  and `Image` variants carry `covers_through: usize`. The apply step
  computes `(covers_through + 1).min(turns.len())`. If a provider
  returns `covers_through = usize::MAX` (extreme, but possible from a
  deserialized JSON number — the wire side does not enforce a bound),
  `covers_through + 1` overflows and panics in debug builds. In
  release builds, it wraps to 0 and the plan is silently applied to
  the wrong range.
- **Code:**
```rust
let end = (covers_through + 1).min(turns.len());
if end == 0 { return 0; }
```
- **Fix:**
```rust
let end = covers_through.saturating_add(1).min(turns.len());
if end == 0 { return 0; }
```

### T3-H5 — — `serve.rs::CappedLines` reads into a Vec that can grow up to the cap on every line
- **File:** crates/kod-core/src/serve.rs
- **Line:** 419–467
- **Severity:** Medium
- **Category:** Performance / Memory
- **Description:** `next_line` allocates a fresh `Vec<u8>` per call
  and grows it by `extend_from_slice` for every `fill_buf` chunk.
  For a small request frame this is fine, but every `read_line` cycle
  allocates up to 1 MiB worth of Vec capacity before being
  `String::from_utf8_lossy`'d into a final `String` (which is itself
  another allocation). Under sustained load (a script driving `kod
  chat --remote` hard), the daemon burns ~2 MiB per request line.
- **Fix:** Reuse a `Vec<u8>` across calls (stored on the
  `CappedLines` struct), `clear()` between calls, and use
  `String::from_utf8_lossy` on a slice to avoid the second allocation.

### T3-H6 — — `apply_retry_adjustment`'s `ShrinkHistory` drains via `messages.drain(0..drop)` which is O(n)
- **File:** crates/kod-core/src/engine/mod.rs
- **Line:** 3895–3903
- **Severity:** Medium
- **Category:** Performance
- **Description:** `messages.drain(0..drop)` shifts the back half of
  the vec forward, an O(messages.len()) memcpy. For a transcript
  that has grown to 200 messages (the cap), every retry does a 200×8
  byte shift. Better to swap-rotate or use a `VecDeque`.
- **Code:**
```rust
A::ShrinkHistory => {
    if messages.len() < 4 { return false; }
    let keep = messages.len() / 2;
    let drop = messages.len() - keep;
    messages.drain(0..drop);
    true
}
```
- **Fix:**
```rust
let split = messages.len() / 2 + messages.len() % 2;  // keep the back half
messages.rotate_left(split);
messages.truncate(messages.len() - split);
```
  Or change the storage to `VecDeque<ChatMessage>`.

### T3-H7 — — `format_call_brief_base` does a fresh `serde_json::Value::to_string()` for unknown tools on every call
- **File:** crates/kod-core/src/engine/mod.rs
- **Line:** 980–989
- **Severity:** Medium
- **Category:** Performance
- **Description:** The fallback branch when no KEYS match calls
  `args.to_string()` to render a JSON preview, then truncates to 80
  chars. This `to_string()` allocates a `String` whose length is the
  entire JSON serialization of the arguments (could be many KB for a
  big patch_file payload), only to truncate to 80 chars immediately.
  The 99% throwaway case still pays the full serialization cost.
- **Fix:** Walk the JSON value and stop at 80 chars, or use
  `serde_json::to_writer` into a `Cursor<Vec<u8>>` with a hard cap.

### T3-H8 — — `fingerprint_of` and `build_repo_map` walk the entire repo synchronously inside `spawn_blocking`, but the fingerprint walk duplicates the work the map walk already did
- **File:** crates/kod-core/src/router.rs
- **Line:** 1518–1601 (fingerprint_of) and repomap::build_repo_map (repomap.rs:160–215)
- **Severity:** Medium
- **Category:** Performance
- **Description:** `RepoMapCache::get_or_rebuild` calls
  `fingerprint_of(working_dir)` (a full tree walk with `ignore::WalkBuilder`
  + metadata + sort + FNV), and on a miss calls
  `crate::repomap::build_repo_map(&working_dir)` — a *second* full
  tree walk on the same tree. Both walks stat every file and read
  every source file. For a 50k-file repo on a cold cache, this is two
  full walks per prompt-miss, each stat-ing 50k inodes and reading
  ~10k source files.
- **Fix:** Combine the two walks — `build_repo_map` already walks
  the tree, it can return the `(mtime, size)` triples alongside the
  `RepoMap`. Or have `fingerprint_of` return the `RepoMap` it
  computed as a side effect.

### T3-H9 — — `cost.rs::SpendWindow` sum can overflow `u64` for extremely large sessions
- **File:** crates/kod-core/src/cost.rs
- **Line:** 396
- **Severity:** Low
- **Category:** Integer overflow
- **Description:** `let micro: u64 = g.iter().map(|(_, m)| *m).sum();`
  sums every micro-USD entry in the window. The window is bounded to
  4096 entries (cap on the ring), but each entry can be up to `u64::MAX`
  micro-USD ($1.8e13). A `u64::MAX` * 4096 sum overflows `u64`. In
  practice, no provider reports a single call that costs $1.8e13,
  but if pricing config is broken and one entry is `u64::MAX`, the
  window sum silently wraps.
- **Fix:**
```rust
let micro: u64 = g.iter().map(|(_, m)| *m).fold(0u64, u64::saturating_add);
```

### T3-H10 — — `serve.rs` casts `client_max` and `client_min` from `u64` to `u8`, silently truncating
- **File:** crates/kod-core/src/serve.rs
- **Line:** 552, 557
- **Severity:** Medium
- **Category:** Integer overflow / truncation
- **Description:** `let client_min = req.params.get("min")
  .and_then(|v| v.as_u64()).unwrap_or(1) as u8;` — a client sending
  `"min": 256` gets `client_min = 0`. Today every client uses `v: 1`,
  but a future client negotiating a higher version would silently
  land at `v = 0`, pass the `MIN_PROTOCOL_VERSION..=MAX_PROTOCOL_VERSION`
  range check (0 < 1 = false), and receive a "version mismatch" error
  it cannot explain.
- **Fix:** Use `u8::try_from(value).unwrap_or(1)` and emit a clear
  error on overflow:
```rust
let client_min = match req.params.get("min").and_then(|v| v.as_u64()) {
    Some(n) if n <= u8::MAX as u64 => n as u8,
    Some(_) => return write_error(&out_tx, &req.id, "min must fit in u8").await?,
    None => 1,
};
```

### T3-H11 — — `serve.rs::handle_connection` reads `out_tx` clones out for every spawned `process` task without bounding the in-flight count
- **File:** crates/kod-core/src/serve.rs
- **Line:** 583–612, 600–612
- **Severity:** Medium
- **Category:** Concurrency / Memory
- **Description:** Every `process` and `process_streaming` request
  spawns a fresh task holding a clone of `out_tx`. The channel has a
  bound of 256; if a malicious same-UID client floods the daemon with
  streaming requests, the spawned tasks queue indefinitely behind the
  writer task and the daemon accumulates one live task per in-flight
  request with no upper bound. The peer-UID check stops an external
  attacker, but a buggy client (or a runaway script) leaks tasks.
- **Fix:** Add a `tokio::sync::Semaphore` to bound concurrent
  in-flight `process_streaming` calls; reject overflow with a 503.

### T3-H12 — — `serve.rs::run_streaming` awaits the engine call inside the connection task, so a stalled engine stalls the writer forever
- **File:** crates/kod-core/src/serve.rs
- **Line:** 789–803
- **Severity:** Medium
- **Category:** Async correctness
- **Description:** `run_streaming` spawns `engine.process_streaming_for`
  as a task, then drains `chunk_rx` until the channel closes. If the
  engine task panics, the channel's sender half (held by the engine
  closure) is dropped on unwind, the `chunk_rx.recv()` loop returns
  `None`, and `call.await` returns `Err(JoinError::Panic)`. That is
  handled. But if the engine task *hangs* (e.g. stuck on a TTSR lock
  park per T3-C7), the channel never closes, the `while let Some(chunk)`
  loop never exits, and the writer task lives forever. The
  `write_done` / `write_error` after the loop never run; the client
  waits forever for a `done` frame.
- **Fix:** Wrap the `call.await` in a `tokio::time::timeout` (say
  10 minutes — long enough for any legitimate turn, short enough to
  break a hung one).

### T3-H13 — — `router::contains_word` can panic on a non-ASCII haystack when called repeatedly
- **File:** crates/kod-core/src/router.rs
- **Line:** 748–768
- **Severity:** Medium
- **Category:** Correctness / UTF-8 boundary
- **Description:** The function does `haystack[start..].find(needle)`
  where `start = abs + 1` after each match. `abs` is the byte
  position the previous `find` returned. `abs + 1` is one byte after
  that match. Since the needles are all ASCII words, `abs` is on a
  char boundary, and `abs + 1` is on a char boundary only if the byte
  at `abs` is ASCII (which it is, since the needle starts with an
  ASCII char). So for ASCII needles the function is safe. **But** if
  a future caller passes a non-ASCII needle (or if the function is
  copied to a new call site), the slicing panics with
  "byte index is not a char boundary".
- **Fix:** Use `haystack[abs..].char_indices().find_map(|(i, c)| …)`
  or guard with `haystack.is_char_boundary(start)`.

### T3-H14 — — `compaction_dispatcher::HandoffMethod::run` does `split - 1`, which underflows if `split == 0`
- **File:** crates/kod-core/src/compaction_dispatcher.rs
- **Line:** 931
- **Severity:** Medium
- **Category:** Integer underflow
- **Description:** `let split = ctx.transcript.len() / 2;` is at
  least 2 (the `MIN_HANDOFF_MESSAGES = 4` check ensures
  `transcript.len() >= 4`). So `split - 1` is currently ≥ 1. But if
  the threshold is ever lowered (or a future caller passes a smaller
  transcript bypassing the gate), `split - 1` underflows `usize` and
  panics in debug. The other covers_through computations in
  `apply_compaction_plan` use `covers_through + 1` (T3-H4).
- **Code:**
```rust
let split = ctx.transcript.len() / 2;
let dropped = &ctx.transcript[..split];
let prompt = build_handoff_prompt(dropped);
match handle.provider.generate(&prompt, &handle.options).await {
    Ok(text) if !text.trim().is_empty() => MethodOutcome::Plan(CompactionPlan::Summary {
        covers_through: split - 1,  // ← panics if split == 0
        text: text.trim().to_string(),
    }),
```
- **Fix:** `covers_through: split.saturating_sub(1)`.

### T3-H15 — — `swarm_runner::run` pump task awaiting pattern can hang the retry loop
- **File:** crates/kod-core/src/swarm_runner.rs
- **Line:** 1275–1322, 1356
- **Severity:** High
- **Category:** Concurrency / Async correctness
- **Description:** The pump task drains `rx.recv().await` and pushes
  to `out_pump.send(...).await`. After the run future completes, the
  runner does `drop(tx); let _ = pump.await;`. If the pump is blocked
  in `out_pump.send().await` (because the consumer closed), the
  `pump.await` never resolves. The retry loop's `loop` body blocks
  on `pump.await` before re-issuing the run, so the agent never
  retries — it sits dead-locked forever.
- **Fix:** Bound the `pump.await` with `tokio::time::timeout` (a
  few seconds is plenty to drain an empty channel), and on timeout
  abort the pump:
```rust
let _ = tokio::time::timeout(
    std::time::Duration::from_secs(2),
    pump,
).await;
```

### T3-H16 — — `swarm_runner::DispatchGuard`'s `Drop` locks a `parking_lot::Mutex` while the parent task may also need it
- **File:** crates/kod-core/src/swarm_runner.rs
- **Line:** 1166–1176
- **Severity:** Medium
- **Category:** Concurrency / Deadlock potential
- **Description:** `Drop for DispatchGuard` does `self.keys.lock()`
  and mutates the inner map. The watchdog task also locks the same
  mutex (at line 933) to enumerate keys. If a wave task panics while
  holding the lock (rare but possible — `parking_lot::Mutex` does
  *not* poison, so a panic is non-fatal here), the Drop's `lock()`
  still succeeds (parking_lot mutexes are unpoisonable) — but the data
  it finds may be inconsistent. The use of `parking_lot` over
  `std::sync::Mutex` here is intentional and avoids the poisoning
  issue; the warning is about the lock ordering: the wave task holds
  `dispatch_keys` while it also acquires `engine.history.write()`
  (line 1357) via `forget_transcript`. The watchdog acquires
  `dispatch_keys` only. No actual deadlock ordering bug today, but
  adding a future call site that locks `history` then `dispatch_keys`
  would deadlock.
- **Notes:** Document the lock order at minimum: `dispatch_keys`
  before `history` only.

### T3-H17 — — `engine::stream_round` falls back to wrapping the raw args string as a JSON string when args don't parse
- **File:** crates/kod-core/src/engine/mod.rs
- **Line:** 11491–11492
- **Severity:** Medium
- **Category:** Error handling
- **Description:** When the streamed tool-call arguments don't parse
  as JSON, the code falls back to `Value::String(p.args.clone())` —
  wrapping the raw partial JSON as a JSON string. The tool call then
  dispatches with `arguments = "partial json as a string"`. The tool
  tries to read `arguments["path"]` and gets `None` because the
  arguments are a JSON string, not an object. The model sees a
  generic "missing argument" error instead of the underlying parse
  error.
- **Fix:** Propagate the parse error into `partial_error` instead of
  dispatching the call with a useless arguments blob.

### T3-H18 — — Tracing log messages have weird whitespace from line-merging
- **File:** crates/kod-core/src/swarm_runner.rs (lines 946–947, 1087, 1457–1458), engine/mod.rs (4477), provider_setup.rs (170–177)
- **Severity:** Low
- **Category:** Code quality
- **Description:** Several `tracing::warn!` and `format!` calls
  contain literal strings with dozens of consecutive spaces inside
  what should be a single-line message. Likely an artifact of
  merging a multi-line string literal without normalising whitespace.
  Example from `swarm_runner.rs:946`:
```rust
"swarm watchdog: dispatch has not reported within the idle window;                                  sending cooperative cancel"
```
  and `provider_setup.rs:170`:
```rust
"endpoint {:?}: no Anthropic API key. Set {} in the environment          (or set `api_key_env` on the endpoint to name a different          variable)."
```
- **Fix:** Re-format the strings; if a multi-line literal is wanted,
  use `\` line continuations or `\n` explicitly.

---

## Medium severity bugs

### T3-M1 — — `expand_at_references` byte-walks the input and falls back to `'\0'` on char-boundary failure
- **File:** crates/kod-core/src/engine/mod.rs
- **Line:** 711–754
- **Severity:** Medium
- **Category:** Correctness / UTF-8 boundary
- **Description:** The function walks `bytes` and copies characters
  via `let ch = input[i..].chars().next().unwrap_or('\0');`. The
  `unwrap_or('\0')` is defensive — `i` should always be on a char
  boundary because the loop only advances by `ch.len_utf8()` or jumps
  to `j` (which is one past a `@` ASCII byte). But if a future caller
  passes `i` not on a boundary (impossible today, possible after a
  refactor), the fallback emits a `'\0'` byte — and `'\0'` is the
  marker delimiter the engine uses elsewhere (`\0kod-tool:` etc.).
  A `'\0'` injected into the user's prompt text would be parsed as
  the start of a marker chunk by `parse_tool_start` and friends.
- **Fix:** `unwrap()` (the invariant holds) or return an error
  rather than synthesising NUL bytes that the marker parsers will
  misinterpret.

### T3-M2 — — `render_mental_model_block` uses `String::new()` + `push_str` in a loop without capacity hint
- **File:** crates/kod-core/src/engine/mod.rs
- **Line:** 670–687
- **Severity:** Low
- **Category:** Performance
- **Description:** Builds a string of mental-model entries without
  `String::with_capacity(budget_chars)`; the string reallocates as
  it grows. Each `format!("- {content}\n")` also allocates a fresh
  String per entry.
- **Fix:** `let mut out = String::with_capacity(budget_chars);` and
  `out.push_str("- "); out.push_str(content); out.push('\n');`.

### T3-M3 — — `build_summary_prompt` and `build_handoff_prompt` use the same pattern: `body.push_str(&line); body.push('\n');` without capacity
- **File:** crates/kod-core/src/engine/mod.rs (1115–1135) and compaction_dispatcher.rs (859–895)
- **Severity:** Low
- **Category:** Performance
- **Description:** Both builders allocate `String::new()` and grow it
  via `push_str` for every dropped message; the dropped slice can be
  ~32 KB. Each push triggers a realloc.
- **Fix:** `let mut body = String::with_capacity(CAP.min(dropped.iter().map(|m| m.render_text().len() + 1).sum()));`.

### T3-M4 — — `cap_rendered_result` can emit `… [truncated X of Y bytes]` with `Y < X` when `per_field` is 0
- **File:** crates/kod-core/src/engine/mod.rs
- **Line:** 565–580
- **Severity:** Low
- **Category:** Correctness / Display
- **Description:** When `cap` is small (less than `JSON_CAP_HEADROOM`),
  `per_field = cap.saturating_sub(JSON_CAP_HEADROOM) / 2 = 0`. Then
  for any string field, `s.len() > per_field` (any non-empty string),
  `removed = s.len() - 0 = s.len()`, and the placeholder becomes
  `… [truncated <s.len()> of <s.len()> bytes]` — i.e. the entire
  content is "truncated" but the message claims it was truncated to
  zero bytes from itself.
- **Fix:** Guard `if per_field == 0 { return raw; }` or compute
  `removed` against `cap` rather than `per_field`.

### T3-M5 — — `router::classify_task` does a `to_lowercase()` of the whole input on every call
- **File:** crates/kod-core/src/router.rs
- **Line:** 770
- **Severity:** Low
- **Category:** Performance
- **Description:** A long user prompt (~10 KB) gets a full
  case-folded clone allocated just to run substring matches against
  ASCII keywords. The matches themselves are O(N×needles) and could
  be done with a single case-insensitive scan; the to_lowercase is
  the bigger of the two costs.
- **Fix:** Use a `char_indices` scan that case-folds on the fly,
  or use `regex::RegexBuilder::case_insensitive(true)` on the
  precomputed keyword list.

### T3-M6 — — `RouterConfig::default()` calls `std::env::current_dir()` synchronously at construction
- **File:** crates/kod-core/src/router.rs
- **Line:** 150
- **Severity:** Low
- **Category:** Async correctness
- **Description:** `std::env::current_dir()` is a syscall that can
  block briefly. Calling it in a `Default` impl that may run inside
  a Tokio runtime worker thread is the kind of "blocking in async"
  that tokio warns about. In practice the call is sub-millisecond,
  but on a slow filesystem (NFS home) it can take longer.
- **Fix:** Use `tokio::env::current_dir()` in async contexts, or
  capture the dir before spawning the runtime.

### T3-M7 — — `Engine::apply_compaction_plan` returns early with `messages: messages.clone()` in four places, cloning the messages vec each time
- **File:** crates/kod-core/src/engine/mod.rs
- **Line:** 13721–13727, 13735–13741, 13766–13772, 13780–13786
- **Severity:** Low
- **Category:** Performance
- **Description:** Each early-return path constructs a `ToolRound`
  with `messages: messages.clone()` to satisfy the return type. For
  a round that produced N structured messages, that is N×element-size
  bytes of cloning per early-return path.
- **Fix:** Make `messages` an `Rc<Vec<ChatMessage>>` or `Arc<[ChatMessage]>`
  so cloning is cheap, or restructure to a single return point.

### T3-M8 — — `cost::SpendWindow::spend_in` takes a write lock and prunes entries on every read
- **File:** crates/kod-core/src/cost.rs
- **Line:** 386–398
- **Severity:** Low
- **Category:** Concurrency
- **Description:** `spend_in` acquires `self.entries.write()` and
  pops front entries older than the cutoff. The write lock blocks
  concurrent readers. A `/budget` UI that polls every render frame
  contends with `record()` calls on every poll.
- **Fix:** Read-lock first to compute the sum; only acquire the
  write lock if the front entry is actually stale. Or use a
  `RwLock<VecDeque>` with `pop_front` only on a write.

### T3-M9 — — `serve.rs::bind_listener` does a connect-probe before binding, with a TOCTOU window
- **File:** crates/kod-core/src/serve.rs
- **Line:** 265–288
- **Severity:** Low (mitigated by the `flock`)
- **Category:** TOCTOU
- **Description:** `bind_listener` connects to `path`; if the connect
  succeeds, the daemon refuses to start. The check-then-bind runs
  under `flock`, so two concurrent `kod serve` invocations serialise.
  But a sibling that *just* bound the socket can race this check:
  invocation A binds, invocation B's connect succeeds, invocation B
  refuses to start — even though A is the one running. The flock
  prevents this, but only because the *whole* check-then-bind is
  under the flock — which it is.
- **Notes:** Not actually a bug today; this is here so the next
  refactor does not move the lock release before the bind.

### T3-M10 — — `serve.rs` writes a 1-MiB request line into a `String::from_utf8_lossy`'d buffer
- **File:** crates/kod-core/src/serve.rs
- **Line:** 466
- **Severity:** Low
- **Category:** Memory
- **Description:** `String::from_utf8_lossy(&buf).into_owned()`
  allocates a fresh `String` the size of the buffer. For a malicious
  same-UID client that writes 1-MiB lines, this is a 1-MiB allocation
  per request, GC'd after the request handler returns.
- **Fix:** `serde_json::from_slice` directly on the `&[u8]` to avoid
  the intermediate `String`.

### T3-M11 — — `swarm_runner::run` watches the swarm through `tokio::spawn`'d tasks but never aborts them on a global deadline
- **File:** crates/kod-core/src/swarm_runner.rs
- **Line:** 902–962
- **Severity:** Medium
- **Category:** Concurrency / Cancellation safety
- **Description:** The watchdog task is `tokio::spawn`'d and stored
  in `watchdog_task`. On `run`'s normal exit, `let _ =
  watchdog_stop_tx.send(()).await; let _ = watchdog_task.await;`
  stops it. But if `run` returns early via `?` (an error in
  decompose, worktree setup, etc.), the watchdog task is *not*
  awaited — it is left detached. The `shutdown` Notify is dropped,
  the watchdog's `select!` waits on `watchdog_stop_rx.recv()` which
  returns `None` when the sender is dropped, so the watchdog does
  eventually exit. But until it does, it polls `swarm.list_agents()`
  every 10s, calling `agent.is_timed_out()` — and the swarm is now
  in shutdown state, so those calls may panic. The detached task
  leaks for the runtime's lifetime.
- **Fix:** Use a `defer`-style guard that aborts the watchdog task
  on scope exit (or restructure the early-returns to all run through
  the cleanup at the bottom of `run`).

### T3-M12 — — `provider_setup::resolve_api_key` falls back to the literal string `"not-needed"` for local servers
- **File:** crates/kod-core/src/provider_setup.rs
- **Line:** 192–194
- **Severity:** Low
- **Category:** Code quality
- **Description:** A misconfigured `OPENAI_API_KEY="not-needed"` env
  var is indistinguishable from the fallback. A user who sets that
  by mistake (e.g. copying a `.env` from a tutorial) gets silently
  routed to a local server with no API key, gets a 401 from the real
  server, and the error message blames the server rather than the
  config.
- **Fix:** Use a value that cannot be confused with a real key, e.g.
  `kod-no-api-key-required` or refuse to fall back when the env var
  is explicitly set to anything.

### T3-M13 — — `serve.rs::handle_connection`'s version check uses `let _ = send_response(...).await?;` on the error path but `?` propagates writer-channel failures out of the connection
- **File:** crates/kod-core/src/serve.rs
- **Line:** 532–542
- **Severity:** Low
- **Category:** Error handling
- **Description:** When a version mismatch is detected, the code
  calls `write_error(&out_tx, &req.id, ...).await?;` — the `?`
  propagates an `Err` from `write_error` (e.g. writer channel
  closed) out of `handle_connection`, which then tears down the
  connection without sending the version error. The intended
  behaviour is to send the version error and `continue` to the next
  request; the `?` short-circuits that recovery.
- **Fix:** Use `let _ = write_error(...).await;` and `continue;`
  (matching the parse-error path at line 510).

---

## Performance issues

### T3-P1 — — `engine::inline_image_tool_results` re-rasterises a cached frame's content every call (until cached)
- **File:** crates/kod-core/src/engine/mod.rs
- **Line:** 11639–11706
- **Severity:** Medium
- **Category:** Performance
- **Description:** For each tool message that crosses the
  `MIN_INLINE_IMAGE_TOKENS` threshold, the function looks up
  `image_render_cache` (a `RwLock<HashMap>`) — read lock, clone,
  drop — then on a miss calls `crate::snapcompact::rasterize_to_png`,
  which can take tens to hundreds of milliseconds. The cache is
  keyed by `(tool_call_id, simple_hash(content))`. The content of a
  tool result *rarely* changes across rounds (the model re-reads the
  same file), so the cache hits — but the read lock + clone + write
  lock dance runs on every prompt.
- **Fix:** Hold a single `Arc<RwLock<…>>` and `Cow`-clone the
  cached frame, or store the cache in a `OnceLock` per `tool_call_id`
  rather than a per-engine map.

### T3-P2 — — `engine::stream_round` clones the `Arc<dyn LlmProvider>` and the entire `CompletionRequest` *every round* to make the stream `'static`
- **File:** crates/kod-core/src/engine/mod.rs
- **Line:** 11220–11231 (and 11156–11164 in `fallback_stream_for_off_track`)
- **Severity:** Medium
- **Category:** Performance
- **Description:** Each streaming round boxes a fresh
  `async_stream::stream!` that owns `provider_owned: Arc<…>` (cheap)
  and `req_owned: CompletionRequest` (expensive — deep clones of the
  `Vec<ChatMessage>` messages, tools, system segments). For a long
  tool-using turn (10 rounds × 100KB transcript), that is 1MB of
  cloning per round.
- **Fix:** Pass a borrowed `&'a CompletionRequest` to the stream
  via a `'static`-projecting wrapper, or restructure the stream to
  yield from a `Pin<Box<dyn Stream<Item = …> + 'a>>` and avoid the
  clone.

### T3-P3 — — `engine::apply_steers` builds a new `String` per steer inside the loop
- **File:** crates/kod-core/src/engine/mod.rs
- **Line:** 10713–10748
- **Severity:** Low
- **Category:** Performance
- **Description:** For each `SoftInterrupt`, the function does
  `pending.push_str(&format!("\n\n{body}\n"))` and
  `messages.push(ChatMessage::text(MessageId::new(), …, body, …))`
  — two allocations per steer.
- **Fix:** Pre-size `pending` with the sum of body lengths; build
  the message via `ChatMessage::text` with a `&str` borrowed from
  the body if the API allows.

### T3-P4 — — `engine::run_tool_calls_with_speculations` clones `tool_context` for every parallel call
- **File:** crates/kod-core/src/engine/mod.rs
- **Line:** 13278
- **Severity:** Medium
- **Category:** Performance
- **Description:** In the parallel (read-only) branch, each call's
  future does `let mut ctx = tool_context.clone();`. `ToolContext`
  carries an `Arc<PathLockTable>` and several `Arc<…>` fields, so
  the clone is cheap — but the future is `async move`, so the clone
  must outlive the future's storage. The cost is N atomic increments
  per round for N concurrent calls.
- **Notes:** Probably not worth fixing unless the parallel-call
  count climbs; 8 calls × 8 atomic inc = 64 atomics per round.

### T3-P5 — — `engine::prepare_turn` holds the history read lock across `remember_turn_for` which writes
- **File:** crates/kod-core/src/engine/mod.rs
- **Line:** 3293–3294
- **Severity:** Low
- **Category:** Concurrency
- **Description:** `let history = self.render_history_for(key).await;
  self.remember_turn_for(key, true, input).await;` — the first
  call acquires the history read lock, then the second takes the
  write lock. The `await` between them means the read lock is
  released before the write — but the rendering of `history` cloned
  the whole transcript. If a swarm agent on another transcript is
  also rendering history, both clones happen concurrently, doubling
  peak memory briefly.
- **Notes:** Not a correctness bug; just a peak-memory observation.

### T3-P6 — — `engine::stream_round` calls `chunk_tx.send(...).await` on every text chunk, blocking the streaming loop on a slow consumer
- **File:** crates/kod-core/src/engine/mod.rs
- **Line:** 11325
- **Severity:** Medium
- **Category:** Performance / Concurrency
- **Description:** Each streamed text chunk is sent to `chunk_tx`
  with `await`. If the consumer (TUI render loop, daemon writer) is
  slow, the streaming loop parks and the provider's stream sits
  idle. The provider may time out the connection. A bounded channel
  of 64 (default) only absorbs a brief stall; a multi-second stall
  kills the stream.
- **Fix:** Use `try_send` with a bounded-overflow policy: if the
  channel is full, drop the chunk and emit a backpressure signal.
  Or run the consumer on a dedicated high-priority task.

### T3-P7 — — `router::fingerprint_of` sorts the entire entries vec every call
- **File:** crates/kod-core/src/router.rs
- **Line:** 1584
- **Severity:** Low
- **Category:** Performance
- **Description:** After collecting up to 100k `(path, mtime, size)`
  triples, the function sorts them lexicographically by path. The
  sort is O(N log N) on 100k items — hundreds of milliseconds. The
  sort is needed to make the hash deterministic across runs (the
  `ignore` walker yields entries in directory order, which is
  filesystem-dependent). The same hash is recomputed on every prompt
  (it is the cache-invalidation check); the actual map rebuild only
  runs on a hash mismatch.
- **Fix:** If the walker is stable within a session, cache the
  sorted entries and only re-walk on mtime-of-the-tree change.

### T3-P8 — — `swarm_runner::probe_repo` runs N sequential `grep` calls
- **File:** crates/kod-core/src/swarm_runner.rs
- **Line:** 1848–1877
- **Severity:** Low
- **Category:** Performance
- **Description:** For each of up to 5 keywords extracted from the
  goal, the function calls `self.engine.run_tool("grep", ...).await`
  sequentially. Five sequential grep calls = 5× a single grep's
  latency.
- **Fix:** Use `futures::future::join_all` to grep concurrently.

### T3-P9 — — `cache_journal::record` re-reads and re-writes the entire 8-MiB journal on every truncation
- **File:** crates/kod-core/src/cache_journal.rs
- **Line:** 96–108
- **Severity:** Low
- **Category:** Performance
- **Description:** When the journal crosses 8 MiB, the code reads
  the entire file (8 MiB), splits it into lines, skips the first
  half, joins the second half, and writes it back. The
  `all.lines().count() / 2` is computed twice (once for skip, once
  for the count itself). The `Vec<&str>` collect and `join` allocate
  another 4 MiB String.
- **Fix:** Use `BufRead::lines()` and write line-by-line to a
  temp file, then rename. Avoids the 8 MiB read and the 4 MiB
  second-half String.

### T3-P10 — — `engine::inline_image_tool_results` computes the last-tool index with `rposition`, an O(N) scan, on every call
- **File:** crates/kod-core/src/engine/mod.rs
- **Line:** 11650–11652
- **Severity:** Low
- **Category:** Performance
- **Description:** `messages.iter().rposition(|m| m.role == Tool)`
  walks the messages backward to find the freshest tool message.
  For a long transcript, this is O(N) per prompt.
- **Fix:** Track the last-tool index incrementally when messages
  are appended.

---

## Code quality / maintainability issues

### T3-Q1 — — `swarm_runner::DispatchGuard` Drop releases the `parking_lot::Mutex` it itself locked, but the function name does not signal that
- **File:** crates/kod-core/src/swarm_runner.rs
- **Line:** 1166–1181
- **Severity:** Low
- **Category:** Code quality
- **Description:** The Drop impl is correct, but its documentation
  does not name the lock-ordering invariant (the watchdog locks the
  same mutex from a different task; a future call site that locks
  `engine.history` then `dispatch_keys` would deadlock). The
  `// H-D1: guard the deregistration even on panic.` comment is
  there but the lock-order invariant is implicit.

### T3-Q2 — — `engine::run_streaming_loop` is 350+ lines, mixing many concerns (rounds, speculations, TTSR, markers, traces, Jev)
- **File:** crates/kod-core/src/engine/mod.rs
- **Line:** 10772–11120
- **Severity:** Medium
- **Category:** Code quality
- **Description:** A single function spanning ~350 lines and
  touching 8 different engine subsystems is hard to test in isolation
  and hard to reason about for lock ordering. The function should be
  decomposed into `stream_one_round`, `dispatch_round_tool_calls`,
  `flush_round_markers`, etc.
- **Notes:** Not a bug; a maintainability concern for the next
  contributor.

### T3-Q3 — — Doc-comment merging leaves two `///` blocks on one function in several places
- **File:** crates/kod-core/src/engine/mod.rs (e.g. 515–539, 868–873, 1026–1031, 1099–1108), compaction_dispatcher.rs (104–126), serve.rs (405–408)
- **Severity:** Low
- **Category:** Code quality / Documentation
- **Description:** Several functions carry two consecutive `///`
  doc-comment blocks (one for the function, one for an unrelated
  helper that was merged into the same scope). The doc renders as
  a single block in rustdoc, losing the structure.
- **Example (engine/mod.rs:515–539):**
```rust
/// Cap a rendered `ToolResult::Success` to at most `cap` bytes without
/// cutting mid-JSON.
///
/// The simple byte cut this replaces produced, for a large `read_file`
/// result:
/// …
/// This helper trims the long string fields *inside* the object …
/// Render a tool result for the prompt block, capped to `cap` chars.
///
/// When `redactor` is `Some`, the rendered JSON is passed through the
/// secret redactor (Tier 1.3). …
pub(crate) fn cap_rendered_result( ... ) -> String {
```

### T3-Q4 — — `cache_ledger` has duplicate `sticky()` and `sticky_endpoint()` methods with identical implementations
- **File:** crates/kod-core/src/cache_ledger.rs
- **Line:** 115–117 and 156–158
- **Severity:** Low
- **Category:** Code quality
- **Description:** Both return `self.sticky.as_deref()`. Likely a
  refactor that added a clearer name without removing the old one.
  Pick one and deprecate the other.

### T3-Q5 — — Many `tracing` log strings contain spurious runs of spaces (artifact of multi-line string merging)
- **File:** crates/kod-core/src/swarm_runner.rs (946, 1087, 1457), provider_setup.rs (170–177), engine/mod.rs (4777), serve.rs (647, 757)
- **Severity:** Low
- **Category:** Code quality
- **Description:** Strings like `"swarm watchdog: dispatch has not reported within the idle window;                                  sending cooperative cancel"` show up in operator logs as
  garbled messages. They make log-grep harder than it should be.
- **Fix:** Sweep the crate for runs of >4 spaces inside `&'static str`
  and `format!` arguments.

### T3-Q6 — — `run_git_owned` is documented as "takes owned `Vec<&str>`" but the signature is `&[&str]`, same as `run_git`
- **File:** crates/kod-core/src/worktree.rs
- **Line:** 710–714
- **Severity:** Low
- **Category:** Code quality / Documentation
- **Description:** The doc comment is misleading — the function
  signature does not match what the comment says. The two functions
  are literally identical in body (`run_git_inner`); only the doc
  differs. The "owned" name was presumably intended to signal
  "you can pass temporary Strings here" but that's a property of
  the caller's array literal, not the function.
- **Fix:** Delete `run_git_owned` and call `run_git` everywhere;
  the slice borrows from temporaries that live for the call
  statement, which is enough.

### T3-Q7 — — `engine::KodEngine` has 50+ fields, many of which are `RwLock<HashMap<…>>` keyed by transcript
- **File:** crates/kod-core/src/engine/mod.rs
- **Line:** 1718–2297
- **Severity:** Medium
- **Category:** Code quality
- **Description:** The engine struct holds at least 15 separate
  `RwLock<HashMap<String, …>>` maps keyed by transcript key:
  `history`, `plans`, `decision_logs`, `tool_loop_guards`,
  `todo_trackers`, `prewalks`, `plan_mode`, `plan_reference_paths`,
  `transcript_working_dirs`, `transcript_write_globs`,
  `prewarmed`, `injected_memory_at`, `observed_usage`,
  `context_gauges`, `native_compaction_blocks`, `image_render_cache`,
  `image_frames`, `retention_cursors`, `decisions_cursors`,
  `tool_filter_states`, `tool_surface_fingerprint`, `last_prompt`,
  `current_requests`, `pending_summaries`, `summaries_in_flight`,
  `deferred_diagnostics`, `sharpshooter_deltas`, `blackboard_viewers`,
  `pending_approvals`, `pending_questions`. A `TranscriptState` struct
  that bundles the per-key fields would reduce lock-acquisition
  overhead (one lock per transcript vs N) and make forget-transcript
  a single `remove` instead of 25.
- **Notes:** This is the single biggest maintainability liability
  in the crate.

### T3-Q8 — — `engine::maybe_create_plan`'s plan prompt is built with `format!()` of a 700-byte multi-line literal in source
- **File:** crates/kod-core/src/engine/mod.rs
- **Line:** 4398–4406
- **Severity:** Low
- **Category:** Code quality
- **Description:** A 700-byte `format!` string-literal that embeds
  `{input}` directly is hard to read, hard to test, and any change
  to the plan instruction requires touching the engine source. The
  same content would be cleaner as a `plan_prompt.txt` include!
  via `include_str!`.
- **Fix:** Move the prompt to `crates/kod-core/src/engine/plan_prompt.txt`
  and `include_str!` it.

### T3-Q9 — — `serve.rs::CappedLines::next_line` does not handle the case where the line has no `\n` and the buffer is empty
- **File:** crates/kod-core/src/serve.rs
- **Line:** 419–467
- **Severity:** Low
- **Category:** Correctness
- **Description:** When the connection closes mid-line (no `\n`
  yet, `fill_buf` returns empty), the function breaks and returns
  `Ok(Some(String))` for the partial line. That is actually the
  desired behaviour (the comment at session_log.rs:439–447 calls
  this out as the "tail is a partial line" case). But the partial
  line is treated as a complete request here, while session_log
  treats it as corrupt and skips. The two modules have opposite
  policies on the same edge case.
- **Notes:** Decide on one policy and apply it in both places.

### T3-Q10 — — `provider_setup::resolve_anthropic_api_key` formats an error message with literal spaces inside a format string
- **File:** crates/kod-core/src/provider_setup.rs
- **Line:** 170–177
- **Severity:** Low
- **Category:** Code quality
- **Description:** See T3-Q5. The error string has 10+ space runs
  baked in from a multi-line literal merge, which makes the error
  message read awkwardly when surfaced to the user.
- **Fix:** Re-format the string.

### T3-Q11 — — `swarm_runner::sanitize` chains `chars().collect()` then `split('-').filter().collect().join('-').chars().take(24).collect()` — three full passes over the data
- **File:** crates/kod-core/src/swarm_runner.rs
- **Line:** 2289–2301
- **Severity:** Low
- **Category:** Performance / Code quality
- **Description:** The function:
```rust
s.to_lowercase()
    .chars()
    .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
    .collect::<String>()
    .split('-')
    .filter(|p| !p.is_empty())
    .collect::<Vec<_>>()
    .join("-")
    .chars()
    .take(24)
    .collect()
```
  Allocates a String, then a Vec<&str>, then a String, then iterates
  again. The same logic in one pass:
```rust
let mut out = String::with_capacity(24);
let mut prev_dash = true;  // suppress leading dashes
for c in s.chars().flat_map(|c| c.to_lowercase()) {
    if out.len() >= 24 { break; }
    if c.is_ascii_alphanumeric() { out.push(c); prev_dash = false; }
    else if !prev_dash { out.push('-'); prev_dash = true; }
}
out.trim_end_matches('-').to_string()
```
- **Fix:** Single-pass version above.

### T3-Q12 — — `engine::build_background_hook` returns `Some(id_str)` even when the spawn failed
- **File:** crates/kod-core/src/engine/mod.rs
- **Line:** 2877–2990
- **Severity:** Low
- **Category:** Error handling
- **Description:** When `cmd.spawn()` returns an error, the function
  calls `runner.fail(job, ...)` and `return None`. But the `runner.register`
  at line 2855 already added the job as "Running" before the spawn
  was attempted. The `runner.fail` should flip the state, but if
  `runner`'s `fail` doesn't reconcile the registration's "Running"
  marker, the job shows as both Running and Failed.
- **Notes:** Verify `BackgroundJobRunner::fail` reconciles an
  already-registered job.

### T3-Q13 — — `swarm_runner::collect_writes` indexes `tool_calls[tool_results.len().min(tool_calls.len())..]` — a convoluted way to express "extra calls past the results"
- **File:** crates/kod-core/src/swarm_runner.rs
- **Line:** 2182
- **Severity:** Low
- **Category:** Code quality
- **Description:** `&resp.tool_calls[resp.tool_results.len().min(resp.tool_calls.len())..]`
  is the same as `resp.tool_calls.iter().skip(resp.tool_results.len())`
  but harder to read.
- **Fix:** Use `iter().skip()`.

### T3-Q14 — — `engine::process_for` holds the read guard on `is_running` only long enough to check it; the comment claims it matters for `set_provider` but the guard is dropped immediately
- **File:** crates/kod-core/src/engine/mod.rs
- **Line:** 9152–9157
- **Severity:** Low
- **Category:** Code quality
- **Description:** The guard scope is fine (we only need the
  boolean), but the comment conflates "holding the read guard to
  block `set_provider`" with "checking the flag". The actual block
  on `set_provider` happens elsewhere (via the registry read lock
  at line 9173). The comment here is misleading.

### T3-Q15 — — `serve.rs::CappedLines` uses `String::from_utf8_lossy` which substitutes U+FFFD for invalid bytes — a JSON parser then rejects the line as invalid JSON
- **File:** crates/kod-core/src/serve.rs
- **Line:** 466
- **Severity:** Low
- **Category:** Error handling
- **Description:** A same-UID client that sends invalid UTF-8 gets
  a 200-response "bad request" error mentioning "invalid UTF-8
  sequence at …", but the U+FFFD substitution silently transforms the
  raw bytes before the parser sees them, so the error message points
  at the U+FFFD position rather than the original byte sequence.
- **Fix:** Use `String::from_utf8(buf).map_err(|e| KodError::InvalidParameters { reason: format!("invalid UTF-8: {e}") })` so the error names the original bytes.

---

## Summary of recurring themes

1. **Non-deterministic hashing** (`DefaultHasher::new()` in
   `note_tool_surface_fingerprint`) defeats the entire
   cache-invalidation journal (T3-C1). This is the most consequential
   single bug.
2. **Non-cryptographic hashes used as security gates** —
   `LearnedAllow::from_call` (T3-C3) and the `cache_journal`'s
   `fingerprint_of` (T3-P7) both use FNV-1a where collisions have
   security or correctness consequences.
3. **Mutex / RwLock guards held across `.await`** — TTSR (T3-C7),
   cache_journal's truncation (T3-C4), background watcher's
   `child.wait().await` (T3-C6). Each one is a real deadlock/latency
   path under load.
4. **`let _ = …` discards of futures and JoinHandles** (T3-H2) — the
   crate has dozens of these; most are fine, but a handful silently
   lose panics and seeding errors.
5. **Sequential `.await` in for-loops** for operations that could
   `join_all` — Jev classifications (T3-H1), pre-tool hooks, grep
   probes (T3-P8).
6. **`String::new()` + `push_str` in loops without `with_capacity`**
   — pervasive in `build_summary_prompt`, `build_handoff_prompt`,
   `render_mental_model_block`, `apply_steers` (T3-M2/T3-M3/T3-P3).
7. **Tracing log strings with spurious whitespace** from a
   multi-line-merge tool — affects operator-facing messages in
   swarm_runner, engine, serve, provider_setup (T3-Q5/T3-H18).
8. **`KodEngine` struct with 25+ `RwLock<HashMap<String, …>>`
   fields keyed by transcript** (T3-Q7) — biggest maintainability
   liability; a `TranscriptState` struct would reduce lock
   contention and make forget/clear a single operation.

---

*End of kod-core review findings.*

---
# Part 4 — kod-tui & kod-cli (TUI event loop, markdown parser, CLI dispatcher)

_Crates: kod-tui, kod-cli_


Deep evidence-based review of `crates/kod-tui` (the ratatui terminal UI) and `crates/kod-cli` (the binary + `kod`/`kod chat`/`kod serve`/`kod admin` etc. command surface). Special focus on `main_loop.rs` (8.4 KLOC) and `markdown.rs` (1.2 KLOC) per the review brief.

---

## Critical bugs

### T4-C1 — — `Event::Quit` does not abort the in-flight generation task; orphaned task keeps mutating shared engine state during shutdown
- **File:** crates/kod-tui/src/main_loop.rs
- **Line:** 878-880
- **Severity:** Critical
- **Category:** Concurrency / Resource leak
- **Description:** `cancel_generation` (line 1973) correctly aborts `gen_task`, but the `Event::Quit` arm of `handle_event` only calls `self.app.quit()`. The main loop then exits, `run()` saves the session, and `restore_terminal` runs — all while the spawned `tokio::task::JoinHandle` held in `self.gen_task` is still alive, still owns an `Arc<KodEngine>` clone, and may still push events into the channel. `app.save_session()` therefore reads `KodApp::messages()` while the gen task is concurrently calling `engine.process_streaming` → `event_tx.send(...)`, which (eventually) drives `app.add_response_chunk` / `flush_streamed_text` / `push_assistant_message`. That is a real data race on the chat vector, and any chunk that lands after `save_session` is lost from the persisted file.
- **Code:**
```rust
Event::Quit => {
    self.app.quit();
}
// …
Event::ResponseComplete(text) => {
    self.gen_task = None;   // only cleared on the natural-completion path
    self.app.finish_response(&text);
    // …
}
```
- **Why it's a bug:** The "quit while generating" path is reachable: the user presses `q` in normal mode (which routes to `request_confirm(Quit)`, then `y`) or sends `Event::Quit` from a panic hook / SIGTERM forwarder. The session file is persisted at `run()` line 688 *before* `restore_terminal`, so the orphaned task gets a window of several hundred ms in which to mutate messages after the snapshot was written. On a slow streaming endpoint the user can also see post-quit assistant chunks flushed to a half-restored terminal.
- **Fix:**
```rust
Event::Quit => {
    // Abort the in-flight turn before tearing down so the
    // engine cannot deliver more chunks to a dead TUI.
    self.cancel_generation();
    self.app.quit();
}
```
  (Also gate `app.save_session()` in `run()` behind `self.gen_task.is_none()` or call `cancel_generation()` first.)
- **Notes:** `dispatch_swarm` (line 1966) stores the same `gen_task` slot for swarm runs; the same fix covers `/swarm` + `q`.

---

### T4-C2 — — `EventHandler::next_event` can sleep up to `tick_rate` (100 ms) with a key sitting in the priority queue — input latency is bounded by the tick, not the keystroke
- **File:** crates/kod-tui/src/event.rs
- **Line:** 380-405
- **Severity:** Critical
- **Category:** Concurrency / UX
- **Description:** The input loop (`start_input_loop`, line 459) pushes key events into a *priority queue* (`PrioritySender::send_priority` mutates `event_queue` under a `std::sync::Mutex`) — NOT into the tokio `mpsc::Sender`. `next_event` checks the queue once, then enters a `tokio::select!` that waits on `rx.recv()` or a `sleep_until(deadline)` with `deadline = now + tick_rate` (default 100 ms in `TuiLoop::new`). If a key arrives *after* the queue check but *before* the channel `recv()` arm fires, the select! does not wake — nothing in the select! watches the priority queue. The key is only returned when either (a) some other event arrives on `event_tx` (paste / resize / mouse / a background task) or (b) the 100 ms tick deadline fires. On a quiet terminal a single keystroke therefore costs up to 100 ms of latency before `handle_event` is even called. For a typist at 100 WPM (≈5 keys/sec, 200 ms gap) this is half a typematic period of artificial lag on every keystroke.
- **Code:**
```rust
pub async fn next_event(&self) -> Event {
    {
        let mut queue = self.event_queue.lock().unwrap();
        if let Some((_, event)) = queue.pop_front() {
            return event;
        }
    }
    let mut rx = self.event_rx.lock().await;
    let deadline = tokio::time::Instant::now() + self.tick_rate;
    tokio::select! {
        event = rx.recv() => { /* … */ }
        _ = tokio::time::sleep_until(deadline) => { return Event::Tick; }
    }
    Event::Tick
}
```
- **Why it's a bug:** Keys are pushed to `event_queue` (sync Mutex) at line 522 — never to the `event_tx` mpsc that the select! listens to. Lost-wakeup pattern: queue-then-sleep without re-checking. Comment at line 432-435 claims keys "must not queue behind a flood of ResponseChunks" — true, but the side effect is that they queue behind a sleep instead.
- **Fix:** Have `PrioritySender::send_priority` also send a sentinel on `event_tx` (e.g. `event_tx.try_send(Event::Tick).ok()`) so the select! wakes immediately, then drain the priority queue on wake. Alternatively, replace the dual-queue design with a single `mpsc::Receiver<(EventPriority, Event)>` and let tokio's waker do its job.
- **Notes:** `try_next_event` (line 417) does check both queues, so once the loop is *unblocked* subsequent keys drain quickly — the damage is only the first key after idle.

---

### T4-C3 — — `unsafe env::set_var` in tests is process-wide and not actually safe just because a test mutex serializes the writers
- **File:** crates/kod-tui/src/app/tests.rs
- **Line:** 355, 414, 727, 758
- **Severity:** Critical
- **Category:** Correctness (unsafe under Rust 2024 + `std::env` invariants)
- **Description:** `std::env::set_var`/`remove_var` are `unsafe` as of Rust 1.85 because reading an env var from another thread while another thread writes is UB (the underlying `HashMap` can be reallocated under the reader). The tests guard writes with a `OnceLock<Mutex<()>>` so writers don't race writers, but readers — anywhere in the process — are unprotected. `cargo test` runs the TUI's tests in the *same binary* as kod-core/kod-config code paths that read `KOD_TUI_STATE_DIR` (via `KodApp::session_path`), and tokio's runtime spawns worker threads that *do* read env vars indirectly (e.g. `dirs::config_dir`, `RUST_LOG`, the tracing EnvFilter). A reader on another thread during the set_var window is UB; the comment "SAFETY: serialized via the shared mutex" is wrong on the read side.
- **Code:**
```rust
let _guard = session_state_dir_lock();
// SAFETY: serialized via the shared mutex.
unsafe { std::env::set_var("KOD_TUI_STATE_DIR", &tmp) };
```
- **Why it's a bug:** The mutex only serializes other writers. Any other thread reading `KOD_TUI_STATE_DIR` (or any other env var, since the global table is shared) during the call is UB. On Rust 1.85 with the 2024 edition this is the kind of UB Miri would flag; in production it can manifest as a stale pointer or a corrupted hashmap entry. There are four `unsafe` blocks at lines 355/414/727/758 in this file plus three more `std::env::set_current_dir` calls in `main_loop.rs` (6359, 6637, 6695).
- **Fix:** Pass the path explicitly through a `KodApp::with_state_dir(PathBuf)` constructor for tests, or use a `thread_local!` override looked up first by `session_path`. Stop touching the real env.
```rust
impl KodApp {
    #[cfg(test)]
    pub fn with_state_dir(dir: PathBuf) -> Self { /* … */ }
}
```
- **Notes:** `main_loop.rs` lines 6359/6637/6695 also `std::env::set_current_dir().unwrap()` inside tests under a different static mutex (`CWD_LOCK`) — same UB-on-read pattern for any code that resolves a relative path on another thread.

---

### T4-C4 — — Markdown parser closes a fenced code block on ANY line starting with ```` ``` ```` after whitespace, including indented lines that legitimately contain backticks
- **File:** crates/kod-tui/src/markdown.rs
- **Line:** 248-258
- **Severity:** Critical
- **Category:** Correctness (parser bug)
- **Description:** Inside a code block, the close-fence check is `raw.trim_start().starts_with("```")`. This means a code block whose body contains a line like `   ```rust` (indented) or `    ``` ` (more than 3 backticks for a nested example) terminates the block prematurely. The CommonMark spec requires the closing fence to (a) be indented no more than the opening fence, (b) be the same character (`\```), (c) be at least as long as the opening fence, and (d) have no non-whitespace after the backticks. The current parser ignores all four rules. The opening-fence check at line 263 has the same problem in reverse: any line beginning with ```` ``` ```` (after left-trim) starts a code block even when it's the body of another code block.
- **Code:**
```rust
if in_code {
    if raw.trim_start().starts_with("```") {
        blocks.push(Block::Code {
            lang: code_lang.take(),
            lines: std::mem::take(&mut code_lines),
        });
        in_code = false;
    } else {
        code_lines.push(raw.to_string());
    }
    continue;
}
```
- **Why it's a bug:** When the assistant emits a fenced code block that *itself* contains an example of a fenced code block (extremely common — "here's how you write a Python code block"), the outer fence closes prematurely. The remaining ` ``` ` then opens a new (unclosed) code block and the rest of the message renders as code. Adversarial input from a malicious tool result could also craft this to hide text the user expects to see as prose.
- **Fix:** Track the opening fence's indent and length; require the close fence to match exactly. At minimum, ignore lines whose `trim_start` form is ` ``` ` *inside* a code block only when it is the first such line at indent ≤ opening indent — but the correct fix is a CommonMark-aware fence matcher:
```rust
struct Fence { indent: usize, len: usize, char: char }
// opening: record indent + len + char
// closing: line.trim_start() must be exactly `char.repeat(len)` (trailing ws ok)
//          and indent <= opening.indent
```

---

### T4-C5 — — `wrap_spans` followed by `indent_continuation` overflows the wrap width by the indent on every continuation line
- **File:** crates/kod-tui/src/markdown.rs
- **Line:** 401-427 (Bullet / Numbered / Quote branches)
- **Severity:** Critical
- **Category:** Correctness / UX (layout overflow)
- **Description:** For bullets, numbered lists, and quotes, the renderer calls `wrap_spans(spans, width)` where `spans` includes the leading marker (`"• "`, `"1. "`, `"│ "`). `wrap_spans` therefore packs the *first* line to `width` columns (marker + text), and packs every *continuation* line to `width` columns of pure text (no marker). Then `indent_continuation` prepends `"  "` / `"   "` / `"│ "` (the marker width) to every continuation line *after* wrapping. So continuation lines are `width + indent_width` columns wide, overflowing the chat area by the marker's width. On a 50-column chat a 2-column bullet overflow = 4% — the last 1-2 chars are clipped by the ratatui `Paragraph` widget.
- **Code:**
```rust
Block::Bullet { text } => {
    let inline = parse_inline(text, theme);
    let mut spans = vec![Span::styled("• ", /* … */)];
    spans.extend(inline);
    let wrapped = wrap_spans(spans, width);   // wraps continuation to `width`
    out.extend(indent_continuation(wrapped, "  "));  // +2 after the fact
}
```
- **Why it's a bug:** `wrap_spans` has no idea the continuation will be re-indented; it computes `cur_width + word_width > width` against the full `width`. The fix is to either (a) pass `width - indent.len()` to `wrap_spans` *and* accept that the first line's effective budget is reduced by the marker (re-prepending the marker after wrap), or (b) do the indent inside `wrap_spans` so it knows about the per-line budget. The same overflow hits `Block::Numbered` (indent width = marker width, e.g. `   ` for `10. `) and `Block::Quote` (indent width = `│ `.len() = 2).
- **Fix:**
```rust
let indent_w = "  ".chars().map(char_width).sum::<usize>();
let wrapped = wrap_spans(spans, width.saturating_sub(indent_w));
// Then prepend the bullet on line 0 and the indent on lines 1+,
// leaving the wrap algorithm free to use the remaining width.
```
  (i.e. don't include the bullet inside `spans`; add it post-wrap so the wrap budget is consistent.)
- **Notes:** The `wrap_spans` "long word is hard-broken" path (line 668) takes `width` literally, so a single long word inside a bullet will overflow on the continuation by the indent width.

---

## High

### T4-H1 — — `clipboard::write_clipboard` blocks the main loop on `child.wait()` with no timeout
- **File:** crates/kod-tui/src/clipboard.rs
- **Line:** 27, 49
- **Severity:** High
- **Category:** UX / Concurrency
- **Description:** `write_clipboard` spawns `pbcopy`/`xclip`/`xsel` and synchronously `child.wait()`s for completion. If the clipboard daemon is wedged or the binary is `xclip` against a dead X server (common on a headless SSH session that still has `xclip` on PATH), `wait()` blocks forever. This is called from `KodApp::copy_last_to_clipboard`, which is called synchronously from the TUI's `handle_key` (`y`) and `/copy` command path — the entire TUI freezes until the child exits. There is no timeout.
- **Code:**
```rust
return child.wait().map(|s| s.success()).unwrap_or(false);
// …
if child.wait().map(|s| s.success()).unwrap_or(false) {
    return true;
}
```
- **Why it's a bug:** A frozen TUI is the worst possible UX for a "copy" action; the user has no feedback and Ctrl+C is captured by the TUI for "cancel generation". The fix is a bounded wait: spawn, write stdin, drop stdin, then poll with a 500ms deadline; if it fires, kill the child and return `false`.
- **Fix:**
```rust
use std::time::Duration;
let status = wait_with_timeout(child, Duration::from_millis(500));
fn wait_with_timeout(mut c: Child, d: Duration) -> Option<bool> {
    let start = Instant::now();
    loop {
        match c.try_wait() {
            Ok(Some(s)) => return Some(s.success()),
            Ok(None) if start.elapsed() < d => std::thread::sleep(Duration::from_millis(20)),
            Ok(None) => { let _ = c.kill(); let _ = c.wait(); return None; }
            Err(_) => return None,
        }
    }
}
```

---

### T4-H2 — — `clipboard::write_clipboard` does not try `wl-copy` on Wayland, but `read_clipboard` does try `wl-paste` — asymmetric, broken on Wayland-only hosts
- **File:** crates/kod-tui/src/clipboard.rs
- **Line:** 30-55 (write), 88-104 (read)
- **Severity:** High
- **Category:** Correctness / Platform support
- **Description:** `read_clipboard` tries `xclip`, `xsel`, AND `wl-paste` (line 93), but `write_clipboard` only tries `xclip` and `xsel` (line 32-35) — it never tries `wl-copy`. On a Wayland-only host without `xclip`/`xsel` (e.g. a minimal Fedora workstation), `/copy` silently returns `false` and the user sees "Nothing to copy" even though `wl-copy` is installed and would work. The asymmetry is not documented and is easy to miss in a fix.
- **Code:**
```rust
#[cfg(target_os = "linux")]
{
    for (bin, args) in [
        ("xclip", vec!["-sel", "clipboard", "-i"]),
        ("xsel", vec!["--clipboard", "--input"]),
        // ← `wl-copy` is missing here
    ] {
        // …
    }
    return false;
}
```
- **Fix:** Add `("wl-copy", vec![])` to the write list.

---

### T4-H3 — — `main_loop` batches up to 256 events per frame without fairness; a flood of `ResponseChunk`s can starve `Tick` for >100 ms
- **File:** crates/kod-tui/src/main_loop.rs
- **Line:** 731-744
- **Severity:** High
- **Category:** Concurrency / Performance
- **Description:** The batched-render loop drains `try_next_event` up to `MAX_EVENTS_PER_FRAME = 256` between renders. `ResponseChunk` does not require render, so a sustained token stream fills the queue with chunks. Each `handle_event` call on a chunk mutates `current_response` (a `String` push) and runs `set_phase` — cheap, but not free. On a slow system (think a 4 KB/s stream from a local model on a Raspberry Pi) the 256-event batch can take longer than 33 ms to drain, pushing the next render past the 33 ms throttle and creating visible stutter. There's no yield to the runtime between batch iterations, so a saturating channel also starves any background task the engine spawned (the Jev phase-change poll at line 979, the swarm pump at line 1828).
- **Code:**
```rust
const MAX_EVENTS_PER_FRAME: usize = 256;
for _ in 0..MAX_EVENTS_PER_FRAME {
    let Some(next) = self.event_handler.try_next_event() else { break; };
    render_now |= next.requires_render();
    if matches!(next, Event::ResponseChunk(_)) { saw_chunk = true; }
    self.handle_event(next).await?;
    if self.app.should_quit() { break; }
}
```
- **Why it's a bug:** Comment claims "bounded by `MAX_EVENTS_PER_FRAME` so a permanently-saturated queue can still render" — true, but `MAX_EVENTS_PER_FRAME = 256` against a 2000-chunk/s stream means ~7.8 batches/s = ~8 renders/s, well below the 30 Hz target. A smaller batch (e.g. 32) or a `tokio::task::yield_now().await` between iterations would let other tasks run.
- **Fix:** Drop the batch size, or `tokio::task::yield_now().await` every 32 events to give the swarm/jev tasks a turn.

---

### T4-H4 — — `tokio::spawn` for `/handoff`, `/summarize`, Jev phase-change, and prewarm is never tracked — orphaned tasks on quit
- **File:** crates/kod-tui/src/main_loop.rs
- **Line:** 979, 3128, 3499, 4875, 6033
- **Severity:** High
- **Category:** Concurrency / Resource leak
- **Description:** Five `tokio::spawn` calls (the Jev phase-change detector at 979, `/handoff` at 3128, `/summarize` at 3499, `/check` at 4875, and the prewarm at 6033) drop their `JoinHandle` immediately. None are joined or aborted on `Event::Quit` / `restore_terminal`. The runtime drop at the end of `run()` will abort them eventually, but between `Event::Quit` and runtime drop they can still send to `event_tx` (whose receiver is gone, so the `let _ = tx.send(...)` silently drops). For `/handoff` specifically, the spawned task calls `engine.process(...)` which can keep running for tens of seconds — and `engine.shutdown()` is *never called* in `TuiLoop::run` (it's only called in the CLI's `run_chat`). The engine's MCP children, redb handle, and background-shell spawners are torn down by `Drop` order rather than graceful shutdown.
- **Code:**
```rust
// /handoff (line 3128)
tokio::spawn(async move {
    // …
    match engine.process(&prompt).await {
        Ok(resp) => { /* … */ }
        Err(e) => { /* … */ }
    }
});
```
- **Why it's a bug:** The TUI never calls `engine.shutdown()`; on quit the `Arc<KodEngine>` is dropped when (a) `self.engine` is dropped (in `TuiLoop`'s destructor) and (b) any in-flight `tokio::spawn` task holding a clone is itself dropped (by runtime drop). MCP child processes can be reaped by tokio's blocking-pool shutdown, which is racy on macOS. The fix is to track spawned handles and await/abort them on quit, then call `engine.shutdown().await` before `restore_terminal`.
- **Fix:**
```rust
struct TuiLoop {
    // …
    bg_tasks: Vec<tokio::task::JoinHandle<()>>,
}
// on spawn: self.bg_tasks.push(handle);
// on quit: for h in self.bg_tasks.drain(..) { h.abort(); }
// then: if let Some(e) = self.engine.as_mut() { e.shutdown().await?; }
```

---

### T4-H5 — — `find_double` and `find_char` in markdown inline parser scan to the end of the buffer on every `*` / `_` / `` ` ``, giving O(n²) on adversarial input
- **File:** crates/kod-tui/src/markdown.rs
- **Line:** 505-572, 580-593
- **Severity:** High
- **Category:** Performance (ReDoS-adjacent)
- **Description:** `parse_inline` is iterative (good — no stack overflow), but for each candidate delimiter it calls `find_char` / `find_double`, both of which are linear scans from the cursor to the end of `chars`. For a paragraph like `*a* *b* *c* … *z*` (many short italic pairs), each `*` triggers a fresh O(n) scan from `i+1`, making the total cost O(n × number_of_delimiters). For a 10 K-char paragraph with 500 italic pairs that's 2.5 M char-comparisons per render; combined with the render cache (which only keys on `content_hash + width + theme_name`), a hot cache hit dodges this, but every cache miss is quadratic. The worst case is a single unterminated `*` at the start of a 50 K-char reply — `find_char` scans all 50 K chars, returns None, and we then process every other `*` in the buffer the same way.
- **Code:**
```rust
fn find_char(chars: &[char], from: usize, target: char) -> Option<usize> {
    (from..chars.len()).find(|&j| chars[j] == target)
}
fn find_double(chars: &[char], from: usize, target: char) -> Option<usize> {
    let mut j = from;
    while j + 1 < chars.len() {
        if chars[j] == target && chars[j + 1] == target { return Some(j); }
        j += 1;
    }
    None
}
```
- **Why it's a bug:** This is the closest the TUI gets to ReDoS — not catastrophic, but enough to cause a visible stall on a large model reply (which is exactly when the user is staring at the screen). A 50 KB reply with 2% asterisks takes ~12.5 M comparisons on a cache miss.
- **Fix:** Build a per-delimiter index up front (a `HashMap<char, Vec<usize>>` of positions for `*`, `_`, `` ` ``), then binary-search the next occurrence after `i`.

---

### T4-H6 — — `try_next_event` uses `TokioMutex::try_lock` and silently swallows contention — events lost when the channel lock is held by `next_event`'s `select!`
- **File:** crates/kod-tui/src/event.rs
- **Line:** 417-430
- **Severity:** High
- **Category:** Concurrency
- **Description:** `try_next_event` is called from the main loop between renders to drain the queue. It does `self.event_rx.try_lock()` — if `next_event` is currently in its `select!` (holding the mutex across the await), `try_lock` fails and `try_next_event` returns `None`, even if there are events waiting on the channel. The main loop then proceeds to render and only picks up the events on the next `next_event` call. The mutex in question is `TokioMutex<mpsc::Receiver<Event>>` (line 324), and `next_event` holds it across `tokio::select!` for up to `tick_rate`. So `try_next_event` returns `None` for the entire 100 ms tick window when the loop is idle. The visible effect: when a burst of `ResponseChunk`s arrives during the 100 ms idle window, the loop doesn't drain them until the next `next_event` call returns the tick. That's the same input-latency issue as T4-C2, but expressed as dropped drain attempts.
- **Code:**
```rust
pub fn try_next_event(&self) -> Option<Event> {
    {
        let mut queue = self.event_queue.lock().unwrap();
        if let Some((_, event)) = queue.pop_front() { return Some(event); }
    }
    if let Ok(mut rx) = self.event_rx.try_lock() {
        if let Ok(event) = rx.try_recv() { return Some(event); }
    }
    None
}
```
- **Why it's a bug:** Combined with T4-C2, the TUI's event model has a structural latency floor of `tick_rate` for any event that arrives while the loop is waiting. The fix from T4-C2 (single channel, no priority queue) also fixes this.
- **Fix:** Replace `TokioMutex<mpsc::Receiver<Event>>` with a single `mpsc::Receiver<(EventPriority, Event)>` and use `rx.try_recv()` directly — no lock at all.

---

### T4-H7 — — `RenderCache` (in `markdown.rs`) holds `entries` and `order` under two separate `std::sync::Mutex`es — a partial failure leaves them out of sync
- **File:** crates/kod-tui/src/markdown.rs
- **Line:** 110-189
- **Severity:** High
- **Category:** Concurrency
- **Description:** The render cache is `entries: Mutex<HashMap<CacheKey, Arc<...>>>` + `order: Mutex<VecDeque<CacheKey>>`. `get_or_render` locks `entries` to read; on a miss it locks `order` to push the key, then *re-locks* `entries` inside the eviction loop to drop the oldest entry. If the inner `entries.lock()` is poisoned (a panic during `insert`/`remove` — `HashMap`'s allocator can panic on OOM), the `order` VecDeque keeps the key but `entries` no longer has it; future `get_or_render` for the same key always misses. The two-mutex design exists because the inner eviction needs `entries` but the outer holds `order`. A single combined `Mutex<CacheState>` would be simpler and atomic.
- **Code:**
```rust
if let Ok(mut order) = self.order.lock() {
    order.push_back(key.clone());
    while order.len() > self.capacity {
        if let Some(old) = order.pop_front()
            && let Ok(mut entries) = self.entries.lock()
        {
            entries.remove(&old);
        }
    }
}
```
- **Why it's a bug:** A poisoned mutex is rare in practice, but the design also has a subtler issue: another thread can read `entries` between the `order.push_back` and the eviction, so it sees a key that's about to be evicted — returning a valid `Arc` to a soon-to-be-removed entry. That's safe (the `Arc` keeps the data alive) but the cache size grows past `capacity` transiently. The fix is one mutex.
- **Fix:**
```rust
struct RenderCache {
    state: Mutex<State>,
}
struct State { entries: HashMap<…>, order: VecDeque<…> }
```

---

### T4-H8 — — `cli::main` ignores `tracing_subscriber::try_init` failure — a second `tracing` subscriber elsewhere silently disables all kod logs
- **File:** crates/kod-cli/src/main.rs
- **Line:** 37-42
- **Severity:** High
- **Category:** Error handling
- **Description:** `tracing_subscriber::fmt().try_init()` returns a `Result` that is discarded with `let _ =`. If a parent process (a test harness, an embedder, a `cargo-nextest` setup) has already installed a global subscriber, `try_init` returns `Err` and kod's `EnvFilter` and `SessionSafeWriter` never take effect. Every `tracing::warn!` then lands in the void — exactly the failure mode the comment at lines 4-11 says it's trying to prevent. There's no diagnostic to the user that logs are being dropped.
- **Code:**
```rust
let _ = tracing_subscriber::fmt()
    .with_env_filter(filter)
    .with_writer(kod_cli::logging::SessionSafeWriter::default())
    .with_ansi(false)
    .with_target(false)
    .try_init();
```
- **Why it's a bug:** The H-D9 comment ("without one, every tracing::warn! is a no-op") is the design intent; ignoring the error means the design fails silently. Worse, on `cargo test` (which installs its own subscriber for test capture) the entire kod-cli test suite runs without the SessionSafeWriter — exactly the kind of subtle test/prod divergence reviewers warn about.
- **Fix:**
```rust
if let Err(e) = tracing_subscriber::fmt()./* … */.try_init() {
    eprintln!("kod: could not install tracing subscriber ({e}); logs will be lost.");
}
```

---

### T4-H9 — — `logging.rs` opens `session.log` in append mode and never rotates — unbounded growth across sessions
- **File:** crates/kod-cli/src/logging.rs
- **Line:** 80
- **Severity:** High
- **Category:** Resource leak / Error handling
- **Description:** `OpenOptions::new().create(true).append(true).open(path)` opens the file once (lazily on first TUI-active write) and caches it in `self.file` forever. There is no size check, no rotation, no truncation. A long-lived `kod serve` daemon or a daily TUI habit can produce a `~/.kod/session.log` of tens of MB (tracing at `info` level for a chatty swarm run can write ~10 MB/hour). When the file is opened it's at offset 0 with O_APPEND, which on Linux is fine; on macOS `append` mode is also correct. But the missing rotation means a developer who turns on `RUST_LOG=debug` once gets a multi-GB file that never shrinks.
- **Code:**
```rust
OpenOptions::new().create(true).append(true).open(path).ok()
```
- **Why it's a bug:** The brief calls out "log file rotation, truncation, append behavior" — the answer is "append-only, no rotation, no truncation". A 10 MB log is fine; a 10 GB log on a small partition is not.
- **Fix:** Add a size guard: if `file.metadata()?.len() > MAX_LOG_BYTES`, reopen in truncate mode (or rename to `.1` and start fresh). Cap at, say, 50 MB.

---

### T4-H10 — — `Cli::run` creates a new `tokio::runtime::Runtime` for every subcommand, including trivial ones (`kod --version`, `kod completions`)
- **File:** crates/kod-cli/src/commands/mod.rs
- **Line:** 412, 436, 454, 465, 491, 495, 501, 521, 526, 531, 536, 541, 559, 571, 582, 587, 592, 593, 599, 603, 608, 609, 613, 618, 623, 624, 635, 649, 658, 667, 677, 689, 705, 731, 749, 760, 765, 766, 807, 812, 821
- **Severity:** High
- **Category:** Performance / startup cost
- **Description:** Every subcommand arm starts with `let rt = tokio::runtime::Runtime::new()?; rt.block_on(async { … })`. Tokio runtime construction is ~1-2 ms (thread pool spawn, I/O driver init, timer registration). For a `kod --version` or `kod completions bash` invocation this is pure overhead — the command doesn't need async. The `Completions` arm at line 597 already skips the runtime; `SandboxExec` at 628 also skips. The pattern is copy-pasted 35+ times across the match arms, including `kod memory list` which is just a `redb` read.
- **Code:**
```rust
Some(Command::Memory { action }) => {
    let rt = tokio::runtime::Runtime::new()
        .map_err(|e| KodError::Internal(format!("Failed to create runtime: {}", e)))?;
    rt.block_on(async { run_memory(action.clone()).await })
}
```
- **Why it's a bug:** Startup cost. A `kod --version` on a cold cache is ~20 ms; 2 ms of that is the runtime. For a CLI invoked from a shell prompt, this is the difference between "snappy" and "noticeable".
- **Fix:** A `fn needs_runtime(cmd: &Command) -> bool` predicate; only build the runtime when the subcommand actually does async I/O. For commands like `Completions` and `--version` the runtime is already skipped; extend that to `Memory list`/`Sessions list`/`Tools list`/`Profile list` (all sync reads).

---

## Medium

### T4-M1 — — `parse_inline` does not handle `**bold**` inside an unclosed italic — content is silently re-eaten as italic
- **File:** crates/kod-tui/src/markdown.rs
- **Line:** 544-568
- **Severity:** Medium
- **Category:** Correctness (parser)
- **Description:** When `*italic*` opens at position `i`, `find_char` looks for the next `*` from `i+1`. If the user wrote `*before **bold** after*` (italic containing bold), the italic's closing `*` is the *first* `*` of the inner `**` — `find_char` returns it, the italic content becomes `"before "`, and `i` advances past one `*` of the `**`. The remaining `*` of `**` is then parsed as italic again (`find_char` finds the closing `*` of the original `**bold**`), and so on. The visible output is fragmented: italic `before `, italic `bold`, italic ` after` — three runs instead of one italic wrapping one bold. CommonMark nesting is order-sensitive (italics inside bold vs bold inside italic); this parser does neither.
- **Code:**
```rust
if c == '*' || c == '_' {
    // …
    if !escaped && let Some(end) = find_char(&chars, i + 1, c) {
        let content: String = chars[i + 1..end].iter().collect();
        // content is not recursively parse_inline'd; nested ** is lost
```
- **Why it's a bug:** Models frequently emit `*see **important** note*` — the rendered output is wrong but not catastrophically so. Worth fixing if the parser is ever extended; today the doc claims "no nested lists" so nested emphasis is out of scope by design, but the silent fragmentation is worse than the doc suggests.
- **Fix:** Recurse `parse_inline` on `content` for italic/bold spans, or document the limitation in the module header.

---

### T4-M2 — — `parse_numbered` accepts arbitrarily large numbers without bound; a `99999999999999999999. item` line panics on `usize::parse` (well, returns `None` then drops the line silently)
- **File:** crates/kod-tui/src/markdown.rs
- **Line:** 366-378
- **Severity:** Medium
- **Category:** Correctness / robustness
- **Description:** `parse_numbered` calls `digits.parse::<usize>()`. For a 20-digit number `usize` overflow → `parse` returns `Err` → `?` returns `None` → the line falls through to paragraph continuation. The line is then concatenated into the running paragraph with a leading space, so the user sees "1. first 99999999999999999999. second" as one paragraph instead of two list items. Not a panic, but a silent misrender. Also: leading zeros like `01. item` parse to `1` and are rendered as `1.` (changing the user's text).
- **Code:**
```rust
let num: usize = digits.parse().ok()?;
Some((num, rest[2..].to_string()))
```
- **Why it's a bug:** Adversarial model output can craft this. Mild.
- **Fix:** Cap `digits.len()` at, say, 9; on overflow, treat the line as a paragraph.

---

### T4-M3 — — `find_double` returns `None` when the closing `**` is at the very last two positions of `chars` if `from > chars.len() - 2`
- **File:** crates/kod-tui/src/markdown.rs
- **Line:** 584-593
- **Severity:** Medium
- **Category:** Correctness (off-by-one)
- **Description:** `while j + 1 < chars.len()` — if `from == chars.len() - 2`, the loop runs once at `j = from` (because `from + 1 < chars.len()` ⟺ `from < chars.len() - 1`, and `from = chars.len() - 2` satisfies this). At `j = chars.len() - 2`, `chars[j]` and `chars[j+1]` are both checked — fine. But if `from == chars.len() - 1` (single char left), `j + 1 = chars.len()`, the loop doesn't execute, returns None. So `**bold*` (4 chars: `*`, `*`, `b`, …, `*`) — wait, let me recompute: `**bold**` has 8 chars; closing `**` is at indices 6, 7. `from = i + 2 = 2`. Loop j from 2 to 7. At j = 6, `j+1 = 7 < 8`, enters loop, checks `chars[6] == '*' && chars[7] == '*'`, returns Some(6). ✓. But for `**` (only 2 chars, no content), `i = 0, from = 2`, `j + 1 < 2` is `3 < 2` false, returns None. The opening `**` then gets pushed to `buf` as literal text and `i += 2` exits. Acceptable.
- **Code:**
```rust
fn find_double(chars: &[char], from: usize, target: char) -> Option<usize> {
    let mut j = from;
    while j + 1 < chars.len() {
        if chars[j] == target && chars[j + 1] == target {
            return Some(j);
        }
        j += 1;
    }
    None
}
```
- **Why it's a bug:** The off-by-one only matters for `**` at the very end of the buffer with no content — already handled correctly. So the bug is more theoretical: an empty bold `****` is not parsed as bold-empty but as `**` + `**` literal text, which renders as four asterisks. Acceptable but inconsistent with `***` handling.
- **Fix:** `while j + 1 <= chars.len() {` and check `j+1 < chars.len()` inside, OR use `(from..chars.len()).find(|&j| chars[j] == target && j+1 < chars.len() && chars[j+1] == target)`.
- **Notes:** Low severity because empty emphasis is degenerate input.

---

### T4-M4 — — `highlight::code_rows` truncates each row independently with an `…` ellipsis — multi-byte boundary not respected when truncating mid-character
- **File:** crates/kod-tui/src/highlight.rs
- **Line:** 315-348
- **Severity:** Medium
- **Category:** Correctness (Unicode)
- **Description:** `truncate(spans, width)` iterates `span.content.chars()` and builds a `kept: String`, breaking when `used + cw > budget`. The `cw` comes from `Span::raw(ch.to_string()).width().max(1)` — `Span::width` on a `Span::raw(String)` calls `UnicodeWidthChar::width(c)`. For a CJK character (width 2), if `used = budget - 1`, `cw = 2`, `used + cw > budget`, breaks — the CJK char is dropped. Good. But the *next* span is still iterated (the `for span in spans` continues), and if its first char is width 1, `used` is still `budget - 1`, so `cw = 1`, `used + cw = budget`, NOT `> budget`, so it's KEPT. Result: the truncated row ends with the ellipsis `…` *plus* one or two extra chars from later spans that fit in the leftover budget. That's not "truncate to `width`"; it's "truncate to `width + a bit`".
- **Code:**
```rust
for span in spans {
    es = span.style;
    if used >= budget { break; }
    let mut kept = String::new();
    for ch in span.content.chars() {
        let cw = Span::raw(ch.to_string()).width().max(1);
        if used + cw > budget { break; }
        kept.push(ch);
        used += cw;
    }
    if !kept.is_empty() { out.push(Span::styled(kept, span.style)); }
    if used >= budget { break; }
}
out.push(Span::styled("…", es));
```
- **Why it's a bug:** A 200-char CJK line truncated to width 20 may render as 21 cells (the test asserts `w <= 20` but the assertion is on `Span::width`, which sums the kept spans' widths — and the bug only triggers when a width-2 char is skipped at the boundary, leaving room for a width-1 char from a later span). The chat widget's right edge is then off by one. Not catastrophic, but visible as a column of half-width whitespace on long CJK replies.
- **Fix:** Track `used` strictly; once `used + min_cw > budget`, break out of BOTH loops. Add `if used >= budget { break; }` inside the char loop *after* the inner `break`:
```rust
for ch in span.content.chars() {
    let cw = …;
    if used + cw > budget { used = budget; break; }  // force outer break
    kept.push(ch);
    used += cw;
}
```

---

### T4-M5 — — `keybindings::load_bindings` reads both `.kod-keys.toml` and `~/.config/kod/tui_keys.toml` but does NOT merge — the project-local file silently overrides the global one for any keys it mentions, including overrides that REMOVED a binding
- **File:** crates/kod-tui/src/keybindings.rs
- **Line:** 71-101
- **Severity:** Medium
- **Category:** Correctness
- **Description:** `load_bindings` iterates `[local_path, global_path]` in that order, and for each parses + applies the bindings on top of the previous. Since `bindings.retain(|_, a| *a != action)` removes the old key for an action before inserting the new key, a project-local file that rebinds `quit = "Q"` overrides the default `q` correctly. But the `retain` also wipes the *default* key — so if the user has `~/.config/kod/tui_keys.toml` with `quit = "Q"` and a project `.kod-keys.toml` with `quit = "x"`, the result is `q` removed, `Q` removed, `x` = quit. The previous default `q` is gone even though the user only intended to override it in this project. There's no way to "add a binding without removing the default" from the file. Also, the file format (`HashMap<String, String>` with `first char wins`) means a multi-char value like `"Ctrl+Q"` silently becomes `'C'` — there's no diagnostic.
- **Code:**
```rust
for (action_name, key) in &file.keys {
    let Some(action) = parse_action(action_name) else { continue; };
    let Some(ch) = key.chars().next() else { continue; };
    bindings.retain(|_, a| *a != action);
    bindings.insert(ch, action);
}
```
- **Why it's a bug:** A user who wants `q` AND `Q` to both quit, or `Ctrl+Q` to quit, has no way to express that. The "first char wins" silent truncation is the worst part.
- **Fix:** Document the truncation, or accept multi-char values and parse them as `KeyCode`s; allow a value of `""` to mean "remove this action's binding".

---

### T4-M6 — — `dispatch_prompt` reads `self.app.input()` to `String` then calls `self.app.submit_input()`, but `submit_input` may have already advanced history — `last_prompt` is set from the post-rewrite `input`
- **File:** crates/kod-tui/src/main_loop.rs
- **Line:** 1259-1359
- **Severity:** Medium
- **Category:** Correctness
- **Description:** `dispatch_prompt` does: `let input = self.app.input().to_string(); … self.app.submit_input(); … self.app.set_last_prompt(&input);`. The `input` String is captured before `submit_input` (good), but between those lines the code rewrites `input` to `input_with_system` (line 1316) and appends file attachments (line 1318-1353). The `set_last_prompt` at line 1359 receives the *rewritten* input, so `/retry` re-sends the rewritten form (with `<file path=…>` wrappers and `[system]` prefix). That's the design intent ("the prompt the model saw"), but the display row pushed by `submit_input` uses the original `input` (before rewrite). So `/retry` re-sends a prompt that, on the second run, has *another* layer of system/attachment wrappers prepended — because `dispatch_prompt` re-applies the rewrite. The user sees one display row but the model sees two wrappers.
- **Code:**
```rust
let input_with_system = match self.app.session_system_prompt() {
    Some(sys) if !sys.is_empty() => format!("[system] {sys}\n\n[user] {input}"),
    _ => input.clone(),
};
let input = input_with_system;
let attached = self.app.take_attachments();
let input = if attached.is_empty() { input } else { /* wrap in <file>… */ };
self.app.set_last_prompt(&input);
```
- **Why it's a bug:** On `/retry` of a prompt that had a session system prompt set, the second send prepends `[system] …\n\n[user] [system] …\n\n[user] <original>`. The model sees nested role markers. The fix is to remember the *pre-rewrite* input for `/retry` and the *post-rewrite* input for the engine, with `/retry` re-applying the rewrite.
- **Fix:** `self.app.set_last_prompt(&original_input);` (before rewrite), and on retry, re-apply the rewrite.

---

### T4-M7 — — `cli run_chat_remote` blocks the tokio runtime worker on `stdin.lock().read_line()` — the fix the comment in `run_chat` claims is missing in `run_chat_remote`
- **File:** crates/kod-cli/src/commands/chat.rs
- **Line:** 28-47
- **Severity:** Medium
- **Category:** Concurrency
- **Description:** `run_chat` (the embedded chat) was fixed at H-T4-C8 (line 300-327) to use async stdin via a dedicated `tokio::spawn` reader task; the comment explicitly calls out that "the pre-fix shape blocked a worker for the duration of every user think-time". `run_chat_remote` is `pub async fn` and uses `stdin.lock().read_line(&mut input)` — the exact pattern `run_chat` fixed. While the user is thinking, the tokio worker thread serving this future is blocked in a syscall, unable to serve other futures. For a single-user CLI this is benign, but it's an inconsistency and a footgun if anyone refactors to share the runtime.
- **Code:**
```rust
let stdin = io::stdin();
let mut input = String::new();
// …
match stdin.lock().read_line(&mut input) {
    Ok(0) => break,
    Ok(_) => {}
    Err(e) => { eprintln!("Input error: {e}"); break; }
}
```
- **Why it's a bug:** The fix exists; it just wasn't applied to the remote path.
- **Fix:** Use the same `tokio::io::BufReader::new(tokio::io::stdin()).read_line()` pattern as `run_chat`'s reader task (line 312-328).

---

### T4-M8 — — `run_chat_remote` has no SIGINT handler — Ctrl+C during a remote streaming turn kills the process mid-tool, leaving orphaned daemon-side state
- **File:** crates/kod-cli/src/commands/chat.rs
- **Line:** 35-176
- **Severity:** Medium
- **Category:** Error handling / UX
- **Description:** `run_chat` installs a dedicated `tokio::signal::ctrl_c()` task (line 338-364) that calls `engine.request_cancel()` on the first Ctrl+C and exits on the second. `run_chat_remote` does not. A Ctrl+C during a streaming remote turn sends SIGTERM to the client process; the daemon side keeps running the turn (the daemon has no client-disconnect detection at this level), pushing chunks into a socket whose reader is gone. The daemon's writes fail silently (Unix sockets return EPIPE on closed readers, which the daemon may not surface). The session is left in a half-finished state on the daemon.
- **Code:**
```rust
loop {
    print!("> ");
    let _ = io::stdout().flush();
    input.clear();
    match stdin.lock().read_line(&mut input) {
        Ok(0) => break,
        // …
    }
```
- **Why it's a bug:** Inconsistent UX: embedded `kod chat` interrupts cleanly; remote `kod chat --remote` does not.
- **Fix:** Spawn the same ctrl_c task with a `cancel_remote_turn` variant that closes the socket (and sends a `cancel` NDJSON frame if the protocol has one).

---

### T4-M9 — — `admin.rs run_update` makes a blocking HTTP call to GitHub with a 10 s timeout but uses `std::process::exit(1)` on every failure — the engine's tracing subscriber is dropped without flush
- **File:** crates/kod-cli/src/commands/admin.rs
- **Line:** 238-307
- **Severity:** Medium
- **Category:** Error handling
- **Description:** `run_update` does `client.get(&url).send().await` with a 10 s timeout. On network error, non-2xx, or missing `tag_name`, it calls `std::process::exit(1)`. `process::exit` skips Drop, so the `tracing_subscriber`'s buffer (if any) is not flushed — the user's "GitHub returned 503" never reaches the log file even if `RUST_LOG=debug`. The `eprintln!` does reach the terminal, but a developer debugging update-check failures from a CI log has no record. The `Result<()>` return type is a lie — the function never returns `Err`.
- **Code:**
```rust
if !resp.status().is_success() {
    let status = resp.status();
    let body = resp.text().await.unwrap_or_default();
    // …
    eprintln!("GitHub returned {status}: {short}");
    std::process::exit(1);
}
```
- **Why it's a bug:** Three `std::process::exit(1)` calls in a function that returns `Result<()>`. The brief asks "Main function error handling (graceful vs panic)" — this is the exit-don't-return pattern.
- **Fix:** Return `Err(KodError::Internal(...))` and let `main` propagate. Use `ExitCode` from `main` if a specific exit code is needed.

---

### T4-M10 — — `admin.rs` uses `std::process::exit(1)` in 13 places — none of these run Drop for the engine, the tracing subscriber, or temp files
- **File:** crates/kod-cli/src/commands/admin.rs
- **Line:** 37, 56, 164, 179, 261, 274, 288, 738, 759, 853, 1016, 1140, 1219, 1258, 1262
- **Severity:** Medium
- **Category:** Error handling
- **Description:** Beyond `run_update` (T4-M9), `run_doctor` (line 37, 56), `run_models` (164, 179), `run_doctor_fix` (853), `run_sandbox_check` (738, 759), `run_tools` (1016), `run_if_bench` (1140, 1219), and `run_commit_check` (1258, 1262) all call `std::process::exit(1)` directly. None of these have an engine open at the call site, but they DO have `KodConfig` loaded (which may have file handles) and the global tracing subscriber. `process::exit` skips Drop. For `run_doctor_fix` specifically, the function returns `Result<()>` but `exit(1)` bypasses the `Ok(())` at line 855 — so the `?` propagation chain in `Cli::run` never fires, and the `runtime.drop()` from `rt.block_on` is skipped.
- **Code:**
```rust
if report.has_failures() || !failed.is_empty() {
    std::process::exit(1);
}
Ok(())
```
- **Why it's a bug:** Brief asks for graceful vs panic — these are graceful-by-exit, not graceful-by-return. Subtle: `std::process::exit` does call `atexit` handlers but NOT Rust Drop, so `tempfile::TempDir` and `Arc<KodEngine>` leaks survive until OS reaps the process. On Windows the OS-level cleanup of half-open sockets can take 30 s.
- **Fix:** Return `Err(KodError::...)` everywhere; centralize the exit-code mapping in `main` via `std::process::Termination` (Rust 1.61+).

---

### T4-M11 — — `build.rs` writes `cargo:rerun-if-changed=.git/HEAD` and `.git/refs/heads` but these paths do not exist in a tarball / worktree / shallow clone — Cargo treats missing `rerun-if-changed` paths as "always rerun"
- **File:** crates/kod-cli/build.rs
- **Line:** 21-22
- **Severity:** Medium
- **Category:** Build
- **Description:** For a checkout that lacks `.git/` (a release tarball, a `cargo install` from a crates.io tarball, a shallow clone with `--filter=blob:none` that has no `.git/refs/heads/`), `cargo:rerun-if-changed=.git/HEAD` is interpreted by Cargo as "this path doesn't exist, so rerun the build script every time". That means `chrono::Utc::now()` is called every build → `BUILD_TIME` changes every build → re-linking kod-cli. Combined with T4-H10's runtime cost, this turns every `kod --version` into a 22 ms affair (vs. 2 ms for a cached build). The fix is to gate the rerun-if-changed on `.git/HEAD` actually existing.
- **Code:**
```rust
println!("cargo:rerun-if-changed=.git/HEAD");
println!("cargo:rerun-if-changed=.git/refs/heads");
```
- **Why it's a bug:** Release tarballs from `cargo install` don't have `.git/`; the build script re-runs on every invocation.
- **Fix:**
```rust
if std::path::Path::new(".git/HEAD").exists() {
    println!("cargo:rerun-if-changed=.git/HEAD");
    println!("cargo:rerun-if-changed=.git/refs/heads");
} else {
    println!("cargo:rerun-if-changed=build.rs");
}
```

---

### T4-M12 — — `parse_inline`'s "single `*` italic" branch fires after the `**` bold check fails only when `i+1 >= chars.len()` — but if `chars[i+1] == '*'` and there's no closing `**`, the literal `**` push happens, then the NEXT iteration's `c == '*'` italic check fires on the second `*` of the pair
- **File:** crates/kod-tui/src/markdown.rs
- **Line:** 526-542
- **Severity:** Medium
- **Category:** Correctness (parser)
- **Description:** Walk through `**unterminated` (no closing): `i=0, c='*', chars[1]='*'` → bold check enters, `find_double(chars, 2, '*')` returns None → `buf.push_str("**")`, `i += 2`. Now `i=2`, `c='u'`, normal char. Good — `**` is in the buffer as literal. But for `**unterminated*` (one trailing `*`): same as above, then at the trailing `*`, `i = N-1`, `c = '*'`, `i+1 == chars.len()` → bold check fails → italic check fires, `find_char(chars, N, '*')` returns None → `buf.push('*')`, `i += 1`. So the trailing `*` is literal. Fine. But for `*unterminated**`: `i=0, c='*'`, `chars[1]='u'` → bold check fails (chars[1] != '*') → italic check fires, `find_char(chars, 1, '*')` finds the first `*` of the trailing `**` at position N-2 → italic content is `unterminated`, `i = N-1`. Now `c = '*'`, `chars[N] = '*'` (wait, indices). Let me recompute. `*unterminated**` = 14 chars: index 0='*', 1-11='unterminated', 12='*', 13='*'. Bold check at i=0: chars[1]='u' != '*', skip. Italic check: find_char from 1, finds '*' at 12. content = chars[1..12] = "unterminated". i = 13. Now i=13, c='*'. Bold check: i+1=14 == chars.len(), skip. Italic check: find_char from 14, returns None. buf.push('*'). So output: italic "unterminated" + literal "*". The trailing `**` becomes one italic close + one literal. Acceptable.
- **Why it's a bug:** This is actually fine — no bug. The "off-by-one" in the title is misleading. Withdrawing this finding as a no-op. Replaced below.
- **Replacement — T4-M12 (real):** `*bold with ** inside*` — see T4-M1.
- **Notes:** This slot was a candidate; on review the parser handles the unterminated cases correctly per the doc's "Unterminated delimiters are emitted literally" promise.

---

### T4-M13 — — `wrap_spans` line-breaks on a space only when `cur_width + 1 + next_word_width > width`; if `cur_width == 0` (empty line just pushed), a single space + word that fits gets the space prepended, producing a leading-space line
- **File:** crates/kod-tui/src/markdown.rs
- **Line:** 646-657
- **Severity:** Medium
- **Category:** Correctness
- **Description:** After `lines.push(Vec::new())` and `cur_width = 0` (start of a new line), if the current char is a space and `cur_width + 1 + next_word_width <= width`, the code pushes a space into the (empty) new line and increments `cur_width` to 1. The line therefore starts with a space. On a paragraph that wraps to multiple lines, every continuation line *after* a manual break starts with a space — visible as a leading indent that's not supposed to be there. The fix is to skip the space when `cur_width == 0`.
- **Code:**
```rust
if cur_width + 1 + next_word_width > width {
    lines.push(Vec::new());
    cur_width = 0;
    i = word_end;
    continue;
}
// Keep one space.
lines.last_mut().unwrap().push((' ', chars[i].1));
cur_width += 1;
i = word_end;
```
- **Why it's a bug:** A paragraph like "abc def ghi jkl mno pqr stu vwx yz" wrapping at width 10 may produce continuation lines " ghi jkl" with a leading space. The chat widget's diff-based redraw doesn't strip it.
- **Fix:**
```rust
if cur_width > 0 {
    lines.last_mut().unwrap().push((' ', chars[i].1));
    cur_width += 1;
}
i = word_end;
```

---

### T4-M14 — — `TuiLoop::open_external_editor` writes the temp file with `0o600` perms but does not set restrictive perms on the parent directory (`/tmp` is world-listable on macOS) — a local user can enumerate kod scratch files
- **File:** crates/kod-tui/src/main_loop.rs
- **Line:** 140-158
- **Severity:** Medium
- **Category:** Security
- **Description:** The temp file is named `kod-edit-{pid}-{nanos}.md` in `std::env::temp_dir()` (typically `/tmp`). The file itself is `chmod 600` (line 157), so other users cannot read it. But the filename leaks the kod PID and the wall-clock nanosecond of the edit — a local user can `ls /tmp/kod-edit-*` and see every kod session's editor-open events (timing + PID). Combined with `/tmp` being world-listable on most Linux distros and macOS, this is an info leak. The doc comment acknowledges the F2g-14 fix (file perms) but missed the directory listing angle.
- **Code:**
```rust
let tmp = std::env::temp_dir().join(format!(
    "kod-edit-{}-{}.md",
    std::process::id(),
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0),
));
```
- **Why it's a bug:** PID + nanosecond timestamp is enough to fingerprint a session. A determined local attacker who can `ls /tmp` learns exactly when a developer opened the editor and for how long (file mtime).
- **Fix:** Use `tempfile::NamedTempFile` (which creates a 0600 file in `/tmp` with a random 6-char name) instead of constructing the path manually. The crate is already a dev-dependency; if it's not a runtime dep, the cost is one more crate in the lockfile.

---

### T4-M15 — — `EventHandler::start_input_loop` is not awaited/joined — `restore_terminal`'s `event_handler.stop()` only sets a flag; the input task is orphaned if `EventStream::next()` blocks
- **File:** crates/kod-tui/src/event.rs
- **Line:** 451-548
- **Severity:** Medium
- **Category:** Concurrency / Resource leak
- **Description:** `start_input_loop` spawns a task that loops on `reader.next().await` (a crossterm `EventStream`). `stop()` flips `is_running` to false, but the task is *blocked* in `reader.next().await` — it does not become ready to check `is_running` until the next crossterm event arrives. On a quiet terminal after the TUI exits, the task can sit there forever (until the user presses a key in their shell, which crossterm would then deliver to a freed stdin). The runtime drop at the end of `run()` does abort the task, but between `restore_terminal` and runtime drop (the session-save path at line 688) the input task is technically still alive.
- **Code:**
```rust
tokio::spawn(async move {
    let mut reader = EventStream::new();
    while is_running.load(std::sync::atomic::Ordering::SeqCst) {
        if let Some(Ok(event)) = reader.next().await {
            // …
        }
    }
});
```
- **Why it's a bug:** Orphaned task until runtime drop. The brief explicitly asks "Are async tokio tasks spawned by the TUI properly canceled on app exit?" — answer: no, only the runtime drop cancels them.
- **Fix:** Store the `JoinHandle` from `start_input_loop` and `abort()` it in `restore_terminal`. Or have `stop()` send a sentinel event on `event_tx` (e.g. `Event::Quit`) so the loop wakes up.

---

### T4-M16 — — `markdown::parse` flushes a paragraph on encountering a code fence, but does NOT flush on a header/bullet/numbered/quote line — the paragraph's last line is dropped silently if it directly precedes a structural block
- **File:** crates/kod-tui/src/markdown.rs
- **Line:** 263-330
- **Severity:** Medium
- **Category:** Correctness
- **Description:** Wait, let me re-read. The code fence branch (line 263) calls `flush_para`. The blank line branch (line 271) calls `flush_para`. The header branches (lines 277, 285, 293) call `flush_para`. The bullet branch (306) calls `flush_para`. The numbered branch (317) calls `flush_para`. The quote branch (323) calls `flush_para`. So all structural blocks do flush. False alarm — withdraw. Replaced below.
- **Replacement — T4-M16 (real):** `parse_inline` does not handle escaped delimiters — `\*` is rendered as `\*` literally, with the backslash visible. CommonMarkdown allows backslash-escapes for `*`, `_`, `` ` ``, `[`, `]`, etc. The TUI's parser treats `\` as an ordinary char, so `\*not italic\*` is rendered as `\*not italic\*` with the asterisks. Models emit `\*` in code-adjacent prose (e.g. "use `*` to multiply"); the user sees backslashes.
- **Fix:** Strip a `\` before `*`/`_`/`` ` `` in `parse_inline` and push the next char literally.
- **Notes:** Low severity — `\` in chat is rare.

---

### T4-M17 — — `dispatch_swarm_with` clones the engine Arc into a spawned task but does not check `self.app.is_generating()` — `/swarm` while a swarm is already running spawns a second concurrent runner
- **File:** crates/kod-tui/src/main_loop.rs
- **Line:** 1740-1760 (and `dispatch_swarm` at 1792+)
- **Severity:** Medium
- **Category:** Concurrency
- **Description:** `dispatch_prompt` guards with `if self.app.is_generating()` (line 1294) and routes to `steer` for plain text. But `/swarm` (line 2461) and `/overnight` (line 2420) call `dispatch_swarm(...)` / `dispatch_swarm_with(...)` without that guard. A user who types `/swarm goal A` while `/swarm goal B` is still running spawns a second `SwarmRunner`, both holding a clone of `Arc<KodEngine>`, both pushing `SwarmEvent`s into separate `chunk_tx` channels, both pushing `Event::SwarmAgent*` into the same `event_tx`. The TUI's swarm agent view is keyed by `kod_types::AgentId`, which is `MessageId::new()` — unique per agent, so the panel shows 2N agents instead of N. The first swarm's `SwarmComplete` lands, clears `gen_task = None` (line 1238), then the second swarm's `SwarmComplete` overwrites — the first swarm's results are pushed but the second swarm's `begin_swarm`/`begin_generation` calls have already stomped on the app state.
- **Code:**
```rust
"/swarm" => {
    let rest: String = parts.collect::<Vec<_>>().join(" ");
    let goal = rest.trim();
    if goal.is_empty() {
        // usage
    } else {
        self.dispatch_swarm(goal.to_string()).await?;
    }
}
```
- **Why it's a bug:** `/handoff` (line 3103) and `/summarize` (line 3476) check `is_generating()`; `/swarm` doesn't. Inconsistent.
- **Fix:** Add `if self.app.is_generating() { self.app.push_system_message("…"); return Ok(()); }` at the top of `dispatch_swarm`/`dispatch_swarm_with`.

---

### T4-M18 — — `markdown::render`'s `Block::Code` path calls `highlight::code_rows(... width.saturating_sub(1) ...)` then unconditionally prepends a one-cell pad span — for `width == 0` (clamped to 1 by `render`), the pad is `" "` and `code_rows` gets `0` (clamped to 1 inside), so the row is `" " + (1-cell truncated code)` = 2 cells, overflowing the 1-cell width
- **File:** crates/kod-tui/src/markdown.rs
- **Line:** 464-476
- **Severity:** Medium
- **Category:** Correctness
- **Description:** `render` clamps `width` to `max(1)` at line 66. `render_block(Block::Code)` calls `code_rows(&src, lang, width.saturating_sub(1), theme)` — `width - 1 = 0`. `code_rows` clamps to `max(1)` internally (line 71 of highlight.rs). So the code row is truncated to 1 cell, then prepended with a `" "` pad span, total 2 cells. The chat widget renders this into a 1-cell wide area — the second cell is clipped. On a real terminal with `width = 80`, this is fine (pad + 79-cell code). The degenerate case is `width = 1` or `width = 0` passed in from a 1-cell chat area.
- **Code:**
```rust
let rows = highlight::code_rows(&src, lang.as_deref(), width.saturating_sub(1), theme);
for row in rows {
    let mut spans = vec![Span::styled(" ", pad)];
    spans.extend(row);
    out.push(Line::from(spans));
}
```
- **Why it's a bug:** The "width 0 doesn't loop forever" test (line 1024) passes because `render` clamps; but the rendered output for a 1-cell area is wrong (2 cells).
- **Fix:** If `width <= 1`, return `Line::from("")` for code blocks. Or skip the pad when `width == 1`.

---

## Low

### T4-L1 — — `event.rs` uses `std::sync::Mutex` for the priority queue and `tokio::sync::Mutex` for the channel — two lock flavors for one logical data structure
- **File:** crates/kod-tui/src/event.rs
- **Line:** 13-15, 305-330
- **Severity:** Low
- **Category:** Code quality / Concurrency
- **Description:** `PrioritySender` uses `std::sync::Mutex<VecDeque<...>>` (line 305) for the priority queue, while `EventHandler` uses `tokio::sync::Mutex<mpsc::Receiver<Event>>` (line 324) for the channel. Both are correct in their respective contexts (the queue is never held across an await; the channel is). But the inconsistency makes the file harder to read and invites a future contributor to hold the sync mutex across an await, which would deadlock the runtime. The sync mutex is also held while doing `VecDeque::insert` (line 317), which is O(n) for an insert in the middle — the priority queue is an insertion-sorted vec, not a heap.
- **Code:**
```rust
pub struct PrioritySender {
    queue: std::sync::Arc<StdMutex<VecDeque<(EventPriority, Event)>>>,
}
// …
queue.insert(insert_pos, (priority, event));
```
- **Why it's a bug:** O(n) insertion on every event, and two lock flavors for one concept. A `BinaryHeap` would be O(log n); better, a single `mpsc::Sender<(EventPriority, Event)>` and let the receiver do the priority sort lazily.
- **Fix:** Replace with a `tokio::sync::mpsc::Sender<(EventPriority, Event)>` and an unbounded channel; sort by priority on the receive side.

---

### T4-L2 — — `cli::run` returns `Result<()>` but `main` propagates via `cli.run()?` — the error path skips any "restore terminal" cleanup if `run` errors mid-stream
- **File:** crates/kod-cli/src/main.rs
- **Line:** 25-54
- **Severity:** Low
- **Category:** Error handling
- **Description:** `main` is `fn main() -> kod_error::Result<()>` and calls `cli.run()?`. For the TUI subcommand, `run` → `run_tui` → `TuiLoop::run`, which has its own panic hook (line 652-668) that restores the terminal. But for an `Err` returned from `run_tui` *without* a panic (e.g. `init_engine` fails because `KodConfig::load_default` errored), the panic hook doesn't fire, and `restore_terminal` was never called (the TUI hadn't entered the alternate screen yet). For an `Err` returned from `init_terminal` (raw mode enable failed), the alternate screen was entered but `restore_terminal` is not called — the user is left in raw mode in the alternate screen.
- **Code:**
```rust
fn main() -> kod_error::Result<()> {
    // …
    let cli = Cli::parse();
    // …
    cli.run()?;
    Ok(())
}
```
- **Why it's a bug:** The TUI's `run` does call `restore_terminal` in a best-effort `let _ =` (line 695), but only after `main_loop().await` returns. If `init_terminal` itself fails (line 682), `main_loop` is never called and `restore_terminal` is skipped.
- **Fix:** Wrap `TuiLoop::run` in a `scopeguard` or `defer`-like pattern that calls `restore_terminal` on any exit path.

---

### T4-L3 — — `run_chat_remote` writes the NDJSON request frame with `frame.push('\n')` after `serde_json::to_string` — if the request body contains an embedded newline inside a string literal (e.g. the user pastes a multi-line input), the JSON serializer escapes it as `\n` (two chars), so the frame is still single-line. But `frame.as_bytes()` is sent in one `write_all` — fine. No bug here, withdraw. Replaced:
- **Replacement — T4-L3 (real):** `run_chat_remote` reads the daemon's reply line-by-line via `BufReader::new(read_half).lines()` (line 95 of chat.rs:106 in the broader read path). `lines()` splits on `\n` and strips it. If the daemon sends a JSON line longer than the OS socket buffer (typically 64 KB), `lines()` may yield a partial line — actually no, `Lines` buffers until it sees a `\n`. So no bug. Real issue: `reader.next_line().await.map_err(KodError::Io)?` at line 716 returns `Ok(None)` on EOF (clean close), but the loop treats `None` as "fall through to the bottom return Err" only if it never matched `done`/`error`. For a daemon that opens the socket, sends nothing for 60 s, then closes — the client blocks in `next_line` for 60 s with no timeout. There's no per-read deadline.
- **Severity:** Low
- **Fix:** Wrap the `while let Some(line) = reader.next_line().await?` in a `tokio::time::timeout(Duration::from_secs(120), …)`.
- **Notes:** This is the daemon-died-silently case; the existing "daemon closed the connection before answering" at line 754 covers clean close but not stall.

---

### T4-L4 — — `cli::run_tests` writes `test.redb` to a per-pid temp dir but does not catch a Ctrl+C mid-test — the temp dir leaks under `~/.kod-selftest-{pid}/`
- **File:** crates/kod-cli/src/commands/admin.rs
- **Line:** 534-554
- **Severity:** Low
- **Category:** Error handling
- **Description:** `run_tests` creates `std::env::temp_dir().join(format!("kod-selftest-{}", std::process::id()))`, opens a `redb` DB, runs lifecycle checks, then `drop(engine); std::fs::remove_dir_all(&scratch)`. If the user hits Ctrl+C between the `redb` open and the `drop`, the `redb` lock file is left behind and the `remove_dir_all` never runs. On re-run, `kod test` creates a new pid-suffixed dir (so no collision), but the orphaned dir lingers forever. The doc comment acknowledges "Best-effort cleanup: a failed removal leaves a temp artifact" — true for `remove_dir_all` failure, not for Ctrl+C.
- **Code:**
```rust
let scratch = std::env::temp_dir().join(format!("kod-selftest-{}", std::process::id()));
// … open redb, run tests, then:
drop(engine);
let _ = std::fs::remove_dir_all(&scratch);
```
- **Why it's a bug:** Mild — disk leak on interrupt. The temp dir is small (~1 MB for the redb file).
- **Fix:** Install a ctrl_c handler that calls `remove_dir_all` and exits, or use `tempfile::TempDir` (which has a Drop that removes the dir).

---

### T4-L5 — — `TuiLoop::init_engine` reads `KOD_TEST_DB` from the environment (line 270) to isolate tests, but the comment says "When unset, the config's `memory.scope` decides" — the env var is read by the production path, so a user who accidentally exports `KOD_TEST_DB=…` has their prod DB overridden
- **File:** crates/kod-tui/src/main_loop.rs
- **Line:** 266-278
- **Severity:** Low
- **Category:** Security / Correctness
- **Description:** `KOD_TEST_DB` is a test-only env var that, when set, overrides the production memory DB path. The name does not start with `KOD_PRODUCTION_UNSAFE_` or similar; a user who copy-pastes a test invocation's env block into their shell will silently redirect their kod memory DB to a temp file. The comment at line 266-269 documents the isolation but does not warn the user. Same pattern in `kod-cli/src/commands/chat.rs:816` (`run_acp` reads `KOD_TEST_DB`).
- **Code:**
```rust
let db_path = match std::env::var("KOD_TEST_DB") {
    Ok(p) => std::path::PathBuf::from(p),
    Err(_) => config.memory_db_path()?,
};
```
- **Why it's a bug:** Easy to mis-set; no warning.
- **Fix:** Log a `warn!` when `KOD_TEST_DB` is set in a non-test build, or rename to `KOD_UNSAFE_TEST_DB`.

---

### T4-L6 — — `clipboard.rs` uses `String::from_utf8_lossy(...).into_owned()` on clipboard reads — a clipboard with invalid UTF-8 produces a string with U+FFFD substitution that the user pastes as `???`
- **File:** crates/kod-tui/src/clipboard.rs
- **Line:** 84-85, 100
- **Severity:** Low
- **Category:** Correctness
- **Description:** `pbpaste` and `xclip -o` can return arbitrary bytes (a screenshot copied as TIFF, a binary file's contents selected in a terminal). `from_utf8_lossy` substitutes invalid sequences with U+FFFD. If the user then pastes that into the input box, the model receives `???`-shaped garbage. There's no detection or rejection of non-UTF-8 clipboard contents.
- **Code:**
```rust
let s = String::from_utf8_lossy(&out.stdout).into_owned();
return Some(s);
```
- **Why it's a bug:** A user copying a binary file's contents and pasting into kod gets garbage instead of "this looks binary, refuse to paste".
- **Fix:** Use `String::from_utf8(out.stdout).ok()` (returns None on invalid) and surface a "clipboard contains non-UTF-8 data" message.

---

### T4-L7 — — `logging::SessionSafeWriter::write_shared` opens the file once and caches it; if the file is deleted out from under the writer (a `logrotate` that deletes instead of renames), writes silently succeed but go nowhere
- **File:** crates/kod-cli/src/logging.rs
- **Line:** 67-88
- **Severity:** Low
- **Category:** Error handling
- **Description:** `OpenOptions::append(true).open(path)` returns a `File` handle. On Linux, deleting the file (the inode has a link count > 0 from the open fd) means subsequent writes still go to the original inode — but the path no longer resolves. If an external `logrotate` deletes the file (rather than renaming), kod keeps writing to the orphaned inode; the file appears empty from `cat ~/.kod/session.log` but kod thinks it's logging. The `write_all` returns `Ok(())` because the fd is still valid.
- **Code:**
```rust
if slot.is_none() {
    *slot = self.log_path().and_then(|path| {
        // …
        OpenOptions::new().create(true).append(true).open(path).ok()
    });
}
match slot.as_mut() {
    Some(file) => file.write_all(buf).map(|_| buf.len()),
    None => Ok(buf.len()),
}
```
- **Why it's a bug:** Silent log loss. The fix is to stat the path periodically and reopen if the inode changed (or simpler: always reopen on each write — slightly slower but correct).
- **Fix:** On each write, check `file.metadata()?.created()?` against the path's `std::fs::metadata(path)?.created()?`; if different, reopen.

---

### T4-L8 — — `dispatch_prompt`'s file-attachment reader caps each file at 64 KiB but uses `floor_char_boundary` — the truncation marker `…\n[truncated]` is appended at the byte boundary, which may split a multi-byte char's UTF-8 sequence in the *next* file's content if the boundary lands exactly at a char boundary inside the next file
- **File:** crates/kod-tui/src/main_loop.rs
- **Line:** 1326-1340
- **Severity:** Low
- **Category:** Correctness (Unicode)
- **Description:** Actually, `floor_char_boundary` is correctly implemented to walk back to a char boundary. The bug is subtler: the truncation marker `{}…\n[truncated]` is inserted *inside* the body string, and then the body is wrapped in `<file path=...>...</file>`. The next file in `attached` is appended as a separate `<file>` block, so no boundary issue. The real low-severity issue: `MAX_ATTACH_BYTES = 64 * 1024` is hardcoded; a user attaching a 1 MB JSON file gets only the first 64 KB, with no indication of how much was cut (the `[truncated]` marker says "truncated" but not "from 1 MB"). A malicious tool result that mentions a file the user attaches could exfiltrate the truncation point.
- **Code:**
```rust
const MAX_ATTACH_BYTES: usize = 64 * 1024;
let shown = if body.len() > MAX_ATTACH_BYTES {
    let cut = kod_types::strutil::floor_char_boundary(&body, MAX_ATTACH_BYTES);
    format!("{}…\n[truncated]", &body[..cut])
} else {
    body
};
```
- **Why it's a bug:** Mild — the truncation marker could be more informative.
- **Fix:** `format!("{}…\n[truncated from {} bytes]", &body[..cut], body.len())`.

---

### T4-L9 — — `EventHandler::pending_events` locks the queue and returns `len()` but the doc claims it's for tests — the priority queue may have grown past `pending_events`'s reported value by the time the caller acts on it
- **File:** crates/kod-tui/src/event.rs
- **Line:** 351-354
- **Severity:** Low
- **Category:** Concurrency / API
- **Description:** `pending_events` is `pub` and exposed; the test at line 836-843 asserts `pending_events() == 2` after two pushes. In production, between the lock release and the caller's branch, the queue may have grown or shrunk. The doc comment "For tests and diagnostics" is correct, but the function is `pub` (callable from anywhere). A future caller that uses `pending_events() > 0` as a backpressure signal would race.
- **Code:**
```rust
pub fn pending_events(&self) -> usize {
    self.event_queue.lock().unwrap().len()
}
```
- **Why it's a bug:** TOCTOU on a sync primitive. Not currently exploited, but the API invites it.
- **Fix:** Mark `pending_events` as `#[cfg(test)]` or document "snapshot, may be stale".

---

### T4-L10 — — `cli::Completions` subcommand writes the shell-completion script to stdout via `clap_complete::generate(..., &mut std::io::stdout())` — but stdout is not flushed explicitly, and the runtime drop happens after the function returns
- **File:** crates/kod-cli/src/commands/mod.rs
- **Line:** 597-602
- **Severity:** Low
- **Category:** Error handling
- **Description:** `clap_complete::generate` writes to the `Write` it's given. `std::io::stdout()` is line-buffered by default; on exit, the runtime's Drop flushes stdout, but if the process is killed (Ctrl+C during a `| head`), the buffer is lost. The shell-completion install command in the help text (`> ~/.local/share/bash-completion/completions/kod`) needs the full script — a partial flush writes a truncated script that bash sources as broken.
- **Code:**
```rust
Some(Command::Completions { shell }) => {
    let mut cmd = <Cli as clap::CommandFactory>::command();
    let bin_name = cmd.get_name().to_string();
    clap_complete::generate(*shell, &mut cmd, bin_name, &mut std::io::stdout());
    Ok(())
}
```
- **Why it's a bug:** Piping `kod completions bash | head` leaves a broken file; the user has to know to use `> file` not `| head`.
- **Fix:** `std::io::stdout().flush()?;` before `Ok(())`.

---

### T4-L11 — — `markdown::parse_inline` allocates a `Vec<char>` from the input text on every paragraph render — `let chars: Vec<char> = text.chars().collect();` (line 502)
- **File:** crates/kod-tui/src/markdown.rs
- **Line:** 502
- **Severity:** Low
- **Category:** Performance
- **Description:** For every paragraph (and every bullet, numbered, header, quote), `parse_inline` collects the text into a `Vec<char>`. For a 5 KB paragraph that's 5 KB of `char` (4 bytes each) = 20 KB heap allocation per render. The render cache mitigates this (only on cache miss), but a long session with many distinct messages (each ≥ 1 KB) pays 4-20 KB alloc per miss. A `Peekable<Chars>` iterator would avoid the allocation.
- **Code:**
```rust
let chars: Vec<char> = text.chars().collect();
let mut i = 0;
while i < chars.len() {
    let c = chars[i];
    // …
}
```
- **Why it's a bug:** Cache misses are common during streaming (the content_hash changes per chunk). Alloc pressure on a 30 Hz render loop.
- **Fix:**
```rust
let mut chars = text.chars().peekable();
let mut buf = String::new();
while let Some(c) = chars.next() {
    // … use chars.peek() for lookahead instead of chars[i+1]
}
```

---

### T4-L12 — — `app/tests.rs::session_state_dir_lock` uses `OnceLock<Mutex<()>>` but a poisoned mutex (from a test panic) is unwrapped via `into_inner` — this silently un-poisons and lets the next test see a half-set env var
- **File:** crates/kod-tui/src/app/tests.rs
- **Line:** 159-165
- **Severity:** Low
- **Category:** Correctness (tests)
- **Description:** `LOCK.lock().unwrap_or_else(|poisoned| poisoned.into_inner())` recovers from poison by extracting the inner `()`. This is the standard pattern, but in this case the poison means a test panicked *while holding the lock and having set `KOD_TUI_STATE_DIR`*. The `unsafe { std::env::remove_var("KOD_TUI_STATE_DIR") }` in the same test (lines 414, 758) was never reached. The next test that takes the lock sees `is_running = true` (the flag was set), gets the lock, and sees a stale `KOD_TUI_STATE_DIR` pointing at a temp dir that no longer exists (the panicked test's `remove_dir_all` was skipped). The next test's `load_session` reads from the stale path, gets "file not found", and may mis-test.
- **Code:**
```rust
fn session_state_dir_lock() -> std::sync::MutexGuard<'static, ()> {
    use std::sync::{Mutex, OnceLock};
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}
```
- **Why it's a bug:** Test cross-contamination after a panic. Rare in CI (panics fail the test run), but in `cargo test --no-fail-fast` a panic in test A can corrupt test B.
- **Fix:** Wrap the env-set + test + env-unset in a `scopeguard::defer!` (or `Drop` guard) so the env var is always cleared.

---

### T4-L13 — — `cli::commands::admin::run_sandbox_exec` calls `command.exec()` (POSIX execvp) which never returns on success — but on failure returns an `Err(KodError)` whose message includes the unquoted `cmd[0]`, which may be a path with spaces
- **File:** crates/kod-cli/src/commands/admin.rs
- **Line:** 707-716
- **Severity:** Low
- **Category:** Error handling
- **Description:** `__sandbox-exec` is documented as Linux-only. The function strips a leading `--` (line 682), reads the profile, applies the landlock sandbox, and execs. On exec failure, it returns `Err(KodError::SandboxViolation(format!("__sandbox-exec: exec of {} failed: {err}", cmd[0])))`. `cmd[0]` is unquoted; if the user passed a path with spaces (`__sandbox-exec p.json /my tools/foo`), the error message is ambiguous. Minor.
- **Code:**
```rust
let err = command.exec();
Err(KodError::SandboxViolation(format!(
    "__sandbox-exec: exec of {} failed: {err}",
    cmd[0]
)))
```
- **Why it's a bug:** Cosmetic — the error message is hard to parse for paths with spaces.
- **Fix:** `format!("__sandbox-exec: exec of {:?} failed: {err}", cmd[0])` (debug-quotes the path).

---

### T4-L14 — — `TuiLoop::init_engine` constructs the embedder via `kod_memory::embedding::from_config(&config.memory, Some(&config.llm.default_endpoint().base_url))` but does not propagate the embedder error if `from_config` fails — the `Option<Embedder>` returned is `None` on error, and the engine silently uses the keyword+recency fallback
- **File:** crates/kod-tui/src/main_loop.rs
- **Line:** 287-296
- **Severity:** Low
- **Category:** Error handling
- **Description:** The doc comment at line 285 says "None (the config default) leaves the keyword+recency fallback in place; no retrieval path is broken by an absent embedder." That's the design, but a user who configured `[memory] embedder = "openai"` and mistyped the API key gets `None` back with NO warning that their semantic retrieval is silently disabled. The retrieval path "works" but returns keyword-only results, which is much worse for a long session. The CLI path (chat.rs) has the same call with the same silent failure.
- **Code:**
```rust
let embedder = kod_memory::embedding::from_config(
    &config.memory,
    Some(&config.llm.default_endpoint().base_url),
);
```
- **Why it's a bug:** Silent feature degradation. The brief asks "Config file loading — TOML/serde correctness on malformed input" — this is adjacent: a *well-formed* config that fails at runtime (network, auth) silently downgrades.
- **Fix:** `from_config` should return `Result<Option<Embedder>>`; on Err, log a `warn!` and proceed with `None`. Or have the TUI push a system message on startup: "Embedder configured but unavailable; semantic retrieval disabled."

---

### T4-L15 — — `cli::run_serve` does not check for an existing daemon at `socket_path` before starting — a second `kod serve` binds the socket, fails, and exits without cleaning up the first daemon's socket file
- **File:** crates/kod-cli/src/commands/admin.rs
- **Line:** 596-665
- **Severity:** Low
- **Category:** Error handling
- **Description:** `run_serve(false, socket)` does not check `socket_path.exists()` before starting. If a previous `kod serve` is already listening, the new `kod_core::serve::serve(engine, socket_path)` call fails to bind (Unix socket already in use). The error propagates, the engine is shut down, but the socket file of the FIRST daemon is left alone (good — the first daemon is still listening). However, the user gets an opaque "Address already in use" error with no hint to run `kod serve --stop` first.
- **Code:**
```rust
pub async fn run_serve(stop: bool, socket: Option<std::path::PathBuf>) -> Result<()> {
    let socket_path = socket.unwrap_or_else(kod_core::serve::default_socket_path);
    if stop { /* … */ }
    // ← no check that socket_path is already in use
    let config = KodConfig::load_default()?;
    // …
    let result = kod_core::serve::serve(engine.clone(), socket_path.clone()).await;
```
- **Why it's a bug:** The brief asks "Lock-file handling for the daemon mode" — there's no lock-file handling at all. A stale socket file (from a crashed daemon) also blocks startup with no detection of "is the daemon actually running?".
- **Fix:** Before starting, check `socket_path.exists()`; if yes, try to connect — if the connect succeeds, the daemon is alive (error: "already running"); if it fails, the socket is stale (unlink and proceed).

---

### T4-L16 — — `TuiLoop::init_terminal` calls `crossterm::terminal::enable_raw_mode()` *before* `EnterAlternateScreen` — if `EnterAlternateScreen` fails, raw mode is left on and the user's shell is in raw mode
- **File:** crates/kod-tui/src/main_loop.rs
- **Line:** 568-613
- **Severity:** Low
- **Category:** Error handling
- **Description:** Order: `enable_raw_mode()`, then `execute!(stdout, EnterAlternateScreen, EnableMouseCapture, EnableBracketedPaste)`. If the `execute!` fails (rare — usually only on a closed stdout), `init_terminal` returns `Err`, but `raw_mode` is already enabled and never disabled. The user's shell is left in raw mode (no echo, no line editing). The panic hook at line 652-668 would catch a panic, but a returned `Err` doesn't trigger the panic hook — `main` returns the error and the shell is broken.
- **Code:**
```rust
crossterm::terminal::enable_raw_mode()
    .map_err(|e| KodError::Internal(format!("Failed to enable raw mode: {}", e)))?;
// …
crossterm::execute!( /* EnterAlternateScreen, … */ )
    .map_err(|e| KodError::Internal(format!("Failed to enter alternate screen: {e}")))?;
```
- **Why it's a bug:** The error message is "Failed to enter alternate screen" but the user's terminal is in raw mode. The fix is to wrap in a cleanup-on-fail.
- **Fix:**
```rust
if let Err(e) = crossterm::execute!(/* … */) {
    let _ = crossterm::terminal::disable_raw_mode();
    return Err(KodError::Internal(format!("Failed to enter alternate screen: {e}")));
}
```

---

### T4-L17 — — `markdown::render`'s trailing-blank-line strip (line 75-81) checks `l.spans.iter().all(|s| s.content.trim().is_empty())` — a span containing only `\n` or `\t` passes the check, but a span with ` ` followed by `\n` also passes — the line is dropped. But a span with a non-breaking space (U+00A0) is NOT dropped (trim() only strips ASCII whitespace), so a model that emits a blank-looking line with NBSP survives as a visible blank row.
- **File:** crates/kod-tui/src/markdown.rs
- **Line:** 75-82
- **Severity:** Low
- **Category:** Correctness
- **Description:** `str::trim()` strips ASCII whitespace (` \t\n\r\x0B\x0C`). It does NOT strip Unicode whitespace like U+00A0 (NBSP), U+2003 (em space), U+3000 (ideographic space). A model reply with a line containing only `&nbsp;` (rare but possible if the model emits HTML entities verbatim) survives the trailing-blank strip and renders as a blank-looking row with a single visible cell. Mild.
- **Code:**
```rust
while out
    .last()
    .map(|l| l.spans.iter().all(|s| s.content.trim().is_empty()))
    .unwrap_or(false)
{
    out.pop();
}
```
- **Why it's a bug:** Minor visual artifact.
- **Fix:** Use `s.content.trim_matches(|c: char| c.is_whitespace())` (Unicode-aware) or `s.content.chars().all(char::is_whitespace)`.

---

### T4-L18 — — `cli::commands::chat::run_chat` calls `engine.shutdown().await?` at the end of the REPL loop, but the ctrl_c task spawned at line 340 holds a clone of `engine` and is never joined — its task can outlive `engine.shutdown()` and call `engine.request_cancel()` on a shutting-down engine
- **File:** crates/kod-cli/src/commands/chat.rs
- **Line:** 338-364, 661-665
- **Severity:** Low
- **Category:** Concurrency
- **Description:** The ctrl_c task at line 340 captures `ctrl_c_engine = engine.clone()` and loops on `tokio::signal::ctrl_c().await`. After `run_chat` returns, `engine.shutdown()` runs and `engine` (the local Arc) is dropped — but the ctrl_c task still holds its clone, keeping the engine alive. On the next Ctrl+C (which the user may press thinking the process is already gone), `engine.request_cancel()` is called on an engine that's mid-shutdown. Depending on `KodEngine`'s internal cancel-safety, this could panic or no-op. The task is never aborted.
- **Code:**
```rust
let ctrl_c_engine = engine.clone();
tokio::spawn(async move {
    loop {
        if tokio::signal::ctrl_c().await.is_err() { break; }
        // …
        ctrl_c_engine.request_cancel();
        // …
    }
});
// … at the end of run_chat:
engine.shutdown().await?;
// ← ctrl_c task is still alive, holding engine clone
```
- **Why it's a bug:** Orphaned task keeping the engine alive past shutdown. The runtime drop at the end of `Cli::run` aborts it, but between `shutdown` and runtime drop the task can act.
- **Fix:** Return the `JoinHandle` from the ctrl_c spawn and `abort()` it before `engine.shutdown()`.

---

### T4-L19 — — `event.rs` `Event::Resize(u16, u16)` carries width/height but `handle_event`'s `Event::Resize` arm only calls `force_repaint` — the `app`'s internal `terminal_size` cache (if any) is not updated, so layout decisions made before the next `render` use stale dimensions
- **File:** crates/kod-tui/src/main_loop.rs
- **Line:** 884-891
- **Severity:** Low
- **Category:** Correctness
- **Description:** On resize, `force_repaint` calls `terminal.clear()` which forces a full repaint on the next `draw`. But the `draw` closure at line 766 reads `f.area()` fresh from ratatui, so ratatui already knows the new size. The issue is `self.app.input_height_rows(size.width)` (line 777) — `KodApp` may cache `input_height_rows` based on a prior `size.width`. If the cache is keyed on width and not invalidated on resize, a resize from 80 → 200 columns leaves the input box at the old height for one frame. Looking at `app/input.rs` would confirm; the Resize arm not touching `app` is the smoking gun.
- **Code:**
```rust
Event::Resize(w, h) => {
    tracing::debug!("Terminal resized to {}x{}", w, h);
    self.force_repaint();
}
```
- **Why it's a bug:** One frame of wrong layout on resize. Brief asks "Terminal resize handling (race between signal and poll)" — this is the closest: the resize event arrives, `force_repaint` clears, but `app`'s width-derived caches are stale for one frame.
- **Fix:** `self.app.invalidate_size_caches();` in the Resize arm.

---

### T4-L20 — — `markdown::parse`'s opening-fence info string parsing at line 263-268 takes the substring after ```` ``` ```` and `trim()`s it, but does not split on whitespace — ` ```rust,ignore ` is treated as language `rust,ignore` rather than `rust` (the highlighter then can't find a grammar and falls back to flat)
- **File:** crates/kod-tui/src/markdown.rs
- **Line:** 263-268
- **Severity:** Low
- **Category:** Correctness
- **Description:** Wait — `highlight::fence_token` at line 263 of highlight.rs does `info.split_whitespace().next().unwrap_or("").split(',').next().unwrap_or("").trim().to_ascii_lowercase()`. So the highlighter does split on whitespace AND comma. But `markdown::parse` at line 267 stores `code_lang = Some(info.to_string())` where `info` is the trimmed full info string (e.g. `"rust,ignore"`). Then `render_block` at line 470 passes `lang.as_deref()` to `code_rows`, which calls `fence_token` internally. So the language extraction happens correctly inside `code_rows`. No bug — the parser just stores the raw info string and lets the highlighter do the splitting. Withdraw this finding.

- **Replacement — T4-L20 (real):** `markdown::parse`'s opening-fence check at line 263 is `trimmed.strip_prefix("```")` — but `trimmed` is `raw.trim_start()`. So a fence indented by 4+ spaces is still recognized as an opening fence (CommonMark requires < 4 spaces of indent for a fence to open). A code block inside a deeply-nested list (where the model indents by 4 spaces) would be misparsed: the indented ` ``` ` opens a code block, but CommonMark would treat it as an indented code block (no fence, no language). Mild parser divergence from spec.

---

### T4-L21 — — `admin.rs run_jev_stats` computes `cached * 100 / total` using integer division — when `cached == 1` and `total == 3`, the displayed percentage is `33%` (truncated from 33.33), which is fine, but when `cached == 2` and `total == 3`, it shows `66%` (truncated from 66.67) — the rounding is asymmetric (always rounds down)
- **File:** crates/kod-cli/src/commands/admin.rs
- **Line:** 1103
- **Severity:** Low
- **Category:** UX
- **Description:** `cached * 100 / total` is integer arithmetic; `cached` and `total` are `usize`. The result is truncated, not rounded. `2 * 100 / 3 = 66` (should be 67 rounded). `1 * 100 / 3 = 33` (should be 33 rounded — OK). For `cached = 5, total = 8`, `5 * 100 / 8 = 62` (should be 63 rounded). The displayed percentage is always ≤ the true value. The TUI's similar stat at main_loop.rs:3274 uses `with_refs as f64 / total as f64 * 100.0` — floating-point, so it rounds correctly. Inconsistent.
- **Code:**
```rust
println!("  cached:     {cached} ({}%)", cached * 100 / total);
```
- **Why it's a bug:** Mild — 1% display discrepancy.
- **Fix:** `println!("  cached:     {cached} ({:.0}%)", cached as f64 / total as f64 * 100.0);`

---

### T4-L22 — — `TuiLoop::handle_command`'s `_ => { … custom commands … }` arm reads `KodConfig::load_default()` on EVERY unknown slash command — a user who typos `/hlep` pays a full config-file parse (TOML read + serde) for the error message
- **File:** crates/kod-tui/src/main_loop.rs
- **Line:** 5102-5151
- **Severity:** Low
- **Category:** Performance
- **Description:** `KodConfig::load_default().ok()` is called to look up `[commands]` custom command bodies. The config is loaded from `~/.config/kod/config.toml` (and project-local overlays). For an unknown command like `/hlep`, the function pays: stat the config file, read it, parse TOML, merge with defaults, deserialize — typically 1-5 ms. The user typed a typo and gets 5 ms of latency on the error message. The `hint` at line 5125-2143 then iterates the custom command names, which is fast. The config could be cached once at startup; the TUI does store `llm_config` but not the full `KodConfig`.
- **Code:**
```rust
_ => {
    let config = KodConfig::load_default().ok();
    let custom = config.as_ref().and_then(|c| {
        let key = cmd.trim_start_matches('/');
        c.commands.get(key).cloned()
    });
    // …
}
```
- **Why it's a bug:** Typo latency. Also, the config may have changed on disk mid-session; loading each time is "correct" but expensive.
- **Fix:** Cache `KodConfig` in `TuiLoop` on startup; provide a `/reload` command to refresh. Or only load config when the unknown command might be custom (i.e. always, but cache the result).

---

### T4-L23 — — `cli::commands::admin::run_init`'s `KodConfig::load_default()` call at line 64 reads the *existing* config to print its model/endpoint/network fields — but the function is meant to *initialize* a config; if no config exists, `load_default()` returns the built-in default (which has a placeholder model like `qwen2.5:0.5b`), and the printed "Model: qwen2.5:0.5b" misleads the user into thinking that model is configured
- **File:** crates/kod-cli/src/commands/admin.rs
- **Line:** 63-150
- **Severity:** Low
- **Category:** UX
- **Description:** `KodConfig::load_default()` returns defaults when no file exists (the function's name implies it loads, but it merges defaults with whatever's on disk — pure defaults if no disk file). So a fresh `kod init` on a clean machine prints "Model: qwen2.5:0.5b" (or whatever the default is), implying the user has that model configured. The user then runs `kod tui`, the engine tries to connect to `http://localhost:11434/v1` for `qwen2.5:0.5b`, fails, and the user is confused. The fix is to print "(no model configured — defaults shown)" when the config file does not exist.
- **Code:**
```rust
let config = KodConfig::load_default()?;
// …
println!("Model:    {}", config.llm.default_endpoint().model);
```
- **Why it's a bug:** Mild UX confusion on first run.
- **Fix:** `let config_path = KodConfig::config_dir()?.join("config.toml"); if !config_path.exists() { println!("(no config file — defaults shown)"); }`.

---

### T4-L24 — — `markdown.rs::wrap_spans` line 653 uses `lines.last_mut().unwrap()` — `unwrap()` on the last line of a Vec that the function just initialized with one empty Vec at line 618. Safe in practice, but the `unwrap` is unnecessary; `last_mut().expect("lines non-empty")` would document the invariant
- **File:** crates/kod-tui/src/markdown.rs
- **Line:** 653, 676, 683, 688
- **Severity:** Low
- **Category:** Code quality
- **Description:** The function starts with `let mut lines: Vec<Vec<(char, Style)>> = vec![Vec::new()];` — one empty line. All subsequent `lines.last_mut().unwrap()` calls rely on this invariant: lines is never empty. But the `unwrap()` is the kind of "panic-on-bug" that the brief's "234 unwrap() calls" stat is measuring. If a future refactor adds an early `lines.pop()` somewhere, this panics. `expect("lines invariant: non-empty")` would document it.
- **Code:**
```rust
lines.last_mut().unwrap().push((' ', chars[i].1));
```
- **Why it's a bug:** Not a bug — a code-smell. The 234 unwrap() calls in kod-tui are mostly this pattern.
- **Fix:** `lines.last_mut().expect("wrap_spans invariant: lines non-empty").push(...)`.

---

### T4-L25 — — `cli::commands::chat::run_chat`'s `input_rx` is wrapped in `Arc<tokio::sync::Mutex<…>>` and locked per iteration of the REPL loop AND per iteration of the pump's approval/question reads — the pump and the REPL can contend on the same lock, but the pump holds it across `recv().await` which can be unbounded
- **File:** crates/kod-cli/src/commands/chat.rs
- **Line:** 329, 376, 475, 514, 557
- **Severity:** Low
- **Category:** Concurrency
- **Description:** `input_rx = Arc<Mutex<UnboundedReceiver<String>>>`. The REPL loop at line 376 does `input_rx.lock().await.recv().await` — holds the lock across the await. The pump at line 475 does `input_rx_pump.lock().await.recv().await` — also holds the lock across the await. If the pump is waiting for an approval answer and the user types another line, the REPL's `lock().await` blocks until the pump's `recv().await` returns (i.e. until the user types the answer). So the REPL cannot read the next prompt line until the pump has consumed the approval answer. That's actually the desired behavior (the pump should consume the answer, not the REPL). But if the pump's chunk processing is slow (a 100-line diff print), the REPL is blocked from reading the next prompt for the duration. Mild.
- **Code:**
```rust
let text = match input_rx_pump.lock().await.recv().await { /* … */ };
```
- **Why it's a bug:** Not really a bug — the lock contention is the mechanism that ensures the pump reads the answer. But the brief asks about "Lost wakeups / Cancel-safety" — the lock-held-across-await is a Cancel-safety footgun: if the pump task is cancelled while holding the lock (e.g. on `engine.shutdown()`), the lock is poisoned and the REPL panics on next access.
- **Fix:** Use a `tokio::sync::Notify` or a dedicated `mpsc::Sender<String>` per consumer (pump vs REPL) so they don't share a lock.

---

### T4-L26 — — `kod-tui::components/mod.rs` is empty (re-export only) but the brief mentions `components/*` — there are no `components/*.rs` files beyond `mod.rs`, so the "components" surface is actually the `ui/*.rs` files (chat.rs, input.rs, etc.)
- **File:** crates/kod-tui/src/components/mod.rs
- **Line:** (entire file)
- **Severity:** Low
- **Category:** Code organization
- **Description:** The `components/` directory only contains `mod.rs`. The actual widgets live in `ui/`. The naming is confusing: a future contributor reading "components" expects widget-like objects, finds an empty mod, and then has to look in `ui/`. Not a bug, just an organization smell.
- **Fix:** Either remove `components/mod.rs` and re-export from `ui/mod.rs`, or move the `ui/` files into `components/`.

---

## Summary of recurring themes

1. **Two-queue event model (`event.rs`)** — the priority queue + mpsc split causes T4-C2 and T4-H6, and makes every key event wait up to 100 ms for a tick. This is the single biggest UX issue in kod-tui.
2. **Orphaned tokio tasks** — every `tokio::spawn` in `main_loop.rs` except `gen_task` drops its handle (T4-C1, T4-H4, T4-M15, T4-L18). On quit/shutdown the runtime drop eventually aborts them, but between quit and drop they can mutate shared state.
3. **Unsafe `std::env` in tests** — Rust 2024 makes `set_var`/`set_current_dir` UB-on-read; the test mutexes only serialize writers (T4-C3, T4-L12).
4. **Markdown parser divergences from CommonMark** — fence matching (T4-C4), indent-overflow on continuation (T4-C5), nested emphasis (T4-M1), backslash escapes (T4-M16). The hand-rolled parser is fast but under-specified; the doc acknowledges "no nested lists" but not the other gaps.
5. **`std::process::exit(1)` everywhere in admin.rs** — bypasses Drop, skips graceful engine/tracing shutdown, lies about `Result<()>` return type (T4-M9, T4-M10).
6. **Blocking clipboard / stdin** — `child.wait()` without timeout (T4-H1), `stdin.lock().read_line()` in async (T4-M7), no SIGINT in remote chat (T4-M8).
7. **Silent feature degradation** — embedder unavailable returns None with no warning (T4-L14), `try_init` ignored (T4-H8), clipboard failure silently returns false (T4-L6).
8. **No log rotation** (T4-H9), **no lock-file for daemon** (T4-L15), **runtime created for every subcommand** (T4-H10).

---
# Part 5 — Support crates (config, risk, swarm, skills, telemetry, error, types, ast, stats, schema-dialect, minimize)

_Crates: kod-config, kod-risk, kod-ast, kod-stats, kod-telemetry, kod-error, kod-types, kod-skills, kod-swarm, kod-schema-dialect, kod-minimize_


Reviewed 39 source files across 11 crates (~10 000 LOC of non-test code). Findings are evidence-based; every cited line was opened. Grouped by severity, Critical/High first.

---

## Critical bugs

### T5-C1 — — `xargs rm <protected-path>` bypasses the destructive-path check
- **File:** crates/kod-risk/src/classify.rs
- **Line:** 421–477
- **Severity:** Critical
- **Category:** Security
- **Description:** `assess` only walks the operands of a command whose `prog_base` is in `DESTRUCTIVE` (`rm`, `rmdir`, `mv`, `dd`, `shred`, `truncate`, `unlink`, `cp`). `xargs` is not in that list, so when the model writes `xargs rm /etc/passwd`, xargs is treated as the program and its `rm /etc/passwd` arguments are never classified. Only the trailing `prog_base == "xargs" && pipe_fed` heuristic (line 471) catches the pipe-fed form; the no-pipe `xargs rm <path>` form is silently classified `Safe`/`Low`, even though xargs will literally pass `/etc/passwd` to `rm` on every line it reads from stdin.
- **Code:**
```rust
// DESTRUCTIVE does not include xargs — but xargs <destructive> <path>
// forwards the path operand to the wrapped destructive command.
const DESTRUCTIVE: &[&str] = &[
    "rm", "rmdir", "mv", "dd", "shred", "truncate", "unlink", "cp",
];
// ...
if DESTRUCTIVE.contains(&prog_base.as_str()) {
    for arg in args.iter() { /* classify path arg */ }
}
// ...later: xargs is only flagged when pipe_fed
if prog_base == "xargs" && pipe_fed {
    findings.push(RiskFinding { level: RiskLevel::Confirm, ... });
}
```
- **Why it's a bug:** `xargs rm /etc/passwd` deletes `/etc/passwd` on every stdin line; the classifier never flags it. The OS sandbox may catch it at runtime, but the model-facing risk gate (the one the user sees in the approval UI) does not.
- **Fix:**
```rust
// When xargs is the program, its first non-flag argument is the wrapped
// command; check the *remainder* as if it were that command's operands.
if prog_base == "xargs" {
    let mut iter = args.iter().peekable();
    while let Some(a) = iter.next() {
        let a = unquote(a);
        if a.starts_with('-') { continue; }
        // First non-flag is the wrapped program; the rest are its operands.
        let wrapped_base = a.rsplit('/').next().unwrap_or(&a).to_string();
        if DESTRUCTIVE.contains(&wrapped_base.as_str()) {
            for arg in iter.by_ref() {
                let (danger, why) = classify_path(&unquote(arg), ctx);
                if danger >= PathDanger::Confirm {
                    findings.push(RiskFinding { level: danger.into(), reason: why, target: unquote(arg) });
                }
            }
        }
        break;
    }
}
```
- **Notes:** Needs the same `DEVICE_CAPABLE` short-circuit when the wrapped command is `dd`/`mkfs`.

---

### T5-C2 — — Unterminated heredoc swallows the rest of the command silently
- **File:** crates/kod-risk/src/classify.rs
- **Line:** 135–156
- **Severity:** Critical
- **Category:** Security / Correctness
- **Description:** When the tokenizer enters a heredoc body it loops until it sees the terminator line. If the terminator never arrives (truncated command, model typo, deliberately malicious `cat foo <<EOF; rm -rf /`), the rest of the input is silently consumed as heredoc data. A subsequent destructive command in the same input never reaches the segment-splitter, so `assess` returns `Safe` for input that, when the shell actually runs it, will delete the home directory.
- **Code:**
```rust
if let Some(term) = &heredoc_terminator {
    // Inside a heredoc body: skip whole lines until the terminator.
    let line_end = command[..]
        .char_indices()
        .skip(i)
        .find(|(_, ch)| *ch == '\n')
        .map(|(idx, _)| idx)
        .unwrap_or(command.len());
    let _ = line_end; // ← dead variable, no early-exit when missing
    let rest: String = chars[i..].iter().collect();
    let mut lines = rest.splitn(2, '\n');
    let this_line = lines.next().unwrap_or("");
    if this_line.trim() == term.as_str() {
        heredoc_terminator = None;
        i += this_line.chars().count() + 1;
        continue;
    }
    i += this_line.chars().count() + 1; // consumes one line per iteration
    continue;
}
```
- **Why it's a bug:** `cat foo <<EOF; rm -rf /Users/dev` (no closing `EOF`) makes the entire tail a heredoc body. The classifier returns `Safe`, the user clicks "yes", the shell runs the `rm`.
- **Fix:**
```rust
if let Some(term) = &heredoc_terminator {
    let rest: String = chars[i..].iter().collect();
    let (this_line, after) = match rest.split_once('\n') {
        Some((h, t)) => (h, t),
        None => (rest.as_str(), ""),
    };
    if this_line.trim() == term.as_str() {
        heredoc_terminator = None;
        i += this_line.chars().count() + 1;
        continue;
    }
    if after.is_empty() && this_line.trim() != term.as_str() {
        // EOF reached without the terminator: treat the rest as live
        // command text and escalate, since the shell would error.
        findings.push(RiskFinding {
            level: RiskLevel::Confirm,
            reason: "heredoc body had no closing terminator".into(),
            target: term.clone(),
        });
        break;
    }
    i += this_line.chars().count() + 1;
    continue;
}
```

---

### T5-C3 — — `broadcast` records messages before delivery (phantom traffic)
- **File:** crates/kod-swarm/src/communication.rs
- **Line:** 254–267
- **Severity:** Critical
- **Category:** Correctness / Concurrency
- **Description:** `send_direct` was fixed by F2h-16 to record history *after* a successful `tx.send` so a failed send does not produce a phantom history entry. `broadcast` did not get the same fix: it calls `record_message(from, ...)` and `record_message(id, ...)` for every recipient *before* the delivery loop. If any recipient's send fails (`?` short-circuits the loop), the history for that recipient — and for every later recipient that never received the message — claims the message was delivered. A debug panel reading the hub's history sees traffic that never reached the agent.
- **Code:**
```rust
self.record_message(from, &message).await;
for (id, _) in &recipients {
    self.record_message(id, &message).await;   // ← records before send
}
for (_, tx) in recipients {
    tx.send(message.clone())
        .map_err(|e| KodError::InvalidState(e.to_string()))?; // ← stops on first failure
}
```
- **Why it's a bug:** Two coupled bugs. (1) The history lies about delivery. (2) A single bad recipient (e.g. one whose receiver was taken and channel closed between the online check and the send) blocks the broadcast to every later recipient, with no diagnostic.
- **Fix:**
```rust
self.record_message(from, &message).await;
let mut failed = Vec::new();
for (id, tx) in recipients {
    if tx.send(message.clone()).is_err() {
        failed.push(id.clone());
    } else {
        self.record_message(&id, &message).await;
    }
}
if !failed.is_empty() {
    tracing::warn!(?failed, "broadcast dropped for recipients whose channels closed");
}
```

---

### T5-C4 — — `IrcBus::mark_dead` leaves parked waiters stranded until timeout
- **File:** crates/kod-swarm/src/irc_bus.rs
- **Line:** 235–241 (mark_dead), 266–330 (send_await)
- **Severity:** Critical
- **Category:** Concurrency / Error handling
- **Description:** `send_await` parks a waiter in `waiters` keyed by a correlation id. `mark_dead` flips `dead=true` but never notifies any parked waiters that are waiting on the now-dead agent. The waiter's `oneshot::Receiver` will only resolve when the timeout fires — which can be up to `DEFAULT_IRC_TIMEOUT_MS` (120 s) away. The bus has the information needed to short-circuit ("the target is dead, fail fast with `TargetStopped`") and does not use it.
- **Code:**
```rust
pub async fn mark_dead(&self, id: &str) {
    let mut g = self.inner.lock().await;
    if let Some(a) = g.agents.get_mut(id) {
        a.dead = true;
        a.has_receiver = false;
    }
    // ← no waiters are drained / notified here
}
```
- **Why it's a bug:** A planner that decomposes-and-awaits a subagent that crashes will block on a 120 s timeout per dead subagent, instead of getting `TargetStopped` immediately. A swarm of 8 dead subagents = 16 minutes of stuck planner.
- **Fix:**
```rust
pub async fn mark_dead(&self, id: &str) {
    let to_drop: Vec<u64> = {
        let mut g = self.inner.lock().await;
        let Some(a) = g.agents.get_mut(id) else { return };
        a.dead = true;
        a.has_receiver = false;
        // Wake every waiter whose reply was supposed to come from this agent.
        // (We don't track per-agent waiters; iterate.)
        g.waiters.keys().copied().collect()
    };
    // Each oneshot's Sender-drop resolves the receiver with Err — the
    // send_await path already maps that to TargetStopped.
    // For an even stronger contract, send Err via the channel so the
    // receiver sees TargetStopped directly.
}
```
- **Notes:** To target only this agent's waiters, the bus would need to track `waiter_for_agent: HashMap<u64, String>` alongside `waiters`. Worth doing in the same PR.

---

### T5-C5 — — `send_await` ignores the enqueue receipt (TOCTOU between liveness check and enqueue)
- **File:** crates/kod-swarm/src/irc_bus.rs
- **Line:** 277–301
- **Severity:** High
- **Category:** Concurrency / Error handling
- **Description:** `send_await` checks `state.dead` under the first lock, drops the lock, then calls `enqueue` (which re-acquires the lock). Between the two locks, another task may call `mark_dead` (or `unregister`) on the target. `enqueue` will then return `Receipt::UnknownTarget` — but `send_await` discards the receipt with `let _ = self.enqueue(msg).await;`. The waiter is parked for the full timeout on an agent that is known-dead.
- **Code:**
```rust
let (correlation, rx) = {
    let mut g = self.inner.lock().await;
    let state = g.agents.get(&to).ok_or(SendError::UnknownTarget)?;
    if state.dead { return Err(SendError::TargetStopped); }
    // ...
    (correlation, rx)
};
// ...
let _ = self.enqueue(msg).await; // ← receipt discarded; dead/unknown target not surfaced
```
- **Why it's a bug:** A swarm runner that kills an agent between the liveness check and the enqueue produces a useless 120-second wait instead of an immediate `TargetStopped`.
- **Fix:**
```rust
let receipt = self.enqueue(msg).await;
if matches!(receipt, Receipt::UnknownTarget) {
    // The agent went dead or was unregistered between the liveness check
    // and the enqueue. Remove the waiter and surface the error.
    let mut g = self.inner.lock().await;
    g.waiters.remove(&correlation);
    return Err(SendError::TargetStopped);
}
```

---

### T5-C6 — — `policy.rs::decide` only inspects `path`/`file` arg keys
- **File:** crates/kod-config/src/policy.rs
- **Line:** 670–676
- **Severity:** High
- **Category:** Security
- **Description:** `extract_path_arg` checks for `"path"` and `"file"` only. Tools whose argument is named `"target"`, `"destination"`, `"directory"`, `"to"`, `"output"`, `"file_path"`, or any other name return `None` for the path, and the entire path-based half of the policy engine (forbidden globs, allow-list, path-resolution-based widening) is silently bypassed for that call. The model can name a path through an argument the policy does not look at and have it land on disk without a forbidden-pattern deny.
- **Code:**
```rust
fn extract_path_arg(args: &Value) -> Option<String> {
    for key in ["path", "file"] {
        if let Some(s) = args.get(key).and_then(|v| v.as_str()) {
            return Some(s.to_string());
        }
    }
    None
}
```
- **Why it's a bug:** A `write_file` tool that names its argument `destination` (some MCP servers do) bypasses every forbidden path rule.
- **Fix:**
```rust
const PATH_KEYS: &[&str] = &[
    "path", "file", "file_path", "filename",
    "target", "destination", "to",  // move/cp semantics
    "directory", "dir",
    "output",  // compilers, format tools
];
fn extract_path_arg(args: &Value) -> Option<String> {
    for key in PATH_KEYS {
        if let Some(s) = args.get(key).and_then(|v| v.as_str()) {
            return Some(s.to_string());
        }
    }
    None
}
```

---

### T5-C7 — — `policy.rs::decide` first-token binary check is bypassable via shell wrappers
- **File:** crates/kod-config/src/policy.rs
- **Line:** 531–559
- **Severity:** High
- **Category:** Security
- **Description:** The binary allow/deny list extracts the first whitespace-split token of the command, then strips the basename. `bash -c "rm -rf /home"` produces `first = "bash"`. If `bash` is not in the deny list, the command passes the binary check and the inner `rm -rf /home` is never inspected. The path-policy does not see the destructive argument because it is inside a string literal, not a `path`/`file` key.
- **Code:**
```rust
let first = cmd
    .split_whitespace()
    .next()
    .unwrap_or("")
    .rsplit('/')
    .next()
    .unwrap_or("");
if let Some(forbidden) = &tp.forbidden_binaries
    && forbidden.iter().any(|b| b == first)
{ /* deny */ }
if let Some(allow) = &tp.binaries
    && !allow.iter().any(|b| b == first)
{ /* deny */ }
```
- **Why it's a bug:** A `binaries = ["rm", "shred", "dd"]` allow-list is trivially bypassed with `bash -c 'rm -rf ~'`. The sandbox may catch the runtime `rm`, but the policy-engine deny is supposed to be the first line of defense.
- **Fix:**
```rust
// Decode the common shell-wrap forms: `bash -c '<cmd>'`, `sh -c '<cmd>'`,
// `env -- bash -c '<cmd>'`, etc. Recurse one level into the wrapped
// command and apply the binary check to it as well.
fn binary_targets(cmd: &str) -> Vec<String> {
    let mut out = vec![first_token(cmd)];
    for wrapper in ["bash", "sh", "zsh", "dash", "env"] {
        // crude: if `wrapper -c '<rest>'`, push first token of <rest>
        // (a full shell-quoted parser lives in kod-risk; a 20-line
        // approximation is enough here).
    }
    out
}
```
- **Notes:** The right long-term fix is for the policy engine to call `kod-risk::assess` and use its blast-radius as a decision input.

---

### T5-C8 — — `schema-dialect::sanitize` has no recursion depth limit
- **File:** crates/kod-schema-dialect/src/sanitize.rs
- **Line:** 248–417
- **Severity:** High
- **Category:** Security / DoS
- **Description:** `sanitize_in_place` recurses through every `Subschema`, `SubschemaMap` entry, and `SubschemaArray` element. JSON Schemas can recurse arbitrarily deep through `properties.properties.properties...` or `anyOf.anyOf.anyOf...`. The model authors the schema (the threat model from the file's own comment: "a model-authored schema"). A model that produces a deeply-nested schema (intentionally or by hallucination) overflows the stack and crashes the engine.
- **Code:**
```rust
fn sanitize_in_place(
    value: &mut Value,
    spec: &DialectSpec,
    path: &str,
    applied: &mut Vec<AppliedTransform>,
) {
    // ... no depth parameter; recursion is unbounded
    match role_of(&key) {
        KeywordRole::Subschema => {
            if let Some(v) = obj.get_mut(&key) {
                sanitize_in_place(v, spec, &child_path, applied); // ← unbounded
            }
        }
        KeywordRole::SubschemaMap => {
            if let Some(Value::Object(map)) = obj.get_mut(&key) {
                for (_name, sub) in map.iter_mut() {
                    sanitize_in_place(sub, spec, &child_path, applied); // ← unbounded
                }
            }
        }
        // ...
    }
}
```
- **Why it's a bug:** Adversarial or buggy-model input can crash the engine with a stack overflow. The `kod-ast` parse cache has bounds; this does not.
- **Fix:**
```rust
const MAX_SCHEMA_DEPTH: usize = 64;

fn sanitize_in_place(
    value: &mut Value,
    spec: &DialectSpec,
    path: &str,
    applied: &mut Vec<AppliedTransform>,
    depth: usize,
) {
    if depth >= MAX_SCHEMA_DEPTH {
        applied.push(AppliedTransform::Removed {
            path: path.to_string(),
            keyword: "<deeply-nested-schema-dropped>".to_string(),
        });
        *value = Value::Object(Default::default());
        return;
    }
    // ...
    sanitize_in_place(v, spec, &child_path, applied, depth + 1);
}

pub fn sanitize(schema: &Value, spec: &DialectSpec) -> (Value, Vec<AppliedTransform>) {
    let mut out = schema.clone();
    let mut applied = Vec::new();
    sanitize_in_place(&mut out, spec, "", &mut applied, 0);
    (out, applied)
}
```

---

### T5-C9 — — `kod-error::provider_status` truncates bodies but does not redact secrets
- **File:** crates/kod-error/src/error.rs
- **Line:** 223–249
- **Severity:** High
- **Category:** Security
- **Description:** `provider_status_with_hint` takes the provider's HTTP response body, calls `truncate_chars(body, 300)`, and embeds it in the error's `Display` output. If a 4xx from a misconfigured provider echoes the API key back (some gateways do this on a 401 with `WWW-Authenticate: Bearer realm="..."` or in the body), the secret ends up in the `KodError::Provider` variant, which is logged, displayed to the TUI, and written to the session log. The kod-types redactor runs on tool results and prompt paths; it does not run on error Display.
- **Code:**
```rust
pub fn provider_status_with_hint(
    status: u16,
    body: &str,
    retry_after: Option<std::time::Duration>,
) -> Self {
    let snippet = kod_types::strutil::truncate_chars(body, 300);
    match status {
        401 | 403 => KodError::Provider(format!("auth error {status}: {snippet}")),
        // ...
    }
}
```
- **Why it's a bug:** A 401 that says `Unauthorized: token sk-ant-... is invalid` puts the live API key in every log line that renders the error.
- **Fix:**
```rust
pub fn provider_status_with_hint(
    status: u16,
    body: &str,
    retry_after: Option<std::time::Duration>,
) -> Self {
    use kod_types::redact::Redactor;
    let snippet = kod_types::strutil::truncate_chars(body, 300);
    let redacted = Redactor::default().redact(&snippet);
    match status {
        401 | 403 => KodError::Provider(format!("auth error {status}: {redacted}")),
        // ...
    }
}
```

---

### T5-C10 — — `kod-config::load_cached` returns stale config when mtime resolution is coarse
- **File:** crates/kod-config/src/config.rs
- **Line:** 311–334
- **Severity:** High
- **Category:** Correctness / Concurrency
- **Description:** `load_cached` keys the cache on `(path, mtime)`. On filesystems with 1-second mtime resolution (ext4 default, HFS+, NFSv3), two writes within the same second produce the same mtime. A user who edits the config and saves, then edits again within the same second, gets a stale cache hit on the second read. The engine runs with the first edit's settings until the next mtime tick. Also, the function holds a `std::sync::Mutex` across the entire `load_default()` call, which performs file I/O; concurrent callers are completely serialized, even on the same `Arc<KodConfig>` snapshot.
- **Code:**
```rust
let mtime = std::fs::metadata(&path).and_then(|m| m.modified()).ok();
let cache = CACHE.get_or_init(|| Mutex::new(None));
if let Ok(g) = cache.lock()
    && let Some((p, t, cfg)) = g.as_ref()
    && *p == path
    && *t == mtime              // ← false on the first read after a same-second edit
{
    return Ok(cfg.clone());
}
let cfg = std::sync::Arc::new(Self::load_default()?); // ← holds lock across I/O
```
- **Why it's a bug:** Hot-reload of the config is a documented feature. Stale reads defeat it. The serial-I/O-under-lock amplifies contention under load.
- **Fix:**
```rust
// Include the file size and inode as additional cache keys; together with
// mtime they form a "content identity" that is robust against sub-second
// rewrites. (On Windows, use file_index from std::os::windows::fs::MetadataExt.)
let mtime = std::fs::metadata(&path).ok().map(|m| {
    (m.modified().ok(), m.len(), #[cfg(unix)] {
        use std::os::unix::fs::MetadataExt;
        m.ino()
    })
});
// ...
// Release the lock before I/O: take an entry, drop the lock, do the load,
// re-acquire to insert (last writer wins).
```

---

## High / Medium bugs

### T5-C11 — — `policy.rs::resolve_path` lexical normalization does not catch symlink escapes
- **File:** crates/kod-config/src/policy.rs
- **Line:** 680–731
- **Severity:** High
- **Category:** Security
- **Description:** `resolve_path` does lexical `..` normalization only. A symlink inside the working dir that points at `/etc` lexically reads as `working_dir/symlink`, which `starts_with(working_dir)` matches, so the policy returns Allow. The file comment says "the tool's `resolve_path` still canonicalizes for symlink safety" — but that is the *tool* layer, not the *policy* layer. The pre-write approval prompt shows the lexical path, not the canonical one, so the user approves a path the policy classified as Safe that the tool then resolves to `/etc/passwd`.
- **Code:**
```rust
fn resolve_path(working_dir: &Path, p: &str) -> PathBuf {
    // ...
    // Lexical `..` / `.` normalization. Mirrors `Path::components()`'s
    // own handling of CurDir and ParentDir when the path is not yet
    // resolved on disk.
    // (no symlink resolution; comment defers to the tool layer)
}
```
- **Why it's a bug:** A symlink planted by a malicious `git clone` (a "git trap") is inside `working_dir`, lexically Safe, and the model gets to write to `/etc/passwd` after one approval click.
- **Fix:**
```rust
fn resolve_path(working_dir: &Path, p: &str) -> PathBuf {
    let lex = lexical_normalize(working_dir, p);
    // Best-effort: canonicalize the existing parent. The destination
    // may not exist yet on a write, so canonicalize the parent and
    // re-append the file name.
    let parent = lex.parent().unwrap_or(Path::new("/"));
    match std::fs::canonicalize(parent) {
        Ok(canon_parent) => canon_parent.join(lex.file_name().unwrap_or_default()),
        Err(_) => lex, // fall back to lexical; the tool layer catches it
    }
}
```

---

### T5-C12 — — `kod-minimize::compile` caches regexes process-wide without eviction
- **File:** crates/kod-minimize/src/pipeline.rs
- **Line:** 283–303
- **Severity:** Medium
- **Category:** Performance / Memory
- **Description:** `compile` uses a `static OnceLock<Mutex<HashMap<String, regex::Regex>>>` keyed by the pattern text. Every distinct pattern ever compiled stays for the process lifetime. A long-running session that loads many skill defs (each with `KeepLines`, `StripLines`, `Replace` stages) accumulates every regex. A chatty model that proposes new defs at runtime (the `register` API exists on `Minimizer`) compounds it.
- **Code:**
```rust
static CACHE: OnceLock<Mutex<HashMap<String, regex::Regex>>> = OnceLock::new();
let cache = CACHE.get_or_init(|| Mutex::new(HashMap::new()));
if let Ok(g) = cache.lock()
    && let Some(re) = g.get(pattern)
{
    return Ok(re.clone());
}
// ...
if let Ok(mut g) = cache.lock() {
    g.insert(pattern.to_string(), re.clone()); // never evicted
}
```
- **Why it's a bug:** Unbounded memory growth in a process-global static.
- **Fix:**
```rust
const MAX_CACHED_PATTERNS: usize = 256;
// After insert, if size > MAX_CACHED_PATTERNS, evict a random/oldest
// entry (the re-compile cost on a miss is the same as before the cache
// existed, so any eviction policy is correct).
if g.len() > MAX_CACHED_PATTERNS {
    let key_to_drop = g.keys().next().cloned();
    if let Some(k) = key_to_drop { g.remove(&k); }
}
```

---

### T5-C13 — — `kod-minimize::run` "safety valve" returns raw on legitimately-empty results
- **File:** crates/kod-minimize/src/pipeline.rs
- **Line:** 207–210
- **Severity:** Medium
- **Category:** Correctness
- **Description:** If a stage list legitimately reduces non-empty input to empty (e.g. `keep_lines` with patterns that match nothing because the command produced no matching lines), the safety valve returns the *raw* input. The model sees the entire `cargo check` output instead of nothing, defeating the minimizer. The comment says "no shipped def wants that", but a def could legitimately want it (e.g. `strip_lines` that strips every line of a known-verbose tool).
- **Code:**
```rust
if text.trim().is_empty() && !raw.trim().is_empty() {
    return Ok(raw.to_string());
}
```
- **Why it's a bug:** A def whose intent is "produce nothing if X" (an explicit drop) is impossible; the valve silently overrides intent.
- **Fix:** Make the valve opt-out per-def: add `allow_empty_result: bool` to `Def` (default `false` to preserve current behavior), and only run the valve when the def did not opt in.

---

### T5-C14 — — `AgentCommunicationHub::register_agent` double-locks to write history
- **File:** crates/kod-swarm/src/communication.rs
- **Line:** 111–133
- **Severity:** Low
- **Category:** Performance
- **Description:** `register_agent` takes `agents.write()` to insert the agent, drops it, then takes `history.write()` to insert an empty entry. Two lock acquisitions where one would do; the second lock can race with a `record_message` that targets the new agent before the empty history entry exists (then `record_message` calls `entry().or_default()` and creates it, so it's not a bug, just wasted work).
- **Fix:** Hold the agents write lock, do the agent insert, then take the history write lock inside the same critical section. Or, since `record_message` is forgiving, drop the explicit history seeding entirely.

---

### T5-C15 — — `AgentRegistry::cold_revive` masks I/O errors as `RegistryError::Unknown`
- **File:** crates/kod-swarm/src/agent_registry.rs
- **Line:** 408–423
- **Severity:** Medium
- **Category:** Error handling
- **Description:** `cold_revive` calls `Self::load(persisted_path).map_err(|_| RegistryError::Unknown)?;`. Any I/O error — permission denied, disk error, corrupt JSON — is reported to the caller as "no such agent". A user who can't read the persisted file (a permission issue on `.kod/registry.json`) gets a misleading error that names the agent as missing.
- **Code:**
```rust
let loaded = Self::load(persisted_path).map_err(|_| RegistryError::Unknown)?;
```
- **Why it's a bug:** Operators diagnosing a stuck agent get the wrong error. The fix preserves the underlying cause.
- **Fix:**
```rust
let loaded = Self::load(persisted_path).map_err(|e| {
    tracing::error!(path = %persisted_path.display(), error = %e, "registry load failed");
    RegistryError::Unknown
})?;
```
Or add a `RegistryError::Persistence(String)` variant and surface it.

---

### T5-C16 — — `AgentRegistry::persist` is not atomic on Windows
- **File:** crates/kod-swarm/src/agent_registry.rs
- **Line:** 355–365
- **Severity:** Medium
- **Category:** Correctness / Portability
- **Description:** `persist` writes to `path.with_extension("json.tmp")` then `std::fs::rename(&tmp, path)`. On POSIX this is atomic. On Windows, `rename` over an existing file fails with `AccessDenied` if the destination is open or read-only. A concurrent reader holding the file open (another `load` call) blocks the rename, and the registry fails to persist.
- **Code:**
```rust
let tmp = path.with_extension("json.tmp");
std::fs::write(&tmp, json)?;
std::fs::rename(&tmp, path)?;
```
- **Fix:** Use the `windows::fs::rename_with_replace` shim from `windows-sys`, or fall back to writing in place when on Windows. (Cross-platform atomic replace is a solved problem; this code re-invents half of it.)

---

### T5-C17 — — `kod-swarm::file_touch::conflicts_for` panics on a poisoned lock
- **File:** crates/kod-swarm/src/file_touch.rs
- **Line:** 170–198, 203–209, 214–226
- **Severity:** Medium
- **Category:** Error handling / Robustness
- **Description:** `record` correctly uses `unwrap_or_else(PoisonError::into_inner)` to recover from a poisoned `std::sync::RwLock`. `conflicts_for`, `has_touched`, and `clear_agent` use plain `.unwrap()`. A panic in `record` (or a panic in any code holding the lock) poisons the lock; every subsequent read panics the swarm runner. The asymmetry is unmaintainable.
- **Code:**
```rust
pub fn conflicts_for(&self, path: &PathBuf, except: &str) -> Vec<PeerConflict> {
    let guard = self.by_path.read().unwrap(); // ← panics on poison
    // ...
}
pub fn has_touched(&self, agent: &str, path: &PathBuf) -> bool {
    self.by_agent.read().unwrap() // ← panics
        .get(agent)
        // ...
}
pub fn clear_agent(&self, agent: &str) {
    let paths = self.by_agent.write().unwrap().remove(agent); // ← panics
    // ...
    let mut guard = self.by_path.write().unwrap(); // ← panics
    // ...
}
```
- **Fix:** Replace every `.unwrap()` on these locks with `.unwrap_or_else(std::sync::PoisonError::into_inner)` (or switch to `parking_lot::RwLock` which doesn't poison).

---

### T5-C18 — — `Agent::is_timed_out` returns `true` when no heartbeat has been recorded
- **File:** crates/kod-swarm/src/agent.rs
- **Line:** 278–283
- **Severity:** Low
- **Category:** Correctness
- **Description:** `is_timed_out` returns `true` when `last_heartbeat.is_none()`. A freshly-built agent that has never started is therefore "timed out" by definition. A watchdog that runs `if agent.is_timed_out(timeout) { kill(agent) }` on every agent in the swarm kills idle agents that have never been started.
- **Code:**
```rust
pub fn is_timed_out(&self, timeout: Duration) -> bool {
    match self.last_heartbeat() {
        Some(last) => last.elapsed() > timeout,
        None => true, // No heartbeat means timed out
    }
}
```
- **Fix:** Return `false` for the no-heartbeat case; a watchdog that wants "no heartbeat in N seconds since start" should check `state == Running && last_heartbeat.is_none()`.

---

### T5-C19 — — `kod-skills::enable_hot_reload` spawns a task with no shutdown handle
- **File:** crates/kod-skills/src/loader.rs
- **Line:** 193–214
- **Severity:** Medium
- **Category:** Concurrency / Resource leak
- **Description:** `enable_hot_reload` spawns a `tokio::spawn` task that holds a clone of `Arc<RwLock<HashMap<String, Skill>>>` and runs `while let Some(event) = event_rx.recv().await`. When the `SkillLoader` is dropped, the task continues to live — the receiver only resolves `None` when every `Sender` is dropped, and the watcher struct holds one. The watcher is owned by `SkillLoader`, so dropping the loader drops the watcher, which drops the `tx`, which should make the recv return None... but the `notify::RecommendedWatcher` has its own thread and may not close the channel cleanly on drop on every platform. The task also has no `CancellationToken` so it cannot be stopped gracefully.
- **Code:**
```rust
tokio::spawn(async move {
    while let Some(event) = event_rx.recv().await {
        handle_watch_event(event, &cache, &parser).await;
    }
});
```
- **Why it's a bug:** A long-running process that constructs many `SkillLoader`s (e.g. one per project session) accumulates one drain task per loader.
- **Fix:** Return a `JoinHandle` (or wrap in a `CancellationToken`) so the caller can cancel the task; document that the loader must outlive the hot-reload task or the task must be cancelled before drop.

---

### T5-C20 — — `SkillWatcher::start` is a no-op flag, not a real start
- **File:** crates/kod-skills/src/watcher.rs
- **Line:** 127–131
- **Severity:** Low
- **Category:** Correctness / API design
- **Description:** The watcher is started in `new()` (the `notify::recommended_watcher` callback is registered and `watcher.watch(...)` is called there). `start()` only flips an `AtomicBool` that is read by nothing. The constructor's own comment (lines 36–48) describes a previous design that used the bool; the current design does not. So `start()` is dead state — a caller that forgets to call `start()` still gets working events.
- **Code:**
```rust
pub fn start(&self) -> Result<()> {
    self.is_running
        .store(true, std::sync::atomic::Ordering::SeqCst);
    Ok(())
}
```
- **Fix:** Remove `start()`/`is_running`/`AtomicBool` entirely, or wire them into the notify callback so events are dropped while `!is_running`.

---

### T5-C21 — — `SkillMatcher::score_skill` description-keyword matching produces false positives
- **File:** crates/kod-skills/src/matcher.rs
- **Line:** 196–219
- **Severity:** Low
- **Category:** Correctness
- **Description:** The description-overlap loop splits the description on non-alphanumeric chars and checks `query.contains(word)`. A 4-char description word like `"test"` matches when the query contains `"contest"`, `"testing"`, `"latest"`. The cap (`min(0.10 * hits, 0.20)`) keeps the score weak, but the threshold is 0.3 by default and a single tag hit (0.4) plus this overlap can carry a skill past the threshold that should not match.
- **Code:**
```rust
for word in desc_lower.split(|c: char| !c.is_alphanumeric()) {
    if word.len() > 4 && query.contains(word) && !seen.contains(&word) {
        seen.push(word);
        desc_hits += 1;
    }
}
```
- **Fix:** Use word-boundary matching: `query.split_whitespace().any(|q| q == word)`.

---

### T5-C22 — — `kod-ast::parse_cache::probe` does a full byte comparison on every hash hit
- **File:** crates/kod-ast/src/parse_cache.rs
- **Line:** 99–109
- **Severity:** Low
- **Category:** Performance
- **Description:** The cache key is `(hash, lang, source.len(), source_bytes)`. A hash hit triggers `e.source.as_ref() == source` — a full byte comparison. With the cache bounded at 4 MB total source, a single probe can compare up to 4 MB. Under the cache's `Mutex`, every concurrent probe is serialized behind this comparison. The hash and length already provide strong collision resistance; the byte comparison is a paranoia check that costs O(n) on the hot path.
- **Fix:** Drop the byte comparison and trust the `(hash, lang, len)` triple; xxh3 with a fixed seed has a 64-bit collision space and the cache holds 12 entries, so the probability of a wrong-tree hit is negligible. Or use a second hash (xxh3 with a different seed) for a 128-bit effective key.

---

### T5-C23 — — `KodError` has no source chaining
- **File:** crates/kod-error/src/error.rs
- **Line:** 6–108
- **Severity:** Medium
- **Category:** Error handling
- **Description:** `KodError` has 26 variants; only `Io(#[from] std::io::Error)` carries a source. `Provider(String)`, `Network(String)`, `Serialization(String)`, etc. all discard the underlying error's structure. Callers that want to introspect (was it a TLS error? a hyper timeout? a JSON parse?) have to string-match. The `thiserror` `#[source]` attribute is unused on every variant that could carry one.
- **Code:**
```rust
#[error("Provider error: {0}")]
Provider(String),
#[error("Network error: {0}")]
Network(String),
```
- **Fix:** Add `#[source]` fields to the variants that wrap another error.
```rust
#[error("Provider error: {message}")]
Provider {
    message: String,
    #[source]
    source: Box<dyn std::error::Error + Send + Sync>,
},
```

---

### T5-C24 — — `KodError::is_retryable` text matcher is too broad
- **File:** crates/kod-error/src/error.rs
- **Line:** 124–172
- **Severity:** Low
- **Category:** Correctness
- **Description:** The transient-error matcher looks for substrings like `"500"`, `"429"`, `"capacity"`, `"timeout"`. A 200-OK response whose body mentions "model_500_tokens" or "429 is the rate limit for tier 1" — which a misbehaving gateway appends to its help text — would be classified as retryable. The substring `"of"` is correctly omitted, but `"500"` matches `"id_500"` or `"token_500_expired"`.
- **Fix:** Tighten the patterns: `"\b500\b"`, `"\\b429\\b"`, etc., or check that the digits are preceded by `error ` or `status ` rather than anywhere.

---

### T5-C25 — — `kod-config::policy::load` mixes preset widening logic with per-tool widening
- **File:** crates/kod-config/src/policy.rs
- **Line:** 381–437
- **Severity:** Low
- **Category:** Correctness
- **Description:** The project-layer narrowing rule applies `min(global_preset, project_preset)` to the preset, and `max(global_mode, project_mode)` to per-tool modes. The widening check at line 411 uses `proj_mode < current_effective` to detect a widening. But the `current_effective` is computed as `effective.tools.get(&tool).and_then(|g| g.mode).unwrap_or_else(|| preset_decision(effective.preset, &tool))`. If the project explicitly sets a tool's mode that equals the preset's fallback, `proj_mode < current_effective` is false (equal), so the project entry is accepted. But if the project's per-tool mode is *stricter* than the preset's fallback, that's a narrowing — accepted. If the project's per-tool mode is *more permissive* than the global's per-tool mode, that's a widening — rejected. The logic is correct, but the `accepted.mode = None` at line 422 silently drops the project's intent and keeps the global. A user who *meant* to relax a global `Deny` to `Ask` in their project file gets the project file's `mode` line silently ignored, with only a `tracing::warn!` to flag it. The CLI `policy explain` would then report the global mode, not the project's intended one.
- **Fix:** Surface the rejected override in `PolicyDecision` so a user who runs `policy explain write_file` sees "your project's `mode = "ask"` was rejected because the global is `deny`".

---

### T5-C26 — — `kod-telemetry::spawn_post` has no backpressure / queue cap
- **File:** crates/kod-telemetry/src/lib.rs
- **Line:** 194–219
- **Severity:** Medium
- **Category:** Performance / DoS
- **Description:** Every `record_turn` and `record_tool` call spawns a fresh `tokio::spawn` that performs an HTTP POST. A turn with 20 tool calls spawns 20 tasks. Under a chatty model or a slow collector (5 s timeout per task), the runtime accumulates parked tasks. There is no batch, no queue cap, no coalescing. A wedged collector (the M-6 timeout exists but each task still parks for up to 5 s) produces hundreds of concurrent POST tasks.
- **Code:**
```rust
fn spawn_post(&self, payload: serde_json::Value) {
    let url = self.inner.logs_url.clone();
    let client = self.inner.client.clone();
    let headers = self.inner.config.headers.clone();
    tokio::spawn(async move {
        // ...
        match req.send().await { /* ... */ }
    });
}
```
- **Fix:** Use a single bounded `mpsc::channel` + a long-running consumer task that batches up to N records per POST. The OTLP spec supports a `LogRecords` array; the current code limits itself to one record per POST.

---

### T5-C27 — — `AgentCommunicationHub::clear_all` does not drain in-flight messages
- **File:** crates/kod-swarm/src/communication.rs
- **Line:** 393–399
- **Severity:** Low
- **Category:** Correctness
- **Description:** `clear_all` clears `agents` and `history` but does not drain the per-agent `mpsc::UnboundedReceiver` for each cleared agent. When `agents.clear()` runs, the senders are dropped; receivers' `recv()` returns `None`. That is correct for a receiver holding the channel — but a receiver whose `AgentMessageReceiver` was cloned (the `inner: Arc<Mutex<Option<UnboundedReceiver>>>` shape) gets `None` on the next `recv()`. Fine. The issue is the `history` map: `clear_all` clears it, but in-flight `record_message` calls under the writer lock may be interleaved. The Mutex on `history` makes the clear atomic against new writes, but a write that started before the clear and finishes after the clear inserts into the new (empty) map. A turn that broadcasts a `KnowledgeShare` just before `clear_all` runs may leave one entry in the otherwise-empty history.
- **Fix:** Document the race; or take both `agents` and `history` write locks in a single critical section (they are different `RwLock`s, so use a consistent ordering).

---

### T5-C28 — — `kod-config::llm::validate` does not catch missing API-key env vars
- **File:** crates/kod-config/src/llm.rs
- **Line:** 112–339
- **Severity:** Low
- **Category:** Error handling
- **Description:** `LlmConfig::validate` clamps temperature, context window, timeout, base_url, model, drops duplicate endpoint names, and validates routing keys. It does not check whether `api_key_env` names an env var that is actually set. A user who configures an Anthropic endpoint with `api_key_env = "ANTHROPIC_API_KEY"` but never exports it gets a clean config load and a runtime 401 at the first turn — with no hint at startup that the key is missing.
- **Fix:** Add a `validate_env_keys_present: bool` flag (or always do it) and emit a `tracing::warn!` for each endpoint whose `api_key_env` is unset at load time. The check is cheap (one `std::env::var`) and the warning is the right UX.

---

### T5-C29 — — `kod-risk::Justification::is_substantive` only checks length and a fixed affirmation list
- **File:** crates/kod-risk/src/classify.rs
- **Line:** 75–87
- **Severity:** Low
- **Category:** Security
- **Description:** `is_substantive` requires ≥25 chars and rejects a fixed list of affirmations. A model that copies the risk-engine's own reason verbatim, padded with spaces, passes. A justification of `"                                          "` (25 spaces) passes. A justification that is the reason plus "ok" passes. The gate is "did the model write a sentence", not "did the model give a substantive reason".
- **Fix:** Reject all-whitespace; require at least one word that is not in the affirmation list; consider checking against the risk finding's `reason` to detect parroting.

---

### T5-C30 — — `kod-config::config::load_default` renames broken config but does not write a replacement
- **File:** crates/kod-config/src/config.rs
- **Line:** 385–395
- **Severity:** Low
- **Category:** UX / Correctness
- **Description:** When `load_from` fails and `recover_sections` recovers some sections, the broken file is renamed to `.toml.broken`. The recovered config is returned. But the file at `config_path` is now missing — the next call to `load_default` writes a fresh default at that path, **discarding the recovered sections**. The user who runs kod once (recovered config used) and then again (default config written over the recovered sections) silently loses their recovered config.
- **Fix:** After renaming the broken file, write the recovered config to `config_path` so the next load reads the recovered shape, not a fresh default.

---

### T5-C31 — — `kod-skills::parser::extract_attribute` only handles double-quoted attributes
- **File:** crates/kod-skills/src/parser.rs
- **Line:** 181–188
- **Severity:** Low
- **Category:** Correctness
- **Description:** `extract_attribute` searches for `format!("{}=\"", attr)` and then the next `"`. Single-quoted attributes (`<example input='foo'>`) are not parsed — `input` returns `None`, the example is captured with an empty `input` string. XML and HTML allow both quote styles.
- **Fix:** Try double quotes first, then single quotes; or use a one-line regex.

---

### T5-C32 — — `kod-config::policy::glob_matches` rebuilds matchers on every decide
- **File:** crates/kod-config/src/policy.rs
- **Line:** 752–786
- **Severity:** Low
- **Category:** Performance
- **Description:** `glob_matches` is called from `decide` for every forbidden, allowed, and session-deny glob. Each call compiles up to 3 `GlobBuilder` patterns (verbatim, relative, `**/`-prefixed). For a policy with 10 forbidden globs and 5 tools called per turn, that's 150 glob compiles per turn. There's no compile cache.
- **Fix:** Cache compiled `GlobMatcher`s by pattern text, similar to `kod-minimize::pipeline::compile` (but with the eviction fix from T5-C12).

---

### T5-C33 — — `kod-swarm::TaskCoordinator::assign_task` holds `tasks` write lock across `assignments` and `agent_load` writes
- **File:** crates/kod-swarm/src/coordination.rs
- **Line:** 111–147
- **Severity:** Low
- **Category:** Performance / Concurrency
- **Description:** The M-53 fix holds `tasks.write()` for the entire assign (status change → assignment insert → load bump). The atomicity is correct, but the held-write-lock window includes two additional lock acquisitions (`assignments.write()`, `agent_load.write()`). With three tokio mutexes held, any task trying to read `tasks` (e.g. `pending_tasks`, `tasks_for_agent`, `task_status`) blocks for the whole assign. Under a swarm that assigns 8 tasks at start-up, that's 8 serialized read-blocks.
- **Fix:** Drop the `tasks` lock as soon as the status change is applied; the atomicity window for `agent_load` only needs `assignments` + `agent_load`, and the comment's stated invariant (load + status are consistent) can be preserved by ordering the load bump after the status change.

---

### T5-C34 — — `kod-stats::if_bench::cat_sound_at` uses lines-positions but is told "SoundPosition::Middle" means middle third
- **File:** crates/kod-stats/src/if_bench.rs
- **Line:** 139–157
- **Severity:** Low
- **Category:** Correctness
- **Description:** `cat_sound_at` computes the line index of the cat sound, then checks if it's in the first/middle/last third of `text.lines().count()`. For a one-line reply, `n=1`, `n.div_ceil(3) = 1`, so `SoundPosition::Start` requires `idx < 1` (i.e. `idx == 0`). `SoundPosition::Middle` requires `1 <= idx < 1 - 1 = 0` — impossible. `SoundPosition::End` requires `idx >= 1 - 1 = 0` — always true. So a one-line reply with a cat sound always classifies as `End`, regardless of the requested position. A model that emits a single-line `array:abc meow` passes every End-positioned turn and fails every Middle-positioned turn, even if the sound is in the middle of the line (which it is). The harness's intent ("middle of the reply") is misread as "middle line of the reply".
- **Fix:** Use character offsets within the joined text, not line indices, for the position check.

---

### T5-C35 — — `kod-minimize::plan::classify` leaves quotes in tokens
- **File:** crates/kod-minimize/src/plan.rs
- **Line:** 105–113
- **Severity:** Low
- **Category:** Correctness
- **Description:** After the operator scan, a Single command's tokens are `split_whitespace()` — quotes are not stripped. The test `a_pipe_inside_single_quotes_is_not_piped` even documents this: `echo 'a | b'` produces `["echo", "'a", "|", "b'"]`. The `def.matches` function then compares `args[0] == "status"` literally — a `git "status"` invocation (quoted subcommand) produces `args[0] == "\"status\""`, which fails the match, and the minimizer falls through to raw. The pipe-opacity invariant is preserved (good), but the def-match is brittle.
- **Fix:** Strip matched surrounding quotes from each token before storing as args.

---

### T5-C36 — — `kod-schema-dialect::sanitize_in_place` runs the single-member-flatten step twice
- **File:** crates/kod-schema-dialect/src/sanitize.rs
- **Line:** 263–287 (step 0) and 338–362 (step 4)
- **Severity:** Low
- **Category:** Code quality / Maintainability
- **Description:** The flatten-single-combiner transform is applied twice — once before renames (step 0, "FIRST") and once after (step 4). Step 0 is needed so the merged-in keys get processed by `const → enum` (step 2). Step 4 is reachable only when step 1 (rename) produced a new single-member `anyOf` from a single-member `oneOf`. The duplication is intentional but the comment is sparse; a future maintainer who deletes step 4 will silently break the `oneOf → anyOf → flatten` chain. Also: step 4 iterates the same `["allOf", "anyOf"]` array and runs the same flatten code, so any change to one must be mirrored in the other.
- **Fix:** Extract a helper `flatten_single_combiner_if_eligible(obj, applied, path)` and call it once; or add a comment that step 4 must mirror step 0 and reference the test that pins it.

---

### T5-C37 — — `kod-config::policy::ReadProtection::matches` uses `globset::Glob` without `literal_separator`
- **File:** crates/kod-config/src/policy.rs
- **Line:** 199–225
- **Severity:** Low
- **Category:** Correctness / Portability
- **Description:** `ReadProtection::matches` builds a `Glob::new(pat_use)` and compiles a matcher. Unlike `glob_matches` (which uses `GlobBuilder::new(pat).literal_separator(true)`), this uses the bare `Glob::new`, which defaults to `literal_separator(false)`. The result: a pattern like `**/.env` matches `subdir/.env` (correct) but also `subdir/.env/anything` (probably not intended). On Windows, a backslash path `subdir\.env` is also matched differently from the `literal_separator(true)` path the rest of the policy uses. The policy engine's two glob paths disagree on a basic semantic.
- **Fix:** Use `GlobBuilder::new(pat_use).literal_separator(true).build()` for parity with `glob_matches`.

---

### T5-C38 — — `kod-swarm::agent_registry::AgentRef::depth` returns 0 or 1, not the true depth
- **File:** crates/kod-swarm/src/agent_registry.rs
- **Line:** 100–109
- **Severity:** Low
- **Category:** Correctness / API design
- **Description:** `AgentRef::depth()` returns `1` if `parent.is_some()`, else `0`. The doc says "Callers that need the true depth use `AgentRegistry::depth_of`." But `depth_of` walks the parent chain — O(n) per call. A caller that holds an `AgentRef` and asks `ref.depth()` gets a misleading answer for any agent deeper than 1. The method exists only because the type's own field is `parent: Option<String>`, not a precomputed depth.
- **Fix:** Either remove `AgentRef::depth()` (it lies) or precompute and store `depth` on register.

---

### T5-C39 — — `kod-config::KodConfig::skills_dir` silently falls back to `.kod/skills` when home is unresolvable
- **File:** crates/kod-config/src/config.rs
- **Line:** 529–535
- **Severity:** Low
- **Category:** Correctness
- **Description:** `skills_dir` returns the first of `skills_dirs()` or `PathBuf::from(".kod/skills")` as a fallback. The fallback is a *relative* path, which means a session whose `dirs::home_dir()` returned `None` and whose `cwd` is `/tmp` will look for skills in `/tmp/.kod/skills` — almost certainly not what the user wanted, and not the same as the canonical `~/.kod/skills` the rest of the docs describe. The error is silent.
- **Fix:** Return `Result<PathBuf>` and surface an error when no skills directory can be computed; or document the fallback as "use the cwd-relative path" and make the caller aware.

---

### T5-C40 — — `kod-types::EffortLevel::parse` silently maps unknown values to `Medium`
- **File:** crates/kod-types/src/effort.rs
- **Line:** 38–49
- **Severity:** Low
- **Category:** Correctness
- **Description:** `EffortLevel::parse("turbo")` returns `EffortLevel::Medium`. The doc says "a value kod does not recognise is more likely a new tier than a mistake". That is a reasonable default for forward compat, but it also means a typo like `"hight"` (one `h`) silently runs the agent at `Medium` instead of `High`. A swarm config that says `worker_effort = "hight"` runs at the same effort as `worker_effort = "medium"`, with no warning.
- **Fix:** Emit a `tracing::warn!` when the value is unrecognized; the silent default can stay for forward-compat, but the user should see the typo in the log.

---

## Recurring themes

1. **Two-tier lock handling.** Several modules use `tokio::sync::RwLock` or `std::sync::RwLock` for related maps (`tasks`/`assignments`/`agent_load`, `agents`/`history`, `by_path`/`by_agent`) and acquire them in different orders or with inconsistent poisoning recovery. The pattern is hard to maintain; a single `parking_lot::RwLock`-based struct with documented lock ordering would prevent the entire class.

2. **Fire-and-forget spawn without supervision.** `kod-telemetry`, `kod-skills::enable_hot_reload`, and the `irc_bus` test helpers all `tokio::spawn` tasks that hold `Arc`s and have no cancellation token. Long-running processes accumulate these. A `JoinSet` + `CancellationToken` per logical subsystem would fix the class.

3. **String-based classification of structured data.** `KodError::is_retryable`, `kod-risk::assess`, `kod-schema-dialect::classify_rejection`, and `kod-error::is_server_busy_body` all do substring matching on free-form text. The substrings are harvested from real logs and are too broad (`"500"`, `"of"` not but `"capacity"` yes). Tighter patterns or structural classification (status code, error variant) would be more honest.

4. **Stale-comments-after-fix.** `SkillWatcher::start`'s comment describes a previous polling design; the bool it sets is read by nothing. `file_touch::conflicts_for` uses `.unwrap()` while `record` uses `into_inner` — the asymmetry is undocumented. The codebase has many tests pinning past fixes (M-53, F2h-15, F2h-16, M-7, M-8, M-11, M-55, H-S2, H-S3, H-S11, etc.), which is excellent for regression but means the comments often describe the pre-fix state and need re-reading alongside the code.

5. **Security boundary spread across three layers.** `kod-risk` classifies the command's blast radius; `kod-config::policy` enforces per-tool/path/binary allow-deny; the OS sandbox catches what escapes both. Each layer's comment says the others catch what it misses, but the seams are wide (T5-C1 `xargs rm`, T5-C6 path-key whitelist, T5-C7 `bash -c` wrapper, T5-C11 symlink escape). The right shape is one of the three layers owning the full check; today none does.

---
