//! `kod init`, `kod profile`, and `kod policy show` — config-only.
//!
//! Each of these reads `KodConfig::config_dir()`, which honours
//! `KOD_CONFIG_DIR`, and each is a pure read or a write confined to
//! that directory. `run_doctor_fix` is NOT in this round: it calls
//! `config.memory_db_path()`, which falls back to `~/.kod/data/` when
//! the config does not override it, and a test must not write there.
//!
//! Every test is `#[serial]` because `KOD_CONFIG_DIR` is process-global.
use kod_cli::commands::ProfileAction;
use kod_cli::commands::admin::{run_init, run_policy, run_profile};
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

// ---- run_init --------------------------------------------------------

/// `kod init` on a fresh config dir writes `config.toml` there.
#[serial]
#[tokio::test]
async fn init_writes_a_config_file() {
    let tmp = tempfile::TempDir::new().unwrap();
    let _guard = ConfigDirEnv::set(tmp.path());

    run_init(false)
        .await
        .expect("init on a fresh dir must succeed");

    let path = tmp.path().join("config.toml");
    assert!(path.exists(), "init did not create {path:?}");
}

/// `kod init --force` on an existing config backs up the previous
/// file before overwriting. The backup name carries pid + seq +
/// nanos (the workspace convention) so the file glob is
/// `config.toml.bak-*`.
#[serial]
#[tokio::test]
async fn init_force_backs_up_the_existing_config() {
    let tmp = tempfile::TempDir::new().unwrap();
    let _guard = ConfigDirEnv::set(tmp.path());

    // First init writes a config.
    run_init(false).await.expect("first init");
    let path = tmp.path().join("config.toml");
    assert!(path.exists());

    // Second init with --force must back up the existing one.
    run_init(true).await.expect("init --force");

    let backups: Vec<_> = std::fs::read_dir(tmp.path())
        .unwrap()
        .filter_map(|e| e.ok())
        .filter(|e| {
            e.file_name()
                .to_string_lossy()
                .starts_with("config.toml.bak-")
        })
        .collect();
    assert!(
        !backups.is_empty(),
        "init --force did not create a config.toml.bak-* backup",
    );
}

// ---- run_profile -----------------------------------------------------

/// `kod profile list` in text mode. The PRESETS table is static —
/// no config read, no file write.
#[serial]
#[tokio::test]
async fn profile_list_text_is_ok() {
    let tmp = tempfile::TempDir::new().unwrap();
    let _guard = ConfigDirEnv::set(tmp.path());

    run_profile(ProfileAction::List { json: false })
        .await
        .expect("profile list text mode must succeed");
}

/// `kod profile list --json` — same table, JSON serialized.
#[serial]
#[tokio::test]
async fn profile_list_json_is_ok() {
    let tmp = tempfile::TempDir::new().unwrap();
    let _guard = ConfigDirEnv::set(tmp.path());

    run_profile(ProfileAction::List { json: true })
        .await
        .expect("profile list json mode must succeed");
}

/// `kod profile show` reads the effective `[llm]` config. With an
/// empty config.toml, `LlmConfig::default()` supplies the endpoint —
/// the show prints it and returns Ok.
#[serial]
#[tokio::test]
async fn profile_show_is_ok() {
    let tmp = tempfile::TempDir::new().unwrap();
    std::fs::write(tmp.path().join("config.toml"), "").unwrap();
    let _guard = ConfigDirEnv::set(tmp.path());

    run_profile(ProfileAction::Show)
        .await
        .expect("profile show must succeed on any valid config");
}

// ---- run_policy ------------------------------------------------------

/// `kod policy show` loads the PolicyEngine against the config and
/// the current cwd. Read-only: no policy.toml is created, no file is
/// touched. The cwd is the test runner's, which exists.
#[serial]
#[tokio::test]
async fn policy_show_is_ok() {
    let tmp = tempfile::TempDir::new().unwrap();
    std::fs::write(tmp.path().join("config.toml"), "").unwrap();
    let _guard = ConfigDirEnv::set(tmp.path());

    run_policy(kod_cli::commands::PolicyAction::Show)
        .await
        .expect("policy show must succeed with a valid config");
}

/// `kod policy forget` with no index and no daemon running. The
/// function checks for the socket, prints "No daemon listening", and
/// returns Ok. Before this round, `run_policy` had the same
/// `process::exit(1)` in the earlier sweep only at the socket-check
/// boundary; the current body returns Ok unconditionally.
#[serial]
#[tokio::test]
async fn policy_forget_with_no_daemon_is_ok() {
    let tmp = tempfile::TempDir::new().unwrap();
    std::fs::write(tmp.path().join("config.toml"), "").unwrap();
    let _guard = ConfigDirEnv::set(tmp.path());

    run_policy(kod_cli::commands::PolicyAction::Forget { n: None })
        .await
        .expect("policy forget with no daemon must return Ok");
}
