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

## Post-v0.1.8 bug hunt

After `v0.1.8` was cut, a targeted bug-hunt pass on recently-changed
code (the TUI windowed render and provider streaming retry) found
three bugs. All fixed; `v0.1.9` ships the fixes.

### Confirmed bugs

- **P1: windowed render omitted streaming body** (`d7f699a`,
  `crates/kod-tui/src/ui/chat.rs`). The windowed branch of
  `ChatWidget::render` materialized only `cumulative[]` messages.
  The streaming body is not a `Message`, so a user watching a reply
  stream in with a scrolled viewport saw blank rows where the
  in-progress text should appear. Fix: append the tail (hidden
  footer + streaming body) when the viewport reaches it
  (`after_msg_row < skip_rows + height`).

- **P1: windowed render double-skip** (`d7f699a`, same file). The
  windowed loop front-skipped `skip_rows_usize - msg_start` rows of
  the first visible message, then used the same value as the
  Paragraph scroll offset. Content placed one message's worth too
  far down. Fix: drop the front-skip; the Paragraph scroll alone
  handles the offset.

- **P2: no per-attempt deadline on streaming** (`3b56e43` anthropic,
  `81d998d` openai). Both providers' streaming read loops wrapped
  `.next()` in a bare `while let Some(...)` with no wall-clock
  bound. A wedged or hostile endpoint trickling unique chunks
  never surfaced an error — `tracker.is_safe_to_retry` never fired
  because there was nothing to retry. `MAX_SSE_LINE_BYTES` bounds
  ONE line, not the attempt DURATION. Fix: 300s per-attempt
  deadline; timeout surfaces as a retryable transport error.

### Cleared hypotheses (no fix needed)

Verified correct by diagnostic; recorded so a future audit does not
re-chase them:

- **`drop(permit)` symmetry** — both providers have two drop sites
  matching the two retry branches (lines 819/1029 OpenAI,
  719/933 Anthropic).
- **Per-attempt buffer / tracker** — `tracker`, `buffered`,
  `attempt_deadline` all declared inside the attempt scope; no
  stale state between retries.
- **Anthropic SSE line cap is correct for `bytes_stream()`** —
  chunk-wise accumulation with a post-chunk check is bounded to
  `MAX + one_chunk`, never unbounded. The `.take()` pattern from
  ACP/MCP applies to `AsyncRead::read_line`, not chunked streams.
- **OpenAI SSE line cap: not needed at this layer** — the stream
  is decoded inside `adk-model`, yielding parsed `Part::*` variants,
  not raw bytes. Nothing to bound at the kod layer.
- **`su -- -c foo`** — nonsensical at runtime; the classifier's
  behavior matches reality.
- **`resolve_path(&absolute)`** — accepts absolute correctly
  (`context.rs:1086-1091`).
- **PERF-06 `spawn_embed` cache invalidation gap** —
  `metadata.embedding` is not part of the cached retrieval view;
  cosine comes from `vector_index`.
- **`as u16` casts** — every site bounded via `.min(u16::MAX)` or
  source already `u16`.

### TPD-01 disposition note

The round-36 commit (`9fbb46f`) claimed "retrying" in the message
but delivered only the "surface" half — the audit's actual goal
(surfacing the in-band error rather than reporting the truncated
turn as success). The retry optimization the sketch implied
(provider-level retry on in-band error when the attempt hasn't
committed) is not present. Downstream chain-fallback handles
recovery. Not a bug; documentation mismatch noted for the record.

### Release notes

`v0.1.9` is a bug-fix release on top of `v0.1.8`. Same gates:
tests, clippy `-D warnings`, fmt --check. See the `v0.1.9` tag
message for the full narrative.
