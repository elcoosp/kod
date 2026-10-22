//! `kod jev stats` populated, `kod skills edit`, `kod skills search`,
//! `kod config edit`.
//!
//! No engine, no network, no prompt. `run_jev_stats` takes its log
//! path directly; `run_skills_search` reads the skills dir through
//! `KOD_CONFIG_DIR`; both edit commands read `$EDITOR`, which the
//! tests set to `:` (the POSIX no-op builtin) so the spawn is
//! deterministic and exits 0.
use kod_cli::commands::config::run_config_edit;
use kod_cli::commands::admin::run_jev_stats;
use kod_cli::commands::skills::{run_skills_edit, run_skills_search};
use kod_core_state::session_log::SessionEntry;
use serial_test::serial;

struct EnvVar {
    key: &'static str,
    prev: Option<std::ffi::OsString>,
}

impl EnvVar {
    fn set(key: &'static str, value: &std::path::Path) -> Self {
        let prev = std::env::var_os(key);
        // Safety: `#[serial]` on every caller ensures exclusivity.
        unsafe {
            std::env::set_var(key, value);
        }
        EnvVar { key, prev }
    }
    fn set_str(key: &'static str, value: &str) -> Self {
        let prev = std::env::var_os(key);
        unsafe {
            std::env::set_var(key, value);
        }
        EnvVar { key, prev }
    }
}

impl Drop for EnvVar {
    fn drop(&mut self) {
        // Safety: same invariant as `set`.
        unsafe {
            match &self.prev {
                Some(prev) => std::env::set_var(self.key, prev),
                None => std::env::remove_var(self.key),
            }
        }
    }
}

fn jev_decision_entry(purpose: &str, latency_ms: u64, cached: bool) -> SessionEntry {
    SessionEntry::JevDecision {
        timestamp_ms: 1_700_000_000_000,
        holder: "session".to_string(),
        purpose: purpose.to_string(),
        state_preview: "preview".to_string(),
        questions_summary: "q".to_string(),
        answers: serde_json::json!({"x": "y"}),
        confidence: 0.9,
        latency_ms,
        cached,
        source: "mock".to_string(),
    }
}

fn write_log(path: &std::path::Path, entries: &[SessionEntry]) {
    let mut s = String::new();
    for e in entries {
        s.push_str(&serde_json::to_string(e).expect("serialize entry"));
        s.push('\n');
    }
    std::fs::write(path, s).expect("write session log");
}

fn write_skill(dir: &std::path::Path, name: &str) {
    std::fs::create_dir_all(dir).unwrap();
    let content = format!(
        "---\nname: {name}\ndescription: A test skill for {name}\nversion: 1.0.0\ncategory: testing\ntags:\n  - demo\n---\n\nBody for {name}.\n"
    );
    std::fs::write(dir.join(format!("{name}.md")), content).unwrap();
}

// ---- run_jev_stats (populated branch) ------------------------------

/// `kod jev stats <log>` with one `JevDecision` entry: the counting,
/// caching, and per-purpose aggregation run. No env, no engine — the
/// log path is the whole input.
#[tokio::test]
async fn jev_stats_with_a_decision_is_ok() {
    let tmp = tempfile::TempDir::new().unwrap();
    let path = tmp.path().join("jev.jsonl");
    write_log(
        &path,
        &[
            jev_decision_entry("tool_filter", 12, false),
            jev_decision_entry("task_classify", 30, true),
        ],
    );
    run_jev_stats(Some(path))
        .await
        .expect("two JevDecision entries must aggregate cleanly");
}

/// A log with a single `JevDecision` also works — the `total == 1`
/// branch (avg latency divides by one, cached percentage is 0 or 100).
#[tokio::test]
async fn jev_stats_with_one_decision_is_ok() {
    let tmp = tempfile::TempDir::new().unwrap();
    let path = tmp.path().join("one.jsonl");
    write_log(&path, &[jev_decision_entry("auto_approve", 5, true)]);
    run_jev_stats(Some(path))
        .await
        .expect("one entry, cached=true, must aggregate");
}

// ---- run_skills_edit -----------------------------------------------

/// `kod skills edit <name>` spawns `$EDITOR`. The test sets
/// `EDITOR=:` — the POSIX no-op builtin — so the spawn is
/// deterministic and exits 0. The skill file must exist first, so the
/// test uses the `KOD_CONFIG_DIR` + `skills_dir` seam to place one.
#[serial]
#[tokio::test]
async fn skills_edit_runs_the_editor() {
    let cfg = tempfile::TempDir::new().unwrap();
    let skills = tempfile::TempDir::new().unwrap();
    std::fs::write(
        cfg.path().join("config.toml"),
        format!(
            "[skills]\nskills_dir = {:?}\n",
            skills.path().to_str().unwrap(),
        ),
    )
    .unwrap();
    write_skill(skills.path(), "edit-me");
    let _cfg = EnvVar::set("KOD_CONFIG_DIR", cfg.path());
    let _ed = EnvVar::set_str("EDITOR", ":");

    run_skills_edit("edit-me")
        .await
        .expect("EDITOR=: must make the editor spawn exit 0");
    assert!(
        skills.path().join("edit-me.md").exists(),
        "edit must not delete the skill",
    );
}

// ---- run_skills_search ---------------------------------------------

/// `kod skills search <query>` scans the configured skills dir and
/// scores matches. A skill whose name contains the query scores 100;
/// the run prints it and returns Ok.
#[serial]
#[tokio::test]
async fn skills_search_finds_a_configured_skill() {
    let cfg = tempfile::TempDir::new().unwrap();
    let skills = tempfile::TempDir::new().unwrap();
    std::fs::write(
        cfg.path().join("config.toml"),
        format!(
            "[skills]\nskills_dir = {:?}\n",
            skills.path().to_str().unwrap(),
        ),
    )
    .unwrap();
    write_skill(skills.path(), "demo-search");
    let _cfg = EnvVar::set("KOD_CONFIG_DIR", cfg.path());

    run_skills_search("demo")
        .await
        .expect("a matching query over a populated skills dir must return Ok");
}

/// A query matching nothing is Ok with an empty result — the
/// "no skills match" case, not an error.
#[serial]
#[tokio::test]
async fn skills_search_with_no_matches_is_ok() {
    let cfg = tempfile::TempDir::new().unwrap();
    let skills = tempfile::TempDir::new().unwrap();
    std::fs::write(
        cfg.path().join("config.toml"),
        format!(
            "[skills]\nskills_dir = {:?}\n",
            skills.path().to_str().unwrap(),
        ),
    )
    .unwrap();
    write_skill(skills.path(), "demo-search");
    let _cfg = EnvVar::set("KOD_CONFIG_DIR", cfg.path());

    run_skills_search("definitely-not-a-match")
        .await
        .expect("no matches is not an error");
}

// ---- run_config_edit -----------------------------------------------

/// `kod config edit` spawns `$EDITOR` on the config file, creating it
/// first when absent. `EDITOR=:` makes the spawn a no-op that exits 0.
#[serial]
#[tokio::test]
async fn config_edit_runs_the_editor() {
    let tmp = tempfile::TempDir::new().unwrap();
    let _cfg = EnvVar::set("KOD_CONFIG_DIR", tmp.path());
    let _ed = EnvVar::set_str("EDITOR", ":");

    run_config_edit()
        .await
        .expect("EDITOR=: must make the editor spawn exit 0");
    assert!(
        tmp.path().join("config.toml").exists(),
        "config edit must create the config file it opens",
    );
}
