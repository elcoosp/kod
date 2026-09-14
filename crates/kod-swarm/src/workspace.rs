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

        // Final-component symlink check.
        //
        // `canonicalize` above resolves any symlink whose target
        // exists — including one at the final component pointing
        // outside the workspace, which the containment check catches.
        // The case it does not catch is a *dangling* symlink: the
        // initial canonicalize fails on a target that does not exist,
        // the fallback canonicalizes the deepest existing ancestor
        // (inside the workspace) and re-appends the leaf, and the
        // resulting lexical path passes containment while an OS-level
        // operation would follow the symlink to a target outside.
        //
        // Refuse a dangling leaf symlink. Resolving its target would
        // require recursively following read_link chains and produces
        // a destination whose containment cannot be verified; the
        // safe answer is "no." A symlink whose target exists is
        // unaffected — canonicalize resolves it, and the lock is
        // taken on the target's canonical form (which is the right
        // semantics for coordination: two agents reaching the same
        // file via two symlinks must take the same lock).
        if let Ok(meta) = std::fs::symlink_metadata(&canonical)
            && meta.file_type().is_symlink()
        {
            match std::fs::canonicalize(&canonical) {
                Ok(target) => {
                    // Should be unreachable given the flow above, but
                    // cheap insurance in case the fallback path was
                    // taken for any other reason.
                    if !target.starts_with(&root_canonical) {
                        return Err(KodError::PermissionDenied {
                            action: "access".to_string(),
                            reason: format!(
                                "Path is a symlink whose target escapes the \
                                 workspace: {} -> {}",
                                path.display(),
                                target.display()
                            ),
                        });
                    }
                }
                Err(_) => {
                    return Err(KodError::PermissionDenied {
                        action: "access".to_string(),
                        reason: format!(
                            "Path is a dangling symlink: {}. Refusing to lock \
                             or resolve — the target does not exist and cannot \
                             be verified as inside the workspace.",
                            canonical.display()
                        ),
                    });
                }
            }
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
    /// A dangling symlink inside the workspace must be rejected.
    /// Regression: the fallback canonicalized the deepest existing
    /// ancestor (inside the workspace) and re-appended the leaf, so
    /// the lexical path passed containment while an OS-level write
    /// would have followed the symlink to a target outside.
    #[cfg(unix)]
    #[test]
    fn test_resolve_path_rejects_dangling_symlink() {
        let (_tmp, ws) = ws();
        let link = ws.root().join("dangling");
        std::os::unix::fs::symlink("/nonexistent-kod-swarm-test", &link).unwrap();

        let result = ws.resolve_path(std::path::Path::new("dangling"));
        assert!(
            result.is_err(),
            "dangling symlink must be rejected: {result:?}"
        );
        let msg = result.unwrap_err().to_string();
        assert!(
            msg.contains("dangling") || msg.contains("symlink"),
            "error should name the problem: {msg}"
        );
    }

    /// A symlink whose target exists inside the workspace still
    /// resolves, and resolves to the *target* form — so two agents
    /// reaching the same file via different symlinks take the same
    /// lock.
    #[cfg(unix)]
    #[test]
    fn test_resolve_path_accepts_symlink_to_inside_file() {
        let (_tmp, ws) = ws();
        std::fs::write(ws.root().join("real.txt"), "x").unwrap();
        std::os::unix::fs::symlink(
            ws.root().join("real.txt"),
            ws.root().join("link.txt"),
        )
        .unwrap();

        let via_link = ws
            .resolve_path(std::path::Path::new("link.txt"))
            .expect("symlink to inside file must resolve");
        let via_real = ws
            .resolve_path(std::path::Path::new("real.txt"))
            .expect("real file must resolve");
        assert_eq!(
            via_link, via_real,
            "both paths must resolve to the same canonical form \
             (the lock key)"
        );
    }

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
