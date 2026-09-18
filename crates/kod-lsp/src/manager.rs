//! Multi-language language-server pool.
//!
//! # Why a manager, not a single client
//!
//! `LspClient`'s methods take `&mut self` (each call bumps a request
//! id and reads the response stream). A single client slot can
//! therefore serve one language per session: a second language either
//! reuses the wrong server (and gets empty diagnostics back, because
//! the file's URI is not in that server's index) or forces a
//! drop-and-respawn that discards the first server's warm index —
//! rust-analyzer's indexing cost is the whole reason to keep a server
//! alive between calls.
//!
//! The manager gives each language its own client behind its own
//! mutex. Requests for different languages run concurrently; requests
//! for the same language serialize on that language's mutex (which is
//! what a language server does anyway — it processes one request at a
//! time).
//!
//! # Lifecycle
//!
//! - `LspManager::new` is cheap: no server is spawned until the first
//!   request for its language.
//! - `client_for(binary)` spawns and initializes on the first call,
//!   and is idempotent thereafter. Two concurrent first-callers race:
//!   both spawn, the loser's is shut down and the winner's is
//!   returned. The alternative — hold the outer lock across the
//!   spawn — would serialize every language on the slowest one.
//! - `shutdown_all` drains the pool and shuts every client down
//!   cleanly. Called by `KodEngine::shutdown`.

use crate::{Diagnostic, Hover, Location, LspClient, LspError, Position};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::Mutex;

/// Which server binary speaks for a given file extension.
///
/// Detection is by extension; the caller decides which binary to
/// spawn. This is the single source of truth for "which language
/// server does this file need" — the `kod doctor` check and the
/// `lsp_*` tools both call it. A `None` return means "no server for
/// this language on this host" and is not an error: the caller
/// reports it as a missing capability and falls back to the
/// compiler-based `check`.
pub fn binary_for_path(path: &Path) -> Option<&'static str> {
    match path.extension().and_then(|s| s.to_str()) {
        Some("rs") if which("rust-analyzer") => Some("rust-analyzer"),
        Some("py") if which("pyright-langserver") => Some("pyright-langserver"),
        Some("py") if which("pylsp") => Some("pylsp"),
        Some("ts") | Some("tsx") | Some("js") | Some("jsx")
            if which("typescript-language-server") =>
        {
            Some("typescript-language-server")
        }
        Some("go") if which("gopls") => Some("gopls"),
        _ => None,
    }
}

/// True if `program` is an executable file on PATH. Small duplicate of
/// the same helper in `kod-core::engine` — keeping it here avoids a
/// crate dependency in the opposite direction (kod-core already
/// depends on kod-lsp).
fn which(program: &str) -> bool {
    let Some(path) = std::env::var_os("PATH") else {
        return false;
    };
    std::env::split_paths(&path).any(|d| d.join(program).is_file())
}

/// The set of language servers this session has started, keyed by
/// binary name.
pub struct LspManager {
    workspace_root: PathBuf,
    servers: Mutex<HashMap<String, Arc<Mutex<LspClient>>>>,
}

impl LspManager {
    /// New manager for a given workspace root. Cheap; no process is
    /// spawned until the first request for a language.
    pub fn new(workspace_root: PathBuf) -> Self {
        Self {
            workspace_root,
            servers: Mutex::new(HashMap::new()),
        }
    }

    /// The workspace root every server is spawned under. Exposed so a
    /// caller (a test, a status panel) can show where a server's
    /// index is being built.
    pub fn workspace_root(&self) -> &Path {
        &self.workspace_root
    }

    /// True when the file's language has a server binary on PATH.
    /// Lets a caller emit a "no language server for .py — install
    /// pyright" error without waiting for a failed spawn.
    pub fn has_server_for(&self, path: &Path) -> bool {
        binary_for_path(path).is_some()
    }

