# kod — audit backlog

This file records the state of the 2026-10 audit
(`KOD-Audit-Report.md`) and the design items that were tracked after
`v0.1.7` was cut. **All items are closed as of `v0.1.8`.**

## Open items

None.

## Closed (v0.1.8)

- **TPD-11b** — external GitHub Actions pinned to commit SHAs
  (`9246f98`). Each `@vX` is the current tag SHA; `@stable` on
  `dtolnay/rust-toolchain` is the current `refs/heads/stable` SHA.
  Update via `git ls-remote`.

- **SEC-A-04b** — `script` / `su` wrappers unwrapped
  (`103a1a8` kod-risk, `ef14aca` kod-config). `-c CMD` escalates to
  Confirm via the `(None, [], false)` non-clean signal (matching
  `env -S`). Positional form reveals the inner program. Interactive
  form stays Safe. Both tables changed in lockstep, both tests added.

- **SEC-B-06** — async fuzz targets `acp_frame` + `mcp_line`
  (`8ef3041` lib wrapper, `46c6b82` lib helper, `3b96946` targets).
  `read_frame_for_fuzz` in `kod-core-serve` builds a single-thread
  runtime and drives the async parser over `&[u8]`. `parse_line_for_fuzz`
  in `kod-mcp` is the sync seam for the JSON parse. Fuzz build
  verified: `cargo +nightly fuzz build` compiles.

- **PERF-01 s2a** — render cache stores materialized
  `Arc<Vec<Line<'static>>>` (`3991b01` + `b79bfe3`). Probe and render
  share one markdown parse per (message, width, content). Warm frame
  is an `Arc::clone` per message, not a re-parse.

- **PERF-01 s2b** — windowed render (`d9ea5e8`). The probe collects
  `(msg_idx, start_row, end_row, has_sep)` cumulative spans; the
  render pass materializes only messages intersecting
  `[skip_rows, skip_rows + height)`. Search-jump uses row-space
  offsets from cumulative, dropping a per-frame `Paragraph::line_count`.

## Closed in v0.1.7

`git tag -n99 v0.1.7` for the full list. Summary:

- **Security** — SEC-A-01/02/03/05/06/07/08/09/10/11/13/14/15/16/17/18/19/20/21,
  SEC-B-01/02/03/04/05.
- **Correctness** — BUG-C-01..08, 11, 12; BUG-D-05..07, 10, 13..15.
- **Performance** — PERF-01a, 02, 04, 05, 06, 09, 10, 11.
- **TUI/CI/deps** — TPD-02/03/04/06/07/08/09/10/11a/12/13.

## Resolved without a code change (audit was behind tree)

Verified already-fixed in prior sessions; documented here so a future
audit does not re-chase them.

- PERF-08 — `RoundContext.definitions` caches per turn
- SEC-A-11 — `git.rs:47`
- BUG-C-10 — `retry_strategy.rs:91`
- BUG-D-04 — `edit_hashline.rs:397`
- BUG-D-09 — `todo.rs:158`
- BUG-D-12 — `tools.rs:878`
- SEC-A-15 — `context.rs:496`
- SEC-A-16 — token + prefix env filters

## Audit disposition

78 findings total:
- **70 closed with code** (v0.1.7 + v0.1.8)
- **8 already-fixed** (audit behind tree; verified in prior sessions)
- **0 open**
