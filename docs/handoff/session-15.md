# Handoff — kod, session 15

**Continues:** session 14 (`docs/handoff/session-14.md`).
**Releases:** `v0.1.10` cut at end of session.

## What v0.1.7 → v0.1.10 ships

Four releases across two sessions:

- **v0.1.7** (`808d4c1`) — mechanical batch from the 2026-10 audit.
  70 of 78 findings closed.
- **v0.1.8** (`1466f5a`) — design batch. The four audit items that
  needed a design decision: TPD-11b SHA pinning, SEC-A-04b
  script/su wrappers, SEC-B-06 async fuzz targets, PERF-01 s2
  windowed render. 0 open.
- **v0.1.9** (`3b0e09d`) — post-release bug hunt. 3 fixes (2 P1, 1
  P2) in the TUI windowed render and provider streaming retry.
- **v0.1.10** (`5ef76b1`) — subsystem-by-subsystem bug hunt. 17
  fixes across 5 P1/P2/P3 tiers. 5 subsystems diagnosed and cleared
  with evidence. Full list in the tag message.


## New lessons from this session

Beyond session 14's list (bash 3.2, `grep -c || echo`, `tokio::fs`,
kache disk pressure, `cargo check --tests`):

1. **Nextest summary format varies.** A single-test run prints
   `1 test run: 1 passed` (singular). The plural grep
   `1 tests run: 1 passed` misses it and every "STILL FAILING"
   I hit for that reason was actually a green run. Use
   `tests? run` in every nextest matcher.

2. **Test scoping trap: source-level pin self-reference.** A test
   that reads the source file it lives in and asserts
   `!src.contains("needle")` will always fail if the test's own
   code contains the needle string. Fix: scope the search to the
   *production* region (a named fn, or everything before
   `#[cfg(test)]`), not the whole file. Rounds 76, 92, 102, and
   110 all cost a round to this exact class.

3. **Test scoping trap: sibling module visibility.** A
   `#[cfg(test)] mod foo { ... }` at file scope cannot see a
   `const` or `fn` declared inside another `mod tests` in the same
   file. If a fixture is private to the existing test module,
   either land the new test *inside* that module or build the
   fixture from literals in the new test's own scope. Rounds 110
   and 112 both cost a round to this.

4. **Test scoping trap: "before first #[cfg(test)]" is not
   "before the impl".** When a file has multiple test modules and
   the code under test appears *after* the first one, cutting at
   `src.find("#[cfg(test)]")` excludes the impl. Scope to the
   impl by name (`src.find("impl Foo")` then brace-scan).

5. **`src[j]` on a `&str` does not compile.** `str` is not
   indexable by `usize`. Use `src.as_bytes()[j]` for byte checks
   or `chars()` for char checks. When brace-matching in a
   source-level test, work on `as_bytes()`.

6. **Struct-literal construction needs the real field set.** A
   test that constructs a struct from the module needs the exact
   field list and type names — check the diagnostic before writing
   the test (`grep -n "pub struct Foo"`). Round 115 burned a round
   on `Pricing` not existing (the struct is `PricingTable` or
   similar).

7. **The fork-pressure mitigation is `--test-threads 2` on the
   full workspace suite.** Round 78's 9 `exec_failed` at default
   thread count were fork-exhaustion, not code defects; they
   vanished at reduced concurrency. Use `CARGO_BUILD_JOBS=1
   --test-threads 2` for the release gate, not for per-crate
   iteration.

8. **Long heredocs truncate at ~120-150 lines.** The v0.1.10 tag
   message truncated in round 124 and cost a whole round. For any
   tag or file write over ~80 lines, use `-m` flags or write in
   verified chunks with `wc -l` after each.


## Where to start

**Stop the static bug-hunt arc here.** 20 fixes since `v0.1.9` (3
in v0.1.9 + 17 in v0.1.10), 5 subsystems cleared with evidence,
and every remaining static candidate is either covered by the
audit's verified-clean lists or in a genuinely low-yield file.

The next productive hunt needs **behavioral integration
scenarios**, not another static pass:

- **Real model turn** — spin up a provider against a real endpoint
  (or a recorded mock), exercise the streaming assembly, tool
  dispatch, and compaction paths end to end.
- **Real ACP client** — drive the ACP server with an actual editor
  (Neovim, Zed) to surface protocol conformance issues that unit
  tests miss.
- **Real MCP server** — spawn `@modelcontextprotocol/server-*`
  packages, exercise tool discovery, tool calls, shutdown,
  reconnection.
- **Real LSP server** — start `rust-analyzer` / `pyright`, verify
  diagnostics, hover, definition, document sync against real
  servers rather than mocked responses.

Each of these is a distinct session's worth of setup and yields
findings in a class that static analysis cannot surface (timing,
protocol edge cases, real-process lifecycle).

## Environment quirks (unchanged from session 14)

See `docs/handoff/session-14.md`:
- `declare -A` absent on bash 3.2
- `grep -c X || echo 0` gives `"0\n0"`
- `tokio::fs` not in workspace features — use `spawn_blocking`
- kache disk pressure can EAGAIN on rustc fork; `rm -rf target`
- Long heredocs truncate
- `cargo fmt --all` exposes `needless_borrow` clippy warnings

## Known-good state

- HEAD: `v0.1.10` tag (`5ef76b1`)
- Working tree: clean
- Clippy: 0 warnings, `-D warnings`
- `cargo fmt --check`: clean
- Tests: **4341 passing, 3 skipped**
- Fuzz build: all targets compile

## What NOT to do

- Do not re-audit anything the 2026-10 audit, session 13, or this
  session marked cleared.
- Do not mechanical-patch a source-level test that reads its own
  file — scope to the production region.
- Do not attempt to reuse a fixture from a sibling test module
  without either moving the test into that module or inlining the
  fixture.
- Do not write a tag message or handoff file in one heredoc — split
  it.
- Do not run the full workspace suite at default thread count on
  this host; the fork pressure produces false `exec_failed`.
