//! In-process advisory locks keyed by canonical filesystem path.
//!
//! # What this is for
//!
//! Two callers writing the same file concurrently — two agents in a
//! swarm, or a swarm agent and the interactive session — must not race
//! on the write. The lock table is the coordination primitive: the
//! writer that acquires first holds the path until its write completes;
//! the second writer waits up to a bounded timeout, then fails with a
//! message naming the holder.
//!
//! # What this is not
//!
//! **The lock is process-local.** Two `kod` processes writing the same
//! file do not see each other's locks. Cross-process coordination needs
//! the OS-level file locks `kod-swarm`'s `SharedWorkspace` provides
//! (via `fs4`); that crate is the right home for cross-process locks,
//! and the two systems compose: a caller can hold an OS-level lock for
//! the duration and take this advisory lock for in-process ordering.
//! The two do not share state today.
//!
//! # Data structure
//!
//! A `HashMap<PathBuf, Arc<tokio::sync::Mutex<()>>>` behind an `RwLock`:
//!
//! - The outer map answers "is there a lock cell for this path?" and
//!   creates one on first use. It holds the `RwLock` for microseconds;
//!   the actual serialization is on the per-path `tokio::sync::Mutex`.
//! - The per-path mutex is `Arc` so a guard can drop the table's
//!   reference without racing the map's own lifetime.
//! - Cells are **not** removed when a guard drops. The cost is one
//!   `Arc<Mutex<()>>` per distinct path ever locked; the benefit is
//!   that the fast path never contends the outer `RwLock` for writes.
//!   For an agent harness whose path set is bounded by the repository's
//!   size, that is the right trade — a real cache would need eviction
//!   on top and has no user-visible benefit here.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{Mutex, OwnedMutexGuard, RwLock};

/// A shared table of per-path locks. One per engine.
#[derive(Debug, Default)]
pub struct PathLockTable {
    cells: RwLock<HashMap<PathBuf, Arc<Mutex<()>>>>,
}

impl PathLockTable {
    pub fn new() -> Self {
        Self::default()
    }

    /// Drop every cell. Called by `KodEngine::shutdown()` so a long-lived
    /// process does not keep `Arc<Mutex<()>>` cells alive after the
    /// engine stops. Any outstanding `PathLockGuard` still holds its own
    /// `Arc` and will release on drop — this only clears the table's
    /// references so the process can exit cleanly.
    ///
    /// Not a force-unlock: a writer mid-critical-section is not interrupted.
    /// The contract is "no new acquires see these cells", not "kill holders".
    pub async fn release_all(&self) {
        self.cells.write().await.clear();
    }

    /// Acquire the lock for `path`, waiting up to `timeout` for a
    /// holder to release. Returns the guard on success.
    ///
    /// `holder` is the caller's own identity — it is what a *blocked*
    /// caller sees in the timeout error, and what a debugging log can
    /// attribute the current write to. It is not used for
    /// re-entrancy: a holder that tries to acquire the same path twice
    /// deadlocks against itself, which is the intended behavior for a
    /// tool that is not supposed to nest writes.
    pub async fn acquire(
        &self,
        path: &Path,
        holder: &str,
        timeout: Duration,
    ) -> Result<PathLockGuard, LockError> {
        let cell = {
            // Fast path: the cell exists.
            {
                let cells = self.cells.read().await;
                if let Some(cell) = cells.get(path) {
                    cell.clone()
                } else {
                    drop(cells);
                    // Slow path: create the cell. A concurrent creator
                    // may win the race; the loser's `Arc` is dropped
                    // unused, which is fine.
                    let mut cells = self.cells.write().await;
                    cells
                        .entry(path.to_path_buf())
                        .or_insert_with(|| Arc::new(Mutex::new(())))
                        .clone()
                }
            }
        };

        match tokio::time::timeout(timeout, cell.lock_owned()).await {
            Ok(guard) => Ok(PathLockGuard {
                _guard: guard,
                path: path.to_path_buf(),
                holder: holder.to_string(),
            }),
            Err(_) => Err(LockError::Timeout {
                path: path.to_path_buf(),
                waited: timeout,
            }),
        }
    }
}

/// The acquired lock. Held for the duration of the write; drops (and so
/// releases the lock) automatically.
pub struct PathLockGuard {
    _guard: OwnedMutexGuard<()>,
    path: PathBuf,
    holder: String,
}

