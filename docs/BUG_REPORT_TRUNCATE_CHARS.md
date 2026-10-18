# Bug report — truncate_chars caps by bytes, not characters

**Found:** session 15, deep-testing arc (property-based test on
kod-types::strutil).
**Severity:** P2 — silent data loss for non-ASCII content; name/contract
mismatch across five modules.
**Status:** **FIXED** in session 16 (v0.1.11). Fix is a multi-commit
structural change, not a one-line edit; see "Fix plan" below.
**Reproducer:** `truncate_chars("¡", 1) == ""` (should be `"¡"`).

## What it was

`truncate_chars` performed a **byte cap under a character-count name**.
The implementation was:

~~~rust
pub fn truncate_chars(s: &str, max: usize) -> &str {
    &s[..floor_char_boundary(s, max)]
}
~~~

`floor_char_boundary(s, 1)` on `"¡"` (U+00A1, 1 char, 2 bytes) returns
0, so the whole 1-char input was discarded when asked to keep 1 char.

## Consequence

~15 display callers pass `MAX_*_CHARS` constants (`MAX_LINE_CHARS =
400`, `MAX_TURN_CHARS = 4000`, `MAX_GOAL_DISPLAY_CHARS = 40`, etc.) or
char-count literals (`truncate_chars(&body, 300)`) to a byte-capping
function. On CJK/accented content they silently received roughly
**1/2 to 1/3** the requested char count. English-only testing never
surfaced it because ASCII chars are 1 byte.

## The full affected set

Session 15's handoff named the strutil definition and three "local
byte-correct" helpers it said not to touch. That classification was
incomplete — the fix surfaced two further classes the handoff's audit
had missed.

### Class A — byte-intent callers of the flipped strutil::truncate_chars

Migrated to truncate_bytes in Round A (no behavior change):

- kod-memory/src/manager.rs:483, 820 — MAX_CONTENT_BYTES = 4 KiB.
- kod-core/src/hooks.rs:178 — MAX_HOOK_OUTPUT_BYTES = 8 KiB (via
  cap_hook_output).
- kod-core/src/router.rs:1154, 1158, 1186 — a.request, a.history,
  a.repomap (byte budgets from BudgetAllocation).

### Class X — byte-intent callers of engine::render::truncate_chars

**Missed by the handoff's Round-A enumeration** (it listed only
manager.rs, hooks.rs, router.rs). engine::render::truncate_chars
delegates to strutil::truncate_chars, so Round B's char-flip silently
flipped these callers too. Migrated to truncate_bytes in Fixup-1
(44addb9):

- engine/tool_dispatch.rs:210 — STRUCTURED_TOOL_MSG_CAP = 16 KiB,
  the "byte wire cap" named in the format string itself.
- engine/render.rs:81 — per_field derived from a byte cap; message
  reads "truncated {} of {} bytes".
- engine/render.rs:94 — guarded by reserialized.len() <= cap.
- engine/render.rs:101 — message reads "truncated {} bytes".

### Class Y — char-intent callers of local byte-cap functions

**Missed by the handoff's "do not touch" rule** (it said the local
helpers were byte-correct and their callers byte-intent; that was
false at the sites below). Flipped to char semantics in Fixup-2
(46e0182, 54c2218):

- kod-core-routing/src/context_engine.rs:165, 188 — local
  truncate_chars byte cap called with char-count display literals
  (80, 200). Flipped; guard in first_sentence rewritten to compare
  the returned prefix's byte length to the input's.
- kod-core/src/router.rs:1313 — local truncate_chars byte cap called
  with MAX_EXAMPLE_CHARS = 1500 (_CHARS-named). Flipped; guard
  rewritten the same way.

## Why the earlier passes missed it

- The 78-finding static audit and the 20-fix bug hunt were both
  **example-based**. Every enumerated test used ASCII (or a hand-picked
  café-style input whose byte and char counts happen to be compared
  wrong). None compared the name's contract against the behavior on
  random non-ASCII input.
- The docstring said "characters". The unit tests asserted
  `.len()` (bytes). Nothing connected the two.
- Five property tests across five modules passed because each asserted
  its own module's invariants, not the workspace invariant that a
  char-named helper counts chars.
- The 16th property test — `truncate_chars(s, s.chars().count()) == s`
  on generated input — failed on the **first non-ASCII input**.

## Fix plan (executed in session 16)

