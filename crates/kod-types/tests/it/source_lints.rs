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

// ---------------------------------------------------------------------------
// Class: a content reader that honours `can_read` but not `read_protection`
// ---------------------------------------------------------------------------
//
// `[policy.read_protection]` (redact/refuse for secret paths) is installed
// by default, and `read_file` honours it. Four tools shipped without it —
// grep (search.rs), git_diff (git.rs), the internal-URL branch of
// read_file, and the speculative-read path — each returning a protected
// file's content in the clear. The fix is always the same: consult
// `context.read_protection` and redact or refuse.
//
// This lint flags a source file that reads file content (`read_to_string`
// or `fs::read(`) and gates it with `can_read`, but never mentions
// `read_protection`. Such a file is reading the filesystem on behalf of
// the model and forgetting the protection layer.
//
// Exemptions: a file whose reads never reach the model (an internal
// consumer, a test helper, a config loader). Listed explicitly so a new
// content reader must either honour the protection or justify an entry.

/// Files that read content + call `can_read` without consulting
/// `read_protection`, reviewed as non-content-returning.
///
/// - `lsp_tools.rs`: reads a file only to send its text to the language
///   server; the tool returns diagnostic metadata (file/line/column/
///   severity/message), never file content. The LSP extension set
///   (`.rs`/`.py`/`.ts`/`.go`) does not overlap the default protected set
///   (`.env`/`.pem`/`.key`/`.ssh/**`), and even an overlapping user glob
///   would only surface a diagnostic's message text.
const READ_PROTECTION_EXEMPT: &[&str] = &["crates/kod-core-tools/src/lsp_tools.rs"];

#[test]
fn content_reader_must_consult_read_protection() {
    let mut bad: Vec<(PathBuf, usize, String)> = Vec::new();
    for path in all_production_sources() {
        let rel = path
            .strip_prefix(workspace_root())
            .unwrap_or(&path)
            .to_string_lossy()
            .replace('\\', "/");
        if READ_PROTECTION_EXEMPT.contains(&rel.as_str()) {
            continue;
        }
        let Ok(content) = fs::read_to_string(&path) else {
            continue;
        };
        let mut reads_content = false;
        let mut calls_can_read = false;
        let mut mentions_protection = false;
        let mut first_read_line = 0usize;
        for (i, line) in production_lines(&path).iter().map(|(n, l)| (*n, l)) {
            if is_comment(line) {
                continue;
            }
            if line.contains("read_to_string") || line.contains("fs::read(") {
                reads_content = true;
                if first_read_line == 0 {
                    first_read_line = i;
                }
            }
            if line.contains("can_read") {
                calls_can_read = true;
            }
            if line.contains("read_protection") {
                mentions_protection = true;
            }
        }
        let _ = content; // silence an unused warning on some targets
        if reads_content && calls_can_read && !mentions_protection {
            bad.push((
                path.clone(),
                first_read_line,
                "reads content + can_read, never read_protection".to_string(),
            ));
        }
    }
    if !bad.is_empty() {
        report(
            &bad,
            "a content reader gates on `can_read` but never consults \
             `read_protection` — a protected file's content would reach \
             the model in the clear. Consult `context.read_protection` \
             (redact or refuse) or add the file to \
             `READ_PROTECTION_EXEMPT` with a reason:",
        );
    }
}

// ---------------------------------------------------------------------------
// Class: a queue filled at one end and drained at the other
// ---------------------------------------------------------------------------
//
// `LspClient::pending_notifications` is filled with `push_back` and must
// be drained FIFO (`pop_front`) so buffered publishes are processed in
// arrival order — the newest last, so it wins. Pre-fix it drained with
// `pop_back` (LIFO): two buffered diagnostics for the same file left the
// *older* one as `latest`, a false-clean the settle rule exists to
// prevent. A one-token mismatch with no functional test reaching it.