impl PathLockGuard {
    /// The path this guard holds. Provided so a caller can log the
    /// attribution.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// The holder string this guard was acquired under.
    pub fn holder(&self) -> &str {
        &self.holder
    }
}

/// Why an acquire failed.
#[derive(Debug, thiserror::Error)]
pub enum LockError {
    /// The holder did not release within the timeout. The message names
    /// the path and the wait — the holder identity is not stored in the
    /// cell (there is only one mutex per path, not a "current holder"
    /// record), so the message cannot name *who* holds it. That is a
    /// deliberate simplification: a caller blocked for two seconds has
    /// enough information to decide what to do, and identity-tracking
    /// would need a second lock to keep the "current holder" field
    /// consistent with the mutex itself.
    #[error("timed out after {waited:?} waiting to write {path} (another caller is holding it)")]
    Timeout { path: PathBuf, waited: Duration },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn release_all_clears_cells_and_allows_reacquire() {
        let table = PathLockTable::new();
        let path = Path::new("/tmp/release-all-test");
        let g = table.acquire(path, "h", Duration::from_millis(50)).await.unwrap();
        drop(g);
        table.release_all().await;
        // Reacquire succeeds (cells were cleared; new cell created).
        let g2 = table.acquire(path, "h2", Duration::from_millis(50)).await.unwrap();
        drop(g2);
    }

    #[tokio::test]
    async fn acquire_and_release() {
        let table = PathLockTable::new();
        let path = Path::new("/tmp/a");
        let guard = table
            .acquire(path, "first", Duration::from_millis(100))
            .await
            .expect("first acquire succeeds");
        assert_eq!(guard.holder(), "first");
        drop(guard);
        // After the drop, a second acquire succeeds.
        table
            .acquire(path, "second", Duration::from_millis(100))
            .await
            .expect("second acquire succeeds after release");
    }

    #[tokio::test]
    async fn second_acquire_waits_then_times_out() {
        let table = Arc::new(PathLockTable::new());
        let path = PathBuf::from("/tmp/b");
        let table_for_task = table.clone();
        let path_for_task = path.clone();
        let holder = tokio::spawn(async move {
            let guard = table_for_task
                .acquire(&path_for_task, "long", Duration::from_millis(100))
                .await
                .expect("holder acquires");
            tokio::time::sleep(Duration::from_millis(300)).await;
            drop(guard);
        });
        // Give the holder a moment to grab the lock.
        tokio::time::sleep(Duration::from_millis(20)).await;

        let start = std::time::Instant::now();
        // `expect_err` needs Debug on the Ok type and `OwnedMutexGuard`
        // is not Debug — match instead of unwrapping.
        let err = match table
            .acquire(&path, "blocked", Duration::from_millis(80))
            .await
        {
            Ok(_) => panic!("blocked acquire must time out"),
            Err(e) => e,
        };
        let waited = start.elapsed();
        match err {
            LockError::Timeout { path: p, .. } => assert_eq!(p, path),
        }
        assert!(
            waited >= Duration::from_millis(70),
            "should have waited at least the timeout: {waited:?}"
        );

        // Clean up the holder.
        let _ = holder.await;
    }

    #[tokio::test]
    async fn different_paths_do_not_block() {
        let table = PathLockTable::new();
        let g1 = table
            .acquire(Path::new("/tmp/x"), "h1", Duration::from_millis(100))
            .await
            .expect("x");
        let g2 = table
            .acquire(Path::new("/tmp/y"), "h2", Duration::from_millis(100))
            .await
            .expect("y");
        drop((g1, g2));
    }

    #[tokio::test]
    async fn contention_is_fifo_enough() {
        // Three acquirers on the same path, released in order. Not a
        // strict FIFO guarantee (tokio's mutex is fair but let us not
        // over-specify) — this asserts all three eventually succeed
        // rather than deadlock.
        let table = Arc::new(PathLockTable::new());
        let path = PathBuf::from("/tmp/serialized");
        let mut handles = Vec::new();
        for i in 0..3 {
            let t = table.clone();
            let p = path.clone();
            handles.push(tokio::spawn(async move {
                let g = t
                    .acquire(&p, &format!("t{i}"), Duration::from_millis(500))
                    .await
                    .expect("acquire");
                tokio::time::sleep(Duration::from_millis(10)).await;
                drop(g);
            }));
        }
        for h in handles {
            h.await.expect("no task panicked");
        }
    }
}
