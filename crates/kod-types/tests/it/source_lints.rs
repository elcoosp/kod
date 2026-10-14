//! Workspace-wide source scans for shapes that were bugs in this
//! codebase's history. Each test fails *before* a future commit
//! re-introduces the shape — the goal is class-level regression, not
//! pinning one specific line.
//!
//! Scans stop at the first `#[cfg(test)]` in each file. Test code is
//! exempt: a fixed temp name or a short-cast in a test is controlled
//! and does not affect production behaviour.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

fn workspace_root() -> &'static Path {
    static ROOT: OnceLock<PathBuf> = OnceLock::new();
    ROOT.get_or_init(|| {
        // kod-types/Cargo.toml → ../.. is the workspace root.
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .unwrap()
            .parent()
            .unwrap()
            .to_path_buf()
    })
}

fn all_production_sources() -> Vec<PathBuf> {
    let mut out = Vec::new();
    walk(&workspace_root().join("crates"), &mut out);
    out.sort();
    out
}

fn walk(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let p = entry.path();
        if p.is_dir() {
            let name = p.file_name().and_then(|s| s.to_str()).unwrap_or("");
            // Skip `target/` and any `tests/` directory outright:
            // test code is allowed to use the shapes the lints below
            // flag because the values are controlled.
            if name == "target" || name == "tests" {
                continue;
            }
            walk(&p, out);
        } else if p.extension().and_then(|s| s.to_str()) == Some("rs") {
            out.push(p);
        }
    }
}

/// A file's non-test, non-comment lines as `(1-based line number,
/// line text)`.
///
/// Stops at the first top-level `#[cfg(test)]` line (or the `all(
/// test, … )` variant): everything after is a test module, where the
/// shapes the lints check are legitimately used.
fn production_lines(path: &Path) -> Vec<(usize, String)> {
    let Ok(content) = fs::read_to_string(path) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for (i, line) in content.lines().enumerate() {
        let trimmed = line.trim_start();
        if trimmed.starts_with("#[cfg(test)]") || trimmed.starts_with("#[cfg(all(test") {
            break;
        }
        out.push((i + 1, line.to_string()));
    }
    out
}

fn is_comment(line: &str) -> bool {
    let t = line.trim_start();
    t.starts_with("//") || t.starts_with("///") || t.starts_with("//!")
}

fn report(bad: &[(PathBuf, usize, String)], header: &str) -> ! {
    panic!(
        "{header}\n{}",
        bad.iter()
            .map(|(p, l, s)| format!("  {}:{}: {}", p.display(), l, s.trim()))
            .collect::<Vec<_>>()
            .join("\n"),
    );
}

// ---------------------------------------------------------------------------
// Class: fixed temp filename in the temp-then-rename pattern
// ---------------------------------------------------------------------------
//
// The temp-file-then-rename idiom is only atomic under concurrent
// writers when the temp name is unique per writer. A literal ending
// in `.tmp` with no format placeholder (no `{}`) produces the same
// path every call. This session fixed eleven instances across five
// crates; this test catches the twelfth.

#[test]
fn no_fixed_temp_filenames_in_production() {
    let mut bad: Vec<(PathBuf, usize, String)> = Vec::new();
    for path in all_production_sources() {
        for (lineno, line) in production_lines(&path) {
            if is_comment(&line) {
                continue;
            }
            for lit in string_literals(&line) {
                if !lit.contains(".tmp") {
                    continue;
                }
                // Two shapes are broken:
                //
                // * A literal `.tmp` with no placeholder at all — the
                //   fixed filename that raced two writers on the same
                //   inode (the shape this lint was written to catch).
                // * A `.tmp` name with only **one** placeholder. The
                //   single placeholder in every such site has been a
                //   wall-clock second (`{ts}`), which is not unique
                //   across concurrent writers in the same second: two
                //   `kod config migrate` invocations both write
                //   `config.toml.migrate-{ts}.tmp`, the first `rename`
                //   succeeds, and the second returns ENOENT. The fix
                //   everywhere else in the workspace is pid + counter
                //   + nanosecond clock, which is two or more
                //   placeholders.
                let braces = lit.matches('{').count();
                if braces < 2 {
                    bad.push((path.clone(), lineno, lit));
                }
            }
        }
    }
    if !bad.is_empty() {
        report(
            &bad,
            "fixed temp filename(s) in production — the temp-then-rename \
             idiom needs a unique name per writer (pid + counter + clock \
             is the shape used elsewhere in the workspace):",
        );
    }
}

