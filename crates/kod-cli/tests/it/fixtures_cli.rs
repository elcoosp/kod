//! `kod fixture` subcommands, with `KOD_FIXTURES_DIR` pointing at a temp dir.
//!
//! `Fixture::fixtures_dir` reads `KOD_FIXTURES_DIR` first, falling
//! back to `~/.kod/fixtures`. The four `run_fixture_*` subcommands all
//! go through `Fixture::default_path`, so a test can point all of them
//! at a `tempfile::TempDir` without writing into the developer's home.
//!
//! Every test that touches the environment is `#[serial]`: `set_var`
//! is process-global, and concurrent writes from different tests in
//! the same binary would race.

use kod_cli::commands::fixtures::{run_fixture_list, run_fixture_save, run_fixture_save_v2};
use kod_core::Fixture;
use serial_test::serial;

/// RAII guard that sets `KOD_FIXTURES_DIR` and restores the prior
/// value on drop.
struct FixturesDirEnv(Option<std::ffi::OsString>);

impl FixturesDirEnv {
    fn set(dir: &std::path::Path) -> Self {
        let prev = std::env::var_os("KOD_FIXTURES_DIR");
        // Safety: the `#[serial]` attribute on every test that calls
        // this guard ensures no other test is reading or writing the
        // environment at the same time.
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

/// `fixtures_dir` returns the env var when it is set. A regression
/// that ignored the override would make every test in this file
/// write into the developer's home directory.
#[serial]
#[test]
fn fixtures_dir_honours_the_env_var() {
    let tmp = tempfile::TempDir::new().unwrap();
    let _guard = FixturesDirEnv::set(tmp.path());
    assert_eq!(
        Fixture::fixtures_dir().as_deref(),
        Some(tmp.path()),
        "fixtures_dir must return KOD_FIXTURES_DIR when set",
    );
}

/// `run_fixture_list` on an empty directory succeeds. "No fixtures
/// yet" is the first-run case, not an error.
#[serial]
#[tokio::test]
async fn list_on_empty_dir_succeeds() {
    let tmp = tempfile::TempDir::new().unwrap();
    let _guard = FixturesDirEnv::set(tmp.path());
    run_fixture_list()
        .await
        .expect("an empty fixtures dir must not be an error");
}

/// `run_fixture_list` on a directory with one fixture succeeds and
/// the fixture is untouched afterwards — listing is a read.
#[serial]
#[tokio::test]
async fn list_reports_one_fixture() {
    let tmp = tempfile::TempDir::new().unwrap();
    let _guard = FixturesDirEnv::set(tmp.path());
    let path = tmp.path().join("demo.json");
    Fixture::new("demo").save_to(&path).expect("save fixture");
    run_fixture_list().await.expect("list should succeed");
    assert!(
        path.exists(),
        "list must not consume or modify the fixture it lists",
    );
}

/// `run_fixture_save` on a nonexistent turns path errors.
#[serial]
#[tokio::test]
async fn save_with_missing_turns_path_errors() {
    let tmp = tempfile::TempDir::new().unwrap();
    let _guard = FixturesDirEnv::set(tmp.path());
    let missing = tmp.path().join("does-not-exist.jsonl");
    let err = run_fixture_save("demo", &missing)
        .await
        .expect_err("a missing turns file must be an error");
    let msg = err.to_string();
    assert!(
        msg.contains("could not read turn traces") || msg.contains("No such file"),
        "unexpected error: {msg}",
    );
}

/// `run_fixture_save` on an empty turns file errors with the specific
/// "no turn traces recorded" message.
#[serial]
#[tokio::test]
async fn save_with_empty_turns_path_errors() {
    let tmp = tempfile::TempDir::new().unwrap();
    let _guard = FixturesDirEnv::set(tmp.path());
    let empty = tmp.path().join("turns.jsonl");
    std::fs::write(&empty, "").unwrap();
    let err = run_fixture_save("demo", &empty)
        .await
        .expect_err("an empty turns file must be an error");
    assert!(
        err.to_string().contains("no turn traces"),
        "unexpected error: {err}",
    );
}

/// `run_fixture_save_v2` refuses to overwrite an existing fixture
/// without `--force`.
#[serial]
#[tokio::test]
async fn save_v2_refuses_to_overwrite_without_force() {
    let tmp = tempfile::TempDir::new().unwrap();
    let _guard = FixturesDirEnv::set(tmp.path());
    let turns = tmp.path().join("turns.jsonl");
    std::fs::write(&turns, "").unwrap();
    Fixture::new("demo")
        .save_to(&tmp.path().join("demo.json"))
        .expect("save existing fixture");
    let err = run_fixture_save_v2("demo", Some(turns), false)
        .await
        .expect_err("existing fixture without --force must error");
    assert!(
        err.to_string().contains("already exists"),
        "unexpected error: {err}",
    );
}
