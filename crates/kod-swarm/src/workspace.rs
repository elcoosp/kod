//! Shared workspace for agent collaboration with file locking.

use kod_error::{KodError, Result};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio::sync::Mutex;

/// Type of file lock
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LockType {
    Shared,
    Exclusive,
}

/// A file lock held by an agent
#[derive(Debug, Clone)]
pub struct FileLock {
    pub path: PathBuf,
    pub agent_id: kod_types::AgentId,
    pub lock_type: LockType,
}

/// Shared workspace for agent file coordination
#[derive(Clone)]
pub struct SharedWorkspace {
    root: PathBuf,
    locks: Arc<Mutex<HashMap<PathBuf, Vec<FileLock>>>>,
}

impl SharedWorkspace {
    pub fn new(root: PathBuf) -> Self {
        Self {
            root,
            locks: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Acquire a file lock
    pub async fn acquire_lock(
        &self,
        path: &Path,
        agent_id: &kod_types::AgentId,
        lock_type: LockType,
    ) -> Result<FileLock> {
        let mut locks = self.locks.lock().await;
        let resolved = self.resolve_path(path)?;

        let entry = locks.entry(resolved.clone()).or_insert_with(Vec::new);

        match lock_type {
            LockType::Shared => {
                // Check for exclusive locks
                let has_exclusive = entry.iter().any(|l| l.lock_type == LockType::Exclusive);
                if has_exclusive {
                    return Err(KodError::InvalidState(format!(
                        "File {} is locked exclusively",
                        resolved.display()
                    )));
                }
            }
            LockType::Exclusive => {
                if !entry.is_empty() {
                    return Err(KodError::InvalidState(format!(
                        "File {} is already locked",
                        resolved.display()
                    )));
                }
            }
        }

        let lock = FileLock {
            path: resolved.clone(),
            agent_id: agent_id.clone(),
            lock_type,
        };
        entry.push(lock.clone());
        Ok(lock)
    }

    /// Release a file lock
    pub async fn release_lock(&self, path: &Path, agent_id: &kod_types::AgentId) -> Result<()> {
        let mut locks = self.locks.lock().await;
        let resolved = self.resolve_path(path)?;

        if let Some(entry) = locks.get_mut(&resolved) {
            entry.retain(|l| l.agent_id != *agent_id);
            if entry.is_empty() {
                locks.remove(&resolved);
            }
        }
        Ok(())
    }

    /// Resolve a path relative to the workspace root and confirm it
    /// lies inside the workspace.
    ///
    /// Files that do not exist yet are handled by canonicalizing the
    /// deepest existing ancestor and re-appending the remainder. Without
    /// this, `std::fs::canonicalize(&resolved)` failed on any path that
    /// did not already exist, so the workspace could never lock a file
    /// *before* it was written — exactly the case pre-write coordination
    /// exists for. A second agent about to write the same path is what
    /// needs the lock.
    ///
    /// Traversal is still blocked: after canonicalization the result is
    /// compared against the canonicalized workspace root, so `../etc/…`
    /// and absolute paths outside the root are rejected whether the
    /// target exists or not.
    pub fn resolve_path(&self, path: &Path) -> Result<PathBuf> {
        let resolved = if path.is_absolute() {
            path.to_path_buf()
        } else {
            self.root.join(path)
        };

        // Canonicalize the deepest existing ancestor, then re-attach the
        // non-existent suffix. Any failure to canonicalize an existing
        // ancestor (permission denied, races) propagates as an I/O error.
        let canonical = match std::fs::canonicalize(&resolved) {
            Ok(c) => c,
            Err(_) => {
                // Walk up until an ancestor exists, then rebuild.
                let mut tail: Vec<std::ffi::OsString> = Vec::new();
                let mut cur = resolved.as_path();
                let canon_ancestor = loop {
                    match cur.parent() {
                        Some(parent) => {
                            if let Some(name) = cur.file_name() {
                                tail.push(name.to_os_string());
                            }
                            if let Ok(c) = std::fs::canonicalize(parent) {
                                break c;
                            }
                            cur = parent;
                        }
                        None => {
                            return Err(KodError::InvalidState(format!(
                                "Path has no existing ancestor: {}",
                                resolved.display()
                            )));
                        }
                    }
                };
                let mut rebuilt = canon_ancestor;
                for name in tail.iter().rev() {
                    rebuilt.push(name);
                }
                rebuilt
            }
        };

        let root_canonical = std::fs::canonicalize(&self.root)
            .map_err(|_| KodError::InvalidState("Workspace root not found".to_string()))?;

        if !canonical.starts_with(&root_canonical) {
            return Err(KodError::PermissionDenied {
                action: "access".to_string(),
                reason: format!("Path outside workspace: {}", canonical.display()),
            });
        }

        Ok(canonical)
    }

    /// List all active locks
    pub async fn list_locks(&self) -> Vec<FileLock> {
        self.locks
            .lock()
            .await
            .values()
            .flatten()
            .cloned()
            .collect()
    }
}


#[cfg(test)]
mod tests {
    use super::*;
    use kod_types::AgentId;
    use std::path::PathBuf;
    use tempfile::TempDir;

    fn ws() -> (TempDir, SharedWorkspace) {
        let tmp = TempDir::new().unwrap();
        let ws = SharedWorkspace::new(tmp.path().to_path_buf());
        (tmp, ws)
    }

    /// A file that does not exist yet but would live inside the
    /// workspace must resolve. This is the pre-write lock case: two
    /// agents coordinating on a new file both call acquire_lock before
    /// either has created it.
    #[test]
    fn test_resolve_path_allows_nonexistent_inside_root() {
        let (tmp, ws) = ws();
        let resolved = ws
            .resolve_path(Path::new("brand_new.txt"))
            .expect("should resolve a not-yet-existing path inside the root");
        assert!(resolved.starts_with(tmp.path().canonicalize().unwrap()));
        assert!(resolved.ends_with("brand_new.txt"));
    }

    /// A `..` climb that escapes the workspace is rejected whether or
    /// not the target exists.
    #[test]
    fn test_resolve_path_rejects_traversal() {
        let (_tmp, ws) = ws();
        let escape = PathBuf::from("..").join("outside.txt");
        assert!(
            ws.resolve_path(&escape).is_err(),
            "traversal to a non-existent parent must be rejected"
        );
    }

    /// An absolute path outside the workspace is rejected.
    #[test]
    fn test_resolve_path_rejects_absolute_outside_root() {
        let (_tmp, ws) = ws();
        assert!(ws.resolve_path(Path::new("/etc/hostname")).is_err());
    }

    /// The full pre-write flow: a lock can be acquired on a file that
    /// does not exist yet, then the file is created, and the lock is
    /// released.
    #[tokio::test]
    async fn test_acquire_lock_on_nonexistent_file() {
        let (tmp, ws) = ws();
        let agent = AgentId::new();
        let path = Path::new("to_be_created.txt");

        let lock = ws
            .acquire_lock(path, &agent, LockType::Exclusive)
            .await
            .expect("should be able to lock a not-yet-existing path inside the root");
        assert_eq!(lock.path, tmp.path().canonicalize().unwrap().join("to_be_created.txt"));

        // Second agent cannot acquire the same exclusive lock.
        let other = AgentId::new();
        assert!(
            ws.acquire_lock(path, &other, LockType::Exclusive).await.is_err(),
            "exclusive lock should be held"
        );

        ws.release_lock(path, &agent).await.unwrap();
        // After release, the second agent can acquire it.
        ws.acquire_lock(path, &other, LockType::Exclusive)
            .await
            .expect("lock should be free after release");
    }
}