/// Extract the bodies of double-quoted string literals on one line.
/// Escapes are skipped; raw strings are not handled (the workspace
/// does not use raw strings for filenames).
fn string_literals(line: &str) -> Vec<String> {
    let mut out = Vec::new();
    let bytes = line.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'"' {
            let start = i + 1;
            let mut j = start;
            while j < bytes.len() {
                if bytes[j] == b'\\' {
                    j += 2;
                    continue;
                }
                if bytes[j] == b'"' {
                    break;
                }
                j += 1;
            }
            if j < bytes.len() && j > start {
                out.push(line[start..j].to_string());
            }
            i = j + 1;
        } else {
            i += 1;
        }
    }
    out
}

// ---------------------------------------------------------------------------
// Class: `as u16` truncation on a length-like value
// ---------------------------------------------------------------------------
//
// `rows.len() as u16` truncates silently above 65535. Every
// occurrence of this shape in the workspace was a bug when the value
// was user-driven (a body with 65536 lines, a bracketed paste, a
// popup). The fix at each site was `u16::try_from(x).unwrap_or(
// u16::MAX)`. This test catches a regression of the same shape.

/// The workspace's existing count of `as u16` on length-like values.
/// Every occurrence was a bug when the value was user-driven; the
/// fix was `u16::try_from(x).unwrap_or(u16::MAX)`. This budget test
/// fails when a future commit adds a new one.
///
/// The value is set to the current count so the test passes on the
/// existing (already-reviewed) sites. A fresh `.len() as u16` added
/// anywhere in the workspace grows the count and the test fires.
const AS_U16_BUDGET: usize = 3;

#[test]
fn as_u16_on_length_like_values_budget_is_not_exceeded() {
    let patterns = [
        ".len() as u16",
        ".count() as u16",
        "line_count as u16",
        "rows as u16",
    ];
    let mut hits: Vec<(PathBuf, usize, String)> = Vec::new();
    for path in all_production_sources() {
        for (lineno, line) in production_lines(&path) {
            if is_comment(&line) {
                continue;
            }
            if line.contains("try_from") {
                continue;
            }
            for pat in &patterns {
                if line.contains(pat) {
                    hits.push((path.clone(), lineno, line.clone()));
                    break;
                }
            }
        }
    }
    if hits.len() > AS_U16_BUDGET {
        let extra = &hits[AS_U16_BUDGET.min(hits.len())..];
        let extra = if extra.is_empty() { &hits[..] } else { extra };
        report(
            extra,
            &format!(
                "`as u16` on a length-like value grew from {AS_U16_BUDGET} to \
                 {}; new site(s) above. Casts truncate silently above \
                 65535; use `u16::try_from(x).unwrap_or(u16::MAX)` (with \
                 `.saturating_add(...)` for any offset), or raise \
                 `AS_U16_BUDGET` with a comment:",
                hits.len(),
            ),
        );
    }
}

// ---------------------------------------------------------------------------
// Class: seconds-only backup filename
// ---------------------------------------------------------------------------
//
// Every backup name in the workspace carries a `bak-` marker with a
// suffix that is meant to be unique per writer. A bare seconds
// timestamp is not: two invocations in the same second produce the
// same path, so the first writer's backup is overwritten by the
// second's. The fix everywhere is pid + process-local counter +
// nanosecond clock, which is three placeholders in the format
// string. This lint flags a `bak-` literal with fewer than two.

#[test]
fn no_seconds_only_backup_filenames_in_production() {
    let mut bad: Vec<(PathBuf, usize, String)> = Vec::new();
    for path in all_production_sources() {
        for (lineno, line) in production_lines(&path) {
            if is_comment(&line) {
                continue;
            }
            for lit in string_literals(&line) {
                if !lit.contains("bak-") {
                    continue;
                }
                if lit.matches('{').count() < 2 {
                    bad.push((path.clone(), lineno, lit));
                }
            }
        }
    }
    if !bad.is_empty() {
        report(
            &bad,
            "seconds-only backup filename(s) in production — a bare \
             timestamp is not unique across concurrent writers, so the \
             first writer's backup is overwritten. Use pid + counter + \
             nanosecond clock (three placeholders):",
        );
    }
}

