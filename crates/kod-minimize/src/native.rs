//! Native (Rust-implemented) minimizer filters.
//!
//! # Why these exist alongside the TOML stages
//!
//! The TOML pipeline stages ([`crate::pipeline::Stage`]) are
//! line-oriented regex transforms. They cover the common cases:
//! strip ANSI, keep error lines, cap the length. They cannot parse
//! *structured* output — `cargo check --message-format=json` emits
//! NDJSON whose diagnostics have a span, a level, a message, and a
//! code; a regex stage would either mangle it or give up.
//!
//! A **native filter** is a Rust function that takes the whole text
//! and returns a rewritten form. It is registered by id and reached
//! from a TOML def through the `native` stage:
//!
//! ```toml
//! stages = [ { kind = "native", filter = "cargo-json" } ]
//! ```
//!
//! # The filters
//!
//! * **`cargo-json`** — `cargo check --message-format=json` output
//!   into a compact `error[E0308]: mismatched types --> src/main.rs:5:5`
//!   form, one diagnostic per line, dropping the JSON envelope.
//! * **`pytest-json`** — `pytest --json-report` output (a single JSON
//!   object) into the failure summaries.
//!
//! Each filter is total: malformed input falls through to the raw
//! text rather than erroring, because a minimizer that loses the
//! model's output is worse than one that does not minimize it.
//!
//! # What this does NOT do
//!
//! * Not a JSON pretty-printer. The filters read *specific* keys
//!   (cargo's `message.level` / `message.spans` / `message.message`);
//!   a shape they do not recognize falls through.
//! * Not a `jq`. A caller that wants arbitrary JSON rewriting
//!   writes a `replace` stage or adds a filter.

use serde_json::Value;

/// Run a native filter by id. `None` when the id is unknown — the
/// caller falls through to the raw text.
///
/// A filter that recognizes its input returns the rewritten form; a
/// filter that does not returns the input unchanged. Neither case
/// errors: the caller's contract is "minimize or pass through".
pub fn run(filter: &str, text: &str) -> Option<String> {
    match filter {
        "cargo-json" => Some(cargo_json(text)),
        "pytest-json" => Some(pytest_json(text)),
        _ => None,
    }
}

/// `cargo check --message-format=json` → compact diagnostics.
///
/// The output is NDJSON: one JSON object per line. Each object has a
/// `reason` field; `"compiler-message"` carries the diagnostic under
/// `message`. The diagnostic's `level` (`error`/`warning`),
/// `message` (the text), `code` (the `E0308`-style code, nested),
/// and `spans` (the source locations) are what a reader needs.
///
/// A line that is not JSON, or a JSON object with no `message`, is
/// skipped. Non-compiler-message reasons (`compiler-artifact`,
/// `build-finished`, …) are skipped — they carry no diagnostic.
fn cargo_json(text: &str) -> String {
    let mut out = String::new();
    let mut saw_diagnostic = false;
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let Ok(v) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        if v.get("reason").and_then(|r| r.as_str()) != Some("compiler-message") {
            continue;
        }
        let Some(msg) = v.get("message") else { continue };
        let level = msg
            .get("level")
            .and_then(|l| l.as_str())
            .unwrap_or("error");
        // The first span's file/line/col is the primary location.
        let location = msg
            .get("spans")
            .and_then(|s| s.as_array())
            .and_then(|spans| {
                spans
                    .iter()
                    .find(|s| s.get("is_primary").and_then(|p| p.as_bool()).unwrap_or(false))
                    .or_else(|| spans.first())
            })
            .and_then(|span| {
                let file = span.get("file_name")?.as_str()?;
                let line = span.get("line_start")?.as_u64()?;
                let col = span.get("column_start")?.as_u64()?;
                Some(format!("{file}:{line}:{col}"))
            });
        let code = msg
            .get("code")
            .and_then(|c| c.get("code"))
            .and_then(|c| c.as_str());
        let message = msg.get("message").and_then(|m| m.as_str()).unwrap_or("");
        let mut line_out = String::new();
        line_out.push_str(level);
        if let Some(c) = code {
            line_out.push('[');
            line_out.push_str(c);
            line_out.push(']');
        }
        line_out.push_str(": ");
        line_out.push_str(message);
        if let Some(loc) = location {
            line_out.push_str(" --> ");
            line_out.push_str(&loc);
        }
        out.push_str(&line_out);
        out.push('\n');
        saw_diagnostic = true;
    }
    if saw_diagnostic { out } else { text.to_string() }
}

