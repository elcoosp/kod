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

    /// Resolve a path relative to workspace root
    pub fn resolve_path(&self, path: &Path) -> Result<PathBuf> {
        let resolved = if path.is_absolute() {
            path.to_path_buf()
        } else {
            self.root.join(path)
        };

        // Check the path is within workspace
        let canonical = std::fs::canonicalize(&resolved)
            .map_err(|_| KodError::InvalidState(format!("Path not found: {}", resolved.display())))?;
        let root_canonical = std::fs::canonicalize(&self.root)
            .map_err(|_| KodError::InvalidState("Workspace root not found".to_string()))?;

        if !canonical.starts_with(root_canonical) {
            return Err(KodError::PermissionDenied {
                action: "access".to_string(),
                reason: format!("Path outside workspace: {}", canonical.display()),
            });
        }

        Ok(canonical)
    }

    /// List all active locks
    pub async fn list_locks(&self) -> Vec<FileLock> {
        self.locks.lock().await
            .values()
            .flatten()
            .cloned()
            .collect()
    }
}
