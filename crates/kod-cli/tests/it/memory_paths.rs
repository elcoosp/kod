//! Failure branches of `kod memory` and `kod sessions`.
//!
//! Each branch used to call `std::process::exit(1)`. It now returns
//! `Err`, so a test can assert on the failure.
//!
//! `KOD_CONFIG_DIR` points the config load at a temp dir; nothing else
//! is written to the home directory.
use kod_cli::commands::SessionsAction;
use kod_cli::commands::memory::run_sessions;
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

/// `kod sessions count` on a machine with no session directory
/// returns `Err` rather than exiting the process.
///
/// `run_sessions` builds its directory from `dirs::home_dir()`, so the
/// test cannot redirect it without changing HOME. The assertion is on
/// the shape: whatever branch fires, the call returns. The
/// no-session-logs branch is one of the six exit sites converted to
/// `Err` in this round — before the fix, a machine without
/// `~/.kod/sessions` would have terminated the test runner. A machine
/// whose directory exists and is populated returns `Ok`; the test
/// tolerates both.
#[serial]
#[tokio::test]
async fn sessions_count_does_not_exit_the_process() {
    let cfg = tempfile::TempDir::new().unwrap();
    let _guard = ConfigDirEnv::set(cfg.path());
    // The contract under test: this call returns, one way or another.
    let _ = run_sessions(SessionsAction::Count).await;
}