/// `pytest --json-report` → failure summaries.
///
/// The report is one JSON object with a `tests` array. Each test has
/// `nodeid` and `outcome`. A failing test also has a `call.longrepr`
/// with the traceback.
fn pytest_json(text: &str) -> String {
    let Ok(v) = serde_json::from_str::<Value>(text) else {
        return text.to_string();
    };
    let Some(tests) = v.get("tests").and_then(|t| t.as_array()) else {
        return text.to_string();
    };
    let mut out = String::new();
    let mut failed = 0usize;
    let mut passed = 0usize;
    for t in tests {
        let nodeid = t.get("nodeid").and_then(|n| n.as_str()).unwrap_or("?");
        let outcome = t.get("outcome").and_then(|o| o.as_str()).unwrap_or("?");
        match outcome {
            "failed" => {
                out.push_str("FAILED ");
                out.push_str(nodeid);
                if let Some(repr) = t
                    .get("call")
                    .and_then(|c| c.get("longrepr"))
                    .and_then(|r| r.as_str())
                {
                    out.push_str(" - ");
                    // Only the last line of the traceback: the
                    // assertion message.
                    if let Some(last) = repr.lines().last() {
                        out.push_str(last.trim());
                    }
                }
                out.push('\n');
                failed += 1;
            }
            "passed" => passed += 1,
            _ => {}
        }
    }
    out.push_str(&format!(
        "{} failed, {} passed\n",
        failed, passed,
    ));
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cargo_json_extracts_a_diagnostic() {
        let line = r#"{"reason":"compiler-message","message":{"level":"error","message":"mismatched types","code":{"code":"E0308"},"spans":[{"file_name":"src/main.rs","line_start":5,"column_start":5,"is_primary":true}]}}"#;
        let out = cargo_json(line);
        assert!(out.contains("error[E0308]"), "got: {out}");
        assert!(out.contains("mismatched types"), "got: {out}");
        assert!(out.contains("src/main.rs:5:5"), "got: {out}");
    }

    #[test]
    fn cargo_json_skips_non_diagnostic_lines() {
        let text = "{\"reason\":\"compiler-artifact\",\"target\":{}}\n\
                    {\"reason\":\"build-finished\",\"success\":true}";
        // No diagnostics: the raw text is returned unchanged.
        assert_eq!(cargo_json(text), text);
    }

    #[test]
    fn cargo_json_skips_non_json_lines() {
        let text = "Compiling foo\n{\"reason\":\"compiler-message\",\"message\":{\"level\":\"warning\",\"message\":\"unused\",\"spans\":[]}}";
        let out = cargo_json(text);
        assert!(out.contains("warning: unused"), "got: {out}");
        assert!(!out.contains("Compiling"), "got: {out}");
    }

    #[test]
    fn cargo_json_handles_a_diagnostic_with_no_spans() {
        let line = r#"{"reason":"compiler-message","message":{"level":"error","message":"aborting due to 1 previous error","spans":[]}}"#;
        let out = cargo_json(line);
        assert!(out.contains("error: aborting"), "got: {out}");
        assert!(!out.contains("-->"), "no location for a span-less diagnostic");
    }

    #[test]
    fn cargo_json_prefers_the_primary_span() {
        let line = r#"{"reason":"compiler-message","message":{"level":"error","message":"x","spans":[{"file_name":"a.rs","line_start":1,"column_start":1,"is_primary":false},{"file_name":"b.rs","line_start":2,"column_start":2,"is_primary":true}]}}"#;
        let out = cargo_json(line);
        assert!(out.contains("b.rs:2:2"), "got: {out}");
    }

    #[test]
    fn pytest_json_summarizes_failures() {
        let report = r#"{"tests":[
            {"nodeid":"test_a.py::test_x","outcome":"failed","call":{"longrepr":"traceback\nassert 1 == 2"}},
            {"nodeid":"test_a.py::test_y","outcome":"passed"}
        ]}"#;
        let out = pytest_json(report);
        assert!(out.contains("FAILED test_a.py::test_x"), "got: {out}");
        assert!(out.contains("assert 1 == 2"), "got: {out}");
        assert!(out.contains("1 failed, 1 passed"), "got: {out}");
    }

    #[test]
    fn pytest_json_passes_through_non_json() {
        let text = "not a report";
        assert_eq!(pytest_json(text), text);
    }

    #[test]
    fn an_unknown_filter_returns_none() {
        assert!(run("no-such-filter", "x").is_none());
    }
}
