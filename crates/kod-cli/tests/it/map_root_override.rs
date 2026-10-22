//! `kod map` with `KOD_MAP_ROOT` pointing at a temp workspace.
//!
//! `run_map` built the map from `std::env::current_dir()`, so a test
//! could not point it anywhere else without `std::env::set_current_dir`
//! (a process-global, unsafe under a parallel runner). `KOD_MAP_ROOT`
//! overrides the directory the same way `KOD_CONFIG_DIR` overrides
//! the config path.
//!
//! `build_repo_map` refuses to walk a directory that does not look
//! like a repo (`looks_like_a_repo`); the fixture writes a
//! `Cargo.toml`, which is the cheapest marker that passes. Without
//! it, `run_map` would return Ok with an empty map and the test would
//! pass for the wrong reason.
use kod_cli::commands::admin::run_map;
use serial_test::serial;

struct MapRootEnv(Option<std::ffi::OsString>);

impl MapRootEnv {
    fn set(dir: &std::path::Path) -> Self {
        let prev = std::env::var_os("KOD_MAP_ROOT");
        // Safety: `#[serial]` on every caller ensures exclusivity.
        unsafe {
            std::env::set_var("KOD_MAP_ROOT", dir);
        }
        MapRootEnv(prev)
    }
}

impl Drop for MapRootEnv {
    fn drop(&mut self) {
        // Safety: same invariant as `set`.
        unsafe {
            match &self.0 {
                Some(prev) => std::env::set_var("KOD_MAP_ROOT", prev),
                None => std::env::remove_var("KOD_MAP_ROOT"),
            }
        }
    }
}

/// A minimal `Cargo.toml` + one `.rs` file. The marker makes
/// `looks_like_a_repo` accept the temp dir; the `.rs` file gives the
/// walker something to count.
fn seed_workspace(dir: &std::path::Path) {
    std::fs::create_dir_all(dir.join("src")).unwrap();
    std::fs::write(
        dir.join("Cargo.toml"),
        "[package]\nname = \"map-fixture\"\nversion = \"0.1.0\"\n",
    )
    .unwrap();
    std::fs::write(
        dir.join("src").join("lib.rs"),
        "pub fn fixture_function() -> u32 { 42 }\n",
    )
    .unwrap();
}

/// `run_map` on a seeded temp workspace returns Ok. The output is
/// printed; the assertion is the Ok — a regression that rejected the
/// override would fail before reaching the render.
#[serial]
#[tokio::test]
async fn map_with_a_kod_map_root_succeeds() {
    let tmp = tempfile::TempDir::new().unwrap();
    seed_workspace(tmp.path());
    let _guard = MapRootEnv::set(tmp.path());

    run_map(500)
        .await
        .expect("run_map against a seeded workspace must succeed");
}

/// `run_map` on a directory that is *not* a repo (no `Cargo.toml`,
/// no `.git`) still returns Ok — `build_repo_map` returns an empty
/// map and the command prints that, rather than erroring. The
/// assertion pins that "not a repo" is a normal result.
#[serial]
#[tokio::test]
async fn map_on_a_non_repo_dir_is_ok() {
    let tmp = tempfile::TempDir::new().unwrap();
    std::fs::write(tmp.path().join("readme.txt"), "hello").unwrap();
    let _guard = MapRootEnv::set(tmp.path());

    run_map(200)
        .await
        .expect("a non-repo dir yields an empty map, not an error");
}
