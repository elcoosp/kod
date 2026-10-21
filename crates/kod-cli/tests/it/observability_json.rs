//! JSON output of the observability subcommands.
//!
//! `run_plan_json`, `run_decisions_json`, and `run_trace_json` each
//! load state or a trace file from a caller-supplied path and print
//! the serialized `serde_json::Value`. Each is now a one-line wrapper
//! over a `_json_string(...) -> Result<String>` helper, so the test
//! asserts on the parsed JSON without capturing stdout.
use kod_cli::commands::observability::{
    decisions_json_string, plan_json_string, trace_json_string,
};

/// An empty state file parses to the empty `EngineState`, and
/// `plans.get("session")` is `None` — which serializes to `null`, not
/// `{}` and not a missing key. That distinction matters: a caller
/// piping `kod plan --json | jq .` sees `null`, and a wrapper that
/// printed `{}` instead would report "an empty plan" where the truth
/// is "no plan".
#[test]
fn plan_json_on_missing_state_is_null() {
    let tmp = tempfile::TempDir::new().unwrap();
    let path = tmp.path().join("state.json");
    let s = plan_json_string(Some(path)).expect("missing state is not an error");
    let v: serde_json::Value = serde_json::from_str(&s).expect("valid JSON");
    assert!(v.is_null(), "expected null, got {v:?}");
}

/// `{}` parses to an empty state, so `decision_logs.get("session")` is
/// `None` and the JSON body is `null`.
#[test]
fn decisions_json_on_empty_state_is_null() {
    let tmp = tempfile::TempDir::new().unwrap();
    let path = tmp.path().join("state.json");
    std::fs::write(&path, "{}").unwrap();
    let s = decisions_json_string(Some(path)).expect("empty state is not an error");
    let v: serde_json::Value = serde_json::from_str(&s).expect("valid JSON");
    assert!(v.is_null(), "expected null, got {v:?}");
}

/// An empty trace file deserializes to `[]`, not `null`. `read_traces`
/// reads line-by-line and returns an empty vec for an empty file; the
/// JSON is an array.
#[test]
fn trace_json_on_empty_file_is_empty_array() {
    let tmp = tempfile::TempDir::new().unwrap();
    let path = tmp.path().join("turns.jsonl");
    std::fs::write(&path, "").unwrap();
    let s = trace_json_string(Some(path)).expect("empty trace file is not an error");
    let v: serde_json::Value = serde_json::from_str(&s).expect("valid JSON");
    assert!(
        v.is_array(),
        "empty trace file must serialize to [], got {v:?}",
    );
    assert_eq!(
        v.as_array().map(|a| a.len()),
        Some(0),
        "expected empty array, got {v:?}",
    );
}

/// The output is always *parseable* JSON. A regression that printed a
/// human-readable prefix (or the wrong serializer) would fail this
/// without needing a specific shape assertion.
#[test]
fn all_three_outputs_parse_as_json() {
    let tmp = tempfile::TempDir::new().unwrap();
    // Both files must exist — plan/decisions read the same state file,
    // trace reads its own JSONL. trace_json_string propagates an IO
    // error for a missing file (unlike run_trace_list, which guards on
    // !path.exists()); see trace_json_on_missing_file_errors below.
    std::fs::write(tmp.path().join("state.json"), "{}").unwrap();
    std::fs::write(tmp.path().join("turns.jsonl"), "").unwrap();
    for (name, s) in [
        (
            "plan",
            plan_json_string(Some(tmp.path().join("state.json"))),
        ),
        (
            "decisions",
            decisions_json_string(Some(tmp.path().join("state.json"))),
        ),
        (
            "trace",
            trace_json_string(Some(tmp.path().join("turns.jsonl"))),
        ),
    ] {
        let s = s.unwrap_or_else(|e| panic!("{name} failed: {e}"));
        serde_json::from_str::<serde_json::Value>(&s)
            .unwrap_or_else(|e| panic!("{name} emitted non-JSON: {e}\n{s}"));
    }
}

/// **Characterization.** `run_trace_list` and `run_trace_json` disagree
/// on a missing trace file. `list` has an `if !path.exists()` guard
/// that prints a friendly hint and returns Ok; `--json` has no such
/// guard, so it propagates the IO error. Both are defensible (a
/// human-facing list should not fail on "no data yet"; a
/// machine-facing JSON pipe has nothing to emit for a missing file),
/// but the asymmetry is worth pinning so a future unification is a
/// decision rather than an accident.
///
/// If a later change makes either side consistent with the other,
/// this test is the one to flip.
#[test]
fn trace_json_on_missing_file_errors() {
    let tmp = tempfile::TempDir::new().unwrap();
    let missing = tmp.path().join("does-not-exist.jsonl");
    let result = trace_json_string(Some(missing));
    assert!(
        result.is_err(),
        "run_trace_json on a missing file must error (no JSON to emit); \
         run_trace_list's friendly-hint-and-Ok is the other half of this \
         asymmetry, pinned by trace_list_on_missing_file_is_ok.",
    );
}
