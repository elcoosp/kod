//! Coverage for four commands whose happy paths were untested after
//! the main arc: `skills validate` (lenient and strict),
//! `memory checkpoint` (list and diff-miss), and `trace replay`
//! (id-not-found and empty-prompt).
//!
//! The two `skills validate` tests run the same loop body twice —
//! once with a lenient parser and once with a strict one — so a
//! regression in either parser path fails here. The trace-replay
//! tests exercise the `read_traces` + `find` + early-return arms;
//! the success arm builds an engine and is out of scope for this
//! round.
use kod_cli::commands::CheckpointAction;
use kod_cli::commands::observability::run_trace_replay;
use kod_cli::commands::skills::{run_skills_validate, run_skills_validate_strict};
use kod_cli::commands::memory::run_checkpoint;
use kod_core_state::trace::{
    RoundKind, RoundTrace, TurnOutcome, TurnTrace,
};
use kod_core_state::trace_writer::TraceWriter;
use serial_test::serial;

// ---- shared env guard ----

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

// ---- helpers ----

fn write_skill(dir: &std::path::Path, name: &str) {
    std::fs::create_dir_all(dir).expect("create skills dir");
    let content = format!(
        "---\nname: {name}\ndescription: A test skill for {name}\nversion: 1.0.0\ncategory: testing\ntags:\n  - demo\n---\n\nBody for {name}.\n"
    );
    std::fs::write(dir.join(format!("{name}.md")), content).expect("write skill md");
}

fn write_config_pointing_at(cfg_dir: &std::path::Path, skills_dir: &std::path::Path) {
    std::fs::create_dir_all(cfg_dir).expect("create config dir");
    std::fs::write(
        cfg_dir.join("config.toml"),
        format!(
            "[skills]\nskills_dir = {:?}\n",
            skills_dir.to_str().expect("utf8 skills path"),
        ),
    )
    .expect("write config.toml");
}

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
            input_tokens: 100,
            output_tokens: 20,
            cache_read_tokens: None,
            tool_calls: vec![],
            retries: vec![],
            jev_decisions: 0,
            jev_cache_hits: 0,
        }],
        user_prompt: user_prompt.to_string(),
        outcome: TurnOutcome::Completed,
        reason: None,
        prompt_tokens: 100,
        completion_tokens: 20,
        cost_usd: 0.001,
        tool_call_count: 0,
        jev_decisions: 0,
        jev_cache_hits: 0,
        prompt_chars: user_prompt.chars().count(),
        reply_chars: 0,
    };
    let w = TraceWriter::open(path.to_path_buf()).expect("open trace writer");
    w.record(&trace).expect("record trace");
}

// ---- skills validate -----------------------------------------------

/// `kod skills validate` on a skills dir with one well-formed `md`:
/// the walkdir loop parses it, increments `ok`, and returns Ok.
#[serial]
#[tokio::test]
async fn skills_validate_with_a_valid_skill_is_ok() {
    let cfg = tempfile::TempDir::new().unwrap();
    let skills = tempfile::TempDir::new().unwrap();
    write_config_pointing_at(cfg.path(), skills.path());
    write_skill(skills.path(), "valid-skill");

    let _g = EnvGuard::set("KOD_CONFIG_DIR", cfg.path());
    run_skills_validate()
        .await
        .expect("a well-formed skill must validate without error");
}

/// Same fixture through the strict parser. The strict and lenient
/// bodies are near-identical; testing both catches a regression in
/// either.
#[serial]
#[tokio::test]
async fn skills_validate_strict_with_a_valid_skill_is_ok() {
    let cfg = tempfile::TempDir::new().unwrap();
    let skills = tempfile::TempDir::new().unwrap();
    write_config_pointing_at(cfg.path(), skills.path());
    write_skill(skills.path(), "strict-skill");

    let _g = EnvGuard::set("KOD_CONFIG_DIR", cfg.path());
    run_skills_validate_strict()
        .await
        .expect("a well-formed skill must validate under strict mode too");
}

// ---- checkpoint ----------------------------------------------------

/// `CheckpointAction::List` on a fresh `KOD_CHECKPOINT_DIR` (no
/// snapshots for the current cwd): prints the empty list and returns
/// Ok. The `KOD_CHECKPOINT_DIR` seam was added at `eacab9b`.
#[serial]
#[tokio::test]
async fn checkpoint_list_on_empty_dir_is_ok() {
    let ckpt = tempfile::TempDir::new().unwrap();
    let _g = EnvGuard::set("KOD_CHECKPOINT_DIR", ckpt.path());
    run_checkpoint(CheckpointAction::List { limit: 20 })
        .await
        .expect("listing an empty checkpoint dir must not error");
}

/// `CheckpointAction::Diff` for an id that does not exist:
/// `manager.find` returns `None`, and the command reports
/// `InvalidParameters` naming the id. That's the intended shape, not
/// a generic error.
#[serial]
#[tokio::test]
async fn checkpoint_diff_with_unknown_id_is_invalid_parameters() {
    let ckpt = tempfile::TempDir::new().unwrap();
    let _g = EnvGuard::set("KOD_CHECKPOINT_DIR", ckpt.path());
    let err = run_checkpoint(CheckpointAction::Diff {
        id: "definitely-not-a-checkpoint".to_string(),
    })
    .await
    .expect_err("an unknown checkpoint id must be an error");
    assert!(
        err.to_string().contains("no checkpoint"),
        "unexpected error: {err}",
    );
}

// ---- trace replay (error arms only) --------------------------------

/// `run_trace_replay` for an id that does not appear in the trace
/// file: prints the "no trace with id" line and returns `Ok(()) —
/// a missing id is not an error, it's an empty match.
#[tokio::test]
async fn trace_replay_with_unknown_id_is_ok() {
    let tmp = tempfile::TempDir::new().unwrap();
    let trace_path = tmp.path().join("traces.jsonl");
    write_trace(&trace_path, 1, "hello");

    run_trace_replay(999, Some(trace_path), false)
        .await
        .expect("an id not present in the trace file is not an error");
}

/// A trace whose `user_prompt` is empty (written by a build that
/// predates the field): replay cannot rebuild the round, and the
/// command returns a `Config` error naming the id.
#[tokio::test]
async fn trace_replay_with_empty_prompt_is_config_error() {
    let tmp = tempfile::TempDir::new().unwrap();
    let trace_path = tmp.path().join("traces.jsonl");
    write_trace(&trace_path, 1, "");

    let err = run_trace_replay(1, Some(trace_path), false)
        .await
        .expect_err("a trace without a user prompt cannot be replayed");
    assert!(
        err.to_string().contains("has no user prompt"),
        "unexpected error: {err}",
    );
}
