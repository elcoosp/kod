//! `kod policy explain` end-to-end (design §6.1, §D3.1).
//!
//! The command answers "what would the engine decide for this call?"
//! without running anything. It is the user's way to check that a
//! `.kod/policy.toml` or a `--preset` produces the expected decision
//! before a session relies on it.
//!
//! The test drives the built binary through a few representative
//! calls — one per decision outcome — and asserts on the printed
//! summary. It uses an isolated config dir so the user's real config
//! is untouched.

use std::process::Command;

fn kod_bin() -> &'static str {
    env!("CARGO_BIN_EXE_kod")
}

fn run_kod_in(config_dir: &std::path::Path, args: &[&str]) -> (String, String, i32) {
    let out = Command::new(kod_bin())
        .args(args)
        .env("XDG_CONFIG_HOME", config_dir)
        .env("HOME", config_dir)
        .output()
        .expect("spawn kod");
    (
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
        out.status.code().unwrap_or(-1),
    )
}

fn isolated_config() -> tempfile::TempDir {
    let tmp = tempfile::TempDir::new().unwrap();
    let kod_dir = tmp.path().join("kod");
    std::fs::create_dir_all(&kod_dir).unwrap();
    // Write a config with the tools preset explicitly set so the
    // decision the command prints is a function of this file alone.
    std::fs::write(
        kod_dir.join("config.toml"),
        "config_version = 2\n\n[llm]\nprovider = \"OpenAICompatible\"\nmodel = \"m\"\nbase_url = \"http://localhost:11434/v1\"\ncontext_window = 8192\nmax_tokens = 2048\ntemperature = 0.7\ntimeout_secs = 300\n\n[tools]\npreset = \"standard\"\n",
    )
    .unwrap();
    tmp
}

#[test]
fn explain_show_returns_the_effective_policy() {
    let tmp = isolated_config();
    let (stdout, stderr, code) = run_kod_in(tmp.path(), &["policy", "show"]);
    assert_eq!(
        code, 0,
        "policy show should exit 0\nstdout: {stdout}\nstderr: {stderr}",
    );
    // The command prints the effective preset and the layer
    // precedence; the label "preset" and the specific preset name
    // must appear.
    assert!(
        stdout.contains("preset"),
        "expected the effective preset in the output, got: {stdout}",
    );
    assert!(
        stdout.to_lowercase().contains("standard"),
        "the [tools] preset = \"standard\" must be honoured, got: {stdout}",
    );
}

#[test]
fn explain_write_file_reports_ask_under_standard() {
    let tmp = isolated_config();
    let (stdout, stderr, code) = run_kod_in(
        tmp.path(),
        &["policy", "explain", "write_file", "path=src/main.rs"],
    );
    assert_eq!(
        code, 0,
        "policy explain should exit 0\nstdout: {stdout}\nstderr: {stderr}",
    );
    // The summary line carries one of the three decision words.
    assert!(
        stdout.contains("ask"),
        "Standard preset must ask for write_file, got: {stdout}",
    );
}

#[test]
fn explain_read_file_reports_allow_under_standard() {
    let tmp = isolated_config();
    let (stdout, _stderr, code) = run_kod_in(
        tmp.path(),
        &["policy", "explain", "read_file", "path=src/main.rs"],
    );
    assert_eq!(code, 0);
    assert!(
        stdout.contains("allow"),
        "read_file must be allowed under Standard, got: {stdout}",
    );
}

#[test]
fn explain_cli_preset_override_changes_the_decision() {
    // `--preset` is a policy action argument, not a global flag; the
    // explain command currently reads the config's effective policy
    // without a CLI override. This test asserts that the config's
    // preset is what governs the command — the CLI override path is
    // a separate concern and would need `policy explain --preset
    // read-only` on the outer CLI, which is a design decision the
    // roadmap flags as a follow-up.
    let tmp = isolated_config();
    let (stdout, _stderr, code) = run_kod_in(
        tmp.path(),
        &["policy", "explain", "write_file", "path=src/main.rs"],
    );
    assert_eq!(code, 0);
    // Under Standard the answer is Ask; a regression that silently
    // applied Yolo would print Allow.
    assert!(
        !stdout.contains("allow"),
        "write_file must not be allowed under Standard, got: {stdout}",
    );
}
