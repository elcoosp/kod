//! `kod doctor --fix` with every path redirected to a temp dir.
//!
//! `run_doctor_fix` touches four roots: `config_dir()` (via
//! `KOD_CONFIG_DIR`), `skills_dirs()` (via `[skills] skills_dir`),
//! `memory_db_path()` (via `[memory] long_term_db_path`), and
//! `CheckpointManager::for_working_dir()` (via the new
//! `KOD_CHECKPOINT_DIR`). All four are redirected here, so the
//! function never writes into the developer's home.
use kod_cli::commands::admin::run_doctor_fix;
use serial_test::serial;

struct TempRoots {
    cfg: Option<std::ffi::OsString>,
    ckpt: Option<std::ffi::OsString>,
}

impl TempRoots {
    fn set(cfg: &std::path::Path, ckpt: &std::path::Path) -> Self {
        let cfg_prev = std::env::var_os("KOD_CONFIG_DIR");
        let ckpt_prev = std::env::var_os("KOD_CHECKPOINT_DIR");
        // Safety: `#[serial]` on every caller ensures exclusivity.
        unsafe {
            std::env::set_var("KOD_CONFIG_DIR", cfg);
            std::env::set_var("KOD_CHECKPOINT_DIR", ckpt);
        }
        TempRoots {
            cfg: cfg_prev,
            ckpt: ckpt_prev,
        }
    }
}

impl Drop for TempRoots {
    fn drop(&mut self) {
        // Safety: same invariant as `set`.
        unsafe {
            match &self.cfg {
                Some(prev) => std::env::set_var("KOD_CONFIG_DIR", prev),
                None => std::env::remove_var("KOD_CONFIG_DIR"),
            }
            match &self.ckpt {
                Some(prev) => std::env::set_var("KOD_CHECKPOINT_DIR", prev),
                None => std::env::remove_var("KOD_CHECKPOINT_DIR"),
            }
        }
    }
}

/// Write a minimal valid `config.toml` whose `[memory]` and `[skills]`
/// sections point at temp paths. `KOD_CONFIG_DIR` points here.
fn write_config(cfg_dir: &std::path::Path, skills: &std::path::Path, db: &std::path::Path) {
    std::fs::write(
        cfg_dir.join("config.toml"),
        format!(
            "[memory]\nlong_term_db_path = {:?}\n\n[skills]\nskills_dir = {:?}\n",
            db.to_str().expect("utf-8"),
            skills.to_str().expect("utf-8"),
        ),
    )
    .unwrap();
}

/// `kod doctor --fix` over an all-temp config: every path it would
/// create is under the temp dir, the function returns, and the
/// checkpoint root is created (that is the fix's own effect, and the
/// assertion that the override reached the checkpoint manager).
#[serial]
#[tokio::test]
async fn doctor_fix_creates_directories_under_temp_roots() {
    let cfg = tempfile::TempDir::new().unwrap();
    let skills = cfg.path().join("skills");
    let db = cfg.path().join("memory.redb");
    let ckpt = cfg.path().join("checkpoints");
    write_config(cfg.path(), &skills, &db);
    let _guard = TempRoots::set(cfg.path(), &ckpt);

    let _ = run_doctor_fix(false).await;

    // The fix creates the checkpoint dir it would otherwise leave to
    // the caller. Assert that — it is the only write the fix does
    // that is not derivable from the config alone.
    assert!(
        ckpt.exists(),
        "run_doctor_fix did not create the KOD_CHECKPOINT_DIR root",
    );
    // And the skills dir, from the config's `[skills] skills_dir`.
    assert!(
        skills.exists(),
        "run_doctor_fix did not create the skills dir"
    );
}

/// The JSON form runs the same code path with a different serializer
/// and returns without panicking.
#[serial]
#[tokio::test]
async fn doctor_fix_json_returns_without_panicking() {
    let cfg = tempfile::TempDir::new().unwrap();
    let skills = cfg.path().join("skills");
    let db = cfg.path().join("memory.redb");
    let ckpt = cfg.path().join("checkpoints");
    write_config(cfg.path(), &skills, &db);
    let _guard = TempRoots::set(cfg.path(), &ckpt);

    let _ = run_doctor_fix(true).await;
}
