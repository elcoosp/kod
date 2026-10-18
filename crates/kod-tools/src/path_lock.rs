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
//! the OS-level advisory locks `fs4` provides; `kod-swarm` was the
//! historical home for that, but as of D4.3 the swarm delegates to
//! this table. The two systems compose: a caller can hold an
//! OS-level lock for the duration and take this advisory lock for
//! in-process ordering.
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
        // Bug-hunt post-v0.1.9 (P2): the pre-fix body called
        // `cells.clear()`, dropping every `Arc<Mutex<()>>` — including
        // one that an outstanding `PathLockGuard` still holds a
        // reference to. The next acquire on that path then created a
        // fresh `Arc<Mutex<()>>`, so the outstanding holder and the new
        // acquirer guarded two different mutexes and both proceeded
        // concurrently on the same file — the exact mutual exclusion
        // the table exists to preserve.
        //
        // Retain a cell if its Arc strong count exceeds 1: the map
        // contributes one reference and every live guard (via its
        // OwnedMutexGuard) contributes one more. A cell at count > 1
        // therefore has an active holder or an acquire in flight and
        // must not be dropped. Only fully-quiescent cells are removed,
        // which is what the docstring's "no new acquires see these
        // cells" contract already promised.
        let mut cells = self.cells.write().await;
        cells.retain(|_, cell| std::sync::Arc::strong_count(cell) > 1);
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
        let g = table
            .acquire(path, "h", Duration::from_millis(50))
            .await
            .unwrap();
        drop(g);
        table.release_all().await;
        // Reacquire succeeds (cells were cleared; new cell created).
        let g2 = table
            .acquire(path, "h2", Duration::from_millis(50))
            .await
            .unwrap();
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

#[cfg(test)]
mod coverage_lock_errors {
    //! The lock table is the coordination primitive a swarm
    //! relies on for serialized writes. Its error path is what
    //! tells the model "the file is busy, try again" — a
    //! regression that swapped a timeout for a silent success
    //! would let two writers race.
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn lock_error_display_names_the_path_and_wait() {
        let e = LockError::Timeout {
            path: PathBuf::from("/tmp/x"),
            waited: Duration::from_millis(250),
        };
        let s = e.to_string();
        assert!(s.contains("/tmp/x"), "path missing: {s}");
        assert!(s.contains("250"), "wait duration missing: {s}");
        assert!(s.contains("timed out"), "cause missing: {s}");
    }

    #[tokio::test]
    async fn guard_reports_path_and_holder() {
        let table = PathLockTable::new();
        let p = Path::new("/tmp/observable");
        let g = table
            .acquire(p, "holder-x", Duration::from_millis(100))
            .await
            .unwrap();
        assert_eq!(g.path(), p);
        assert_eq!(g.holder(), "holder-x");
    }

    #[tokio::test]
    async fn a_second_acquire_after_the_first_guard_drops_succeeds() {
        // The core guarantee: releasing a guard makes the lock
        // available. A regression that leaked the guard's Arc into
        // the table forever (instead of dropping the outer
        // reference) would make every subsequent acquire wait for
        // the timeout.
        let table = PathLockTable::new();
        let p = Path::new("/tmp/serialized");
        {
            let _g = table
                .acquire(p, "first", Duration::from_millis(100))
                .await
                .unwrap();
        }
        let g = table
            .acquire(p, "second", Duration::from_millis(100))
            .await
            .expect("acquire after release should succeed");
        assert_eq!(g.holder(), "second");
    }

    #[tokio::test]
    async fn many_paths_do_not_contend_with_each_other() {
        let table = PathLockTable::new();
        let mut guards = Vec::new();
        for i in 0..20 {
            let p = PathBuf::from(format!("/tmp/p{i}"));
            guards.push(
                table
                    .acquire(&p, &format!("h{i}"), Duration::from_millis(50))
                    .await
                    .unwrap(),
            );
        }
        // All 20 guards held simultaneously; no contention.
        assert_eq!(guards.len(), 20);
    }

