# Session 20 detail — kod

Starting doc: `docs/handoff/session-20.txt`. This file is the narrative
and the commit-level record the starting doc pointed at but did not
contain.

## State at the end of the session

- HEAD: `1b0c5ba` (strutil properties).
- Tag: `v0.1.15` -> `612d26e`, still on origin. **No new tag cut.**
- 21 commits ahead of `origin/main` (`a37f8a4`). Nothing pushed.
- Working tree clean.
- **Full-workspace gate GREEN at `1b0c5ba`:** 5035/5035 passed, 1 leaky,
  5 skipped, exit 0, 80 s. First full run since `5ba0dec` (4973/4973);
  the host was quiet, no `EAGAIN`. The 62-test delta is this session's
  new properties plus the pre-existing drift.

## The 19 local commits

Generated, not transcribed:

```
0b9c9c7 fix(kod-tools): treat newline as a top-level separator in the bash interceptor
ebeb5e3 test(kod-core): property tests for the stream control-marker protocol
cb9564e fix(kod-types): resolve_tool_name strips all functions. prefixes
3916f34 test(kod-types): property tests for the secret scanners
f110082 test(kod-types): security and round-trip properties for SecretVault
d545771 chore(kod-config): apply pending rustfmt drift
9dcd0c0 test(kod-config): recover_sections return-shape and config_version guards
6f00d6d chore(proptest): commit the judgment colon-id regression seed
490f6da fix(kod-provider): reject ":" in judgment question ids
9ac40be test(kod-swarm): history semantics for Blackboard::put
5ba0dec fix(kod-swarm): has_touched recovers from a poisoned lock
c5461ee test(kod-swarm): conservation properties for YieldQueue::drain
b12cd45 test(kod-swarm): property tests for CleanseScheduler::take_batch
d6c6332 fix(kod-swarm): take_batch returns None on a zero budget
acbde6a chore(proptest): commit the WorkPool FreshAgents regression seed
f29e6e3 fix(kod-swarm): WorkPool::dispatch drops the item under FreshAgents
b1184ef fix(kod-core): tool_quota::check saturates the soft-threshold arithmetic
5305cd9 test(kod-provider): property tests for the JSON Schema validator
000c7c2 test(kod-tools): property tests for TodoTracker state machine
```


## Rounds opened and closed this session

Five commits on top of the handoff's `d545771`:

| Commit | Crate | Outcome |
|---|---|---|
| `f110082` | kod-types | SecretVault security + round-trip: 6 props, 3 controls. Harden. Resolved the handoff's determinism worry by reading the body: `Reverse(len)` over a stable sort cannot tie on two secrets where one is a substring of the other, because equal-length strings are never substrings of each other. |
| `3916f34` | kod-types | Secret scanners (`scan_env`, `scan_vendors`, `scan_connection_urls`, `scan_text`): 7 props. Harden. The security contract is `scanned value never survives obfuscate` plus `deobfuscate` restoring the text. Pinned, not fixed: `scan_text` runs vendors before URLs, so a URL password that also matches a vendor regex keeps the `Vendor` label. `Discovered.origin` is dropped by `register_discovered` and `SecretSource` is telemetry-only, so the secret is redacted either way. |
| `cb9564e` | kod-types | **Real bug.** `resolve_tool_name` stripped exactly one `functions.` prefix, so a doubly-prefixed name (`functions.functions.read_file`) resolved to `functions.read_file`, which is not a registered tool — and `resolve(resolve(x)) != resolve(x)`. Seed `functions.functions.` committed with the fix (strip in a loop). |
| `ebeb5e3` | kod-core | Stream control markers: 7 props + an exhaustive collision test (each builder output claimed by exactly one of 13 parsers). Harden. `tool_done`, `turn`, `tool_approval`, `tool_approval_batch`, `format_duration_ms` had no test at all; the code was correct. |
| `0b9c9c7` | kod-tools | **Real bug.** `has_top_level_operator` did not treat `\n` as a top-level separator, and `split_shell_words` did not split on it. A two-statement command whose first statement is grep/cat/find was tokenized as one command and misrouted into a single garbled redirect instead of passing through to the shell. Not an injection hole (nothing is spawned on intercept), but a documented-contract violation. Seed `a\na` committed with the fix. |

## Deferred: `kod-tools/src/patch.rs` (found, not fixed)

A real bug was confirmed and then reverted because the fix regressed three
tests. Two separate defects, both reachable through `patch_file`:

### Defect 1 — a patch that ADDS a trailing newline loses it

`apply_unified_diff` decides the result's trailing newline from the
**original**'s shape (`had_trailing_newline`) plus a `\ No newline`
marker on the patch. It never consults what the patch's last body line
does. Repro:

- `old = "A"` (no final newline), `new = " \n"` (does end with one).
- `render_unified_diff(old, new, "f.rs")` emits a `-A` line, a `\ No
  newline at end of file` marker for it, and a `+ ` line terminated by
  `\n`.
- `apply_unified_diff(old, &diff)` returns `" "`, losing the final `\n`.
- Seed that found it: `old = "A", new = " \n"`.

### Defect 2 — an empty original splits to `[""]`

