//! Delta §7.7: global filesystem-scan cache.
//!
//! # Why
//!
//! A single `grep` walks the tree once. A single `list_files` walks
//! it again. A turn that uses both walks it twice. `ignore::WalkBuilder`
//! consults `.gitignore` at every directory and stats every entry, so
//! on a large workspace the second walk pays the full cost of the
//! first for an answer that has not changed between them.
//!
//! # Cache shape
//!
//! Key is `(root, recursive)` — the two parameters that change what
//! the walk returns. Value is the resulting `Vec<PathBuf>` plus the
//! instant it was produced.
//!
//! # Invalidation
//!
//! Two mechanisms, both required:
//!
//! * **Per-write.** Every tool that mutates disk calls
//!   [`invalidate_all`] after a successful write. A cache that
//!   survives a write returns a filename the walker would no longer
//!   find — the classic stale-listing bug.
//! * **TTL.** Each entry carries its birth instant. The TTL is
//!   [`DEFAULT_TTL_SECS`], longer than one turn (so a `grep` and a
//!   `list_files` in the same turn share) but short enough that an
//!   un-hooked write (a tool we forgot, an out-of-band editor,
//!   `git checkout`) is bounded. Five seconds is the balance.
//!
//! # What this is NOT
//!
//! Not a directory watcher. A watcher fires on every filesystem event
//! and needs a persistent thread; kod's usage does not justify it.
//! Not a substitute for `.gitignore` — the cache stores the *result*
//! of the ignore-aware walk, not the ignore rules.
//!
//! # Bounds
//!
//! [`MAX_ENTRIES`] caps the cache; past it, the oldest entry is
//! evicted on insert. [`MAX_PATHS_PER_ENTRY`] caps one entry's
//! result. A root larger than that is unusual; the entry is then
//! cached as truncated, and the caller sees the same result it would
//! have gotten from a live walk of the same cap.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

/// How long a cached walk stays valid, absent an explicit
/// invalidation. See the module docs for the trade.
pub const DEFAULT_TTL_SECS: u64 = 5;

/// Cap on the number of distinct `(root, recursive)` entries.
/// Past the cap the oldest is evicted on insert. Sixty-four is
/// generous — a session touches a handful of roots.
pub const MAX_ENTRIES: usize = 64;

/// Cap on one entry's path count. A root with more than this is
/// unusual; the extra paths are not stored and the entry is marked
/// truncated.
pub const MAX_PATHS_PER_ENTRY: usize = 100_000;

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct Key {
    root: PathBuf,
    recursive: bool,
}

struct Entry {
    paths: Vec<PathBuf>,
    at: Instant,
}

/// Per-process counters, for a debug surface and for tests that
/// assert hit/miss behavior.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Stats {
    pub hits: u64,
    pub misses: u64,
    pub entries: usize,
}

/// The cache. A single process-wide instance lives behind
/// [`global`]; tests construct their own to avoid cross-test state.
pub struct WalkCache {
    entries: Mutex<HashMap<Key, Entry>>,
    hits: AtomicU64,
    misses: AtomicU64,
    ttl: Duration,
}

impl WalkCache {
    /// A cache with [`DEFAULT_TTL_SECS`]. A test that needs a
    /// different TTL uses [`WalkCache::with_ttl`].
    pub fn new() -> Self {
        Self::with_ttl(Duration::from_secs(DEFAULT_TTL_SECS))
    }

    /// A cache with an explicit TTL. `Duration::ZERO` makes every
    /// entry stale on the next read (used by tests).
    pub fn with_ttl(ttl: Duration) -> Self {
        Self {
            entries: Mutex::new(HashMap::new()),
            hits: AtomicU64::new(0),
            misses: AtomicU64::new(0),
            ttl,
        }
    }

