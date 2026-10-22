//! `kod memory add/forget/delete/search` — the four memory
//! subcommands that had no test after the rest of the arc.
//!
//! Every other `memory` subcommand (`list`, `export`, `import`,
//! `clear`) is covered by `memory_admin_paths` and
//! `memory_manager_paths`. The four here are tested by spawning the
//! built binary — same pattern as `dispatch_arms.rs` — because their
//! testable surface is the exit code and the printed message, not an
//! in-process return value.
//!
//! The DB is a redb under `KOD_CONFIG_DIR`'s
//! `[memory] long_term_db_path`. Each spawn opens and closes it; a
//! two-spawn test (add then search) proves the value survives.
use std::path::Path;

fn kod_bin() -> &'static str {
    env!("CARGO_BIN_EXE_kod")
}

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

/// Write a `config.toml` that points the long-term memory DB into
/// `tmp`, so the process never touches `~/.kod/data/kod.redb`.
fn write_config(tmp: &Path) {
    std::fs::write(
        tmp.join("config.toml"),
        format!(
            "[memory]\nlong_term_db_path = \"{}\"\n",
            tmp.join("mem.redb").to_str().expect("utf8 db path"),
        ),
    )
    .expect("write config.toml");
}

fn env_for(tmp: &Path) -> [(&'static str, &Path); 3] {
    [
        ("HOME", tmp),
        ("KOD_CONFIG_DIR", tmp),
        ("KOD_SESSIONS_DIR", tmp),
    ]
}

// ---- add -----------------------------------------------------------

/// `kod memory add <content> --tags <tags>`: writes the entry and
/// prints its short id. The second spawn in the same tempdir (a
/// `search` for the content) proves the redb round-tripped.
#[test]
fn memory_add_then_search_round_trips() {
    let tmp = tempfile::TempDir::new().unwrap();
    write_config(tmp.path());
    let env = env_for(tmp.path());

    let add = run_kod(
        &["memory", "add", "remember this", "--tags", "demo,rust"],
        &env,
    );
    assert!(
        add.status.success(),
        "memory add must exit 0: {}",
        both_io(&add),
    );
    let add_stdout = String::from_utf8_lossy(&add.stdout);
    assert!(
        add_stdout.contains("Added memory entry"),
        "add must print the id line, got: {add_stdout}",
    );

    let search = run_kod(&["memory", "search", "remember"], &env);
    assert!(
        search.status.success(),
        "memory search must exit 0: {}",
        both_io(&search),
    );
    let search_stdout = String::from_utf8_lossy(&search.stdout);
    assert!(
        search_stdout.contains("remember this"),
        "search after add must find the content, got: {search_stdout}",
    );
}

// ---- forget --------------------------------------------------------

/// `kod memory forget <key>` on a key that matches nothing: the
/// command prints "No entries match" and exits 0. The
/// missing-key-is-not-an-error contract is the whole point of this
/// subcommand's `forget` arm.
#[test]
fn memory_forget_unknown_key_exits_zero() {
    let tmp = tempfile::TempDir::new().unwrap();
    write_config(tmp.path());
    let env = env_for(tmp.path());

    let out = run_kod(&["memory", "forget", "no-such-tag-xyz"], &env);
    assert!(
        out.status.success(),
        "memory forget on a missing key must exit 0: {}",
        both_io(&out),
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("No entries match"),
        "forget must report the empty match, got: {stdout}",
    );
}

// ---- delete --------------------------------------------------------

/// `kod memory delete <id>` for an id prefix that names no entry:
/// `InvalidParameters`, exit 1, message names the prefix. Distinct
/// from `forget`, which tolerates a miss.
#[test]
fn memory_delete_unknown_id_exits_nonzero() {
    let tmp = tempfile::TempDir::new().unwrap();
    write_config(tmp.path());
    let env = env_for(tmp.path());

    let out = run_kod(&["memory", "delete", "nope"], &env);
    assert_eq!(
        out.status.code(),
        Some(1),
        "memory delete on an unknown id must exit 1: {}",
        both_io(&out),
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("no entry with id prefix"),
        "delete must name the missing prefix, got: {stderr}",
    );
}

// ---- search --------------------------------------------------------

/// `kod memory search <query>` on an empty store: prints
/// "No entries match" and exits 0. The empty-result case is a
/// success, matching the `forget` contract and the rest of the CLI's
/// no-match-is-not-an-error shape.
#[test]
fn memory_search_on_empty_store_exits_zero() {
    let tmp = tempfile::TempDir::new().unwrap();
    write_config(tmp.path());
    let env = env_for(tmp.path());

    let out = run_kod(&["memory", "search", "anything"], &env);
    assert!(
        out.status.success(),
        "memory search on an empty store must exit 0: {}",
        both_io(&out),
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("No entries match"),
        "search on empty store must report the empty match, got: {stdout}",
    );
}
