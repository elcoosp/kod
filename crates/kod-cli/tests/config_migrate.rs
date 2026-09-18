//! `kod config migrate` end-to-end (design §12 migration de
//! configuration).
//!
//! The command brings a v1 config.toml to `config_version = 2`, writes
//! a timestamped backup, and leaves the original bytes intact if
//! anything fails. This test drives the round trip on a throwaway
//! config directory so the user's real config is not touched.
//!
//! # What this proves
//!
//! - The subcommand exists and is dispatched.
//! - A config without `config_version` (the v1 shape) is rewritten with
//!   `config_version = 2`.
//! - A `config.toml.bak-*` file is produced with the original bytes.
//! - A second invocation reports "already current" and does not write a
//!   second backup.

use std::process::Command;

fn kod_bin() -> &'static str {
    env!("CARGO_BIN_EXE_kod")
}

/// Run `kod <args>` with `KodConfig::config_dir()` pointed at
/// `config_dir` via `KOD_CONFIG_DIR`. The variable names the config
/// directory itself (the one holding `config.toml`), so it works
/// identically on Linux, macOS, and Windows — unlike `XDG_CONFIG_HOME`
/// (ignored on macOS by `dirs::config_dir`) or `HOME` (which expands
/// to `$HOME/Library/Application Support/kod` on macOS, not `$HOME/kod`).
/// Returns (stdout, stderr, status_code).
fn run_kod_in(config_dir: &std::path::Path, args: &[&str]) -> (String, String, i32) {
    let out = Command::new(kod_bin())
        .args(args)
        .env("KOD_CONFIG_DIR", config_dir)
        .output()
        .expect("spawn kod");
    (
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
        out.status.code().unwrap_or(-1),
    )
}

/// A minimal v1 config: `[llm]` block, no `config_version`. The load
/// path synthesizes a single `default` endpoint from these fields.
const V1_CONFIG: &str = r#"[llm]
provider = "OpenAICompatible"
model = "test-model"
base_url = "http://localhost:11434/v1"
context_window = 8192
max_tokens = 2048
temperature = 0.7
timeout_secs = 300

[memory]
short_term_capacity = 100

[skills]
enable_hot_reload = true
"#;

#[test]
fn migrate_brings_a_v1_config_to_v2_with_a_backup() {
    let tmp = tempfile::TempDir::new().unwrap();
    let kod_dir = tmp.path().join("kod");
    std::fs::create_dir_all(&kod_dir).unwrap();
    let cfg_path = kod_dir.join("config.toml");
    std::fs::write(&cfg_path, V1_CONFIG).unwrap();

    let (stdout, stderr, code) = run_kod_in(&kod_dir, &["config", "migrate"]);
    assert_eq!(
        code, 0,
        "config migrate should exit 0\nstdout: {stdout}\nstderr: {stderr}",
    );
    // The migrated file must carry `config_version = 2`.
    let after = std::fs::read_to_string(&cfg_path).expect("read migrated config");
    assert!(
        after.contains("config_version = 2"),
        "migrated config must be stamped v2:\n{after}",
    );
    // A backup with the original bytes must exist.
    let backups: Vec<std::path::PathBuf> = std::fs::read_dir(&kod_dir)
        .unwrap()
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| {
            p.file_name()
                .and_then(|n| n.to_str())
                .map(|n| n.starts_with("config.toml.bak-"))
                .unwrap_or(false)
        })
        .collect();
    assert_eq!(
        backups.len(),
        1,
        "exactly one backup must be written; found {:?}",
        backups,
    );
    let backup_bytes = std::fs::read_to_string(&backups[0]).unwrap();
    assert_eq!(
        backup_bytes, V1_CONFIG,
        "backup must contain the original bytes verbatim",
    );
}

#[test]
fn migrate_is_idempotent_on_a_v2_config() {
    let tmp = tempfile::TempDir::new().unwrap();
    let kod_dir = tmp.path().join("kod");
    std::fs::create_dir_all(&kod_dir).unwrap();
    let cfg_path = kod_dir.join("config.toml");
    std::fs::write(
        &cfg_path,
        "config_version = 2\n\n[llm]\nprovider = \"OpenAICompatible\"\nmodel = \"m\"\nbase_url = \"http://localhost:11434/v1\"\ncontext_window = 8192\nmax_tokens = 2048\ntemperature = 0.7\ntimeout_secs = 300\n",
    )
    .unwrap();

    let (stdout, _stderr, code) = run_kod_in(&kod_dir, &["config", "migrate"]);
    assert_eq!(code, 0);
    // The message tells the user the file is already current; a
    // regression that rewrote it (creating a second backup) would
    // show up here.
    assert!(
        stdout.contains("already") || stdout.contains("nothing to migrate"),
        "expected an 'already current' message, got: {stdout}",
    );
    // No backup should exist.
    let backups = std::fs::read_dir(&kod_dir)
        .unwrap()
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| {
            p.file_name()
                .and_then(|n| n.to_str())
                .map(|n| n.starts_with("config.toml.bak-"))
                .unwrap_or(false)
        })
        .count();
    assert_eq!(backups, 0, "an already-v2 file must not be re-migrated");
}