    /// Return the cached walk for `(root, recursive)` if fresh, else
    /// run `walk` and cache its result.
    ///
    /// The walk runs outside the lock — two callers on a cold cache
    /// both walk, and the second insert overwrites the first. That
    /// is a rare race (a cold cache in two threads) and the cost of
    /// the alternative — a per-key lock, or a lock held across the
    /// walk — is worse than one redundant walk.
    pub fn get_or_walk<F>(&self, root: &Path, recursive: bool, walk: F) -> Vec<PathBuf>
    where
        F: FnOnce() -> Vec<PathBuf>,
    {
        let key = Key {
            root: root.to_path_buf(),
            recursive,
        };
        {
            let g = self.entries.lock().expect("walk_cache poisoned");
            if let Some(e) = g.get(&key)
                && e.at.elapsed() < self.ttl
            {
                self.hits.fetch_add(1, Ordering::Relaxed);
                return e.paths.clone();
            }
        }
        self.misses.fetch_add(1, Ordering::Relaxed);
        let mut paths = walk();
        if paths.len() > MAX_PATHS_PER_ENTRY {
            tracing::warn!(
                root = %root.display(),
                total = paths.len(),
                cap = MAX_PATHS_PER_ENTRY,
                "walk_cache: entry truncated at the cap",
            );
            paths.truncate(MAX_PATHS_PER_ENTRY);
        }
        let mut g = self.entries.lock().expect("walk_cache poisoned");
        if g.len() >= MAX_ENTRIES && !g.contains_key(&key) {
            if let Some(oldest_key) = g.iter().min_by_key(|(_, e)| e.at).map(|(k, _)| k.clone()) {
                g.remove(&oldest_key);
            }
        }
        g.insert(
            key,
            Entry {
                paths: paths.clone(),
                at: Instant::now(),
            },
        );
        paths
    }

    /// Drop every entry. Called by every tool that writes to disk.
    pub fn invalidate_all(&self) {
        self.entries.lock().expect("walk_cache poisoned").clear();
    }

    /// Drop every entry whose root is a prefix of `path` (or equals
    /// it). Finer-grained than [`Self::invalidate_all`]: a write to
    /// `/work/a.rs` invalidates a walk rooted at `/work` but leaves a
    /// walk rooted at `/tmp` alone.
    pub fn invalidate_path(&self, path: &Path) {
        let mut g = self.entries.lock().expect("walk_cache poisoned");
        g.retain(|k, _| !path.starts_with(&k.root));
    }

    /// Number of entries currently cached.
    pub fn len(&self) -> usize {
        self.entries.lock().expect("walk_cache poisoned").len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Hit / miss / entry counts.
    pub fn stats(&self) -> Stats {
        let g = self.entries.lock().expect("walk_cache poisoned");
        Stats {
            hits: self.hits.load(Ordering::Relaxed),
            misses: self.misses.load(Ordering::Relaxed),
            entries: g.len(),
        }
    }
}

impl Default for WalkCache {
    fn default() -> Self {
        Self::new()
    }
}

/// The process-wide cache. A `OnceLock` so the init is one-shot and
/// lock-free on the hot path.
static GLOBAL: OnceLock<WalkCache> = OnceLock::new();

/// The process-wide cache. First call initializes it.
pub fn global() -> &'static WalkCache {
    GLOBAL.get_or_init(WalkCache::new)
}

/// Drop every entry in the process-wide cache. Every tool that
/// mutates disk calls this on success.
pub fn invalidate_all() {
    global().invalidate_all();
}

/// Drop every entry whose root is a prefix of `path`.
pub fn invalidate_path(path: &Path) {
    global().invalidate_path(path);
}

