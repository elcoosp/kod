# Handoff — kod, session 13

`v0.1.7` cut at HEAD. Session 12's mechanical-batch arc completed
here; see `docs/handoff/session-12.md` for environment quirks.

## What v0.1.7 closes

Full list in tag message (`git tag -n99 v0.1.7`). Summary:
- Security: SEC-A-01..21, SEC-B-01..05 (all P0/P1)
- Correctness: BUG-C-01..08, 11, 12; BUG-D-05..07, 10, 13..15
- Performance: PERF-01a, 02, 04, 05, 06, 09, 10, 11
- TUI/CI/deps: TPD-02..04, 06..13

Gates: 4321 passing / 3 skipped, clippy 0 warnings, fmt clean.

## What remains

See `docs/BACKLOG.md` for the 4 design items with decision shape.
Nothing mechanical is left — each remaining item needs a design
decision or a network call.

## New environment quirks from this session

1. **`declare -A` fails on macOS bash 3.2.** Round 5 aborted; use
   string-dedupe (`case ",$list," in *",$needle,"*)`).

2. **`grep -c X || echo 0` produces `"0\n0"`.** The `||` runs even
   when grep prints `0` and exits 1. Use `grep -c X; true` or
   capture + strip.

3. **`tokio::fs` is not in the workspace feature set.** Round 34's
   `tokio::fs::read_to_string` failed to compile. Use
   `spawn_blocking` for sync file IO in async contexts (round 35).

4. **Long heredocs truncate at ~100-150 lines** (this session: round
   46's BACKLOG write cut at "Recommended approach for the next
   session"). Write long files in chunks with `wc -l` verify, or
   defer to a separate round.

5. **kache disk pressure**: 34G `target/` + 49G kache cache at 13 GiB
   free triggered a round-17 flake where rustc died before emitting
   JSON diagnostics (no `error[E0XXX]`). Recovery: `rm -rf target`
   (never kache). Rounds 18+ added a baseline-check step.

6. **Clippy `needless_borrow` surfaced only after `cargo fmt --all`**
   in round 52. The fmt reflow exposed `&file` where `file:
   &PathBuf` from the loop iterator. `cargo clippy --fix` cleared it.

## Where to start

1. `docs/BACKLOG.md` — 4 items, each with decision shape.
2. `git log v0.1.6..v0.1.7 --oneline` — 105 commits.
3. Do not batch. One finding, one branch, compile gate as the
   completion signal.
