//! Dispatch-arm smoke tests: spawn the real `kod` binary per
//! subcommand, assert on exit code.
//!
//! Every other test in this crate calls a `run_*` function directly,
//! which means `commands::Cli::run`'s dispatch `match` — the big one
//! at `mod.rs:430` that maps every `Command::*` variant to its
//! `run_*` function — is only exercised by the handful of arms that
//! `replay.rs`, `policy_explain.rs`, and `config_migrate.rs` spawn.
//!
//! These tests spawn the binary via `CARGO_BIN_EXE_kod` (cargo sets
//! that for integration tests of a `[[bin]]` target). Each test sets
//! its env via `Command::env` — per-child-process, so no `#[serial]`
//! attribute is needed and no `std::env::set_var` is called; the
//! parent process's env is untouched.
//!
//! Exit code 0 pins the `Command::X => Ok(())` dispatch arm; exit
//! code non-zero pins the `Command::X => Err(...)` arm and the
//! binary's error-to-exit-1 mapping.

use std::path::{Path, PathBuf};

fn kod_bin() -> &'static str {
    env!("CARGO_BIN_EXE_kod")
}

/// Run `kod <args>` with `env` set on the child, return the output.
fn run_kod(args: &[&str], env: &[(&str, &Path)]) -> std::process::Output {
    let mut cmd = std::process::Command::new(kod_bin());
    cmd.args(args);
    for (k, v) in env {
        cmd.env(k, v);
    }
    cmd.output().expect("spawn kod")
}

fn both_io(out: &std::process::Output) -> String {
    format!(
        "exit={:?}\nstdout:\n{}\nstderr:\n{}",
        out.status.code(),
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr),
    )
}

/// Write a `config.toml` pointing at `skills_dir`, so a `skills`
/// subcommand that calls `skills_dirs()` reads the tempdir and not
/// `~/.kod/skills`.
fn write_config(cfg_dir: &Path, skills_dir: &Path) {
    std::fs::create_dir_all(cfg_dir).expect("create config dir");
    std::fs::create_dir_all(skills_dir).expect("create skills dir");
    std::fs::write(
        cfg_dir.join("config.toml"),
        format!(
            "[skills]\nskills_dir = {:?}\n",
            skills_dir.to_str().expect("utf8 skills path"),
        ),
    )
    .expect("write config.toml");
}

// ---- exit 0: dispatch arm returns Ok(()) --------------------------

#[test]
fn config_path_exits_zero() {
    let tmp = tempfile::TempDir::new().unwrap();
    let out = run_kod(&["config", "path"], &[("KOD_CONFIG_DIR", tmp.path())]);
    assert!(
        out.status.success(),
        "config path must exit 0: {}",
        both_io(&out),
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("config.toml"),
        "config path must print a config.toml path, got: {stdout}",
    );
}

#[test]
fn config_show_merged_exits_zero() {
    let tmp = tempfile::TempDir::new().unwrap();
    let out = run_kod(
        &["config", "show-merged"],
        &[("KOD_CONFIG_DIR", tmp.path())],
    );
    assert!(
        out.status.success(),
        "config show-merged must exit 0: {}",
        both_io(&out),
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains('['),
        "show-merged must emit a TOML section header, got: {stdout}",
    );
}

#[test]
fn config_validate_exits_zero_on_default() {
    let tmp = tempfile::TempDir::new().unwrap();
    let out = run_kod(&["config", "validate"], &[("KOD_CONFIG_DIR", tmp.path())]);
    assert!(
        out.status.success(),
        "config validate must exit 0 on a default config: {}",
        both_io(&out),
    );
}

#[test]
fn trace_list_without_a_file_exits_zero() {
    let tmp = tempfile::TempDir::new().unwrap();
    let absent = tmp.path().join("absent.jsonl");
    let absent_str = absent.to_str().unwrap();
    let out = run_kod(&["trace", "list", "--path", absent_str], &[]);
    assert!(
        out.status.success(),
        "trace list without a file must exit 0: {}",
        both_io(&out),
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("no trace file"),
        "trace list must explain the missing file, got: {stderr}",
    );
}

#[test]
fn fixture_list_on_empty_dir_exits_zero() {
    let tmp = tempfile::TempDir::new().unwrap();
    let out = run_kod(
        &["fixture", "list"],
        &[("KOD_FIXTURES_DIR", tmp.path())],
    );
    assert!(
        out.status.success(),
        "fixture list on empty dir must exit 0: {}",
        both_io(&out),
    );
}

#[test]
fn checkpoint_list_on_empty_dir_exits_zero() {
    let tmp = tempfile::TempDir::new().unwrap();
    let out = run_kod(
        &["checkpoint", "list"],
        &[("KOD_CHECKPOINT_DIR", tmp.path())],
    );
    assert!(
        out.status.success(),
        "checkpoint list on empty dir must exit 0: {}",
        both_io(&out),
    );
}

// ---- exit non-zero: dispatch arm returns Err(...) -----------------

#[test]
fn skills_show_missing_skill_exits_nonzero() {
    let tmp = tempfile::TempDir::new().unwrap();
    let cfg = tmp.path().join("cfg");
    let skills = tmp.path().join("skills");
    write_config(&cfg, &skills);
    let out = run_kod(
        &["skills", "show", "does-not-exist"],
        &[("KOD_CONFIG_DIR", &cfg)],
    );
    assert!(
        !out.status.success(),
        "skills show on a missing skill must exit non-zero: {}",
        both_io(&out),
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("No skill named"),
        "skills show must name the missing skill in stderr, got: {stderr}",
    );
}

#[test]
fn skills_source_missing_skill_exits_nonzero() {
    let tmp = tempfile::TempDir::new().unwrap();
    let cfg = tmp.path().join("cfg");
    let skills = tmp.path().join("skills");
    write_config(&cfg, &skills);
    let out = run_kod(
        &["skills", "source", "does-not-exist"],
        &[("KOD_CONFIG_DIR", &cfg)],
    );
    assert!(
        !out.status.success(),
        "skills source on a missing skill must exit non-zero: {}",
        both_io(&out),
    );
}

#[test]
fn fixture_replay_missing_fixture_exits_nonzero() {
    let tmp = tempfile::TempDir::new().unwrap();
    let out = run_kod(
        &["fixture", "replay", "does-not-exist"],
        &[("KOD_FIXTURES_DIR", tmp.path())],
    );
    assert!(
        !out.status.success(),
        "fixture replay on a missing fixture must exit non-zero: {}",
        both_io(&out),
    );
}

#[test]
fn checkpoint_diff_unknown_id_exits_nonzero() {
    let tmp = tempfile::TempDir::new().unwrap();
    let out = run_kod(
        &["checkpoint", "diff", "--id", "definitely-not-a-checkpoint"],
        &[("KOD_CHECKPOINT_DIR", tmp.path())],
    );
    assert!(
        !out.status.success(),
        "checkpoint diff on an unknown id must exit non-zero: {}",
        both_io(&out),
    );
}

// Silence the unused import if `PathBuf` ends up unused.
#[allow(dead_code)]
fn _unused(_p: PathBuf) {}
