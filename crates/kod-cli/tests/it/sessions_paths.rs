//! `kod sessions` variants reachable through `KOD_TUI_STATE_DIR`.
//!
//! `run_sessions` calls `KodApp::session_path()`, which reads
//! `KOD_TUI_STATE_DIR` (persistence.rs:11) and joins
//! `tui_session.json`. The four variants covered here — Show, Clear,
//! Export, Import — read or write that one file.
//!
//! `Count` and `Latest` are NOT covered: they bypass `session_path`
//! and walk `dirs::home_dir().join(".kod/sessions")` directly, so they
//! need a different seam.
//!
//! The session file is written as `Vec<Message>` (a bare JSON array)
//! by `main_loop.rs`'s export path; the test's `[]` fixture matches
//! that shape. If `Show` rejects `[]`, the read and write formats
//! disagree — a real finding, not a test-shape mistake.
use kod_cli::commands::SessionsAction;
use kod_cli::commands::memory::run_sessions;
use serial_test::serial;

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

fn session_file(state_dir: &std::path::Path) -> std::path::PathBuf {
    state_dir.join("tui_session.json")
}

// ---- Show ----------------------------------------------------------

/// `kod sessions show` on a state dir with no `tui_session.json` prints
/// "No saved session" and returns Ok — the first-run case, not an error.
#[serial]
#[tokio::test]
async fn sessions_show_missing_file_is_ok() {
    let tmp = tempfile::TempDir::new().unwrap();
    let _guard = StateDirEnv::set(tmp.path());
    run_sessions(SessionsAction::Show)
        .await
        .expect("a missing session file is the first-run case");
}

/// `kod sessions show` on a `[]` session (zero messages). If this fails
/// with a deserialization error, the read format and the write format
/// disagree — a real finding. If it passes, the file really is a bare
/// array and the round is a harden.
#[serial]
#[tokio::test]
async fn sessions_show_empty_messages_is_ok() {
    let tmp = tempfile::TempDir::new().unwrap();
    std::fs::write(session_file(tmp.path()), "[]").unwrap();
    let _guard = StateDirEnv::set(tmp.path());
    run_sessions(SessionsAction::Show)
        .await
        .expect("a [] session must deserialize as an empty Vec<Message>");
}

// ---- Clear ---------------------------------------------------------

/// `kod sessions clear` on an existing `[]` file deletes it and returns
/// Ok.
#[serial]
#[tokio::test]
async fn sessions_clear_deletes_the_file() {
    let tmp = tempfile::TempDir::new().unwrap();
    let path = session_file(tmp.path());
    std::fs::write(&path, "[]").unwrap();
    let _guard = StateDirEnv::set(tmp.path());

    run_sessions(SessionsAction::Clear)
        .await
        .expect("clear on an existing session file");
    assert!(!path.exists(), "clear did not delete {path:?}");
}

/// `kod sessions clear` on a missing file returns Ok — nothing to
/// clear is not an error.
#[serial]
#[tokio::test]
async fn sessions_clear_missing_file_is_ok() {
    let tmp = tempfile::TempDir::new().unwrap();
    let _guard = StateDirEnv::set(tmp.path());
    run_sessions(SessionsAction::Clear)
        .await
        .expect("clear on a missing file is not an error");
}

// ---- Export --------------------------------------------------------

/// `kod sessions export` writes the session to a caller-supplied path.
/// Markdown is the default format; the file exists afterward.
#[serial]
#[tokio::test]
async fn sessions_export_writes_a_file() {
    let tmp = tempfile::TempDir::new().unwrap();
    std::fs::write(session_file(tmp.path()), "[]").unwrap();
    let out = tmp.path().join("export.md");
    let _guard = StateDirEnv::set(tmp.path());

    run_sessions(SessionsAction::Export {
        path: out.clone(),
        format: "markdown".to_string(),
    })
    .await
    .expect("export of an empty session must succeed");
    assert!(out.exists(), "export did not create {out:?}");
}

/// `kod sessions export --format json` writes JSON. The assertion is
/// that the file exists and parses as JSON — not on a specific shape,
/// so a format change does not break the pin.
#[serial]
#[tokio::test]
async fn sessions_export_json_is_valid_json() {
    let tmp = tempfile::TempDir::new().unwrap();
    std::fs::write(session_file(tmp.path()), "[]").unwrap();
    let out = tmp.path().join("export.json");
    let _guard = StateDirEnv::set(tmp.path());

    run_sessions(SessionsAction::Export {
        path: out.clone(),
        format: "json".to_string(),
    })
    .await
    .expect("export json of an empty session");
    let raw = std::fs::read_to_string(&out).expect("export file");
    serde_json::from_str::<serde_json::Value>(&raw)
        .expect("export --format json must emit valid JSON");
}

// ---- Import --------------------------------------------------------

/// `kod sessions import <path>` replaces the session with the file's
/// contents. Round-trip: export an empty session, import it back, and
/// Show still works.
#[serial]
#[tokio::test]
async fn sessions_import_round_trips() {
    let tmp = tempfile::TempDir::new().unwrap();
    let _guard = StateDirEnv::set(tmp.path());

    // Seed the session, export it.
    std::fs::write(session_file(tmp.path()), "[]").unwrap();
    let out = tmp.path().join("round-trip.json");
    run_sessions(SessionsAction::Export {
        path: out.clone(),
        format: "json".to_string(),
    })
    .await
    .expect("export");

    // Clear, then import the export back.
    run_sessions(SessionsAction::Clear).await.expect("clear");
    run_sessions(SessionsAction::Import { path: out })
        .await
        .expect("import of the export round-trips");

    // The session file exists and Show reads it.
    assert!(
        session_file(tmp.path()).exists(),
        "import did not write the session"
    );
    run_sessions(SessionsAction::Show)
        .await
        .expect("Show after Import");
}
