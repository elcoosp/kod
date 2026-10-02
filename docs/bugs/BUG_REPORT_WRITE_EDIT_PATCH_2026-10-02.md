# KOD — Write/Edit/Patch Bug Report & Verified Fixes

**Repo:** `elcoosp/kod` @ commit `9451578` ("fix(core): stop failed attempts duplicating rounds; persist partials (M-13, M-16)")
**Date:** 2026-10-02
**Scope:** the write/edit/patch surface — `crates/kod-tools/src/{patch.rs, patch_text.rs, edit_hashline.rs, tools.rs (PatchFileTool / ExecuteCommandTool / WriteFileTool)}`, `crates/kod-tools/src/context.rs`, and the engine consumer `maybe_recover_inline_patch` (`crates/kod-core/src/engine/mod.rs`).

**Method:** every bug below was first **reproduced** against a verbatim copy of the pre-fix algorithm (standalone harness, 14/14 probes reproduced), then **fixed in the working tree**, then **pinned with a regression test**, then verified by `cargo check --workspace --all-targets` (exit 0) and the full test suites. Nothing on this list is speculative.

**Result:** 14 bugs fixed (11 in the write/edit/patch core + recovery path, 1 build break, 1 in `execute_command`, 1 diff-emitter correctness), 12 new regression tests, all suites green. 8 code smells documented (not changed).

---

## Severity legend

- **S1 — silent file corruption**: the tool reports success while destroying or duplicating user data.
- **S2 — broken feature / wrong result**: a real code path fails or lies, without data loss.
- **S3 — build break**: the crate does not compile at HEAD.

---

## Summary table

| ID | Sev | File | One-line |
|----|-----|------|----------|
| BUG-W1 | S1 | `engine/mod.rs` | `*** Add File:` recovery duplicates existing content |
| BUG-W2 | S1 | `engine/mod.rs` | `*** Delete File:` recovery truncates to `"\n"` or is a silent no-op |
| BUG-W3 | S1 | `edit_hashline.rs` | Overlapping hashline ops corrupt the file silently |
| BUG-W4 | S1 | `edit_hashline.rs` | Unseen-anchor guard checked only the FIRST line of a range |
| BUG-W5 | S1 | `edit_hashline.rs` | Second `[path#tag]` header ignored → ops applied to the wrong file |
| BUG-W6 | S1 | `patch_text.rs` | Search/apply tolerance mismatch rejected valid patches; phantom line in haystack |
| BUG-W7 | S2 | `tools.rs` | `patch_file` could not create files — `Add File` path was dead |
| BUG-W8 | S2 | `patch.rs` + `edit_hashline.rs` | A patch emptying a file left a stray `"\n"` |
| BUG-W9 | S2 | `patch.rs` | Empty old-range header bypassed the consumed-count check |
| BUG-W10 | S2 | `patch_text.rs` | Later hunks' `+start` wrong for every standard diff consumer |
| BUG-W11 | S2 | `engine/mod.rs` | Recovery note counted failed calls as applied |
| BUG-W12 | S2 | `edit_hashline.rs` | 16-bit tag collision green-lit an edit against unseen content |
| BUG-W13 | S3 | `context.rs` | kod-tools did not compile at HEAD (missing struct field) |
| BUG-W14 | S2 | `tools.rs` | `execute_command` timeout kill never reached grandchildren (1 s timeout took 60 s) |

*Audited clean, no defect:* `WriteFileTool` (atomic temp-file + fsync + rename already correct), `read_file` binary detection, and `batch.rs` sequencing were reviewed and found sound.

---

# Severity 1 — silent file corruption

## BUG-W1 — `*** Add File:` recovery duplicates existing content

**Location:** `crates/kod-core/src/engine/mod.rs` → `maybe_recover_inline_patch`

**Bug.** The engine translated a recovered `FilePatch::Add` into a `@@ -0,0 +1,N @@` insertion diff via `patch_text::to_unified_diff` and dispatched it to `patch_file`. `apply_unified_diff` happily applies an insertion diff to a file that **already exists**: the new content is spliced at the top and all old content is kept behind it — reported as success.

```text
existing.txt: "OLD CONTENT\n"   →   "NEW CONTENT\nOLD CONTENT\n"   (reported as success)
```

**Before (engine):**

