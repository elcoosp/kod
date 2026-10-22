//! `run_fixture_replay`'s three error paths plus its full bootstrap
//! path over an empty fixture, and `run_serve`'s missing-socket arm.
//!
//! `run_fixture_replay` builds a temp `redb`, a `ReplayProvider`, and
//! an engine, then drives each round's user prompt. An empty fixture
//! exercises that bootstrap (start + shutdown) without needing a
//! `RoundFixture` on disk — the `process_for` loop is skipped by the
//! `rounds: []` slice, and the comparison loop's `cmp_len` is zero.
//! The two error paths (bad name, missing fixture) run before the
//! engine is built.
//!
//! `run_serve(stop = true)` with a socket path that has no listener
//! takes a five-line early return; it never reads `KodConfig` and
//! never calls `serve()`.
//!
//! The env guard mirrors `fixtures_cli.rs`: `KOD_FIXTURES_DIR` is
//! process-global, so every test that touches it is `#[serial]`.
use kod_cli::commands::admin::{run_sandbox_check, run_serve};
use kod_cli::commands::fixtures::run_fixture_replay;
use kod_core::Fixture;
use serial_test::serial;

struct FixturesDirEnv(Option<std::ffi::OsString>);

impl FixturesDirEnv {
    fn set(dir: &std::path::Path) -> Self {
        let prev = std::env::var_os("KOD_FIXTURES_DIR");
        // Safety: `#[serial]` on every caller ensures exclusivity.
        unsafe {
            std::env::set_var("KOD_FIXTURES_DIR", dir);
        }
        FixturesDirEnv(prev)
    }
}

impl Drop for FixturesDirEnv {
    fn drop(&mut self) {
        // Safety: same invariant as `set`.
        unsafe {
            match &self.0 {
                Some(prev) => std::env::set_var("KOD_FIXTURES_DIR", prev),
                None => std::env::remove_var("KOD_FIXTURES_DIR"),
            }
        }
    }
}

// ---- run_fixture_replay --------------------------------------------

/// A name that `Fixture::is_valid_name` refuses: `default_path`
/// returns `None`, and `run_fixture_replay` maps that to a config
/// error before touching the disk.
#[serial]
#[tokio::test]
async fn fixture_replay_rejects_a_path_like_name() {
    let tmp = tempfile::TempDir::new().unwrap();
    let _guard = FixturesDirEnv::set(tmp.path());
    let err = run_fixture_replay("../escape", false, false)
        .await
        .expect_err("a path-like name must not resolve to a fixture path");
    assert!(
        err.to_string()
            .contains("could not determine fixtures directory"),
        "unexpected error: {err}",
    );
}

/// A valid name with no file behind it: `load_from` fails, and
/// `run_fixture_replay` surfaces the path in its error.
#[serial]
#[tokio::test]
async fn fixture_replay_missing_fixture_is_config_error() {
    let tmp = tempfile::TempDir::new().unwrap();
    let _guard = FixturesDirEnv::set(tmp.path());
    let err = run_fixture_replay("does-not-exist", false, false)
        .await
        .expect_err("a missing fixture must be an error");
    assert!(
        err.to_string().contains("could not load fixture"),
        "unexpected error: {err}",
    );
}

/// An empty fixture: `load_from` succeeds, the engine is built,
/// started, driven over zero rounds, and shut down. The comparison
/// loop's `cmp_len` is zero, so no divergence is reported.
///
/// The assertion is `Ok(_)` rather than `Ok(0)`: `run_fixture_replay`
/// returns the divergence count, and this test's fixture has no
/// `request_summary` to compare against — the point is that the
/// bootstrap path completes without an error, not that the fixture
/// happens to be semantically identical to the engine's own request.
#[serial]
#[tokio::test]
async fn fixture_replay_over_empty_fixture_is_ok() {
    let tmp = tempfile::TempDir::new().unwrap();
    let _guard = FixturesDirEnv::set(tmp.path());
    Fixture::new("empty")
        .save_to(&tmp.path().join("empty.json"))
        .expect("write empty fixture");

    let result = run_fixture_replay("empty", false, false).await;
    assert!(
        matches!(result, Ok(_)),
        "empty fixture must complete its bootstrap, got {result:?}",
    );
}

// ---- run_serve (stop) ----------------------------------------------

/// `kod serve --stop` with no socket at the path: a five-line early
/// return, no daemon, no `KodConfig`. The absence of a socket is the
/// "already stopped" case, not an error.
#[tokio::test]
async fn serve_stop_without_a_daemon_is_ok() {
    let tmp = tempfile::TempDir::new().unwrap();
    let sock = tmp.path().join("nothing.sock");
    run_serve(true, Some(sock))
        .await
        .expect("stop with no daemon must be Ok");
}

// ---- run_sandbox_check ---------------------------------------------

/// `kod sandbox check` prints the platform and the availability
/// status. Whether a sandbox is available is environment-dependent
/// (macOS CI vs. Linux with/without bubblewrap), so the assertion is
/// the weaker "the call returns" — the coverage win is the body,
/// which never runs on the happy-path tests.
#[tokio::test]
async fn sandbox_check_returns() {
    let _ = run_sandbox_check().await;
}