/// Rank `paths` by mtime descending and take the top `limit`. A path
/// whose mtime cannot be read is skipped — it will be re-stat'd on
/// the next call and either appears then or never did.
///
/// This is the design's `collect_ranked(mtime_desc, limit)`. Callers
/// use it to put the freshest files first in a context listing.
pub fn collect_ranked(paths: &[PathBuf], limit: usize) -> Vec<PathBuf> {
    let mut scored: Vec<(PathBuf, std::time::SystemTime)> = paths
        .iter()
        .filter_map(|p| {
            let mtime = std::fs::metadata(p).ok()?.modified().ok()?;
            Some((p.clone(), mtime))
        })
        .collect();
    scored.sort_by(|a, b| b.1.cmp(&a.1));
    scored.into_iter().take(limit).map(|(p, _)| p).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_miss_then_a_hit_runs_the_walk_once() {
        let cache = WalkCache::new();
        let calls = std::sync::atomic::AtomicU32::new(0);
        let walk = || {
            calls.fetch_add(1, Ordering::SeqCst);
            vec![PathBuf::from("/a"), PathBuf::from("/b")]
        };
        let a = cache.get_or_walk(Path::new("/r"), true, walk);
        assert_eq!(a.len(), 2);
        let b = cache.get_or_walk(Path::new("/r"), true, || {
            calls.fetch_add(1, Ordering::SeqCst);
            vec![PathBuf::from("/never-observed")]
        });
        assert_eq!(b.len(), 2, "second call must reuse the first result");
        assert_eq!(calls.load(Ordering::SeqCst), 1, "walk ran exactly once");
        assert_eq!(cache.stats().hits, 1);
        assert_eq!(cache.stats().misses, 1);
    }

    #[test]
    fn different_roots_are_different_entries() {
        let cache = WalkCache::new();
        cache.get_or_walk(Path::new("/a"), true, || vec![PathBuf::from("/a/x")]);
        cache.get_or_walk(Path::new("/b"), true, || vec![PathBuf::from("/b/y")]);
        assert_eq!(cache.len(), 2);
    }

    #[test]
    fn recursive_flag_partitions_the_key() {
        let cache = WalkCache::new();
        cache.get_or_walk(Path::new("/r"), true, || vec![PathBuf::from("/r/deep")]);
        cache.get_or_walk(Path::new("/r"), false, || vec![PathBuf::from("/r/shallow")]);
        assert_eq!(cache.len(), 2, "same root, different depth = two entries");
    }

    #[test]
    fn invalidate_all_clears_everything() {
        let cache = WalkCache::new();
        cache.get_or_walk(Path::new("/a"), true, || vec![PathBuf::from("/a/x")]);
        cache.get_or_walk(Path::new("/b"), true, || vec![PathBuf::from("/b/y")]);
        cache.invalidate_all();
        assert!(cache.is_empty());
    }

    #[test]
    fn invalidate_path_drops_only_matching_roots() {
        let cache = WalkCache::new();
        cache.get_or_walk(Path::new("/work"), true, || vec![PathBuf::from("/work/a")]);
        cache.get_or_walk(Path::new("/tmp"), true, || vec![PathBuf::from("/tmp/b")]);
        assert_eq!(cache.len(), 2);
        cache.invalidate_path(Path::new("/work/a.rs"));
        assert_eq!(cache.len(), 1, "only the /work root was dropped");
    }

    #[test]
    fn zero_ttl_means_every_read_is_a_miss() {
        let cache = WalkCache::with_ttl(Duration::ZERO);
        cache.get_or_walk(Path::new("/r"), true, || vec![PathBuf::from("/r/a")]);
        // Ensure the elapsed time is nonzero.
        std::thread::sleep(Duration::from_millis(2));
        cache.get_or_walk(Path::new("/r"), true, || vec![PathBuf::from("/r/b")]);
        let s = cache.stats();
        assert_eq!(s.misses, 2, "two walks, no hits: TTL is zero");
        assert_eq!(s.hits, 0);
    }

    #[test]
    fn collect_ranked_orders_by_mtime_desc() {
        let dir = tempfile::tempdir().unwrap();
        let old = dir.path().join("old");
        let mid = dir.path().join("mid");
        let new = dir.path().join("new");
        std::fs::write(&old, "a").unwrap();
        std::thread::sleep(Duration::from_millis(15));
        std::fs::write(&mid, "b").unwrap();
        std::thread::sleep(Duration::from_millis(15));
        std::fs::write(&new, "c").unwrap();
        let ranked = collect_ranked(&[old.clone(), mid.clone(), new.clone()], 2);
        assert_eq!(ranked.len(), 2);
        assert_eq!(ranked[0], new, "newest first");
        assert_eq!(ranked[1], mid);
    }

    #[test]
    fn collect_ranked_skips_missing_files() {
        let ranked = collect_ranked(&[PathBuf::from("/does/not/exist/really")], 10);
        assert!(ranked.is_empty());
    }

    #[test]
    fn max_entries_evicts_oldest_on_insert() {
        let cache = WalkCache::new();
        // Fill past the cap. Each entry has a distinct root, so
        // they are distinct keys.
        for i in 0..(MAX_ENTRIES + 5) {
            let root = format!("/r{i}");
            cache.get_or_walk(Path::new(&root), true, || vec![]);
        }
        assert!(
            cache.len() <= MAX_ENTRIES,
            "cache must not grow past MAX_ENTRIES; len = {}",
            cache.len(),
        );
    }
}
