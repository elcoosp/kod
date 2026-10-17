# kod — design backlog (post-v0.1.7)

Source: `KOD-Audit-Report.md` at the repo root. This file records the
items from that audit that need a design decision before patching.

## Open — design items

### PERF-01 steps 2-3 — TUI windowed render
Step 1 (LRU markdown cache) landed in v0.1.7. Steps 2-3 rebuild the
render path from "materialize every message's lines every frame" to
"materialize only visible messages".

**Decision needed:** criterion benchmark proving the win, then
`ChatWidget::render` restructuring around `(message_index,
offset_within_message)` from the existing row-count probe.
**Effort:** 1 bench round + 2 implementation rounds.
**Files:** `kod-tui/src/ui/chat.rs`, `kod-tui/src/render_cache.rs`,
new `kod-tui/benches/`.

### TPD-11b — GitHub Actions SHA pinning
`@master` → `@stable` landed. Full SHA pinning remains for
`actions/checkout@v4`, `actions/cache@v4`, `actions/{up,down}load-artifact@v4`,
`codecov/codecov-action@v4`.

**Decision needed:** resolve current SHAs via `git ls-remote`.
**Effort:** 1 round (network-dependent).
**Files:** `.github/workflows/{ci,release}.yml`.

### SEC-A-04b — `script` / `su` wrappers
11 of 13 wrappers in both tables. Remaining two have an OPTIONAL
positional (filename for `script`, username for `su`) that a
mechanical entry misclassifies.

**Decision needed:** per-wrapper custom parser distinguishing the
optional positional from the command.
**Effort:** 1 round per wrapper + test.

### SEC-B-06 — async fuzz targets (`acp_frame`, `mcp_line`)
Sync fuzz targets (`extract_json`, `schema`) land in v0.1.7. The two
async targets need the parse logic extracted from `async fn` bodies
that own `tokio::io::AsyncRead`.

**Decision needed:** extract `parse_frame_bytes(&[u8]) -> Result<T>`
in `kod-core-serve::acp` and `kod-mcp::client`.
**Effort:** 2 rounds (one per parser).
**Files:** `kod-core-serve/src/acp.rs`, `kod-mcp/src/client.rs`.

## Resolved without a code change (audit was behind tree)

- PERF-08 (tool-def churn) — `RoundContext.definitions` caches per turn
- SEC-A-11 (run_git env) — `git.rs:47`
- BUG-C-10 (TurnFailure order) — `retry_strategy.rs:91`
- BUG-D-04 (edit payload) — `edit_hashline.rs:397`
- BUG-D-09 (todo unblock) — `todo.rs:158`
- BUG-D-12 (append fsync) — `tools.rs:878`
- SEC-A-15 (seatbelt kube) — `context.rs:496`
- SEC-A-16 (PASSPHRASE/MYSQL_) — token + prefix filters

## How to use

Pick one item. Open a branch. Resolve the "decision needed" first —
compile gate + tests are the completion signal. Do not batch.
