//! `kod jev tune`, `kod budget show`, `kod limits show`, `kod config migrate`.
//!
//! All reachable through `KOD_CONFIG_DIR`, plus `run_budget_show`'s
//! path argument. None builds an engine, spawns a process, or reads
//! stdin on the paths covered here.
//!
//! `run_jev_test` is not tested end-to-end: it builds a JevClient and
//! calls an external service. Its *disabled-config* failure branch
//! IS tested — `from_config` returns Ok(None) when `[jev]` is absent,
//! so the call fails before any network I/O.
use kod_cli::commands::config::run_config_migrate;
use kod_cli::commands::observability::{
    run_budget_show, run_jev_test, run_jev_tune_reset, run_jev_tune_set, run_jev_tune_show,
    run_limits_show,
};
use kod_core_state::session_log::SessionEntry;
use serial_test::serial;

struct ConfigDirEnv(Option<std::ffi::OsString>);

impl ConfigDirEnv {
    fn set(dir: &std::path::Path) -> Self {
        let prev = std::env::var_os("KOD_CONFIG_DIR");
        // Safety: `#[serial]` on every caller ensures exclusivity.
        unsafe {
            std::env::set_var("KOD_CONFIG_DIR", dir);
        }
        ConfigDirEnv(prev)
    }
}

impl Drop for ConfigDirEnv {
    fn drop(&mut self) {
        // Safety: same invariant as `set`.
        unsafe {
            match &self.0 {
                Some(prev) => std::env::set_var("KOD_CONFIG_DIR", prev),
                None => std::env::remove_var("KOD_CONFIG_DIR"),
            }
        }
    }
}

fn write_session_log(path: &std::path::Path, entries: &[SessionEntry]) {
    let mut s = String::new();
    for e in entries {
        s.push_str(&serde_json::to_string(e).expect("serialize entry"));
        s.push('\n');
    }
    std::fs::write(path, s).expect("write session log");
}

fn cost_entry(endpoint: &str, prompt: usize, completion: usize, usd: f64) -> SessionEntry {
    SessionEntry::Cost {
        timestamp_ms: 1_700_000_000_000,
        holder: "session".to_string(),
        endpoint: endpoint.to_string(),
        model: "test-model".to_string(),
        prompt_tokens: prompt,
        completion_tokens: completion,
        cost_usd: usd,
    }
}

// ---- run_jev_test (disabled-config branch) --------------------------

/// An empty config has `[jev] enabled = false` by default, so
/// `run_jev_test` fails at the disabled check before any network I/O.
/// The test does not need a running Jev service.
#[serial]
#[tokio::test]
async fn jev_test_with_jev_disabled_is_err() {
    let tmp = tempfile::TempDir::new().unwrap();
    let _guard = ConfigDirEnv::set(tmp.path());
    let err = run_jev_test()
        .await
        .expect_err("a disabled Jev must be an error, not a network call");
    assert!(
        err.to_string().contains("Jev") || err.to_string().contains("disabled"),
        "unexpected error: {err}",
    );
}

// ---- run_jev_tune_show / run_limits_show ---------------------------

/// `run_jev_tune_show` and `run_limits_show` are pure reads of the
/// effective config (the extracted `_lines` helpers do the work,
/// they are tested separately in `observability_render`; these are
/// the async wrappers around them).
#[serial]
#[tokio::test]
async fn jev_tune_show_and_limits_show_return_ok() {
    let tmp = tempfile::TempDir::new().unwrap();
    let _guard = ConfigDirEnv::set(tmp.path());
    run_jev_tune_show().await.expect("jev tune show");
    run_limits_show().await.expect("limits show");
}

// ---- run_jev_tune_set / run_jev_tune_reset -------------------------

/// **Characterization.** `run_jev_tune_set` on an unknown name
/// returns `InvalidParameters`, and the config file may exist on disk
/// afterward — not because the rejection wrote it, but because
/// `KodConfig::load_default()` writes a default config when the file
/// is absent. The set's own save is only reached on the success
/// branch; a rejected name returns before that.
///
/// The pin that matters is the error variant and message. Asserting
/// the file's absence would be asserting a property of
/// `load_default`, not of `run_jev_tune_set` — wrong test, right
/// code.
#[serial]
#[tokio::test]
async fn jev_tune_set_with_unknown_name_is_err() {
    let tmp = tempfile::TempDir::new().unwrap();
    let _guard = ConfigDirEnv::set(tmp.path());
    let err = run_jev_tune_set("definitely-not-a-threshold", 0.5)
        .await
        .expect_err("an unknown threshold name must be rejected");
    assert!(
        err.to_string().contains("unknown threshold"),
        "unexpected error: {err}",
    );
}