The fix is three rounds, not one commit. The order matters: byte-intent
callers migrate **before** the char-cap flip, or prompt-budget safety
regresses (a 4096-byte budget passed to a char cap allows up to 16 KB of
CJK). Every intermediate commit compiles and passes the relevant tests.

### Round A — additive, no behavior change (5 commits)

1. 238e960 — add strutil::truncate_bytes (exact copy of the old
   truncate_chars body). Add its own test module.
2. d7dc006 — migrate manager.rs MAX_CONTENT_BYTES sites to
   truncate_bytes.
3. fa07492 — migrate hooks.rs cap_hook_output to truncate_bytes.
4. 9db3309 — add engine::render::truncate_bytes (parallel to the
   existing truncate_chars wrapper; engine::mod's `pub use render::*`
   re-exports it with pub(crate) effective visibility, so no mod.rs
   edit is needed). Migrate the three router.rs byte-budget sites.
5. 4e1c525 — retarget the byte-semantics property test to
   truncate_bytes.

### Round B — behavior change, gated by full-workspace tests

6. 162e3d7 — flip strutil::truncate_chars to char cap
   (char_indices().nth(max)). Split the test module into
   truncate_chars_tests (char semantics) and truncate_bytes_tests (byte
   semantics). Includes the reproducer case ("¡" kept whole at cap 1).
7. Full workspace gate: 4364 tests, 3 failed — all kod-core engine
   display tests asserting byte semantics on the char-named helper.
   Fixed in 3e3b0be; re-ran gate: 4364 passed, 3 skipped, 0 failed.
8. 04e1a91 — add truncate_chars_never_panics_and_is_char_bounded and
   truncate_chars_at_char_count_is_identity property tests.

### Fixups — the sites the handoff's Round-A enumeration missed

- 44addb9 (Fixup-1) — restore byte-intent caps in engine/tool_dispatch
  and engine/render after the implicit char-flip; four sites.
- 46e0182 (Fixup-2a) — flip kod-core-routing local truncate_chars to
  char semantics; update first_sentence's truncation-detection guard.
- 54c2218 (Fixup-2b) — flip router local truncate_chars to char
  semantics; update the skill-example truncation-detection guard.

### Round C — regression guard (revised)

9. 022c139 — source lint:
   - Every `fn truncate_chars` body must use char_indices().nth( or
     chars().nth( or delegate to strutil::truncate_chars.
   - Every `fn truncate_bytes` body must use floor_char_boundary( or
     delegate to strutil::truncate_bytes.
   - No truncate_chars call with a _BYTES/_BYTE budget or .len().
   - No truncate_bytes call with a _CHARS/_CHAR budget or
     .chars().count().

   The handoff originally proposed a definitions-only lint. Round B's
   gate surfaced the call-site mismatch class (Class X, Class Y), so
   the lint scans both directions. This is the guard that would have
   caught both of the Round-A misses.

## Do not do

- Do not flip truncate_chars to char cap without first migrating the
  byte-intent callers. MAX_CONTENT_BYTES and MAX_HOOK_OUTPUT_BYTES are
  byte budgets; a 4x overshoot for CJK content is a prompt-budget
  regression.
- Do not rename the local truncate_chars in router.rs or
  context_engine.rs. They keep their names; the fix was their
  behavior. A rename would lose the historical note in the docstring.
- Do not attempt the fix as one commit. The additive-then-flip ordering
  is what keeps the codebase compiling at every step.
- Do not skip the full-workspace gate on Round B's char-flip commit.
  That is where ~30 display tests may start failing on byte-vs-char
  assertions. Each failure is a genuine signal, not flake.
- Do not accept a property-test failure as "the test is wrong" without
  proving the contract. The "¡" reproducer is the test being *right*.

## Related tests (committed, not to be re-written)

- kod-types strutil::truncate_chars_tests — 8 tests (char semantics,
  including one_char_nonascii_is_kept_whole and
  cjk_count_matches_char_count).
- kod-types strutil::truncate_bytes_tests — 8 tests (byte semantics,
  including one_char_nonascii_byte_cap_drops_whole_char, the
  deliberate byte-drop pin).
- kod-types::it string_safety::truncate_chars_never_panics_and_is_char_bounded.
- kod-types::it string_safety::truncate_chars_at_char_count_is_identity.
- kod-types::it string_safety::truncate_bytes_never_panics_and_is_bounded.
- kod-types::it source_lints::truncate_helpers_name_matches_contract.
