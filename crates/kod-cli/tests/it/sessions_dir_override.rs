//! `sessions count`, `sessions latest`, and `budget show`'s fallback
//! through `KOD_SESSIONS_DIR`.
//!
//! `run_sessions(Count)` and `run_sessions(Latest)` walked
//! `~/.kod/sessions` directly; `newest_session_log` (used by
//! `run_budget_show(None)` and `run_jev_stats(None)`) did too. All
//! three now go through `commands::sessions_dir()`, which honours
//! `KOD_SESSIONS_DIR` first.
use kod_cli::commands::SessionsAction;
use kod_cli::commands::memory::run_sessions;
use kod_cli::commands::observability::run_budget_show;
use kod_core_state::session_log::SessionEntry;
use serial_test::serial;

struct SessionsDirEnv(Option<std::ffi::OsString>);

impl SessionsDirEnv {
    fn set(dir: &std::path::Path) -> Self {
        let prev = std::env::var_os("KOD_SESSIONS_DIR");
        // Safety: `#[serial]` on every caller ensures exclusivity.
        unsafe {
            std::env::set_var("KOD_SESSIONS_DIR", dir);
        }
        SessionsDirEnv(prev)
    }
}

impl Drop for SessionsDirEnv {
    fn drop(&mut self) {
        // Safety: same invariant as `set`.
        unsafe {
            match &self.0 {
                Some(prev) => std::env::set_var("KOD_SESSIONS_DIR", prev),
                None => std::env::remove_var("KOD_SESSIONS_DIR"),
            }
        }
    }
}

fn write_cost_log(path: &std::path::Path) {
    let e = SessionEntry::Cost {
        timestamp_ms: 1_700_000_000_000,
        holder: "session".to_string(),
        endpoint: "mock".to_string(),
        model: "m".to_string(),
        prompt_tokens: 10,
        completion_tokens: 5,
        cost_usd: 0.001,
    };
    let mut s = String::new();
    s.push_str(&serde_json::to_string(&e).unwrap());
    s.push('\n');
    std::fs::write(path, s).unwrap();
}

// ---- SessionsAction::Count -----------------------------------------

/// `kod sessions count` on an empty `KOD_SESSIONS_DIR` reports zero
/// and returns Ok — the first-run case.
#[serial]
#[tokio::test]
async fn sessions_count_empty_dir_is_ok() {
    let tmp = tempfile::TempDir::new().unwrap();
    let _guard = SessionsDirEnv::set(tmp.path());
    run_sessions(SessionsAction::Count)
        .await
        .expect("an empty sessions dir is not an error");
}

/// `kod sessions count` with two log files reports two and returns Ok.
#[serial]
#[tokio::test]
async fn sessions_count_with_two_logs_is_ok() {
    let tmp = tempfile::TempDir::new().unwrap();
    std::fs::write(tmp.path().join("a.jsonl"), "").unwrap();
    std::fs::write(tmp.path().join("b.jsonl"), "x").unwrap();
    let _guard = SessionsDirEnv::set(tmp.path());
    run_sessions(SessionsAction::Count)
        .await
        .expect("count over a populated sessions dir");
}

// ---- SessionsAction::Latest ----------------------------------------

/// `kod sessions latest` on an empty dir is `FileNotFound` — there is
/// no newest file to report.
#[serial]
#[tokio::test]
async fn sessions_latest_empty_dir_is_err() {
    let tmp = tempfile::TempDir::new().unwrap();
    let _guard = SessionsDirEnv::set(tmp.path());
    let err = run_sessions(SessionsAction::Latest)
        .await
        .expect_err("an empty sessions dir has no latest log");
    assert!(
        err.to_string().contains("not found") || err.to_string().contains("File"),
        "unexpected error: {err}",
    );
}

/// `kod sessions latest` with a populated dir returns Ok. The function
/// prints the path; the test pins that the populated-dir branch runs
/// rather than erroring.
#[serial]
#[tokio::test]
async fn sessions_latest_with_a_log_is_ok() {
    let tmp = tempfile::TempDir::new().unwrap();
    std::fs::write(tmp.path().join("only.jsonl"), "").unwrap();
    let _guard = SessionsDirEnv::set(tmp.path());
    run_sessions(SessionsAction::Latest)
        .await
        .expect("one log means that log is the latest");
}

// ---- run_budget_show(None) — the newest_session_log path -----------

/// `kod budget show` with no `--log` falls back to `newest_session_log`,
/// which now honours `KOD_SESSIONS_DIR`. With a populated dir and a
/// valid cost log, the function reads it and returns Ok.
#[serial]
#[tokio::test]
async fn budget_show_with_no_log_arg_uses_sessions_dir() {
    let tmp = tempfile::TempDir::new().unwrap();
    let path = tmp.path().join("costs.jsonl");
    write_cost_log(&path);
    let _guard = SessionsDirEnv::set(tmp.path());
    run_budget_show(None)
        .await
        .expect("budget show with no --log falls back to the newest session log");
}

/// `kod budget show` with no `--log` and an empty dir is an error
/// (`newest_session_log` returns None, `run_budget_show` maps it to a
/// Config error). Before this round, the fallback walked the real home
/// directory and could not be tested.
#[serial]
#[tokio::test]
async fn budget_show_with_no_log_arg_and_empty_dir_is_err() {
    let tmp = tempfile::TempDir::new().unwrap();
    let _guard = SessionsDirEnv::set(tmp.path());
    let err = run_budget_show(None)
        .await
        .expect_err("an empty sessions dir has no log for budget show to read");
    assert!(
        err.to_string().contains("no session log"),
        "unexpected error: {err}",
    );
}
