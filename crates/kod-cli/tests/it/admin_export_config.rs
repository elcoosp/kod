//! `kod config display`, `kod config init-from`, `kod skills export`.
//!
//! Each is reachable through `KOD_CONFIG_DIR` (plus `skills_dir` for
//! the export), builds no engine, spawns nothing, and reads no stdin.
//! `run_commit_check` is deliberately absent — it runs `git diff
//! --cached`, a spawn the STEP 0 guard catches.
use kod_cli::commands::config::{run_config_display, run_config_init_from};
use kod_cli::commands::skills::run_skills_export;
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

fn write_skill(dir: &std::path::Path, name: &str) {
    std::fs::create_dir_all(dir).unwrap();
    let content = format!(
        "---\nname: {name}\ndescription: A test skill\nversion: 1.0.0\ncategory: testing\n---\n\nBody for {name}.\n"
    );
    std::fs::write(dir.join(format!("{name}.md")), content).unwrap();
}

// ---- run_config_display --------------------------------------------

/// `kod config display` on an empty config.toml: every field is the
/// built-in default, the display runs, and it returns Ok.
#[serial]
#[tokio::test]
async fn config_display_returns_ok() {
    let tmp = tempfile::TempDir::new().unwrap();
    std::fs::write(tmp.path().join("config.toml"), "").unwrap();
    let _guard = ConfigDirEnv::set(tmp.path());
    run_config_display()
        .await
        .expect("config display must succeed on any valid config");
}

// ---- run_config_init_from ------------------------------------------

/// `run_config_init_from` writes a named profile's values into
/// `config.toml`. The profile name is read from `PRESETS` at test
/// time, so a rename of the list does not break this test.
#[serial]
#[tokio::test]
async fn config_init_from_writes_config() {
    let tmp = tempfile::TempDir::new().unwrap();
    let _guard = ConfigDirEnv::set(tmp.path());
    let name = kod_config::profiles::PRESETS
        .first()
        .expect("PRESETS is non-empty by construction")
        .name;
    run_config_init_from(name)
        .await
        .expect("init-from a real profile must succeed");
    assert!(
        tmp.path().join("config.toml").exists(),
        "init-from must write config.toml",
    );
}

/// `run_config_init_from` on an existing config backs the old file up
/// as `config.toml.bak-<pid>.<seq>.<nanos>` before overwriting — the
/// same pid+seq+nanos convention as `run_config_migrate` and
/// `run_init --force`.
#[serial]
#[tokio::test]
async fn config_init_from_backs_up_an_existing_config() {
    let tmp = tempfile::TempDir::new().unwrap();
    let _guard = ConfigDirEnv::set(tmp.path());
    let name = kod_config::profiles::PRESETS
        .first()
        .expect("PRESETS non-empty")
        .name;

    // First write creates config.toml.
    run_config_init_from(name).await.expect("first init-from");
    let path = tmp.path().join("config.toml");
    assert!(path.exists());

    // Second write must back the first up.
    run_config_init_from(name).await.expect("second init-from");
    let backups = std::fs::read_dir(tmp.path())
        .unwrap()
        .filter_map(|e| e.ok())
        .filter(|e| {
            e.file_name()
                .to_string_lossy()
                .starts_with("config.toml.bak-")
        })
        .count();
    assert!(backups >= 1, "second init-from did not create a backup");
}

/// `run_config_init_from` on an unknown profile name returns an error
/// naming the known profiles.
#[serial]
#[tokio::test]
async fn config_init_from_unknown_is_err() {
    let tmp = tempfile::TempDir::new().unwrap();
    let _guard = ConfigDirEnv::set(tmp.path());
    let err = run_config_init_from("definitely-not-a-profile")
        .await
        .expect_err("an unknown profile must be rejected");
    assert!(
        err.to_string().contains("Unknown profile"),
        "unexpected error: {err}",
    );
}

// ---- run_skills_export ---------------------------------------------

/// `run_skills_export` writes a named skill's file to a caller-supplied
/// destination. `KOD_CONFIG_DIR` + `skills_dir` point the source scan
/// at a temp dir.
#[serial]
#[tokio::test]
async fn skills_export_writes_to_destination() {
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
    write_skill(skills.path(), "demo-skill");
    let _guard = ConfigDirEnv::set(cfg.path());

    let dest = cfg.path().join("exported.md");
    run_skills_export("demo-skill", dest.clone(), false)
        .await
        .expect("exporting an existing skill must succeed");
    assert!(dest.exists(), "export did not create {dest:?}");
}

/// `run_skills_export` to a destination that already exists without
/// `--force` is `InvalidParameters`, and the existing file is
/// untouched.
#[serial]
#[tokio::test]
async fn skills_export_refuses_overwrite_without_force() {
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
    write_skill(skills.path(), "demo-skill");
    let _guard = ConfigDirEnv::set(cfg.path());

    let dest = cfg.path().join("existing.md");
    std::fs::write(&dest, "preexisting content").unwrap();

    let err = run_skills_export("demo-skill", dest.clone(), false)
        .await
        .expect_err("export over an existing destination must be refused");
    assert!(
        err.to_string().contains("Refusing to overwrite"),
        "unexpected error: {err}",
    );
    // The existing file was not modified.
    assert_eq!(
        std::fs::read_to_string(&dest).unwrap(),
        "preexisting content"
    );
}
