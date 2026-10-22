//! The success paths of `kod skills` create / copy / rename / remove.
//!
//! The earlier round (`skills_missing_paths`) covered only the failure
//! branches — missing skill, empty query, overwrite guard. These four
//! functions' success paths write, copy, rename, and delete real skill
//! files, and the `KOD_CONFIG_DIR` + `SkillsConfig::skills_dir` seam
//! (used by `skills_config_cli` and `skills_missing_paths`) makes them
//! testable in a `tempfile::TempDir`.
//!
//! Every test writes a valid skill file with the front-matter shape
//! `kod_skills::SkillParser::validate_metadata` requires: `name`,
//! `description`, `version`, `category`.
use kod_cli::commands::skills::{
    run_skills_copy, run_skills_new, run_skills_remove, run_skills_rename,
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

/// Two temp dirs: config (with `skills_dir` pointing at the second)
/// and the skills directory itself.
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

fn write_skill(dir: &std::path::Path, name: &str) {
    let content = format!(
        "---\nname: {name}\ndescription: A test skill for {name}\nversion: 1.0.0\ncategory: testing\n---\n\nBody for {name}.\n"
    );
    std::fs::write(dir.join(format!("{name}.md")), content).unwrap();
}

fn skill_path(skills: &std::path::Path, name: &str) -> std::path::PathBuf {
    skills.join(format!("{name}.md"))
}

// ---- run_skills_new -------------------------------------------------

/// `kod skills new <name>` writes a template skill file into the
/// configured skills directory.
#[serial]
#[tokio::test]
async fn skills_new_writes_a_file() {
    let (cfg, skills) = setup();
    let _guard = ConfigDirEnv::set(cfg.path());

    run_skills_new("fresh-skill")
        .await
        .expect("creating a new skill must succeed");

    let path = skill_path(skills.path(), "fresh-skill");
    assert!(path.exists(), "skills new did not create {path:?}");
}

// ---- run_skills_copy -------------------------------------------------

/// `kod skills copy <name> <new>` produces a second file with the new
/// name and leaves the original in place.
#[serial]
#[tokio::test]
async fn skills_copy_creates_new_and_keeps_original() {
    let (cfg, skills) = setup();
    write_skill(skills.path(), "original-skill");
    let _guard = ConfigDirEnv::set(cfg.path());

    run_skills_copy("original-skill", "copied-skill")
        .await
        .expect("copying an existing skill must succeed");

    assert!(
        skill_path(skills.path(), "original-skill").exists(),
        "copy must not remove the source",
    );
    assert!(
        skill_path(skills.path(), "copied-skill").exists(),
        "copy must create the destination",
    );
}

// ---- run_skills_rename ----------------------------------------------

/// `kod skills rename <name> <new>` moves the file: the source path is
/// gone and the destination exists.
#[serial]
#[tokio::test]
async fn skills_rename_moves_the_file() {
    let (cfg, skills) = setup();
    write_skill(skills.path(), "old-name");
    let _guard = ConfigDirEnv::set(cfg.path());

    run_skills_rename("old-name", "new-name")
        .await
        .expect("renaming an existing skill must succeed");

    assert!(
        !skill_path(skills.path(), "old-name").exists(),
        "rename must remove the source",
    );
    assert!(
        skill_path(skills.path(), "new-name").exists(),
        "rename must create the destination",
    );
}

// ---- run_skills_remove ----------------------------------------------

/// `kod skills remove <name> --yes` deletes the file. `yes = true`
/// skips the confirmation prompt; the test never reaches stdin.
#[serial]
#[tokio::test]
async fn skills_remove_with_yes_deletes_the_file() {
    let (cfg, skills) = setup();
    write_skill(skills.path(), "doomed-skill");
    let _guard = ConfigDirEnv::set(cfg.path());

    run_skills_remove("doomed-skill", true)
        .await
        .expect("removing with yes=true must succeed");

    assert!(
        !skill_path(skills.path(), "doomed-skill").exists(),
        "remove must delete the file",
    );
}