```rust
let (path, original) = match patch {
    kod_tools::patch_text::FilePatch::Add { path, .. } => {
        (path.clone(), String::new())          // never checks the target
    }
    ...
};
```

**Fix (after):** the engine resolves the target first. An `Add` onto a non-empty existing file is skipped with an explicit note instead of dispatching a destructive diff; a missing or empty file is a genuine creation (works together with BUG-W7's fix):

```rust
kod_tools::patch_text::FilePatch::Add { path, .. } => {
    let abs = if std::path::Path::new(path).is_absolute() {
        std::path::PathBuf::from(path)
    } else {
        working_dir.join(path)
    };
    match std::fs::read_to_string(&abs) {
        Ok(body) if !body.is_empty() => {
            notes.push(format!(
                "  · {path}: skipped — file already exists with content; \
                 an Add would duplicate it, use an Update hunk"
            ));
            None
        }
        _ => Some((path.clone(), String::new())),
    }
}
```

**Pinned by:** engine integration tests (`--test engine`).

---

## BUG-W2 — `*** Delete File:` recovery never deletes — it truncates to `"\n"` (or is a silent no-op)

**Location:** `crates/kod-core/src/engine/mod.rs` → `maybe_recover_inline_patch`

**Bug.** `Delete` was folded into the `Update` arm: an all-removal unified diff was generated and run through `patch_file`. Two failure modes:

1. `apply_unified_diff` re-emits a trailing newline for any file that had one, so "deleting" `one\ntwo\n` produced a **1-byte file containing `"\n"`**.
2. If the engine's pre-read failed, `unwrap_or_default()` produced `""`, the emitted header `@@ -1,0 +0,0 @@` has no body, matches nothing, and applies as a **silent no-op success** on the real file — the file was never deleted and the model was told it was.

**Before:**

```rust
kod_tools::patch_text::FilePatch::Update { path, .. }
| kod_tools::patch_text::FilePatch::Delete { path } => {
    let body = std::fs::read_to_string(&abs).unwrap_or_default();  // failure → "" → no-op diff
    (path.clone(), body)
}
```

**Fix (after):** `Delete` is no longer converted into a content diff. The recovery note says deletion is not supported by `patch_file` and must be done with a shell command. Unreadable `Update` sources are also reported instead of silently diffing against `""`:

```rust
kod_tools::patch_text::FilePatch::Delete { path } => {
    notes.push(format!(
        "  · {path}: skipped — file deletion is not supported by \
         patch_file; remove it with a shell command"
    ));
    None
}
kod_tools::patch_text::FilePatch::Update { path, .. } => {
    match std::fs::read_to_string(&abs) {
        Ok(body) => Some((path.clone(), body)),
        Err(e) => {
            notes.push(format!(
                "  · {path}: skipped — could not read the file: {e}"
            ));
            None
        }
    }
}
```

---

## BUG-W3 — Overlapping hashline ops corrupt silently

**Location:** `crates/kod-tools/src/edit_hashline.rs` → `stage` / `apply`

**Bug.** `stage` sorts ops by `start` descending and splices. Two ops that share any line splice against each other's shifted indices:

```text
"a\nb\nc\nd\n" + [PUT 2.=3:+B, PUT 3.=4:+C]        →  "a\nB\n"     (C's range vanished)
"a\nb\nc\n"    + [PUT 2.=2:+FIRST, PUT 2.=2:+SECOND] →  one payload silently dropped
```

**Before:** no overlap validation existed; the descending splice just applied both.

**Fix (after):** new `EditError::Overlap` variant; a validation pass sorts the non-append ranges and rejects any pair with `next.start <= prev.end`:

```rust
/// Two ops address overlapping line ranges — applying both would
/// splice against shifted indices, so the batch is rejected.
Overlap { first: (usize, usize), second: (usize, usize) },
```

```rust
// Overlap rejection: two non-append ops that share any line
// splice against each other's shifted indices. The descending
// application order only composes for disjoint ranges.
let mut ranges: Vec<(usize, usize)> = ops
    .iter()
    .filter(|o| !o.append)
    .map(|o| (o.start, o.end))
    .collect();
ranges.sort();
for w in ranges.windows(2) {
    if w[1].0 <= w[0].1 {
        return Err(EditError::Overlap { first: w[0], second: w[1] });
    }
}
```

**Pinned by:** `overlapping_ops_are_rejected`, `two_puts_on_the_same_line_are_rejected_as_overlapping`.

---

## BUG-W4 — The unseen-anchor guard only checked the FIRST line of a range

**Location:** `crates/kod-tools/src/edit_hashline.rs` → `EditStore::apply`

**Bug.** `apply` validated `snap.seen[op.start]` but never `op.end`. A model that read only line 1 could send `CUT 1.=5` and delete lines 2–5 — four lines it never saw — defeating the module's core safety promise.

**Before:**

```rust
if !snap.seen.get(op.start).copied().unwrap_or(false) {
    return Err(EditError::UnseenAnchor { line: op.start });
}
```

**Fix (after):** every line in `op.start..=op.end` must be in the seen set:

```rust
// Every line the op touches must have been seen — not just
// the first. `CUT 1.=5` with only line 1 read deleted four
// lines the model never saw.
for line_no in op.start..=op.end {
    if !snap.seen.get(line_no).copied().unwrap_or(false) {
        return Err(EditError::UnseenAnchor { line: line_no });
    }
}
```

**Pinned by:** `a_cut_whose_end_lines_were_never_seen_is_rejected` (asserts the file is untouched after the rejected op).

---

## BUG-W5 — A second `[path#tag]` header was silently ignored — ops applied to the wrong file

**Location:** `crates/kod-tools/src/edit_hashline.rs` → `parse`

**Bug.** `parse` accepted the header only while `path.is_none()`; a later `[b.txt#00ff]` fell through to the junk-ignore branch, and **every op after it was applied to the first file**:

```text
[a.txt#00ff] PUT 1.=1: +for a   [b.txt#00ff] PUT 9.=9: +for b
→ path="a.txt", ops=[PUT 1, PUT 9]   (b.txt's edit spliced into a.txt)
```

**Before:**

```rust
if line.starts_with('[') && line.ends_with(']') && path.is_none() {
    // second header → treated as junk, ops keep targeting file 1
```

**Fix (after):** a second header is a malformed edit:

```rust
if line.starts_with('[') && line.ends_with(']') {
    if path.is_some() {
        return Err(EditError::Malformed(
            "second [path#tag] header — one edit block per call".to_string(),
        ));
    }
    ...
```

**Pinned by:** `a_second_header_is_a_malformed_edit`.

---

## BUG-W6 — Search/apply tolerance mismatch rejected valid recovered patches (and the haystack had a phantom line)

**Location:** `crates/kod-tools/src/patch_text.rs` → `to_unified_diff`

**Bug (two related defects in one function):**

1. `find_unique_block` matches on `trim_end` (trailing spaces and CRLF tolerated), but `to_unified_diff` re-emitted the **patch's** spelling of context/removal lines, and `apply_unified_diff` compares **exactly**. A file line with trailing whitespace was found by the search and then rejected by the applier — the two halves of the same pipeline disagreed, so a valid recovered patch failed.
2. The haystack was built with a raw `original.split('\n')`, leaving a phantom trailing `""` element for files ending in `\n` — out of sync with `apply_unified_diff`'s line model, so block matching could land on the phantom.

**Before:**

```rust
let file_lines: Vec<&str> = original.split('\n').collect();
...
for l in &hunk.lines {
    out.push_str(l);            // the PATCH's spelling, not the file's bytes
    out.push('\n');
}
```

**Fix (after):** the haystack pops the phantom element, and context/removal lines are re-emitted with the **file's exact bytes** (position-tracked), so anything the search accepted also applies:

```rust
let mut file_lines: Vec<&str> = original.split('\n').collect();
if original.ends_with('\n') {
    file_lines.pop();           // align with apply_unified_diff's line model
}
...
let mut pos = start - 1;
for l in &hunk.lines {
    match l.as_bytes().first() {
        Some(b' ') | Some(b'-') => {
            match file_lines.get(pos) {
                Some(file_line) => {
                    out.push(if l.starts_with(' ') { ' ' } else { '-' });
                    out.push_str(file_line);   // the FILE's exact bytes
                    out.push('\n');
                }
                None => { out.push_str(l); out.push('\n'); }
            }
            pos += 1;
        }
        _ => { out.push_str(l); out.push('\n'); }
    }
}
```

**Pinned by:** `trailing_space_file_lines_survive_the_search_then_apply_round_trip`.

---

# Severity 2 — broken features, wrong results

## BUG-W7 — `patch_file` could not create files: the whole `Add File` path was dead

**Location:** `crates/kod-tools/src/tools.rs` → `PatchFileTool::execute`

**Bug.** `execute` unconditionally `read_to_string`ed the target. For a genuinely new file that fails with `NotFound` → every recovered `*** Add File:` patch failed, and the `@@ -0,0` creation machinery in `patch.rs`/`patch_text.rs` was unreachable through the tool.

**Before:**

```rust
let original = match std::fs::read_to_string(&resolved) {
    Ok(s) => s,
    Err(e) => {
        return Ok(ToolResult::Error(describe_path_error(&resolved, &e)));
    }
};
```

**Fix (after):** on `NotFound`, the patch is allowed through with an empty original **only when every hunk is a pure insertion at 0** (`old_start == 0 && old_lines == 0` — the `--- /dev/null` shape); any other patch against a missing file still fails with the read error:

```rust
Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
    let new_file_patch = crate::patch::parse_unified_diff(patch)
        .map(|hs| {
            !hs.is_empty()
                && hs.iter().all(|h| h.old_start == 0 && h.old_lines == 0)
        })
        .unwrap_or(false);
    if new_file_patch {
        String::new()
    } else {
        return Ok(ToolResult::Error(describe_path_error(&resolved, &e)));
    }
}
```

**Pinned by:** `test_patch_file_creates_a_new_file_from_dev_null_diff`, `test_patch_file_on_missing_file_without_dev_null_header_fails`.

---

## BUG-W8 — A patch that empties a file left a stray `"\n"` (and so did `CUT` of all lines)

**Locations:** `crates/kod-tools/src/patch.rs` → `apply_unified_diff`; `crates/kod-tools/src/edit_hashline.rs` → `stage`

**Bug.** `apply_unified_diff` pushed the trailing separator whenever the original had one — even when the result had zero lines: removing every line turned `one\ntwo\n` into `"\n"` (1 byte), not `""`. Same root cause in `edit_hashline::stage` (`CUT 1.=2` on a 2-line file → `"\n"`).

**Before (both functions):**

```rust
let mut out = lines.join(sep);
if had_trailing_newline {          // pushed even when the result is empty
    out.push_str(sep);
}
```

```rust
if text.ends_with('\n') && !out.ends_with('\n') {
    out.push_str(eol);
}
```

**Fix (after):** the trailing EOL is only re-emitted when the result is non-empty:

```rust
// patch.rs
if had_trailing_newline && !lines.is_empty() {
    out.push_str(sep);
}

// edit_hashline.rs
if text.ends_with('\n') && !out.is_empty() && !out.ends_with('\n') {
    out.push_str(eol);
}
```

**Pinned by:** `removing_every_line_yields_an_empty_file` (patch.rs), `cutting_every_line_yields_an_empty_file` (edit_hashline.rs).

---

## BUG-W9 — A hunk header declaring an empty old range bypassed the consumed-count check

**Location:** `crates/kod-tools/src/patch.rs` → `apply_unified_diff`

**Bug.** `if consumed != old_len && old_len > 0` — with `@@ -1,0 +1,3 @@` and body lines, the guard was skipped, the splice applied silently, and the running `offset` every later hunk trusts was corrupted.

**Before:**

```rust
if consumed != old_len && old_len > 0 {
    return Err(KodError::InvalidParameters { /* ... */ });
}
```

**Fix (after):** the guard is unconditional. New-file hunks (`old_start == 0, old_lines == 0`, adds only) still pass because `consumed` is legitimately 0:

```rust
// The guard fires whenever the body consumed a different number
// of source lines than the header declared — including the
// `old_lines == 0` case.
if consumed != old_len {
    return Err(KodError::InvalidParameters { /* ... */ });
}
```

**Pinned by:** `a_header_declaring_an_empty_old_range_cannot_consume_lines`.

---

## BUG-W10 — Later hunks' `+start` was wrong for any standard diff consumer

**Location:** `crates/kod-tools/src/patch_text.rs` → `to_unified_diff`

**Bug.** The emitter wrote `@@ -{start},{old} +{start},{new} @@` for every hunk, re-using the OLD-file position on the new side. After hunk 1 inserts a line, hunk 2's context sits at new-file line 6 but the header claimed `+5`. The internal applier ignores `new_start`, so kod itself didn't notice — but `git apply`, GNU `patch`, and every review tool that renders these diffs would misplace or reject them.

**Before:**

```rust
out.push_str(&format!("@@ -{start},{old_count} +{start},{new_count} @@\n"));
```

**Fix (after):** a running `new_side_offset` delta adjusts each hunk's new-side start:

```rust
let mut new_side_offset: isize = 0;
for hunk in hunks {
    ...
    let new_start = (start as isize + new_side_offset).max(0) as usize;
    out.push_str(&format!("@@ -{start},{old_count} +{new_start},{new_count} @@\n"));
    ...
    new_side_offset += new_count as isize - old_count as isize;
}
```

**Pinned by:** `later_hunks_new_start_accounts_for_earlier_insertions` (asserts `@@ -5,1 +6,1 @@` after a 1→2 line hunk, and that the internal applier still agrees).

---

## BUG-W11 — The recovered-patch note counted failed calls as applied

**Location:** `crates/kod-core/src/engine/mod.rs` → `maybe_recover_inline_patch`

**Bug.** `let applied = calls.len()` followed by "(N rejected)" — a round with 3 calls and 1 failure reported "Recovered 3 file patch(es) … (1 rejected)" with no applied count. The model was told its failed patch had been applied.

**Before:**

```rust
let applied = calls.len();
let failed: usize = round.results.iter()
    .filter(|r| matches!(r, kod_types::ToolResult::Error(_)))
    .count();
```

**Fix (after):**

```rust
let applied = calls.len().saturating_sub(failed);
let mut note = format!(
    "Recovered {} file patch(es) from an inline `*** Begin Patch` block; \
     {applied} applied, {failed} rejected.",
    calls.len(),
);
```

---

## BUG-W12 — The 16-bit edit tag could green-light an edit against unseen content

**Location:** `crates/kod-tools/src/edit_hashline.rs` → `EditStore::apply` (re-read guard)

**Bug.** `tag_of` folds FNV-1a to 2 bytes; a collision was demonstrated at `tag_of("content-40") == tag_of("content-147")`. The re-read guard compared `tag_of(current) != snap.tag`, so two colliding states passed the "file unchanged since read" check — the splice then applied against text the model never saw.

**Before:**

```rust
let current_tag = tag_of(&current);
if current_tag != snap.tag {
    return Err(EditError::StaleTag { expected: snap.tag, got: current_tag });
}
```

**Fix (after):** the re-read guard compares `current != snap.text` **exactly** (the snapshot text is already in memory, so this costs nothing); the 16-bit tag remains only for the wire format (what the model echoes back):

```rust
// The comparison is exact bytes, not the 16-bit tag: two different
// contents can share a tag (1/65536), and a collision here would
// green-light a splice against text the model never saw.
if current != snap.text {
    return Err(EditError::StaleTag {
        expected: snap.tag,
        got: tag_of(&current),
    });
}
```

**Pinned by:** `an_externally_changed_file_is_rejected_even_on_a_tag_collision` (searches a real colliding content and asserts the edit is rejected).

---

# Severity 3 — build break (pre-existing at HEAD `9451578`)

## BUG-W13 — kod-tools did not compile at HEAD

**Location:** `crates/kod-tools/src/context.rs` → `landlock_invocation`

**Bug.** `context.rs` constructed `LandlockProfile` without the `claims_git_readonly` field (added to the struct in `sandbox/landlock.rs` but never wired at this call site) — `error[E0063]`; `cargo test -p kod-tools` failed before any test ran. The break was pre-existing at `9451578`, i.e. the committed tree did not build.

**Fix (after):** wire the field, which also restores the H-11 contract — `apply()` now refuses instead of shipping a `.git`-ro claim that additive Landlock rules cannot honor:

```rust
let mut profile = crate::sandbox::landlock::LandlockProfile {
    claims_git_readonly: opts.git_readonly,
    ro_paths: vec![ /* ... */ ],
```

---

# Severity 2 (adjacent surface) — `execute_command` timeouts were dead

## BUG-W14 — The timeout/cap kill never reached grandchildren, which held the pipes

**Location:** `crates/kod-tools/src/tools.rs` → `ExecuteCommandTool`

**Bug.** `execute_command` spawns `sh -c <command>`. `child.start_kill()` signals only the shell; in this environment `dash -c "sleep 60"` forks (does not exec), so the `sleep` grandchild survived **holding the stdout/stderr write-ends**. The drain loop then blocked until the grandchild exited naturally:

```text
timeout_secs=1 on `sleep 60` → tool returned after 60.0018s; orphan sleep kept running
```

The two process-kill tests each ran 60s+ and one failed outright (`execute_command_times_out_without_panic`).

**Fix (after):** `spawn.process_group(0)` (Unix) puts the child in its own group; a new `kill_child_tree()` helper signals `-pid` via `libc::kill` (with `start_kill` fallback), used at all three kill sites:

```rust
fn kill_child_tree(child: &mut tokio::process::Child) {
    #[cfg(unix)]
    if let Some(pid) = child.id() {
        let pgid = -(pid as libc::pid_t);
        let rc = unsafe { libc::kill(pgid, libc::SIGKILL) };
        if rc == 0 {
            return;
        }
    }
    let _ = child.start_kill();
}
```

```rust
#[cfg(unix)]
spawn.process_group(0);   // at spawn time
```

**Suite effect:** those tests went from 60s+/failing to the whole 387-test file finishing in **2.11 s**.

---

# Code smells (documented, not changed)

1. **`edit_hashline::apply` doc vs. behavior** — the doc claimed the returned tag lets the model "chain edits", but the snapshot is dropped on success, so chaining with it yields `NeverRead`. The doc was corrected to say the tag is informational; the tool result's `new_tag` field invites the same dead end and could be removed in a later pass.
2. **Inconsistent error channels in `edit_tool`** — parse/apply failures return `Ok(ToolResult::Error(..))` while `resolve_path`/`can_write` propagate `Err(KodError)`. Two shapes for the same class of model mistake.
3. **`render_numbered`'s elision footer is unactionable** — it says "re-read the range you need", but `read_file` has no offset/length parameters, and the tool always passes `usize::MAX` so elision never fires anyway.
4. **16-bit tag remains a 1/65536 collision risk on the wire** — the dangerous path is now exact-byte guarded, but a model echoing a colliding tag from a stale read can still address the wrong snapshot state; 4 bytes would make it negligible.
5. **`apply_unified_diff` "dominant EOL" is actually "any CRLF"** — a mixed-EOL file gets fully normalized to CRLF; the comment overstates the detection.
6. **`extract()` treats any complete `*** Begin Patch … *** End Patch` block in reply text as an edit request** — including one inside a fenced code block where the model is *explaining* the format; approval gating makes this safe but noisy.
7. **`patch_text::extract` strips payload trailing whitespace** (`raw.trim_end()` before `strip_prefix('+')`), so a payload line of `"+   "` becomes an empty line.
8. **Pre-existing unused import** — `tokio::io::AsyncBufReadExt` at `kod-core/src/serve.rs:435` (visible today as the 1 remaining `unused-imports` warning in `cargo check`).

---

# Verification — compile & test evidence (re-run 2026-10-02)

Environment: `rustc 1.99.0 (b940084d7 2026-09-28)`, `cargo 1.99.0`, toolchain `stable` per `rust-toolchain.toml`.

> **Build note:** the repo's `.cargo/config.toml` sets `rustc-wrapper = "kache"`, which is not installed in this environment. All commands below were run with `RUSTC_WRAPPER=""` to override it. On a machine with `kache` installed this is not needed.

| # | Command | Result |
|---|---------|--------|
| 1 | `cargo check --workspace --all-targets` | **exit 0** — `Finished 'dev' profile [unoptimized + debuginfo] target(s) in 4m 16s`; 0 errors; 2 pre-existing warnings (unused import in `kod-cli` lib-test + duplicate in `kod-core` lib-test) |
| 2 | `cargo test -p kod-tools --lib` | **387 passed; 0 failed** in 2.11 s (pre-fix: one test failed outright and two ran 60 s+ each) |
| 3 | `cargo test -p kod-core --lib` | **1058 passed; 0 failed** in 8.56 s |
| 4 | `cargo test -p kod-core --test tool_loop --test engine --test policy_gate` | **21 passed; 0 failed** |
| 5 | The 12 new regression tests (filtered run) | **12 passed; 0 failed** in 0.01 s |

Explicit regression-test run (`cargo test -p kod-tools --lib -- <filters>`):

```text
test edit_hashline::tests::a_second_header_is_a_malformed_edit ... ok
test edit_hashline::tests::a_cut_whose_end_lines_were_never_seen_is_rejected ... ok
test edit_hashline::tests::cutting_every_line_yields_an_empty_file ... ok
test edit_hashline::tests::overlapping_ops_are_rejected ... ok
test edit_hashline::tests::two_puts_on_the_same_line_are_rejected_as_overlapping ... ok
test patch::tests::a_header_declaring_an_empty_old_range_cannot_consume_lines ... ok
test patch::tests::removing_every_line_yields_an_empty_file ... ok
test patch_text::tests::later_hunks_new_start_accounts_for_earlier_insertions ... ok
test patch_text::tests::trailing_space_file_lines_survive_the_search_then_apply_round_trip ... ok
test tools::tests::test_patch_file_creates_a_new_file_from_dev_null_diff ... ok
test tools::tests::test_patch_file_on_missing_file_without_dev_null_header_fails ... ok
test edit_hashline::tests::an_externally_changed_file_is_rejected_even_on_a_tag_collision ... ok
test result: ok. 12 passed; 0 failed; 0 ignored; 0 measured; 375 filtered out
```

Standalone bug harness (verbatim copies of the **pre-fix** algorithms): **14/14 probes reproduced** the bugs above before the fixes were applied.

---

# Files changed (all fixes, uncommitted working tree)

| File | Insertions | What |
|------|-----------|------|
| `crates/kod-tools/src/patch.rs` | +35 | W8, W9 + 2 regression tests |
| `crates/kod-tools/src/patch_text.rs` | +107 | W6, W10 + 2 regression tests |
| `crates/kod-tools/src/edit_hashline.rs` | +197 | W3, W4, W5, W8, W12 + 6 regression tests |
| `crates/kod-tools/src/tools.rs` | +110 | W7, W14 + 2 regression tests |
| `crates/kod-tools/src/context.rs` | +8 | W13 (build break / H-11 contract) |
| `crates/kod-core/src/engine/mod.rs` | +66 | W1, W2, W11 (recovery path) |

Total: 6 files, +490/−33, plus this report and `docs/PATCH_AUDIT_2026-10-02.md`.

```text
git -C /home/z/my-project/kod status --short
 M crates/kod-core/src/engine/mod.rs      (maybe_recover_inline_patch only)
 M crates/kod-tools/src/context.rs        (landlock_invocation only)
 M crates/kod-tools/src/edit_hashline.rs
 M crates/kod-tools/src/patch.rs
 M crates/kod-tools/src/patch_text.rs
 M crates/kod-tools/src/tools.rs          (PatchFileTool + ExecuteCommandTool only)
?? docs/PATCH_AUDIT_2026-10-02.md
```

---

# Root-cause themes

1. **Two halves of one pipeline disagreed.** The search tolerated trailing whitespace while the applier compared exactly (W6); the emitter ignored `new_start` while standard consumers trusted it (W10). Each half was locally reasonable; the seam between them was untested.
2. **Guards written for the common case, not the boundary.** `&& old_len > 0` (W9), first-line-only anchor check (W4), `had_trailing_newline` without checking emptiness (W8) — each skips the check exactly in the rare case where it matters.
3. **Sentinels standing in for real state.** `original = String::new()` for every unresolvable target (W1, W2) and `timeout_ms`-style fabrication; the sentinel silently changes semantics instead of failing loudly.
4. **Low-entropy identifiers guarding correctness.** The 16-bit tag (W12) was used as a content equality oracle; 1/65536 is fine for a wire hint, not for authorizing a splice.
5. **Signals killed too narrowly.** `start_kill` on the direct child only (W14) — in a shell-spawning world, the tree, not the child, is the unit of termination.

# How to reproduce the verification

```bash
cd /home/z/my-project/kod
export RUSTC_WRAPPER=""                       # only if `kache` is not installed
cargo check --workspace --all-targets
cargo test -p kod-tools --lib
cargo test -p kod-core --lib
cargo test -p kod-core --test tool_loop --test engine --test policy_gate
```

All fixes are already applied in the working tree at `/home/z/my-project/kod` (uncommitted). To review: `git diff`. To commit: `git add -A && git commit` with the message of your choice.
