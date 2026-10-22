//! The missing-path branches of the skills subcommands.
//!
//! Each of these branches used to call `std::process::exit(1)`, which
//! killed the test runner. They now return `Err`, so a test can assert
//! on the failure without dying.
//!
//! `KOD_CONFIG_DIR` points the config load at a temp dir, and
//! `SkillsConfig::skills_dir` in the temp config points the skill
//! scan at another temp dir. Same setup as `skills_config_cli`.
use kod_cli::commands::skills::{
    run_skills_copy, run_skills_edit, run_skills_export, run_skills_remove, run_skills_rename,
    run_skills_search, run_skills_show, run_skills_source,
};
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

fn setup() -> (tempfile::TempDir, tempfile::TempDir) {
    let cfg = tempfile::TempDir::new().unwrap();
    let skills = tempfile::TempDir::new().unwrap();
    std::fs::write(
        cfg.path().join("config.toml"),
        format!(
            "[skills]\nskills_dir = {:?}\n",
            skills.path().to_str().expect("utf-8 temp path"),
        ),
    )
    .unwrap();
    (cfg, skills)
}

#[serial]
#[tokio::test]
async fn skills_show_missing_is_err() {
    let (cfg, _skills) = setup();
    let _guard = ConfigDirEnv::set(cfg.path());
    let err = run_skills_show("does-not-exist")
        .await
        .expect_err("a missing skill must be an error, not an exit");
    assert!(err.to_string().contains("No skill named"), "{err}");
}

#[serial]
#[tokio::test]
async fn skills_source_missing_is_err() {
    let (cfg, _skills) = setup();
    let _guard = ConfigDirEnv::set(cfg.path());
    let err = run_skills_source("does-not-exist")
        .await
        .expect_err("a missing skill must be an error");
    assert!(err.to_string().contains("No skill named"), "{err}");
}

#[serial]
#[tokio::test]
async fn skills_remove_missing_is_err() {
    let (cfg, _skills) = setup();
    let _guard = ConfigDirEnv::set(cfg.path());
    // `yes = true` skips the confirmation prompt; the not-found check
    // runs first regardless.
    let err = run_skills_remove("does-not-exist", true)
        .await
        .expect_err("a missing skill must be an error");
    assert!(err.to_string().contains("No skill named"), "{err}");
}

#[serial]
#[tokio::test]
async fn skills_edit_missing_is_err() {
    let (cfg, _skills) = setup();
    let _guard = ConfigDirEnv::set(cfg.path());
    // The not-found check fires before the editor is spawned, so
    // this never opens $EDITOR.
    let err = run_skills_edit("does-not-exist")
        .await
        .expect_err("a missing skill must be an error");
    assert!(err.to_string().contains("No skill named"), "{err}");
}

#[serial]
#[tokio::test]
async fn skills_export_missing_is_err() {
    let (cfg, _skills) = setup();
    let _guard = ConfigDirEnv::set(cfg.path());
    let dest = cfg.path().join("out.md");
    let err = run_skills_export("does-not-exist", dest, false)
        .await
        .expect_err("exporting a missing skill must be an error");
    assert!(err.to_string().contains("No skill named"), "{err}");
}

#[serial]
#[tokio::test]
async fn skills_copy_missing_is_err() {
    let (cfg, _skills) = setup();
    let _guard = ConfigDirEnv::set(cfg.path());
    let err = run_skills_copy("does-not-exist", "new-name")
        .await
        .expect_err("copying a missing skill must be an error");
    assert!(err.to_string().contains("No skill named"), "{err}");
}

#[serial]
#[tokio::test]
async fn skills_rename_missing_is_err() {
    let (cfg, _skills) = setup();
    let _guard = ConfigDirEnv::set(cfg.path());
    let err = run_skills_rename("does-not-exist", "new-name")
        .await
        .expect_err("renaming a missing skill must be an error");
    assert!(err.to_string().contains("No skill named"), "{err}");
}

#[serial]
#[tokio::test]
async fn skills_search_empty_query_is_err() {
    let (cfg, _skills) = setup();
    let _guard = ConfigDirEnv::set(cfg.path());
    let err = run_skills_search("")
        .await
        .expect_err("an empty query must be a usage error");
    assert!(
        err.to_string().contains("usage"),
        "expected a usage error, got: {err}",
    );
}