#[test]
fn pending_notifications_fill_and_drain_match() {
    let path = workspace_root().join("crates/kod-lsp/src/client.rs");
    let lines = production_lines(&path);

    let fills: Vec<(usize, String)> = lines
        .iter()
        .filter(|(_, l)| l.contains("pending_notifications.push_back("))
        .cloned()
        .collect();
    let drains: Vec<(usize, String)> = lines
        .iter()
        .filter(|(_, l)| {
            l.contains("pending_notifications.pop_front(")
                || l.contains("pending_notifications.pop_back(")
        })
        .cloned()
        .collect();

    if fills.is_empty() {
        panic!("no pending_notifications.push_back found; the lint's anchor moved");
    }
    let mut bad: Vec<(PathBuf, usize, String)> = Vec::new();
    for (line, text) in &drains {
        if text.contains("pending_notifications.pop_back(") {
            bad.push((path.clone(), *line, text.trim().to_string()));
        }
    }
    // The overflow-eviction site legitimately pops the front; a
    // `pop_back` there would drop the newest, which is also wrong, so
    // flagging any `pop_back` is correct.
    if !bad.is_empty() {
        report(
            &bad,
            "pending_notifications is filled with `push_back`; it must be \
             drained with `pop_front` (FIFO) so buffered publishes are \
             processed in arrival order. A `pop_back` makes the newest \
             arrive first and leaves the oldest as the latest answer:",
        );
    }
}

// ---------------------------------------------------------------------------
// Class: a model-supplied tool name reaches a gate un-canonicalized
// ---------------------------------------------------------------------------
//
// SEC-A-01: every gate (risk assessment, per-tool policy, taint
// escalation, session deny) keys on `call.tool_name` as the model wrote
// it, while alias resolution happens only at dispatch. A raw alias
// (`bash`, `exec`, `shell_exec`) therefore skipped every gate:
// `{"name":"bash","arguments":{"command":"rm -rf ~"}}` never ran the
// risk assessment because the gate compared against `execute_command`.
//
// The fix canonicalizes at parse time — every `ToolCall` constructed
// from model output must pass its name through
// `resolve_tool_name`. This lint flags a `tool_name:` assignment in a
// provider-facing call construction that does not.

#[test]
fn tool_calls_are_canonicalized_at_parse() {
    let mut bad: Vec<(PathBuf, usize, String)> = Vec::new();
    // Files that construct a ToolCall from model/provider output.
    let sites = [
        "crates/kod-core/src/engine/agent_loop.rs",
        "crates/kod-provider/src/replay.rs",
    ];
    for rel in sites {
        let path = workspace_root().join(rel);
        let lines = production_lines(&path);
        for (lineno, line) in &lines {
            let t = line.trim();
            if !t.starts_with("tool_name:") {
                continue;
            }
            // Allowed: the canonicalizing form, or a re-export of an
            // already-resolved value (a cloned call, a literal test
            // fixture).
            if t.contains("resolve_tool_name(") || t.contains(".tool_name.clone()") {
                continue;
            }
            // A literal string is a hand-built call (tests, fixtures).
            if t.contains('"') {
                continue;
            }
            bad.push((path.clone(), *lineno, t.to_string()));
        }
    }
    if !bad.is_empty() {
        report(
            &bad,
            "a ToolCall built from model output must canonicalize its \
             tool_name via resolve_tool_name, or an alias (bash, exec) \
             skips the risk/policy/taint gates that key on the real name:",
        );
    }
}

// ---------------------------------------------------------------------------
// Class: a truncate helper's name and its contract diverge
// ---------------------------------------------------------------------------
//
// P2 (session 15-16): `truncate_chars` performed a byte cap under a
// char name; `truncate("¡", 1)` returned "" because byte 1 is inside
// "¡". Roughly 15 display callers passed `MAX_*_CHARS` constants to the
// byte cap and silently received 1/2 to 1/3 the requested character
// count on CJK/accented content. The fix split the helper into
// `truncate_chars` (char cap) and `truncate_bytes` (byte cap), migrated
// byte-intent callers, and added two property tests for the name's
// contract.
//
// The property tests prove today's code. This lint prevents tomorrow's
// regression: a new `fn truncate_chars` that caps by bytes, or a new
// `truncate_chars(x, MAX_FOO_BYTES)` call, fails here before it can
// ship.
//
// Two scans:
//   1. Definitions: each `fn truncate_chars` body must use
//      `.char_indices().nth(` / `.chars().nth(` / delegate to
//      `strutil::truncate_chars`; each `fn truncate_bytes` body must
//      use `floor_char_boundary(` / delegate to `strutil::truncate_bytes`.
//   2. Call sites: no `truncate_chars(_, <_BYTES> | _.len())`; no
//      `truncate_bytes(_, <_CHARS> | _.chars().count())`.

