//! Tripwire: session-reachable crates must never write to the terminal
//! directly.
//!
//! While the TUI owns the terminal (alternate screen + raw mode), a
//! bare `println!` / `eprintln!` lands at the live cursor, garbles the
//! ratatui frame, and pushes transcript rows over the input box. The
//! diff-based redraw never repairs cells it believes are unchanged, so
//! the damage persists for the rest of the session.
//!
//! Log through `tracing` (routed to `~/.kod/session.log` by
//! `kod_cli::logging::SessionSafeWriter`) or surface through the
//! transcript (`push_system_message`) instead.
//!
//! This is a heuristic tripwire, not a parser: it scans non-test code
//! for the four write macros. Extend `CRATES` when a new crate becomes
//! reachable from the TUI loop; shrink `ALLOWED` over time, never grow.

use std::path::{Path, PathBuf};

/// Crates whose `src/` is reachable from the running TUI loop.
const CRATES: &[&str] = &[
    "kod-tui",
    "kod-core",
    "kod-provider",
    "kod-provider-openai",
    "kod-provider-anthropic",
    "kod-tools",
];

/// Known, deliberate exceptions. Must shrink over time, never grow.
const ALLOWED: &[&str] = &[];

/// Inline escape hatch for the (single) sanctioned print path:
/// `suspend_and_print` writes only while the TUI is already
/// suspended, so it tags its lines `// tripwire:allow`. Anything else
/// — including new prints in the same file — still trips the test.
const ALLOW_MARKER: &str = "tripwire:allow";

fn collect_rs_files(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(rd) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in rd.flatten() {
        let path = entry.path();
        if path.is_dir() {
            collect_rs_files(&path, out);
        } else if path.extension().map(|x| x == "rs").unwrap_or(false) {
            out.push(path);
        }
    }
}

#[test]
fn no_direct_terminal_writes_outside_tests() {
    let manifest_dir = Path::new(env!("CARGO_MANIFEST_DIR"));
    let crates_dir = manifest_dir.parent().expect("crates dir");
    let mut hits: Vec<String> = Vec::new();

    for name in CRATES {
        let mut files = Vec::new();
        collect_rs_files(&crates_dir.join(name).join("src"), &mut files);
        files.sort();
        for file in files {
            let Ok(src) = std::fs::read_to_string(&file) else {
                continue;
            };
            // A whole file that is a test module (its body starts with
            // the inner `#![cfg(test)]`) is test code throughout.
            if src.trim_start().starts_with("#![cfg(test)]") {
                continue;
            }
            // Everything after the first `#[cfg(test)]` marker is test
            // code: test binaries and test modules may legitimately
            // write to the terminal.
            let non_test = src.split("#[cfg(test)]").next().unwrap_or("");
            for (i, line) in non_test.lines().enumerate() {
                let t = line.trim_start();
                let is_write = t.starts_with("println!")
                    || t.starts_with("eprintln!")
                    || t.starts_with("print!")
                    || t.starts_with("eprint!")
                    || t.starts_with("dbg!");
                if is_write
                    && !t.contains(ALLOW_MARKER)
                    && !ALLOWED.iter().any(|a| file.to_string_lossy().contains(a))
                {
                    hits.push(format!("{}:{}: {}", file.display(), i + 1, t));
                }
            }
        }
    }

    assert!(
        hits.is_empty(),
        "raw terminal writes found — they garble the TUI frame \
         (use `tracing` or `push_system_message` instead):\n{}",
        hits.join("\n")
    );
}
