# Session handoff — kod (2026-10-02)

**Repo:** `/Users/adm/Documents/Repos/kod`
**Host:** macOS aarch64, rustc 1.98.1, edition 2024
**Branch:** `main`
**HEAD:** `d52a2718`

---

## 1. TL;DR

Three workstreams this session, all landed and pushed:

1. **Audit deferred MEDIUMs** — all closed (M-11, M-13, M-16, M-22,
   M-39, M-40, M-44, M-47, M-48).
2. **Server-busy / HTTP 503 overload retry** — the parallel agent's
   feature, committed one-file-per-commit.
3. **LOW findings + the W-series** (14 write/edit/patch bugs from
   `docs/bugs/BUG_REPORT_WRITE_EDIT_PATCH_2026-10-02.md`).

`cargo nextest run --workspace --lib` = **3565/3565 green**.
`cargo check --workspace --tests` clean (1 pre-existing unused-import
warning in `kod-core/src/serve.rs`).

---

## 2. Commits this session (on top of `2509a09`)

### Audit MEDIUMs
| Commit | Finding |
|---|---|
| `50389fe` | M-22 / F2d-7 — subtask worktrees keyed by capability |
| `5767995` | M-47 / F2h-2 — one stdin reader for the chat REPL |
| `84fee6e` | M-48 / F2h-3 — Ctrl+C cancels the running turn |
| `7f57048` + `575d5cc` | M-40 / F2f-21 — artifacts per-holder + tests |
| `6a7e7f9` | M-39 / F2f-16 — re-validate write parent (TOCTOU) |
| `45164a6` | M-44 / F2g-7 — chat rows from cached probe |
| `67cc3e6` | stale test left failing on main by `2509a09` |
| `e674d51` | M-16 — failed attempts no longer duplicate rounds |
| `f60d5fc` | M-13 — dying round's tool calls run when the turn ends |
| `8604eae` | M-16 regression test |

### Server-busy / 503 overload (13 files, one commit each)
`b63cec0` `5121d54` `ec3ad8a` `bf71582` `3081c22` `ba729d7`
`6e6a1a1` `1cb7558` `8ff62bc` `65cd320` `e9a520a` `80cf894` `e7a4a4b`

### LOWs
| Commit | Finding |
|---|---|
| `7242a45` | F2i-6 — extract_json skips braces in strings |
| `13606c9` | F2a-11 — fd-prefixed truncating redirects |
| `9183786` | F2f-17, F2f-23, F2h-11, F2h-19 |
| `f4d7a92` | F2d-10 — drop pending question sender |
| `093e28d` | F2d-14 — bound ACP header loop |
| `46e68af` | F2e-9 — cap lsp_* read |
| `f631ed7` | F2h-16 — record after delivery |
| `461976b` | F2i-9 — update() hygiene |
| `025f3fe` | F2a-12 — canonicalize off worker |
| `c53507c` | F2g-14 — $EDITOR draft 0o600 |

### W-series (write/edit/patch)
| Commit | Bug |
|---|---|
| `8050eb3e` | W8, W9 — empty-file patches, empty-old-range |
| `af8319b6` | W6, W10 — patch emitter |
| `9b87ee4b` | W3, W4, W5, W12 — hashline |
| `d7029f76` | W7, W14 — new-file patch, process-tree kill |
| `0a8472a5` | W1, W2, W11 — recovery path |
| `976d2888` | the bug report doc |
| `d52a2718` | W1/W2/W11 regression tests |

---

## 3. BISECT CAVEAT — read before `git bisect`

Two intermediates in the server-busy range do **not** compile (an
enum variant lands in one file before its match arm in another):

- `6e6a1a1` (`GenPhase::ServerBusy`) — non-exhaustive match in
  `ui_state.rs`, not yet committed.
- `65cd320` (`Event::ServerBusy`) — non-exhaustive match in
  `main_loop.rs`, not yet committed.

`e9a520a` onward compiles. This is the cost of one-commit-per-file
for an enum+handler feature; HEAD is correct. Squashing
`6e6a1a1..=e9a520a` would fix it but rewrites history — not done.

---

## 4. What remains

### W13 does not apply
The bug report claims `kod-tools` did not compile (missing
`claims_git_readonly`). **Our tree compiles** — `context.rs` here
never had the break. No patch applied; the field is wired where it
matters (`sandbox/landlock.rs`).

### No deferred MEDIUMs or HIGHs remain
The audit's MEDIUM table is exhausted. ~12 of the listed LOWs were
already fixed before this session (the audit is ~50 commits stale):
F2g-11, F2e-7, F2i-4, F2e-5, F2h-13, F2h-14, F2h-17, F2f-19, F2f-20,
F2f-22, F2g-12, F2g-15.

### Perf LOWs not done (each a real change)
- F2e-6 `safe_cutoff` O(n²)
- F2g-9 completion `read_dir` per frame
- F2a-13 minimize regex recompiled per call
- F2i-10 memory full-table deserialize per op
- F2b-11 instructions re-read per turn
- F2c-11 / F2c-12 blocking fs / history clone in async hot paths

### Structural LOWs not done
- F2c-8 approval oneshot eviction on timeout (F2d-10 was the Jev
  twin and IS fixed)
- F2c-9 background spool retention
- F2c-10 shutdown cancels only the default transcript key
- F2d-11 failed subtasks inserted into `completed`
- F2h-12 round-robin cursor defeats least-loaded dispatch
- F2h-15 irc_bus waiter leaks on cancelled `send_await`
- F2i-8 anthropic `complete()` has no transient retry

---

## 5. Traps learned this session

1. **Per-file staging is not content-safe.** `staged == "$FILE"`
   checks the file *set*, not the *content*. The parallel agent had
   an in-progress `Event::ServerBusy` edit; a file-level stage swept
   it into a test-only commit. Fix: `git reset --soft` + an
   `awk`-filtered hunk patch. **When two workstreams share a file,
   stage hunks, not files.**

2. **Verify fixes with the target test, not `cargo check`.** Three of
   my own fixes were wrong on first write and only failed at test
   time: M-16's snapshot placement, W9's test context, W11's test
   premise. `cargo check` passed all three.

3. **The file watcher truncates long scripts.** A large heredoc gets
   cut mid-save and the partial file runs. Write big files in small
   appends; keep scripts short. Cost several retries this session.

4. **Blanket string replace hits unintended sites.** Replacing every
   `child.start_kill()` also rewrote the helper's own fallback line
   (infinite recursion). Anchor replacements, then verify.

5. **`cargo check` after a fix is not enough for `engine/mod.rs`.**
   The crate is ~20k lines; run the filtered engine tests.

---

## 6. Environment

```bash
ulimit -n 65536
export CARGO_BUILD_JOBS=6
```

Without these, `cargo check` fails with a fake EAGAIN. No
`timeout` binary on macOS — background + `kill` for a bound.
The repo `.cargo/config.toml` sets `rustc-wrapper = "kache"`;
present on this host.

---

## 7. Next prompt is probably

- "do the perf LOWs" — six items in section 4, each a real
  change; start with F2a-13 (regex cache) and F2g-9 (read_dir),
  the two most contained.
- "do the structural LOWs" — seven items in section 4.
- Or nothing: the audit is effectively exhausted of cheap work.