fn is_test_flavored_file(path: &Path) -> bool {
    let name = path.file_name().and_then(|s| s.to_str()).unwrap_or("");
    let stem = path
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("");
    name == "tests.rs"
        || name == "prop_tests.rs"
        || stem.ends_with("_tests")
        || stem.starts_with("coverage_")
}

fn truncate_lint_scan() -> Vec<(PathBuf, usize, String)> {
    let mut bad = Vec::new();
    for path in all_production_sources() {
        if is_test_flavored_file(&path) {
            continue;
        }
        let lines = production_lines(&path);

        // --- Scan 1: definition bodies ---
        for (i, (lineno, line)) in lines.iter().enumerate() {
            let t = line.trim();
            // Match the signature; tolerate `pub`, `pub(crate)`, `#[inline]`.
            let is_chars_def = t.contains("fn truncate_chars(");
            let is_bytes_def = t.contains("fn truncate_bytes(");
            if !is_chars_def && !is_bytes_def {
                continue;
            }
            // Collect the next ~10 lines — enough for any realistic body.
            let window: String = lines
                .iter()
                .skip(i)
                .take(10)
                .map(|(_, l)| l.as_str())
                .collect::<Vec<_>>()
                .join("\n");
            if is_chars_def {
                let ok = window.contains("char_indices().nth(")
                    || window.contains("chars().nth(")
                    || window.contains("strutil::truncate_chars(")
                    || window.contains("super::truncate_chars(");
                if !ok {
                    bad.push((
                        path.clone(),
                        *lineno,
                        format!("fn truncate_chars body lacks char-cap: {t}"),
                    ));
                }
            } else if is_bytes_def {
                let ok = window.contains("floor_char_boundary(")
                    || window.contains("strutil::truncate_bytes(")
                    || window.contains("super::truncate_bytes(");
                if !ok {
                    bad.push((
                        path.clone(),
                        *lineno,
                        format!("fn truncate_bytes body lacks byte-cap: {t}"),
                    ));
                }
            }
        }

        // --- Scan 2: call-site budgets ---
        // Budget tokens that indicate a byte budget (mismatched with chars).
        let byte_budget_tokens: &[&str] =
            &["_BYTES", "_BYTE", "STRUCTURED_TOOL_MSG_CAP"];
        // Budget tokens that indicate a char budget (mismatched with bytes).
        let char_budget_tokens: &[&str] = &["_CHARS", "_CHAR"];
        for (lineno, line) in &lines {
            let t = line.trim_start();
            // Skip comments and definitions.
            if t.starts_with("//") || t.starts_with('*') {
                continue;
            }
            if t.contains("fn truncate_chars(") || t.contains("fn truncate_bytes(") {
                continue;
            }
            // Only look at actual calls.
            let has_chars_call = t.contains("truncate_chars(");
            let has_bytes_call = t.contains("truncate_bytes(");
            if has_chars_call {
                for tok in byte_budget_tokens {
                    if t.contains(tok) {
                        bad.push((
                            path.clone(),
                            *lineno,
                            format!("truncate_chars with byte budget `{tok}`: {t}"),
                        ));
                        break;
                    }
                }
                if t.contains(".len()") {
                    bad.push((
                        path.clone(),
                        *lineno,
                        format!("truncate_chars with .len() budget: {t}"),
                    ));
                }
            }
            if has_bytes_call {
                for tok in char_budget_tokens {
                    if t.contains(tok) {
                        bad.push((
                            path.clone(),
                            *lineno,
                            format!("truncate_bytes with char budget `{tok}`: {t}"),
                        ));
                        break;
                    }
                }
                if t.contains(".chars().count()") {
                    bad.push((
                        path.clone(),
                        *lineno,
                        format!("truncate_bytes with .chars().count() budget: {t}"),
                    ));
                }
            }
        }
    }
    bad
}