`original.split('\n')` on `""` yields `[""]`, not `[]`. An all-context
patch against an empty original therefore emits `" \n"` instead of `""`.
Independent of defect 1. Seed: `prop_all_context_patch_is_identity` on
`text = ""`.

### Why the one-line fix failed, and what the real fix needs

The naive fix — `patch.ends_with('\n')` as the middle branch — is always
true, so it appended a newline to every patch and regressed:

- `test_patch_file_creates_a_new_file_from_dev_null_diff` (pre-existing):
  got `"hello\nworld\n\n"`, expected `"hello\nworld\n"`.
- `prop_all_context_patch_is_identity` and `prop_render_apply_round_trip`
  (new).

The correct fix tracks per-line marker placement in `parse_unified_diff`
(a parallel `Vec<bool>` of `has_no_newline_marker`, or a field on
`HunkLine`) and rewrites the trailing-newline decision against the last
**emitted** line, distinguishing `Add`/`Context` from `Remove` and from a
hunk that stops before the original's end. Defect 2 is a separate
`"" -> []` normalization. Both are write-tests-first rounds, and both
must run the **full `kod-tools` suite**, not just `patch::`.

### A ceiling to know about

`similar` does not emit `\ No newline at end of file` for
`old = "", new = "a"`, so a render->apply round-trip property cannot hold
for that input. Any property that demands it is a test-shape mistake.

## The process lesson worth carrying

The first red run for `patch.rs` failed **two** properties:
`prop_render_apply_round_trip` and `prop_all_context_patch_is_identity`,
the second on trivial input (`text = ""`). One red seed is a bug; two red
seeds in the same function, one on a trivial input, means the diagnosis
is not finished. The fix was written anyway and regressed a pre-existing
test. That pre-existing test is why the full crate suite runs after every
fix, not just the filtered module.

## `kod-error` classifiers: surveyed, no round

`has_status_code` is already token-bounded (`split(!is_alphanumeric)`,
`tok == needle`) with `has_status_code_is_token_bounded` pinning
`"5001"`, `"a500b"`, `"n500"` false. `is_transient_transport_error` and
`is_server_busy_body` are prose-substring by documented design, and the
T5-C24 comment records the `"500"`-as-substring misfire as already fixed.
Harden-only, no bug. The handoff's feared digit-substring shape does not
exist there.

## Next targets, in order

1. `kod-tools/src/patch.rs` — the two defects above. Fresh session,
   write-tests-first, per-line markers, full-crate verification.
2. `kod-tools/src/check.rs` — 11 shape functions (diagnostic parsers for
   rustc/tsc/ruff/go), already has 3 proptests; likely harden or a
   position-off-by-one seed.
3. `kod-types/src/strutil.rs` — `truncate_bytes`/`truncate_chars`/
   `truncate_with_ellipsis`, 25 tests, 0 proptest. Boundary shape
   (multibyte truncation).
4. `kod-tools/src/context.rs` — `matches_pattern_inner` is `globset`-backed,
   so only the `{pattern}/**` fallback and the `fail_closed` branch are
   kod-authored logic; a narrow round, not a full one. The interesting
   paths (`resolve_path`, `can_write`) need `tempfile` fixtures.
5. `kod-core-quality/src/preflight.rs` — `parse_loadavg`/`parse_meminfo`/
   `parse_pmset`, 3 parsers, 0 proptest.

## Tag question, still open

Six production fixes beyond `v0.1.15` (`tool_quota` saturation,
`WorkPool` FreshAgents item drop, `take_batch` empty `Some`,
`has_touched` poison gap, `judgment` colon drop, `resolve_tool_name`
double prefix, plus the bash-interceptor newline) — that count matches
the release cadence (`v0.1.13` three, `v0.1.14` three, `v0.1.15` six).
The gate has now run green at `1b0c5ba` (5035/5035), so the code side of
the tag question is settled. What remains is the user's decision: cut
`v0.1.16` and push (which fires the six-target release workflow), or
hold. `EAGAIN` on a future run is host contention, not code;
`error[E...]` is the real classifier.

## Update — full gate and one more round

Since the body above was written, two commits landed:

| Commit | Crate | Outcome |
|---|---|---|
| `63f4f26` | docs | This file. |
| `1b0c5ba` | kod-types | `floor_char_boundary` maximality + `truncate_with_ellipsis` contract: 5 props. Harden. `truncate_bytes` / `truncate_chars` were already covered in `tests/it/string_safety.rs`; `floor_char_boundary` had boundary-validity there but nothing pinning maximality, and `truncate_with_ellipsis` had no property at all. Closes next-target item 3. |

Then the full-workspace gate ran at `1b0c5ba`:

    cargo nextest run --workspace --no-fail-fast
    Summary [80.043s] 5035 tests run: 5035 passed (1 leaky), 5 skipped

Exit 0. No `EAGAIN`. This is the first full verification since
`5ba0dec`; it covers every commit this session added, and it confirms
the `patch.rs` revert left the tree sound.

The `patch.rs` defects remain open and are unaffected by this run — the
revert restored the tree at `0b9c9c7` and the gate is green on top.
Next target remains `patch.rs` (write-tests-first, per-line markers,
full-crate verification), then `kod-core-quality/src/preflight.rs`.
