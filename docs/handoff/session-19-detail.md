# Session 19 — detail

Companion to `session-19.md`. The starting document is the
short entry point; this file carries the narrative for rounds
that land. Read `session-19.md` first.


## Rounds landed

### R1 — `repomap` extractor properties (`c934ab0`)

Eight properties generalizing the AST/regex agreement example
tests, plus `extract_imports` coverage for `rs`. The example
tests `the_ast_and_regex_rust_paths_agree`,
`the_ast_and_regex_paths_agree_for_every_language`,
`the_paths_agree_past_the_symbol_cap` were already the right
properties as single examples; this generalizes them across an
input generator. Small round — generalizes rather than fills a
gap.

Shape note: `Symbol` does not derive `PartialEq`, so the
properties compare through a `sig(&[Symbol]) -> Vec<(kind,
name, line)>` helper. The existing example tests do the same.

### R2 — Go block imports lose all but the last (`6945a77`)

**Real bug found.** `extract_imports`'s `go` branch had a
second pattern written as a single regex with a repeated inner
capture group:

    r#"(?m)^\s*import\s*\(\s*
    (?:\s*['"]([^'"]+)['"]\s*
    )*"#

Rust's `regex` crate keeps only the **last** capture across a
repetition. A Go block import with three imports therefore
returned one:

    import (
        "a"
        "b"
        "c"
    )

produced `["c"]`, not `["a", "b", "c"]`. The pattern was
intended to handle exactly this shape.

The property `prop_go_block_import_all_returned` was written
first, ran against the pre-fix code, and failed with minimal
input `a="a", b="a_", c="a0"` → got `["a0"]`. The fix replaced
the broken pattern with a line-by-line scan for `import ( ...
)` blocks; all 10 new properties and all 48 existing repomap
tests pass. Regression seed committed at
`kod-core-quality/proptest-regressions/repomap.txt`.

Caveat on the pre-fix round: the classifier in the first
attempt at this round mis-parsed its own nextest output and
reported "unexpected failure class", which aborted the script
before applying the fix. The second attempt ran the fix
directly against the known-good failure and committed clean.
The classifier bug was a shell-script mistake, not a Rust bug,
but it delayed the fix by one round. Noted here so a future
agent reading the two commits in sequence understands the gap.

**Pre-fix failure surface:** callers of `extract_imports("go",
_)` inside `build_repo_map` silently lost two-thirds of every
block-imported Go file's imports. The repo map's import
graph was wrong for any Go project using parenthesized
imports — which is the canonical `goimports` formatting.

**Not fixed, deliberate:**
- `build_repo_map` — needs `tempfile` fixtures; the L51 flake
  shape at scale. `build_repo_map_walks_a_tempdir` already
  covers it as an example test.
- `render` — `render_honors_budget` already covers it.
- `looks_like_a_repo` — filesystem walk; example-testable but
  low value.

### Handoff state after R2

- `repomap` is now property-covered on every pure function.
  Filesystem- and budget-shaped functions remain
  example-tested.
- `kod-core-quality` no longer has any one-round target: the
  remaining modules (`worktree`, `worktree_isolation_ownership`)
  are `tempfile` + `git init` + `libc::kill` shape — real
  work, but the L51 flake at scale, and outside session 18's
  cadence.
- Next pure-function target outside `kod-core-quality`:
  `kod-telemetry::otlp::{turn_payload, tool_payload}`. Pure
  `(&Config, &Record) -> serde_json::Value`; no proptest
  dev-dep yet. Survey pending.
