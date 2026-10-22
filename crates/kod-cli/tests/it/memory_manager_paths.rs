//! `kod memory` with the DB path redirected by `[memory] long_term_db_path`.
//!
//! `run_memory` calls `MemoryManager::new(config.memory_db_path(), …)`.
//! `memory_db_path()` honours `[memory] long_term_db_path` when set, so
//! a temp `config.toml` with that field points the whole chain at a
//! `tempfile::TempDir` — no `~/.kod/data/kod.redb` is touched, and the
//! `KOD_CONFIG_DIR` seam is the same one used by every other CLI test
//! this session.
//!
//! The `MemoryAction` variants here avoid the network (no embedder is
//! configured; `MemoryManager::new` hard-codes `embedder: None`, so
//! semantic search falls back to keyword+recency) and the interactive
//! prompt (`Forget` on a key that does not match anything returns Ok
//! without deleting).
use kod_cli::commands::MemoryAction;
use kod_cli::commands::memory::run_memory;
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

/// Write a `config.toml` whose `[memory] long_term_db_path` points at
/// `db`, so no code path reaches `~/.kod/data/kod.redb`.
fn write_config(cfg_dir: &std::path::Path, db: &std::path::Path) {
    std::fs::write(
        cfg_dir.join("config.toml"),
        format!(
            "[memory]\nlong_term_db_path = {:?}\nenable_semantic_search = false\n",
            db.to_str().expect("utf-8 temp path"),
        ),
    )
    .unwrap();
}

/// Tempdir + a `config.toml` with `long_term_db_path` set to a file
/// inside it. Returns `(config_dir, db_path)`.
fn setup() -> (tempfile::TempDir, std::path::PathBuf) {
    let cfg = tempfile::TempDir::new().unwrap();
    let db = cfg.path().join("memory.redb");
    write_config(cfg.path(), &db);
    (cfg, db)
}

/// `kod memory list` on an empty DB. `get_all_long_term` returns an
/// empty Vec; the listing prints nothing and returns Ok.
#[serial]
#[tokio::test]
async fn memory_list_empty_db_is_ok() {
    let (cfg, _db) = setup();
    let _guard = ConfigDirEnv::set(cfg.path());
    run_memory(MemoryAction::List)
        .await
        .expect("an empty memory DB is the first-run case, not an error");
}

/// `kod memory add` stores an entry. The store path uses keyword
/// fallback (no embedder configured), so no network is reached.
#[serial]
#[tokio::test]
async fn memory_add_stores_an_entry() {
    let (cfg, _db) = setup();
    let _guard = ConfigDirEnv::set(cfg.path());
    run_memory(MemoryAction::Add {
        content: "the test stores a fact".to_string(),
        tags: Some("test,memory".to_string()),
    })
    .await
    .expect("adding a memory entry must succeed against a temp DB");
}

/// `kod memory forget` on a key that matches nothing returns Ok with
/// "No entries match" printed. It does not error and does not attempt
/// the confirmation prompt (there is nothing to delete).
#[serial]
#[tokio::test]
async fn memory_forget_missing_key_is_ok() {
    let (cfg, _db) = setup();
    let _guard = ConfigDirEnv::set(cfg.path());
    run_memory(MemoryAction::Forget {
        key: "no-such-id-or-tag".to_string(),
    })
    .await
    .expect("a forget that matches nothing is not an error");
}

/// `kod memory export` writes the long-term store to a caller-supplied
/// path. On an empty DB that file is a valid (empty) JSON array; the
/// assertion is that the file exists and `run_memory` returned Ok.
#[serial]
#[tokio::test]
async fn memory_export_writes_a_file() {
    let (cfg, _db) = setup();
    let _guard = ConfigDirEnv::set(cfg.path());
    let out = cfg.path().join("export.json");
    run_memory(MemoryAction::Export { path: out.clone() })
        .await
        .expect("exporting to a file path must succeed");
    assert!(out.exists(), "export did not create {out:?}");
}

/// Export-then-import round-trip. Exporting an empty store writes the
/// shape Import consumes; importing it back is a no-op that returns
/// Ok. This pins the *format* contract without pinning a specific
/// JSON shape (the shape is whatever Export produces).
#[serial]
#[tokio::test]
async fn memory_export_import_round_trip_is_ok() {
    let (cfg, _db) = setup();
    let _guard = ConfigDirEnv::set(cfg.path());
    let out = cfg.path().join("round-trip.json");
    run_memory(MemoryAction::Export { path: out.clone() })
        .await
        .expect("export");
    run_memory(MemoryAction::Import { path: out })
        .await
        .expect("import of an export round-trips");
}
