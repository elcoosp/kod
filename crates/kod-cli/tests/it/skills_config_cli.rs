//! `kod skills` and `kod config` subcommands, via `KOD_CONFIG_DIR`.
//!
//! `KodConfig::config_dir` reads `KOD_CONFIG_DIR` before the platform
//! default, and `SkillsConfig::skills_dir` takes precedence over the
//! home-directory walk, so a test controls both the config source and
//! the skills source by writing a `config.toml` into a temp dir.
//!
//! Every test that touches the environment is `#[serial]`.
use kod_cli::commands::config::{run_config_export, run_config_path, run_config_validate};
use kod_cli::commands::skills::{run_skills_list, run_skills_show};
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

fn config_toml_with_skills_dir(skills: &std::path::Path) -> String {
    format!(
        "[skills]\nskills_dir = {:?}\n",
        skills.to_str().expect("utf-8 temp path")
    )
}

fn write_skill(dir: &std::path::Path, name: &str) {
    std::fs::create_dir_all(dir).unwrap();
    let content = format!(
        "---\nname: {name}\ndescription: A test skill\nversion: 1.0.0\ncategory: testing\n---\n\nBody for {name}.\n"
    );
    std::fs::write(dir.join(format!("{name}.md")), content).unwrap();
}

fn setup() -> (tempfile::TempDir, tempfile::TempDir) {
    let cfg = tempfile::TempDir::new().unwrap();
    let skills = tempfile::TempDir::new().unwrap();
    std::fs::write(
        cfg.path().join("config.toml"),
        config_toml_with_skills_dir(skills.path()),
    )
    .unwrap();
    (cfg, skills)
}

// ---- config ------------------------------------------------------------

#[serial]
#[tokio::test]
async fn config_path_resolves_under_the_override() {
    let (cfg, _skills) = setup();
    let _guard = ConfigDirEnv::set(cfg.path());
    let dir = kod_config::KodConfig::config_dir().expect("config_dir");
    assert_eq!(dir.as_path(), cfg.path(), "KOD_CONFIG_DIR ignored");
    run_config_path()
        .await
        .expect("run_config_path should succeed");
}

#[serial]
#[tokio::test]
async fn config_validate_on_valid_config_succeeds() {
    let (cfg, _skills) = setup();
    let _guard = ConfigDirEnv::set(cfg.path());
    run_config_validate()
        .await
        .expect("a minimal valid config must validate");
}

/// Export writes to a fresh path, refuses to overwrite without
/// `--force`, and overwrites with it.
///
/// The overwrite refusal used to call `std::process::exit(1)`, which
/// terminated the test process. It now returns
/// `KodError::InvalidParameters`; the caller (`commands::run`) maps
/// `Err` to exit code 1, so the shell-level behaviour is unchanged
/// and `Drop` on everything still alive runs.
#[serial]
#[tokio::test]
async fn config_export_writes_and_refuses_overwrite() {
    let (cfg, _skills) = setup();
    let _guard = ConfigDirEnv::set(cfg.path());
    let out_dir = tempfile::TempDir::new().unwrap();
    let dest = out_dir.path().join("export.toml");

    run_config_export(dest.clone(), false)
        .await
        .expect("export to a fresh path should succeed");
    assert!(dest.exists(), "export did not create {dest:?}");

    let err = run_config_export(dest.clone(), false)
        .await
        .expect_err("export over an existing file without --force must error");
    let msg = err.to_string();
    assert!(
        msg.contains("refusing to overwrite") || msg.contains("--force"),
        "unexpected error message: {msg}",
    );

    // --force overwrites without error.
    run_config_export(dest, true)
        .await
        .expect("export with --force should overwrite");
}

// ---- skills ------------------------------------------------------------

#[serial]
#[tokio::test]
async fn skills_list_on_empty_dir_succeeds() {
    let (cfg, _skills) = setup();
    let _guard = ConfigDirEnv::set(cfg.path());
    run_skills_list(true)
        .await
        .expect("an empty skills dir must not be an error");
}

#[serial]
#[tokio::test]
async fn skills_list_finds_a_configured_skill() {
    let (cfg, skills) = setup();
    write_skill(skills.path(), "demo-skill");
    let _guard = ConfigDirEnv::set(cfg.path());
    run_skills_list(true)
        .await
        .expect("a valid skill file must parse");
}

#[serial]
#[tokio::test]
async fn skills_show_reads_the_named_skill() {
    let (cfg, skills) = setup();
    write_skill(skills.path(), "demo-skill");
    let _guard = ConfigDirEnv::set(cfg.path());
    run_skills_show("demo-skill")
        .await
        .expect("show of an existing skill should succeed");
}