/// `run_jev_tune_set` on a real name writes the config file. The name
/// is the first entry in `JevThresholds::NAMES` — the test reads it
/// from the crate rather than pinning a specific one, so a rename of
/// the list does not break the test.
#[serial]
#[tokio::test]
async fn jev_tune_set_with_known_name_writes_config() {
    let tmp = tempfile::TempDir::new().unwrap();
    let _guard = ConfigDirEnv::set(tmp.path());
    let name = kod_config::JevThresholds::NAMES
        .first()
        .expect("NAMES is non-empty by construction");
    run_jev_tune_set(name, 0.5)
        .await
        .expect("setting a real threshold must succeed");
    assert!(
        tmp.path().join("config.toml").exists(),
        "set must write config.toml",
    );
}

/// `run_jev_tune_reset` writes the config even on an empty dir —
/// it writes the default thresholds, then saves.
#[serial]
#[tokio::test]
async fn jev_tune_reset_writes_config() {
    let tmp = tempfile::TempDir::new().unwrap();
    let _guard = ConfigDirEnv::set(tmp.path());
    run_jev_tune_reset().await.expect("reset must succeed");
    assert!(
        tmp.path().join("config.toml").exists(),
        "reset must write config.toml",
    );
}

// ---- run_budget_show -----------------------------------------------

/// An empty session log has no Cost entries, so the totals are zero
/// and the function returns Ok.
#[tokio::test]
async fn budget_show_empty_log_is_ok() {
    let tmp = tempfile::TempDir::new().unwrap();
    let path = tmp.path().join("empty.jsonl");
    std::fs::write(&path, "").unwrap();
    run_budget_show(Some(path))
        .await
        .expect("an empty session log is a zero-cost run, not an error");
}

/// A log with two Cost entries sums both. The assertion is on `Ok`
/// only — the totals are printed, not returned; asserting them would
/// need stdout capture. What the test pins is that the parsing path
/// (which sees a Cost variant) returns cleanly.
#[tokio::test]
async fn budget_show_with_cost_entries_is_ok() {
    let tmp = tempfile::TempDir::new().unwrap();
    let path = tmp.path().join("costs.jsonl");
    write_session_log(
        &path,
        &[
            cost_entry("openai", 100, 50, 0.01),
            cost_entry("anthropic", 200, 100, 0.02),
        ],
    );
    run_budget_show(Some(path))
        .await
        .expect("a log with Cost entries must parse cleanly");
}

// ---- run_config_migrate --------------------------------------------

/// `run_config_migrate --dry-run` on a fresh dir prints the candidate
/// config but does NOT write a file. The absence of `config.toml` is
/// the pin: a dry run that wrote would be the bug the flag exists to
/// prevent.
#[serial]
#[tokio::test]
async fn config_migrate_dry_run_does_not_write() {
    let tmp = tempfile::TempDir::new().unwrap();
    let _guard = ConfigDirEnv::set(tmp.path());
    run_config_migrate(true)
        .await
        .expect("dry run on a fresh dir");
    assert!(
        !tmp.path().join("config.toml").exists(),
        "dry run must not create config.toml",
    );
}

/// `run_config_migrate` on a fresh dir writes a v2 default config.
/// A second call on the same dir reports "already at v2" and leaves
/// the file unchanged.
#[serial]
#[tokio::test]
async fn config_migrate_writes_then_reports_no_migration() {
    let tmp = tempfile::TempDir::new().unwrap();
    let _guard = ConfigDirEnv::set(tmp.path());

    run_config_migrate(false)
        .await
        .expect("migrate on a fresh dir");
    let path = tmp.path().join("config.toml");
    assert!(path.exists(), "migrate did not create {path:?}");

    // Second call is a no-op that must not error.
    run_config_migrate(false)
        .await
        .expect("second migrate on an already-v2 file");
    assert!(path.exists(), "the v2 file survives the second call");
}