// ---------------------------------------------------------------------------
// Class: bare wall-clock value used as a unique filename
// ---------------------------------------------------------------------------
//
// A wall-clock value is not unique across concurrent writers in the
// same clock tick. This session fixed four separate instances where a
// filename (or a name from which a filename and a git branch are
// built) was composed from a bare clock token:
//
//   * `~/.kod/sessions/{ts}.jsonl`            (session_log)
//   * `.kod/handoff-{stamp}.md`               (TUI handoff)
//   * `.kod/session-{ts}.html`                (TUI export)
//   * worktree `wt-{token}` and `wt/{token}`  (kod-core-quality)
//
// Each produced a real collision when two writers ran in the same
// tick: interleaved JSONL, a silently overwritten handoff document,
// an export that clobbered its predecessor, and a `git worktree add`
// that failed because the branch already existed.
//
// The fix everywhere is to mix in a per-writer discriminator — pid,
// a process-local counter, or sub-second nanos. This lint flags a
// format literal that ends in a filename extension, contains a
// clock-shaped placeholder, and contains none of those
// discriminators. It is deliberately narrow: a log line that names a
// time is not flagged, only a filename.

/// Placeholder names that read as "a wall clock".
const CLOCK_TOKENS: &[&str] = &[
    "{ts}",
    "{stamp}",
    "{now}",
    "{timestamp}",
    "{time}",
    "{date}",
    "{secs}",
    "{ms}",
    "{unix_ms}",
    "{unix_secs}",
    "{unix_nanos}",
    "{ts_ms}",
];

/// Substrings that mean "this writer is distinguished from another".
/// A literal containing any of these is not flagged.
const DISCRIMINATORS: &[&str] = &[
    "process::id",
    "pid",
    "seq",
    "counter",
    "uuid",
    "nanos",
    "subsec",
    "{}-{}", // an extra positional placeholder
];

/// Filename extensions that mark a literal as "this is a filename".
const FILENAME_EXTS: &[&str] = &[
    ".json", ".jsonl", ".html", ".md", ".log", ".txt", ".toml", ".yaml", ".yml",
];

#[test]
fn no_bare_wall_clock_filenames_in_production() {
    let mut bad: Vec<(PathBuf, usize, String)> = Vec::new();
    for path in all_production_sources() {
        // Escape hatch: a file that opens with `create_new(true)`
        // is explicitly refusing to overwrite, so the writer is
        // already collision-aware — `plan::autosave_plan` walks a
        // numbered candidate list under that flag and is correct.
        // The class this lint guards against is a *silent* overwrite
        // via `fs::write` / `create(true)`, which is what the four
        // bugs this session all used.
        let whole = std::fs::read_to_string(&path).unwrap_or_default();
        if whole.contains("create_new(true)") {
            continue;
        }
        for (lineno, line) in production_lines(&path) {
            if is_comment(&line) {
                continue;
            }
            for lit in string_literals(&line) {
                let is_filename = FILENAME_EXTS.iter().any(|e| lit.ends_with(e));
                if !is_filename {
                    continue;
                }
                if !CLOCK_TOKENS.iter().any(|t| lit.contains(t)) {
                    continue;
                }
                if DISCRIMINATORS.iter().any(|d| lit.contains(d)) {
                    continue;
                }
                bad.push((path.clone(), lineno, lit));
            }
        }
    }
    if !bad.is_empty() {
        report(
            &bad,
            "bare wall-clock filename(s) in production — a clock token is not \
             unique across writers in the same tick. Mix in a discriminator \
             (pid, a process-local counter, sub-second nanos) before the \
             extension, or open the file with `create_new(true)` and handle \
             `AlreadyExists` explicitly:",
        );
    }
}

// ---------------------------------------------------------------------------
// Class: subtraction on `.len()` without a saturating shape
// ---------------------------------------------------------------------------
//
// The release profile has `overflow-checks = false`, so an underflow
// panics in dev/test and wraps to a huge value in release — a class
// the tests cannot catch by running the code. Every subtraction on
// `.len()` in the workspace is guarded by an explicit bounds check,
// a `saturating_sub`, or a prior `if x.len() > n`.
//
// A file-scan cannot tell "guarded by an earlier line" from
// "unguarded", so this is a **budget** test: the current count is
// recorded, and a change that grows the count fails. The intent is
// to catch a *new* unguarded subtraction introduced by a future
// commit, not to police the existing (guarded) shapes.
const SUBTRACTION_BUDGET: usize = 63;

