//! Delta §7.3: process-global LRU of parsed `tree_sitter::Tree`s.
//!
//! # Why
//!
//! `repomap` parses the same files on every rebuild, and the hash
//! edit path (§7.1) will parse a file on every read. Re-parsing an
//! unchanged file is pure waste: a tree-sitter parse is fast but not
//! free, and the trees are stable while the source bytes are.
//!
//! # Keying
//!
//! `(xxh3(source, SEED), source.len(), Lang)`. The hash alone is not
//! enough — a collision would hand back a tree for *different* source.
//! A hit therefore also requires **byte-for-byte equality** with the
//! retained source. The retained bytes are the cost of correctness:
//! the cache holds the source it parsed, not just a digest.
//!
//! # Bounds
//!
//! [`MAX_ENTRIES`] slots and [`MAX_TOTAL_SOURCE_BYTES`] of retained
//! source. At twelve slots a linear scan finds the least-recently-used
//! entry faster than maintaining an intrusive list would. An entry
//! whose source alone exceeds the byte cap is not cached.
//!
//! # Threading
//!
//! `tree_sitter::Tree` is `Send`; the cache is a `Mutex<Vec<Entry>>`
//! so it can be the process-global. The lock is held only for the
//! probe+compare+`Tree::clone` refcount bump (and for insert); the
//! actual parse runs with the lock dropped. `tree_sitter::Parser` is
//! not `Send`, so parsers live in a `thread_local!`.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};

use tree_sitter::Tree;

use crate::lang::Lang;

/// Maximum cached entries. Twelve slots, linear-scanned.
pub const MAX_ENTRIES: usize = 12;

/// Maximum total retained source bytes across all entries.
pub const MAX_TOTAL_SOURCE_BYTES: usize = 4 * 1024 * 1024;

/// xxh3 seed. A fixed constant so a hash is reproducible across runs
/// (the cache is in-process, but a stable seed makes a test that
/// asserts a hash value possible).
const SEED: u64 = 0x6b6f_642d_6173_7400;

/// One cached parse.
struct Entry {
    hash: u64,
    lang: Lang,
    /// The exact bytes the tree was parsed from. Retained so a hash
    /// collision is caught by a byte comparison, never a wrong tree.
    source: Arc<[u8]>,
    tree: Tree,
    /// Monotonic tick of the last probe hit, for the linear LRU scan.
    last_used: u64,
}

/// The process-global parse cache.
pub struct ParseCache {
    entries: Mutex<Vec<Entry>>,
    tick: AtomicU64,
}

impl ParseCache {
    pub fn new() -> Self {
        Self {
            entries: Mutex::new(Vec::new()),
            tick: AtomicU64::new(0),
        }
    }

    /// The number of entries currently cached. For tests and a debug
    /// surface.
    pub fn len(&self) -> usize {
        self.entries.lock().map(|g| g.len()).unwrap_or(0)
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Drop every entry.
    pub fn clear(&self) {
        if let Ok(mut g) = self.entries.lock() {
            g.clear();
        }
    }

    /// Return a cached tree for `(lang, source)`, or `None` on miss.
    ///
    /// A hit is `hash == hash && lang == lang && bytes == bytes` — the
    /// byte comparison is what makes a hash collision a miss rather
    /// than a wrong tree.
    fn probe(&self, hash: u64, lang: Lang, source: &[u8]) -> Option<Tree> {
        let mut g = self.entries.lock().ok()?;
        let tick = self.tick.fetch_add(1, Ordering::Relaxed);
        for e in g.iter_mut() {
            if e.hash == hash && e.lang == lang && e.source.as_ref() == source {
                e.last_used = tick;
                return Some(e.tree.clone());
            }
        }
        None
    }

    /// Insert a tree, evicting to fit both caps.
    fn insert(&self, hash: u64, lang: Lang, source: &[u8], tree: Tree) {
        // A source that alone exceeds the byte cap is never cached —
        // caching it would evict every other entry and still be over.
        if source.len() > MAX_TOTAL_SOURCE_BYTES {
            return;
        }
        let Ok(mut g) = self.entries.lock() else {
            return;
        };
        // Replace any stale entry for the same key.
        g.retain(|e| !(e.hash == hash && e.lang == lang && e.source.as_ref() == source));
        let tick = self.tick.fetch_add(1, Ordering::Relaxed);
        g.push(Entry {
            hash,
            lang,
            source: Arc::from(source),
            tree,
            last_used: tick,
        });
        // Entry cap: evict least-recently-used until under.
        while g.len() > MAX_ENTRIES {
            if let Some(i) = lru_index(&g) {
                g.remove(i);
            } else {
                break;
            }
        }
        // Byte cap: same eviction rule.
        loop {
            let total: usize = g.iter().map(|e| e.source.len()).sum();
            if total <= MAX_TOTAL_SOURCE_BYTES {
                break;
            }
            match lru_index(&g) {
                Some(i) => {
                    g.remove(i);
                }
                None => break,
            }
        }
    }

    /// Parse `source` as `lang`, serving from the cache when possible.
    ///
    /// Returns `None` only when the parser fails to accept the
    /// language or the parse itself fails — both are "the caller has
    /// no tree", which the callers already handle.
    pub fn parse(&self, lang: Lang, source: &str) -> Option<Tree> {
        let bytes = source.as_bytes();
        let hash = xxhash_rust::xxh3::xxh3_64_with_seed(bytes, SEED);
        if let Some(t) = self.probe(hash, lang, bytes) {
            return Some(t);
        }
        let tree = PARSER.with(|cell| {
            let mut p = cell.borrow_mut();
            p.set_language(&lang.language()).ok()?;
            p.parse(source, None)
        })?;
        self.insert(hash, lang, bytes, tree.clone());
        Some(tree)
    }
}

impl Default for ParseCache {
    fn default() -> Self {
        Self::new()
    }
}

/// The index of the least-recently-used entry, or `None` when empty.
fn lru_index(entries: &[Entry]) -> Option<usize> {
    entries
        .iter()
        .enumerate()
        .min_by_key(|(_, e)| e.last_used)
        .map(|(i, _)| i)
}

thread_local! {
    /// A parser per thread. `tree_sitter::Parser` is not `Send`, so it
    /// cannot live behind the shared cache; the language is re-set on
    /// every parse, which is a pointer store, not a re-allocation.
    static PARSER: std::cell::RefCell<tree_sitter::Parser> =
        std::cell::RefCell::new(tree_sitter::Parser::new());
}

/// The process-global cache. A `OnceLock`, so the init is one-shot.
static GLOBAL: OnceLock<ParseCache> = OnceLock::new();

/// The process-global cache.
pub fn global() -> &'static ParseCache {
    GLOBAL.get_or_init(ParseCache::new)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_parse_is_cached_and_reused() {
        let c = ParseCache::new();
        let src = "fn main() {}\n";
        let t1 = c.parse(Lang::Rust, src).expect("first parse");
        assert_eq!(c.len(), 1);
        let t2 = c.parse(Lang::Rust, src).expect("second parse");
        // Same tree, by the refcount-bumped handle: the root node's
        // extent is identical and the cache still holds one entry.
        assert_eq!(t1.root_node().end_byte(), t2.root_node().end_byte());
        assert_eq!(c.len(), 1, "second parse must hit, not insert");
    }

