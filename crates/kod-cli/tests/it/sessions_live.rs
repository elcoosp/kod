//! `kod sessions live` reads the presence markers `kod tui` writes.
//!
//! `presence` was written for exactly this question -- "which sessions
//! are alive right now" -- and had no caller. The marker root is
//! `KOD_TUI_STATE_DIR` (or `~/.kod`), one level above the
//! `~/.kod/sessions` directory `sessions count` walks, so this test
//! pins that the command reads the right one.

// The entire file is gated on unix. Every assertion below depends on
// `presence::process_alive` distinguishing a dead pid from a live one,
// which is a real probe only on unix. The `#[cfg(not(unix))]` stub
// returns `pid != 0` (documented policy: never offer to recover a
// session that may be running, since we cannot tell without libc), so
// on Windows every marker would appear live and the test would fail.
#![cfg(unix)]
use kod_cli::commands::SessionsAction;
use kod_cli::commands::memory::run_sessions;
use serial_test::serial;

/// Set `KOD_TUI_STATE_DIR` for the test, restoring the previous value
/// on drop.
struct StateDirEnv(Option<std::ffi::OsString>);

impl StateDirEnv {
    fn set(dir: &std::path::Path) -> Self {
        let prev = std::env::var_os("KOD_TUI_STATE_DIR");
        // Safety: `#[serial]` on every caller ensures exclusivity.
        unsafe {
            std::env::set_var("KOD_TUI_STATE_DIR", dir);
        }
        StateDirEnv(prev)
    }
}

impl Drop for StateDirEnv {
    fn drop(&mut self) {
        // Safety: same invariant as `set`.
        unsafe {
            match &self.0 {
                Some(prev) => std::env::set_var("KOD_TUI_STATE_DIR", prev),
                None => std::env::remove_var("KOD_TUI_STATE_DIR"),
            }
        }
    }
}

/// A live marker (this process) is reported; a marker naming a dead pid
/// is not.
#[tokio::test]
#[serial]
async fn sessions_live_reports_only_running_sessions() {
    let dir = tempfile::TempDir::new().unwrap();
    let _env = StateDirEnv::set(dir.path());

    kod_core_state::presence::mark(
        dir.path(),
        kod_core_state::presence::MarkerKind::Interactive,
        "alive-session",
    )
    .expect("marker write");

    // A pid that cannot exist.
    let dead = kod_core_state::presence::marker_dir(
        dir.path(),
        kod_core_state::presence::MarkerKind::Interactive,
    )
    .join("dead-session");
    std::fs::create_dir_all(dead.parent().unwrap()).unwrap();
    std::fs::write(&dead, "999999 -").unwrap();

    let live = kod_core_state::presence::list(
        dir.path(),
        kod_core_state::presence::MarkerKind::Interactive,
    );
    assert_eq!(live.len(), 1, "only the running session is live: {live:?}");
    assert_eq!(live[0].name, "alive-session");

    run_sessions(SessionsAction::Live).await.expect("live runs");
}
