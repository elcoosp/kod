# Session handoff — kod (2026-10-02, second)

**Repo:** `/Users/adm/Documents/Repos/kod`
**Branch:** `main`  **HEAD:** `aa1309be`

Supersedes the earlier 2026-10-02 handoff. Full workspace lib
suite: **3571/3571 green**.

---

## 1. This session added (on top of the first handoff)

### Perf LOWs (6)
- `1fdf0524` F2e-6 — safe_cutoff interval-merge O(n log n)
- `fe64219c` F2a-13 — process-wide regex cache
- `026d603e` F2g-9 — thread-local TTL dir cache for path completion
- `9943ed0c` F2b-11 — TTL cache on InstructionChain::load
- `96d3fc8e` F2c-11 — cited-file reads via spawn_blocking
- `c418f00a` F2c-9 — background spool retention sweep

### Structural LOWs (7)
- `b05f5f6e` F2c-8 + F2c-10 — evict timed-out approval/question
  senders; cancel every transcript key on shutdown
- `c502dc18` + `b516d394` F2d-11 — failed dependency gates dependents
- `37ee7e57` + `cb5d705f` F2i-8 — complete() retries transient 503
- `a0d80403` F2h-12 — round-robin rotates only among equal-load ties
- `2899779e` F2h-15 — WaiterGuard Drop cleans up cancelled send_await

### Dedicated regression tests
- `d8a918c0` F2h-12, `f036f2f6` F2h-15, `325a9c82` F2c-9+F2c-10,
  `9b16bd65` F2i-8, `aa1309be` F2d-11

---

## 2. Two fixes were inert on first write — caught by tests

1. **F2i-8.** The first commit (`37ee7e57`) wrapped only
   `.send()` in `with_retry`. A 503 is a *successful* HTTP
   exchange, so `with_retry` saw `Ok(resp)` and never retried.
   `cb5d705f` moves the status check inside the closure. Proven:
   the test fails against `37ee7e57` (hits==1), passes after.

2. **F2d-11.** The first commit (`c502dc18`) only stopped
   inserting a failed subtask into `completed`. The wave loop's
   cycle-fallback (`if ready.is_empty() { run blocked anyway }`)
   fired exactly when a failure left nothing ready, so dependents
   still ran. `b516d394` adds a `failed` set + three-way
   classification. Proven: a probe showed `[A,B,C]` both pre-fix
   and after `c502dc18`, `[A]` after `b516d394`.

**Lesson:** a passing `cargo check` and even a green existing
suite prove nothing about a fix that has no test targeting it.

---

## 3. What remains

### F2c-12 — PARTIAL (`87b7d068`)

The audit listed two costs. One is fixed, one is not:

1. **Per-message secret re-obfuscation — FIXED.** `SecretVault::obfuscate`
   cloned and re-sorted the whole secrets map on every call (once per
   message per round). Now a longest-first `Arc<Vec>` cached in
   `VaultInner.sorted`, invalidated by `register`. Tests pin the
   invalidation and the ordering.

2. **Full-history deep-clone — NOT fixed.** `messages.clone()` in the
   grounded-request build is still there. It is *required* while
   `CompletionRequest.messages: Vec<ChatMessage>` owns its messages;
   removing it needs `Arc`/`Cow` on that field plus a provider-side
   adjustment (18 read sites across 3 provider crates). That is a
   cross-crate refactor, not a LOCAL change — deliberately not attempted.

### Bisect caveat (unchanged)
`6e6a1a1` (`GenPhase::ServerBusy`) and `65cd320` (`Event::ServerBusy`)
do not compile in isolation — an enum variant lands before its match
arm. Consequence of one-commit-per-file; HEAD is correct.

### Stale audit
~12 of the audit LOWs were already fixed before this work (the
report is ~50 commits stale). The MEDIUM table is exhausted.

---

## 4. Traps

1. **Verify with a test that bites.** Two fixes this session were
   inert until a dedicated test exposed them (F2i-8, F2d-11).
   A negative-proof (run the test against the pre-fix code) is the
   only real evidence a test bites.
2. **Per-file staging is content-unsafe.** `staged == "$FILE"`
   checks the set, not the content. Stage hunks when two
   workstreams share a file.
3. **The file watcher truncates long scripts.** Write big files in
   small `printf` appends; keep each script short.
4. **Blanket string replace hits unintended sites.** Replacing
   every `child.start_kill()` also rewrote the helper's own
   fallback line (infinite recursion). Anchor + verify.

## 5. Environment

```bash
ulimit -n 65536
export CARGO_BUILD_JOBS=6
```

Without these, `cargo check` fails with a fake EAGAIN.