    #[tokio::test]
    async fn release_all_does_not_force_unlock_holders() {
        // `release_all` drops the table's *references* to cells;
        // it does not kill outstanding holders. A guard held
        // across `release_all` still keeps its own Arc, and a new
        // acquire after that guard drops must succeed against a
        // fresh cell.
        let table = PathLockTable::new();
        let p = Path::new("/tmp/survives-release");
        let g = table
            .acquire(p, "h", Duration::from_millis(50))
            .await
            .unwrap();
        table.release_all().await;
        drop(g);
        let _g = table
            .acquire(p, "h2", Duration::from_millis(50))
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn zero_timeout_still_acquires_when_uncontended() {
        // A zero-duration timeout must succeed when nobody holds
        // the lock — a regression that short-circuited on 0
        // would make the "wait zero, give up if busy" idiom
        // impossible.
        let table = PathLockTable::new();
        let g = table
            .acquire(Path::new("/tmp/zero"), "h", Duration::ZERO)
            .await
            .unwrap();
        assert_eq!(g.holder(), "h");
    }
}

/// Bug-hunt post-v0.1.9 (P2): `release_all` used to call
/// `cells.clear()`, dropping the `Arc<Mutex<()>>` for a path
/// even while an outstanding `PathLockGuard` still held a
/// reference to it. The *next* acquire on that path then created
/// a fresh `Arc<Mutex<()>>`, so the outstanding holder and the
/// new acquirer guarded two different mutexes and both proceeded
/// — the exact mutual-exclusion break the table exists to
/// prevent.
///
/// This test acquires a path, calls `release_all` while the guard
/// is still alive, and asserts a second acquire on the same path
/// still blocks. On HEAD (cells.clear) it fails immediately; with
/// the retain-by-strong-count fix it passes.
#[tokio::test]
async fn release_all_preserves_mutual_exclusion() {
    let table = PathLockTable::new();
    let path = Path::new("/tmp/release-all-mutex-test");
    let g1 = table
        .acquire(path, "h1", Duration::from_secs(1))
        .await
        .unwrap();
    // release_all while the guard is held — must NOT clear the
    // cell, or mutual exclusion is lost.
    table.release_all().await;
    // Second acquire on the same path must block while g1 held.
    let second = tokio::time::timeout(
        Duration::from_millis(200),
        table.acquire(path, "h2", Duration::from_secs(1)),
    )
    .await;
    assert!(
        second.is_err(),
        "second acquire must block while the first holder is live; \
             got {:?}",
        second.map(|r| r.is_ok())
    );
    // Release g1; now the second acquire should succeed.
    drop(g1);
    table
        .acquire(path, "h3", Duration::from_secs(1))
        .await
        .expect("acquire after release must succeed");
}

#[cfg(test)]
mod path_lock_concurrency {
    use super::*;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// Mutual exclusion under real contention: 8 concurrent tasks
    /// serialize on one path. The atomic counter `inside` records how
    /// many tasks believe they hold the lock at any moment; if the
    /// per-path mutex works, `fetch_add` always returns 0 (no other
    /// task already inside).
    ///
    /// Existing tests cover pairwise acquire/timeout, but none
    /// observes the invariant "no two tasks hold simultaneously"
    /// across N>2 tasks. This is the shape a regression in the
    /// cells map (e.g. release_all clearing while a holder is live —
    /// v0.1.10's bug) would violate.
    #[tokio::test]
    async fn concurrent_acquires_never_overlap() {
        let table = Arc::new(PathLockTable::new());
        let path = Arc::new(PathBuf::from("/tmp/pl-concurrent-no-overlap"));
        let inside = Arc::new(AtomicUsize::new(0));
        let mut handles = Vec::new();
        for i in 0..8 {
            let t = table.clone();
            let p = path.clone();
            let c = inside.clone();
            handles.push(tokio::spawn(async move {
                let g = t
                    .acquire(&p, &format!("w{i}"), Duration::from_secs(5))
                    .await
                    .unwrap();
                let prev = c.fetch_add(1, Ordering::SeqCst);
                assert_eq!(
                    prev, 0,
                    "another task held the lock when we acquired —                      mutual exclusion broken"
                );
                tokio::time::sleep(Duration::from_millis(2)).await;
                c.fetch_sub(1, Ordering::SeqCst);
                drop(g);
            }));
        }
        for h in handles {
            h.await.unwrap();
        }
        assert_eq!(inside.load(Ordering::SeqCst), 0);
    }

    /// A timed-out acquire must not poison the cell. After the holder
    /// drops, the next acquirer succeeds — the timeout is a fact about
    /// the *waiter's* budget, not the cell's state.
    ///
    /// Untested pre-fix: the code path where `tokio::time::timeout`
    /// fires on `cell.lock_owned()` could, in a buggy implementation,
    /// leave an internal queue entry or a "poisoned" flag behind.
    #[tokio::test]
    async fn acquire_timeout_does_not_poison_cell() {
        let table = PathLockTable::new();
        let path = Path::new("/tmp/pl-timeout-no-poison");
        let g1 = table
            .acquire(path, "holder", Duration::from_secs(1))
            .await
            .unwrap();
        // Second acquire with a tiny timeout must fail while held.
        let second = table
            .acquire(path, "waiter", Duration::from_millis(50))
            .await;
        assert!(
            second.is_err(),
            "second acquire must time out while holder is live"
        );
        // Drop the holder; the cell must not be poisoned.
        drop(g1);
        let third = table
            .acquire(path, "after", Duration::from_millis(100))
            .await;
        assert!(third.is_ok(), "acquire after timeout+drop must succeed");
    }

    /// A zero-timeout acquire against a *contended* path fails
    /// immediately. The companion test `zero_timeout_still_acquires_
    /// when_uncontended` covers the uncontended case (must succeed);
    /// this covers the contended case (must fail fast, no busy-wait).
    ///
    /// `tokio::time::timeout(Duration::ZERO, fut)` polls `fut` once
    /// before checking the deadline, so uncontended → Ok, contended →
    /// Err. Anything else (blocking, sleeping) would violate the
    /// zero-timeout contract.
    #[tokio::test]
    async fn zero_timeout_fails_immediately_when_contended() {
        let table = PathLockTable::new();
        let path = Path::new("/tmp/pl-zero-timeout-contended");
        let _holder = table
            .acquire(path, "holder", Duration::from_secs(1))
            .await
            .unwrap();
        let start = std::time::Instant::now();
        let r = table.acquire(path, "waiter", Duration::ZERO).await;
        let elapsed = start.elapsed();
        assert!(r.is_err(), "zero-timeout acquire must fail while contended");
        assert!(
            elapsed < Duration::from_millis(100),
            "zero-timeout acquire must return immediately; took {elapsed:?}"
        );
    }
}
