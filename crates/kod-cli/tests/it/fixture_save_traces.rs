//! `run_fixture_save` end-to-end: a trace written by `TraceWriter`,
//! converted to a fixture.
//!
//! `fixtures_cli.rs` covers the three error arms (missing turns path,
//! empty turns path, refuse-to-overwrite). The happy path — read
//! traces, build one `RoundFixture` per turn with its tool calls and
//! results, write the fixture to disk — has never run. This file
//! covers it, plus `save_v2 --force` overwriting a seeded fixture.
//!
//! The trace is written through `TraceWriter`, the same code path
//! production uses, so a schema drift in `TurnTrace` breaks the test
//! where the engine would break — not one layer below.
use kod_cli::commands::fixtures::{run_fixture_save, run_fixture_save_v2};
use kod_core::Fixture;
use kod_core_state::trace::{
    RoundKind, RoundTrace, ToolCallTrace, ToolOutcomeKind, TurnOutcome, TurnTrace,
};
use kod_core_state::trace_writer::TraceWriter;
use serial_test::serial;

struct FixturesDirEnv(Option<std::ffi::OsString>);

impl FixturesDirEnv {
    fn set(dir: &std::path::Path) -> Self {
        let prev = std::env::var_os("KOD_FIXTURES_DIR");
        // Safety: `#[serial]` on every caller ensures exclusivity.
        unsafe {
            std::env::set_var("KOD_FIXTURES_DIR", dir);
        }
        FixturesDirEnv(prev)
    }
}

impl Drop for FixturesDirEnv {
    fn drop(&mut self) {
        // Safety: same invariant as `set`.
        unsafe {
            match &self.0 {
                Some(prev) => std::env::set_var("KOD_FIXTURES_DIR", prev),
                None => std::env::remove_var("KOD_FIXTURES_DIR"),
            }
        }
    }
}

fn one_tool_turn() -> TurnTrace {
    TurnTrace {
        id: 1,
        holder: "session".to_string(),
        parent: None,
        started_at_ms: 1_700_000_000_000,
        ended_at_ms: 1_700_000_001_000,
        rounds: vec![RoundTrace {
            kind: RoundKind::Unknown,
            endpoint: "mock".to_string(),
            model: "mock-model".to_string(),
            started_at_ms: 1_700_000_000_100,
            duration_ms: 50,
            input_tokens: 100,
            output_tokens: 20,
            cache_read_tokens: None,
            tool_calls: vec![ToolCallTrace {
                name: "write_file".to_string(),
                args_hash: "abc123".to_string(),
                arguments: serde_json::json!({"path": "x.txt", "content": "hi"}),
                duration_ms: 5,
                outcome: ToolOutcomeKind::Success,
                output_bytes: 10,
                elided_lines: None,
                result_summary: Some("wrote 2 bytes".to_string()),
            }],
            retries: vec![],
            jev_decisions: 0,
            jev_cache_hits: 0,
        }],
        user_prompt: "please write the file".to_string(),
        outcome: TurnOutcome::Completed,
        reason: None,
        prompt_tokens: 100,
        completion_tokens: 20,
        cost_usd: 0.001,
        tool_call_count: 1,
        jev_decisions: 0,
        jev_cache_hits: 0,
        prompt_chars: 22,
        reply_chars: 5,
    }
}

fn write_one_turn(path: &std::path::Path) {
    let w = TraceWriter::open(path.to_path_buf()).expect("open trace writer");
    w.record(&one_tool_turn()).expect("record one turn");
}

// ---- run_fixture_save happy path -----------------------------------

/// One turn, one tool call: `run_fixture_save` reads the trace, builds
/// a `RoundFixture` carrying the call and its result, and writes the
/// fixture at the `KOD_FIXTURES_DIR`-derived path.
#[serial]
#[tokio::test]
async fn save_writes_a_fixture_from_a_one_tool_trace() {
    let tmp = tempfile::TempDir::new().unwrap();
    let _guard = FixturesDirEnv::set(tmp.path());
    let trace_path = tmp.path().join("turns.jsonl");
    write_one_turn(&trace_path);

    run_fixture_save("saved", &trace_path)
        .await
        .expect("run_fixture_save over a valid trace");

    let fixture_path = Fixture::default_path("saved").expect("default_path('saved')");
    assert_eq!(
        fixture_path.parent(),
        Some(tmp.path()),
        "the fixture must land under KOD_FIXTURES_DIR, got {}",
        fixture_path.display(),
    );
    let fixture = Fixture::load_from(&fixture_path).expect("load saved fixture");
    assert_eq!(fixture.name, "saved");
    assert_eq!(fixture.rounds.len(), 1, "one trace turn -> one round");

    let round = &fixture.rounds[0];
    assert_eq!(round.seq, 0);
    assert_eq!(round.user_prompt, "please write the file");
    assert!(
        !round.request_hash.is_empty(),
        "request_hash must be set on every round",
    );
    assert_eq!(
        round.response.tool_calls.len(),
        1,
        "the tool call must be copied into the fixture",
    );
    assert_eq!(round.response.tool_calls[0].name, "write_file");
    assert_eq!(
        round.response.tool_calls[0].arguments,
        serde_json::json!({"path": "x.txt", "content": "hi"}),
        "arguments must round-trip verbatim",
    );
    assert_eq!(
        round.response.tool_results.len(),
        1,
        "the tool result must be copied alongside the call",
    );
    assert_eq!(round.response.tool_results[0].tool_name, "write_file");
    assert!(!round.response.tool_results[0].is_error);
}

// ---- run_fixture_save_v2 with force --------------------------------

/// A fixture already exists at the destination; `save_v2(force = true)`
/// must overwrite it rather than error. The `force = false` refusal is
/// covered in `fixtures_cli.rs::save_v2_refuses_to_overwrite_without_force`.
#[serial]
#[tokio::test]
async fn save_v2_with_force_overwrites_a_seeded_fixture() {
    let tmp = tempfile::TempDir::new().unwrap();
    let _guard = FixturesDirEnv::set(tmp.path());
    let trace_path = tmp.path().join("turns.jsonl");
    write_one_turn(&trace_path);

    // Seed a fixture that the trace-derived one must replace.
    let seeded = Fixture::new("pre");
    assert_eq!(seeded.rounds.len(), 0, "seed starts empty");
    seeded
        .save_to(&tmp.path().join("pre.json"))
        .expect("seed fixture");

    run_fixture_save_v2("pre", Some(trace_path), true)
        .await
        .expect("save_v2 with force must overwrite an existing fixture");

    let overwritten = Fixture::load_from(&tmp.path().join("pre.json")).expect("load fixture");
    assert_eq!(
        overwritten.rounds.len(),
        1,
        "force overwrite must replace the seed with the trace-derived round",
    );
    assert_eq!(overwritten.rounds[0].user_prompt, "please write the file");
}
