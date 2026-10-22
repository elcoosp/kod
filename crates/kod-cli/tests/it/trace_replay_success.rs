//! `run_trace_replay`'s success and divergence arms.
//!
//! The two error arms (id-not-found, empty-user-prompt) are already in
//! `residual_paths.rs`. This file covers what comes after — the call to
//! `drive_prompt_through_fresh_engine` (a ~40-line function that builds
//! a fresh engine with a `ReplayProvider` and captures the request the
//! engine sends), the `RequestSummary::from_request` comparison, and
//! both `strict` branches.
//!
//! The trace written by the test has a `request_summary` built from
//! arbitrary numbers — so its hash will almost certainly differ from
//! the hash of the request the fresh engine actually sends. That's
//! fine: with `strict = false` the function prints "diverged" and
//! returns `Ok(())`; with `strict = true` it returns `Err` naming the
//! divergence. Both paths exercise the whole body.
//!
//! `drive_prompt_through_fresh_engine` uses `ReplayProvider`, not the
//! config's `base_url`, so no mock HTTP endpoint is needed — the only
//! environment redirections are `HOME` (the fresh engine's DB) and
//! `KOD_CONFIG_DIR` (its config load).
use kod_cli::commands::observability::run_trace_replay;
use kod_core_state::trace::{RoundKind, RoundTrace, TurnOutcome, TurnTrace};
use kod_core_state::trace_writer::TraceWriter;
use serial_test::serial;

struct EnvGuard {
    key: &'static str,
    prev: Option<std::ffi::OsString>,
}

impl EnvGuard {
    fn set(key: &'static str, value: &std::path::Path) -> Self {
        let prev = std::env::var_os(key);
        // Safety: `#[serial]` on every caller ensures exclusivity.
        unsafe {
            std::env::set_var(key, value);
        }
        EnvGuard { key, prev }
    }
}

impl Drop for EnvGuard {
    fn drop(&mut self) {
        // Safety: same invariant as `set`.
        unsafe {
            match &self.prev {
                Some(prev) => std::env::set_var(self.key, prev),
                None => std::env::remove_var(self.key),
            }
        }
    }
}

/// A trace with a non-empty `user_prompt` so `run_trace_replay` gets
/// past the empty-prompt guard.
fn write_trace(path: &std::path::Path, id: u64, user_prompt: &str) {
    let trace = TurnTrace {
        id,
        holder: "session".to_string(),
        parent: None,
        started_at_ms: 1_700_000_000_000,
        ended_at_ms: 1_700_000_001_000,
        rounds: vec![RoundTrace {
            kind: RoundKind::ToolExec,
            endpoint: "mock".to_string(),
            model: "mock-model".to_string(),
            started_at_ms: 1_700_000_000_100,
            duration_ms: 50,
            input_tokens: 10,
            output_tokens: 5,
            cache_read_tokens: None,
            tool_calls: vec![],
            retries: vec![],
            jev_decisions: 0,
            jev_cache_hits: 0,
        }],
        user_prompt: user_prompt.to_string(),
        outcome: TurnOutcome::Completed,
        reason: None,
        prompt_tokens: 10,
        completion_tokens: 5,
        cost_usd: 0.0001,
        tool_call_count: 0,
        jev_decisions: 0,
        jev_cache_hits: 0,
        prompt_chars: user_prompt.chars().count(),
        reply_chars: 0,
    };
    let w = TraceWriter::open(path.to_path_buf()).expect("open trace writer");
    w.record(&trace).expect("record trace");
}

/// `strict = false`: the trace's request_summary hash almost certainly
/// differs from what the fresh engine produces (the trace has
/// `model = "mock-model"`; the engine will build its own model from
/// whatever the default endpoint is). Divergence is printed and
/// `Ok(())` is returned — the function's contract for non-strict
/// replay.
///
/// If the engine build itself fails, the test fails on the
/// "replay engine:" path — which is exactly the diagnostic we want,
/// not a silent pass through the engine-error arm.
#[serial]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn trace_replay_non_strict_reports_divergence_as_ok() {
    let tmp = tempfile::TempDir::new().unwrap();
    let trace_path = tmp.path().join("traces.jsonl");
    write_trace(&trace_path, 1, "replay me");
    let _home = EnvGuard::set("HOME", tmp.path());
    let _cfg = EnvGuard::set("KOD_CONFIG_DIR", tmp.path());
    let _sess = EnvGuard::set("KOD_SESSIONS_DIR", tmp.path());

    run_trace_replay(1, Some(trace_path), false)
        .await
        .expect("non-strict replay of a diverging trace must be Ok");
}

/// `strict = true`: the same divergence that is `Ok` non-strictly
/// becomes `Err(InvalidState("replay diverged on turn {id}"))`.
///
/// The assertion is on `"diverged"` specifically, not on
/// `"replay engine:"`. If the engine build fails (the other error
/// path), the test fails with the engine's own message and we know
/// to investigate that rather than silently pass.
#[serial]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn trace_replay_strict_reports_divergence_as_err() {
    let tmp = tempfile::TempDir::new().unwrap();
    let trace_path = tmp.path().join("traces.jsonl");
    write_trace(&trace_path, 7, "replay me strictly");
    let _home = EnvGuard::set("HOME", tmp.path());
    let _cfg = EnvGuard::set("KOD_CONFIG_DIR", tmp.path());
    let _sess = EnvGuard::set("KOD_SESSIONS_DIR", tmp.path());

    let err = run_trace_replay(7, Some(trace_path), true)
        .await
        .expect_err("strict replay of a diverging trace must be Err");
    let msg = err.to_string();
    assert!(
        msg.contains("diverged"),
        "strict replay must report divergence, got: {msg} \
         (if it says 'replay engine:', the fresh-engine build failed \
         -- investigate that instead of weakening the assertion)",
    );
    assert!(
        msg.contains("turn 7"),
        "the error must name the turn id, got: {msg}",
    );
}
