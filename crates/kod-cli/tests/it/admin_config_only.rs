//! The config-only and pure subcommands of `admin.rs`.
//!
//! `run_jev_status` is config-driven (two branches: Jev disabled and
//! Jev enabled, both reachable via `KOD_CONFIG_DIR`). `run_tools` is
//! pure — it builds a `ToolRegistry` and lists it, no config, no
//! engine. `run_doctor` runs `kod_core::doctor::run_diagnostics` and
//! its success/failure branch depends on the host, so its test is
//! shape-agnostic: what it pins is that the diagnostic pipeline runs
//! end to end without panicking.
use kod_cli::commands::ToolsAction;
use kod_cli::commands::admin::{run_doctor, run_jev_status, run_tools};
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

fn write_config(dir: &std::path::Path, toml: &str) {
    std::fs::write(dir.join("config.toml"), toml).unwrap();
}

// ---- run_jev_status ------------------------------------------------

/// A config with no `[jev]` section has `enabled = false` by default,
/// so `kod jev status` prints "Jev is disabled" and returns Ok.
#[serial]
#[tokio::test]
async fn jev_status_disabled_returns_ok() {
    let tmp = tempfile::TempDir::new().unwrap();
    write_config(tmp.path(), "");
    let _guard = ConfigDirEnv::set(tmp.path());
    run_jev_status()
        .await
        .expect("a disabled Jev is a first-run case, not an error");
}

/// A config with `[jev] enabled = true` takes the listing branch. The
/// listing is a series of `println!`s; the test pins that the branch
/// runs and returns Ok. Asserting on specific values would tie the
/// test to a specific `JevConfig::default()`, which is a different
/// contract (`kod-config`'s tests cover it).
#[serial]
#[tokio::test]
async fn jev_status_enabled_returns_ok() {
    let tmp = tempfile::TempDir::new().unwrap();
    write_config(tmp.path(), "[jev]\nenabled = true\n");
    let _guard = ConfigDirEnv::set(tmp.path());
    run_jev_status()
        .await
        .expect("an enabled Jev status listing must return Ok");
}

// ---- run_tools -----------------------------------------------------

/// `kod tools list` builds a registry of every tool the engine
/// constructs without engine state and lists it. No config, no
/// engine — a pure listing.
#[tokio::test]
async fn tools_list_returns_ok() {
    run_tools(Some(ToolsAction::List))
        .await
        .expect("tools list must return Ok");
}

/// `kod tools` with no subcommand. The CLI dispatches this as the
/// default (list), and the function must handle it without an error.
#[tokio::test]
async fn tools_none_is_ok() {
    run_tools(None)
        .await
        .expect("tools with no action defaults to list");
}

/// `kod tools show read_file` finds a tool the registry registers
/// and prints its definition. `read_file` is unconditionally
/// registered by `run_tools`; if that changes, this test is the
/// signal.
#[tokio::test]
async fn tools_show_known_tool_returns_ok() {
    run_tools(Some(ToolsAction::Show {
        name: "read_file".to_string(),
    }))
    .await
    .expect("showing a registered tool must return Ok");
}

// ---- run_doctor ----------------------------------------------------

/// The diagnostic pipeline runs end-to-end. The success/failure branch
/// depends on the host's state (does `~/.kod` exist, is git on PATH,
/// etc.), so this test pins what it can: that `run_doctor` returns
/// rather than panics. Both `Ok(())` (all checks pass) and
/// `Err(Internal)` (a check failed) are acceptable — the assertion is
/// on the absence of a panic, not on the branch.
///
/// A branch-specific test would need a host configuration the test
/// controls; that is a fixture round, not this one.
#[serial]
#[tokio::test]
async fn doctor_runs_without_panicking() {
    let tmp = tempfile::TempDir::new().unwrap();
    write_config(tmp.path(), "");
    let _guard = ConfigDirEnv::set(tmp.path());
    let _ = run_doctor(false).await;
}

/// The JSON mode (`kod doctor --json`) takes the same pipeline
/// through a different serializer path. Same shape-agnostic
/// assertion.
#[serial]
#[tokio::test]
async fn doctor_json_runs_without_panicking() {
    let tmp = tempfile::TempDir::new().unwrap();
    write_config(tmp.path(), "");
    let _guard = ConfigDirEnv::set(tmp.path());
    let _ = run_doctor(true).await;
}