#[test]
fn truncate_helpers_name_matches_contract() {
    let bad = truncate_lint_scan();
    if !bad.is_empty() {
        let header = "a truncate helper whose name and contract diverge \
            silently truncates the wrong unit on non-ASCII input. \
            `fn truncate_chars` must cap by chars; `fn truncate_bytes` \
            by bytes. A caller must pass a budget of the kind the name \
            advertises (chars-named call -> chars-named budget; \
            bytes-named call -> bytes-named budget):";
        report(&bad, header);
    }
}

// ---------------------------------------------------------------------------
// Class: a truncate_chars display site guarded by a byte-length check
// ---------------------------------------------------------------------------
//
// Session-16's P2 fix made `truncate_chars` a char cap. Seven display
// sites in `kod-core/src/engine/render.rs` and nine more across the
// workspace still had the pre-fix shape:
//
//     if s.len() > N {
//         format!("{ellipsis}", truncate_chars(s, N))
//     } else {
//         owned
//     }
//
// On CJK input whose char count is at the cap but whose byte count
// exceeds it, the byte-length guard fires, `truncate_chars` returns
// the string unchanged, and the caller appends an ellipsis to a
// string that was never truncated. All sites were migrated to
// `kod_types::strutil::truncate_with_ellipsis` in sessions 16-18.
//
// This lint is the class-level guard: a future site that reintroduces
// the shape fails here. The helper itself (`truncate_with_ellipsis`)
// is allowed — it must call `truncate_chars` to do its job. The
// definition of `truncate_chars` itself is allowed.
//
// The class was found by hand across two sessions; this lint stops the
// third reintroduction.

#[test]
fn truncate_display_sites_use_helper() {
    let mut bad: Vec<(PathBuf, usize, String)> = Vec::new();
    for path in all_production_sources() {
        // Read only this file's production lines (past any test module).
        let lines = production_lines(&path);
        for (i, (lineno, line)) in lines.iter().enumerate() {
            // The call site — truncate_chars( present, not the definition.
            if !line.contains("truncate_chars(") || line.contains("fn truncate_chars") {
                continue;
            }
            // Skip the definition-in-scope case where the call is
            // inside the truncate_with_ellipsis helper.
            let near_helper = lines
                .iter()
                .skip(i.saturating_sub(6))
                .take(7)
                .any(|(_, l)| l.contains("fn truncate_with_ellipsis"));
            if near_helper {
                continue;
            }
            // Skip comments — the helper's own doc mentions the call
            // shape, and other docs explain the class.
            if is_comment(line) {
                continue;
            }
            // Look back up to 4 production lines for a byte-length guard.
            let start = i.saturating_sub(4);
            for (j, (_, prev)) in lines.iter().enumerate().skip(start).take(i - start) {
                if is_comment(prev) {
                    continue;
                }
                if prev.contains(".len() >") && !prev.contains("truncate_chars") {
                    let _ = j;
                    bad.push((
                        path.clone(),
                        *lineno,
                        format!("preceded by byte-length guard: {}", prev.trim()),
                    ));
                    break;
                }
            }
        }
    }

    if !bad.is_empty() {
        report(
            &bad,
            "a truncate_chars display site is guarded by a byte-length check \
             (s.len() > N). On CJK input whose char count is at the cap but \
             whose byte count exceeds it, the guard fires, truncate_chars \
             returns the string unchanged, and an ellipsis is appended to an \
             untruncated string. Use \
             kod_types::strutil::truncate_with_ellipsis(s, N, \"…\") instead:",
        );
    }
}
