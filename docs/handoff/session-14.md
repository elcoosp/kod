# Handoff — kod, session 14

**Continues:** session 13 (`docs/handoff/session-13.md`).
**Releases:** `v0.1.7` cut mid-session; `v0.1.8` cut at end of session.

## What this session closes

The 4 design items left open at `v0.1.7` are now closed. `v0.1.8`
sits on top of 13 post-`v0.1.7` commits:

- **TPD-11b** — 8 external actions pinned to commit SHAs.
- **SEC-A-04b** — `script`/`su` wrappers, both tables, 2 regression
  tests.
- **SEC-B-06** — `acp_frame` + `mcp_line` fuzz targets, verified
  build.
- **PERF-01 s2a** — render cache holds `Arc<Vec<Line<'static>>>`;
  warm frame is one `Arc::clone`, not a re-parse.
- **PERF-01 s2b** — windowed render; materializes only visible
  messages.

`docs/BACKLOG.md` records the full disposition: 70 closed with code,
8 already-fixed, 0 open.

## v0.1.7 → v0.1.8 sequence

`v0.1.7` was cut at `808d4c1` — before the backlog closure work landed.
`v0.1.8` supersedes it with the 13 additional commits. `v0.1.7`
remains valid for anyone who fetched it; `v0.1.8` is the "audit fully
closed" release.

Tag messages carry the full ID lists.

## Environment quirks (unchanged from session 13)

See `docs/handoff/session-13.md` "New environment quirks":
- `declare -A` absent on bash 3.2
- `grep -c X || echo 0` gives `"0\n0"`
- `tokio::fs` not in workspace features — use `spawn_blocking`
- Long heredocs truncate ~100-150 lines — write in chunks with
  `wc -l` after
- kache disk pressure can trigger EAGAIN on `rustc` forking; `rm -rf
  target` when free space drops under ~30 GiB; never delete kache
- `cargo fmt --all` can expose `needless_borrow` clippy warnings in
  code that compiled fine — run `cargo clippy --fix` after fmt

## New from this session

1. **Test-compile gate is separate from lib-compile.** `cargo check -p X`
   compiles the lib only. `cargo check --tests -p X` compiles the test
   binaries. Round 63 caught a `Vec<String>` vs `Vec<&str>` mismatch
   that `cargo check` missed — this discipline matters for new tests.

2. **Anchor-and-dump discipline for structural patches.** Large
   function rewrites (render loop, provider retry, landlock apply)
   need a diagnostic round first: dump the exact function body with
   line numbers, then patch with anchors matched to what's there.
   Round 71's 250-line dump made round 72's replacement compile
   first-try. Rounds that skipped this (22, 34, 42, 58, 70) each
   cost a revert.

3. **The backlog is genuinely empty.** No known mechanical or design
   items from the 2026-10 audit remain. The next work is
   user-reported bugs or new features — not audit findings.

## Where to start

1. `git log --oneline v0.1.6..v0.1.8` — the two-release arc.
2. `docs/BACKLOG.md` — disposition of every audit finding.
3. `docs/BACKLOG.md` is the "what this repo has" record. Do not
   re-chase any "Resolved without a code change" item; each was
   verified already-fixed at the SHAs listed.

## Known-good state at end of session 14

- HEAD: `v0.1.8` tag
- Working tree: clean
- Clippy: 0 warnings (workspace, all-targets, `-D warnings`)
- `cargo fmt --check`: clean
- Tests: pass (exact count from round 73's test run)
- `cargo deny check bans`: ok (TPD-10 skip documented)
- Fuzz build: both sync + async targets compile
