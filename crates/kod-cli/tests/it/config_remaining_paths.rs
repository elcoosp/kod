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
