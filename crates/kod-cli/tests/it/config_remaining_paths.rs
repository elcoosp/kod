//! The two remaining `process::exit` branches in `config.rs`.
//!
//! Both are reached via `KodConfig::config_dir`, which honours
//! `KOD_CONFIG_DIR`, so a `tempfile::TempDir` is the whole fixture.
use kod_cli::commands::config::{run_config_show_raw, run_config_validate};
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

/// `run_config_show_raw` on a machine with no `config.toml` returns
/// `FileNotFound` rather than exiting.
#[serial]
#[tokio::test]
async fn config_show_raw_missing_file_is_err() {
    let tmp = tempfile::TempDir::new().unwrap();
    let _guard = ConfigDirEnv::set(tmp.path());
    let err = run_config_show_raw()
        .await
        .expect_err("a missing config.toml must be an error, not an exit");
    let msg = err.to_string();
    assert!(
        msg.contains("No config file") || msg.contains("not found") || msg.contains("config.toml"),
        "unexpected error: {msg}",
    );
}

/// `run_config_validate` on an invalid `config.toml` returns `Config`
/// with the path and the parse error in the message, rather than
/// exiting.
#[serial]
#[tokio::test]
async fn config_validate_invalid_toml_is_err() {
    let tmp = tempfile::TempDir::new().unwrap();
    std::fs::write(
        tmp.path().join("config.toml"),
        "this is not valid toml: [[[",
    )
    .unwrap();
    let _guard = ConfigDirEnv::set(tmp.path());
    let err = run_config_validate()
        .await
        .expect_err("an invalid config.toml must be an error, not an exit");
    let msg = err.to_string();
    assert!(
        msg.contains("INVALID") || msg.contains("config"),
        "unexpected error: {msg}",
    );
}

// ---- run_config_show_merged ----------------------------------------

/// `kod config show-merged` loads the default config, serializes it to
/// TOML, and prints. No subcommand dispatch, no file write — the
/// `load_default` call will create `config.toml` in `KOD_CONFIG_DIR`
/// as a side effect, which is why the guard redirects the directory.
struct MergedConfigDir(Option<std::ffi::OsString>);

impl MergedConfigDir {
    fn set(dir: &std::path::Path) -> Self {
        let prev = std::env::var_os("KOD_CONFIG_DIR");
        // Safety: `#[serial]` on the caller ensures exclusivity.
        unsafe {
            std::env::set_var("KOD_CONFIG_DIR", dir);
        }
        MergedConfigDir(prev)
    }
}

impl Drop for MergedConfigDir {
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

#[serial]
#[tokio::test]
async fn config_show_merged_ok() {
    use kod_cli::commands::config::run_config_show_merged;
    let tmp = tempfile::TempDir::new().unwrap();
    let _g = MergedConfigDir::set(tmp.path());
    run_config_show_merged()
        .await
        .expect("show-merged over a fresh config dir must be Ok");
}