#[test]
fn unguarded_length_subtraction_budget_is_not_exceeded() {
    let mut hits: Vec<(PathBuf, usize, String)> = Vec::new();
    for path in all_production_sources() {
        for (lineno, line) in production_lines(&path) {
            if is_comment(&line) {
                continue;
            }
            if line.contains("saturating_sub") || line.contains("checked_sub") {
                continue;
            }
            if let Some(idx) = line.find(".len() -") {
                let after = &line[idx + ".len() -".len()..];
                let after = after.trim();
                if after.is_empty() {
                    continue;
                }
                let first = after.chars().next().unwrap();
                if first.is_ascii_alphanumeric() || first == '_' {
                    hits.push((path.clone(), lineno, line.clone()));
                }
            }
        }
    }
    if hits.len() > SUBTRACTION_BUDGET {
        // Print only the new-looking entries (the last few) so the
        // failure is actionable. The full list would be ~60 lines of
        // pre-existing guarded sites.
        let extra = &hits[SUBTRACTION_BUDGET.min(hits.len())..];
        let extra = if extra.is_empty() { &hits[..] } else { extra };
        report(
            extra,
            &format!(
                "`.len() -` sites grew from {SUBTRACTION_BUDGET} to {}; new \
                 site(s) above. The class is checked in dev/test but wraps \
                 silently in release (`overflow-checks = false`). Use \
                 `saturating_sub` or a `checked_sub` at the new site(s), or \
                 raise `SUBTRACTION_BUDGET` in this test with a comment:",
                hits.len(),
            ),
        );
    }
}

// ---------------------------------------------------------------------------
// Class: a watchdog-shaped cancel that emits a bogus retry/failure event
// ---------------------------------------------------------------------------
//
// The heartbeat watchdog in `swarm_runner.rs` cancels a wedged agent
// so the retry loop re-dispatches it. It does not know the attempt
// number, so it must not emit `AgentRetrying { attempt, max_attempts
// }` — a hardcoded `0, 0` renders as "retrying (0/0)" (design H-R6
// names this). Nor may it emit `AgentFailed`, which marks the agent
// finished+failed in the panel; `swarm_set_retrying` does not clear
// that, so a retrying agent would show as dead. The retry loop emits
// the accurate event when the cancelled attempt fails.

#[test]
fn no_bogus_watchdog_retry_event() {
    let src = fs::read_to_string(workspace_root().join("crates/kod-core/src/swarm_runner.rs"))
        .expect("swarm_runner.rs is readable");

    // The watchdog task block: from its spawn to the line that ends
    // it (`let mut remaining`, the wave loop that follows). Bounded
    // this way so the scan does not run into the *wave loop*, whose
    // `AgentFailed` emissions are legitimate (a real agent failure).
    let Some(start) = src.find("let watchdog_task = tokio::spawn") else {
        panic!("watchdog_task marker not found in swarm_runner.rs");
    };
    let Some(end_rel) = src[start..].find("let mut remaining") else {
        panic!("watchdog_task end marker (`let mut remaining`) not found");
    };
    let block = &src[start..start + end_rel];
    // Line number of `block`'s first line in the file, for the report.
    let start_line = src[..start].lines().count() + 1;

    let mut bad: Vec<(PathBuf, usize, String)> = Vec::new();
    let path = workspace_root().join("crates/kod-core/src/swarm_runner.rs");
    for (i, line) in block.lines().enumerate() {
        let trimmed = line.trim();
        if is_comment(trimmed) {
            continue;
        }
        if trimmed.contains("AgentRetrying") || trimmed.contains("AgentFailed") {
            bad.push((path.clone(), start_line + i, trimmed.to_string()));
        }
    }
    if !bad.is_empty() {
        report(
            &bad,
            "the watchdog must not emit AgentRetrying (it cannot know the \
             attempt number) or AgentFailed (it would mark a retrying agent \
             as dead). The retry loop emits the accurate event:",
        );
    }
}