    /// Get (or lazily start + initialize) the client for `binary`.
    ///
    /// First-call semantics under concurrency: both callers may spawn
    /// a client, and the loser's is shut down rather than leaked.
    /// Holding the outer lock across the spawn would serialize the
    /// first request for every language on the slowest one — a
    /// rust-analyzer index that takes 3 s would hold up a Python
    /// server that takes 100 ms.
    async fn client_for(&self, binary: &str) -> Result<Arc<Mutex<LspClient>>, LspError> {
        {
            let guard = self.servers.lock().await;
            if let Some(c) = guard.get(binary) {
                return Ok(Arc::clone(c));
            }
        }

        let mut client = LspClient::start(binary, &self.workspace_root).await?;
        client.initialize().await?;
        let arc = Arc::new(Mutex::new(client));

        let mut guard = self.servers.lock().await;
        if let Some(existing) = guard.get(binary) {
            let existing = Arc::clone(existing);
            drop(guard);
            // Another task won the race; shut ours down rather than
            // leak a child process. `Arc::try_unwrap` succeeds when no
            // other holder of `arc` exists — which is the case here
            // (we never published it). If it ever does not, the Arc
            // is dropped and `kill_on_drop` reaps the child.
            if let Ok(mutex) = Arc::try_unwrap(arc) {
                // `tokio::sync::Mutex::into_inner` returns `T`, not
                // `LockResult<T>`: the tokio mutex is not poisoned.
                let client = mutex.into_inner();
                client.shutdown().await;
            }
            return Ok(existing);
        }
        guard.insert(binary.to_string(), Arc::clone(&arc));
        Ok(arc)
    }

    /// `Ok(Some)` when a server exists and is up; `Ok(None)` when the
    /// file's language has no server on PATH; `Err` when the server
    /// exists but failed to start or initialize.
    async fn client_for_path(
        &self,
        path: &Path,
    ) -> Result<Option<Arc<Mutex<LspClient>>>, LspError> {
        let Some(binary) = binary_for_path(path) else {
            return Ok(None);
        };
        Ok(Some(self.client_for(binary).await?))
    }

    /// Diagnostics for one file.
    ///
    /// Empty on any error: no server for the language, spawn failure,
    /// protocol failure, timeout. The caller treats empty as "no LSP
    /// feedback" and falls back to the compiler-based `check`, which
    /// disambiguates "clean" from "unreachable" — the reason a
    /// `Vec`-returning signature is the right shape here.
    pub async fn diagnostics(
        &self,
        path: &Path,
        content: &str,
        timeout: Duration,
    ) -> Vec<Diagnostic> {
        let Some(client) = self.client_for_path(path).await.ok().flatten() else {
            return Vec::new();
        };
        let mut guard = client.lock().await;
        guard
            .diagnostics(path, content, timeout)
            .await
            .unwrap_or_default()
    }

    /// Go-to-definition for the symbol at `pos`. Empty on any error.
    pub async fn definition(&self, path: &Path, pos: Position) -> Vec<Location> {
        let Some(client) = self.client_for_path(path).await.ok().flatten() else {
            return Vec::new();
        };
        let mut guard = client.lock().await;
        guard.definition(path, pos).await.unwrap_or_default()
    }

    /// Find-references for the symbol at `pos`. Empty on any error.
    pub async fn references(
        &self,
        path: &Path,
        pos: Position,
        include_declaration: bool,
    ) -> Vec<Location> {
        let Some(client) = self.client_for_path(path).await.ok().flatten() else {
            return Vec::new();
        };
        let mut guard = client.lock().await;
        guard
            .references(path, pos, include_declaration)
            .await
            .unwrap_or_default()
    }

    /// Hover summary for the symbol at `pos`. `None` on any error, or
    /// when the server has nothing to say about the position.
    pub async fn hover(&self, path: &Path, pos: Position) -> Option<Hover> {
        let client = self.client_for_path(path).await.ok().flatten()?;
        let mut guard = client.lock().await;
        guard.hover(path, pos).await.ok()
    }

    /// Shut down every server.
    ///
    /// Drains the map first (so a subsequent call is a no-op and no
    /// new spawn can race the drain) and then consumes each client.
    /// A client that is still referenced by an in-flight method call
    /// (`Arc::try_unwrap` returns `Err`) is left to `kill_on_drop`;
    /// the process is killed when the last reference drops.
    pub async fn shutdown_all(&self) {
        let servers = {
            let mut guard = self.servers.lock().await;
            std::mem::take(&mut *guard)
        };
        for (_binary, arc) in servers {
            if let Ok(mutex) = Arc::try_unwrap(arc) {
                // `tokio::sync::Mutex::into_inner` returns `T`, not
                // `LockResult<T>`: the tokio mutex is not poisoned.
                let client = mutex.into_inner();
                client.shutdown().await;
            }
        }
    }
}

impl Default for LspManager {
    fn default() -> Self {
        Self::new(std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")))
    }
}
