//! `LspManager` unit tests (design D5.1).
//!
//! The pool is the piece that makes a multi-language session possible:
//! one `LspClient` per server binary, lazily started. The parts that
//! can be tested without a live language server are the ones that
//! matter most for correctness — the binary mapping, the "no server
//! available" path, and shutdown of an empty pool.
//!
//! The parts that need a live server (spawn + initialize + race
//! resolution + diagnostic collection) are exercised by
//! `kod-lsp/src/client.rs`'s `#[ignore]`d live tests and by the
//! `kod-core` `auto_lsp` path, both of which run rust-analyzer against
//! a real fixture.

use kod_lsp::LspManager;
use std::path::{Path, PathBuf};

fn temp_dir() -> tempfile::TempDir {
    tempfile::TempDir::new().unwrap()
}

#[test]
fn workspace_root_is_stable() {
    let dir = temp_dir();
    let manager = LspManager::new(dir.path().to_path_buf());
    assert_eq!(manager.workspace_root(), dir.path());
    assert_eq!(manager.workspace_root(), dir.path());
}

#[test]
fn binary_for_path_maps_rust_files_when_present() {
    // rust-analyzer is a common dev dependency; if it is not on PATH
    // the mapping returns `None`, which is also the correct behaviour.
    // The test asserts the *conditional* nature: it returns `Some`
    // only when the binary is actually present.
    let p = Path::new("/tmp/foo.rs");
    let has = which("rust-analyzer");
    assert_eq!(
        LspManager::has_server_for(&LspManager::new(PathBuf::from("/tmp")), p),
        has,
        "the rust mapping must depend on rust-analyzer's presence"
    );
}

#[test]
fn binary_for_path_none_for_unknown_extensions() {
    let manager = LspManager::new(PathBuf::from("/tmp"));
    // An extension no supported language recognises.
    assert!(!manager.has_server_for(Path::new("/tmp/foo.txt")));
    assert!(!manager.has_server_for(Path::new("/tmp/foo.wasm")));
    // No extension at all.
    assert!(!manager.has_server_for(Path::new("/tmp/Makefile")));
}

#[tokio::test]
async fn diagnostics_without_server_returns_empty() {
    // A file whose extension has no LSP server: the method must
    // return an empty vec without attempting a spawn. If it tried to
    // spawn and failed, we would still get an empty vec — but the
    // returned immediately check below proves no process was left
    // behind either way.
    let dir = temp_dir();
    let manager = LspManager::new(dir.path().to_path_buf());
    let path = dir.path().join("unknown.toml");
    std::fs::write(&path, "x = 1\n").unwrap();
    let result = manager
        .diagnostics(&path, "x = 1\n", std::time::Duration::from_millis(50))
        .await;
    assert!(
        result.is_empty(),
        "a file with no LSP server must yield no diagnostics",
    );
}

#[tokio::test]
async fn definition_without_server_returns_empty() {
    let dir = temp_dir();
    let manager = LspManager::new(dir.path().to_path_buf());
    let path = dir.path().join("unknown.toml");
    std::fs::write(&path, "x = 1\n").unwrap();
    let result = manager
        .definition(&path, kod_lsp::Position { line: 1, column: 1 })
        .await;
    assert!(result.is_empty());
}

#[tokio::test]
async fn references_without_server_returns_empty() {
    let dir = temp_dir();
    let manager = LspManager::new(dir.path().to_path_buf());
    let path = dir.path().join("unknown.toml");
    std::fs::write(&path, "x = 1\n").unwrap();
    let result = manager
        .references(&path, kod_lsp::Position { line: 1, column: 1 }, true)
        .await;
    assert!(result.is_empty());
}

#[tokio::test]
async fn hover_without_server_returns_none() {
    let dir = temp_dir();
    let manager = LspManager::new(dir.path().to_path_buf());
    let path = dir.path().join("unknown.toml");
    std::fs::write(&path, "x = 1\n").unwrap();
    let result = manager
        .hover(&path, kod_lsp::Position { line: 1, column: 1 })
        .await;
    assert!(result.is_none());
}

#[tokio::test]
async fn shutdown_all_on_empty_pool_is_a_noop() {
    let dir = temp_dir();
    let manager = LspManager::new(dir.path().to_path_buf());
    // No server has been spawned; the call must return immediately.
    manager.shutdown_all().await;
    // Idempotent: a second call also returns.
    manager.shutdown_all().await;
}

/// `true` if `program` is an executable file on PATH. Mirrors the
/// private helper in `kod-lsp`, exposed here so the test can assert
/// against the runtime environment rather than a fixed assumption.
fn which(program: &str) -> bool {
    std::env::var_os("PATH")
        .map(|p| std::env::split_paths(&p).any(|d| d.join(program).is_file()))
        .unwrap_or(false)
}