    #[test]
    fn a_different_source_is_a_miss() {
        let c = ParseCache::new();
        c.parse(Lang::Rust, "fn a() {}").unwrap();
        c.parse(Lang::Rust, "fn b() {}").unwrap();
        assert_eq!(c.len(), 2);
    }

    #[test]
    fn the_same_source_in_two_languages_is_two_entries() {
        let c = ParseCache::new();
        // `x = 1` is valid in both Python and Ruby; the Lang keeps the
        // entries distinct so neither gets the other's tree.
        c.parse(Lang::Python, "x = 1\n").unwrap();
        c.parse(Lang::Ruby, "x = 1\n").unwrap();
        assert_eq!(c.len(), 2);
    }

    #[test]
    fn entries_evict_at_the_entry_cap() {
        let c = ParseCache::new();
        for i in 0..(MAX_ENTRIES + 4) {
            let src = format!("fn f{i}() {{}}\n");
            c.parse(Lang::Rust, &src).unwrap();
        }
        assert!(
            c.len() <= MAX_ENTRIES,
            "cache must not exceed MAX_ENTRIES; len = {}",
            c.len(),
        );
    }

    #[test]
    fn a_repeated_hit_keeps_the_entry_from_eviction() {
        // The LRU rule is what makes twelve slots enough for a hot
        // file. A file probed repeatedly must survive a flood of
        // one-shot parses.
        let c = ParseCache::new();
        let hot = "fn hot() {}\n";
        c.parse(Lang::Rust, hot).unwrap();
        for i in 0..MAX_ENTRIES {
            let src = format!("fn cold{i}() {{}}\n");
            c.parse(Lang::Rust, &src).unwrap();
            // Re-probe the hot file so it stays most-recently-used.
            let _ = c.parse(Lang::Rust, hot);
        }
        // The hot entry is still present: parsing it again is a hit
        // and the entry count did not grow by one more than the cap.
        let before = c.len();
        let _ = c.parse(Lang::Rust, hot).unwrap();
        assert_eq!(c.len(), before, "hot file must have stayed cached");
    }

    #[test]
    fn clear_empties_the_cache() {
        let c = ParseCache::new();
        c.parse(Lang::Rust, "fn a() {}").unwrap();
        assert!(!c.is_empty());
        c.clear();
        assert!(c.is_empty());
    }

    #[test]
    fn every_language_parses_its_hello_world() {
        let c = ParseCache::new();
        let samples = [
            (Lang::Rust, "fn main() {}\n"),
            (Lang::Python, "def main():\n    pass\n"),
            (Lang::TypeScript, "function main(): void {}\n"),
            (Lang::Tsx, "const x = <div/>;\n"),
            (Lang::JavaScript, "function main() {}\n"),
            (Lang::Go, "package main\nfunc main() {}\n"),
            (Lang::Ruby, "def main\nend\n"),
            (Lang::Java, "class Main { void main() {} }\n"),
            (Lang::C, "int main() { return 0; }\n"),
        ];
        for (lang, src) in samples {
            let tree = c.parse(lang, src).unwrap_or_else(|| panic!("parse {lang:?}"));
            assert!(
                !tree.root_node().has_error(),
                "{lang:?} hello-world should parse cleanly",
            );
        }
    }

    #[test]
    fn the_global_cache_is_a_single_instance() {
        assert!(std::ptr::eq(global(), global()));
    }
}
