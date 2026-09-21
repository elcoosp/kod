//! Per-frame render caches for the TUI chat widget.
//!
//! Why this exists: the chat widget runs `Paragraph::line_count` up to
//! three times per frame — once for the scrollbar probe, once for the
//! total, and once for the search skip row — each time on a freshly
//! cloned line vector. On a long session that is O(2×transcript) work
//! per streamed token. The cache here keeps the last measured row count
//! for a message and invalidates it only when one of the inputs that
//! can change the count changes: the message content or the render
//! width. (Styling — the theme — does not affect row count.)
//!
//! The cache is keyed by `MessageId`, never by index, because the
//! message list is re-sorted each frame by `sequence`.

use std::cell::RefCell;
use std::collections::hash_map::DefaultHasher;
use std::collections::{HashMap, HashSet};
use std::hash::{Hash, Hasher};

use kod_types::MessageId;

/// A cache entry: the inputs that can change a message's wrapped row
/// count, plus the row count itself and the tail-blank flag the
/// separator logic in `chat::render` needs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Entry {
    /// Hash of the message content. A collision only costs one
    /// re-measure — the cache is never a correctness dependency.
    content_hash: u64,
    /// Render width in cells.
    width: u16,
    /// The cached visual row count.
    rows: usize,
    /// True if the message's last wrapped line is blank (width 0).
    /// `chat::render` suppresses the inter-turn rule when the previous
    /// message already ended on a blank line, so the probe's row total
    /// depends on this flag.
    tail_blank: bool,
}

/// Per-frame cache of message row counts.
///
/// Typical use: [`begin_frame`](Self::begin_frame) at the top of a
/// render, then [`lookup`](Self::lookup) per message; on a miss,
/// measure and call [`insert`](Self::insert). Call
/// [`end_frame`](Self::end_frame) to evict entries for messages that
/// did not render this frame (a `/clear` or session restore should not
/// leak memory forever).
#[derive(Debug, Default)]
pub struct RenderCache {
    entries: HashMap<MessageId, Entry>,
    /// Ids seen this frame. Used by `end_frame` to evict stale
    /// entries without a second pass over the message list.
    seen: HashSet<MessageId>,
}

impl RenderCache {
    pub fn new() -> Self {
        Self::default()
    }

    /// Mark the start of a frame. Cheap: clears the `seen` set.
    pub fn begin_frame(&mut self) {
        self.seen.clear();
    }

    /// Look up a cached `(rows, tail_blank)` pair. Returns `None` on a
    /// miss or if the content or width changed since the entry was
    /// written.
    pub fn lookup(
        &mut self,
        id: &MessageId,
        content_hash: u64,
        width: u16,
    ) -> Option<(usize, bool)> {
        self.seen.insert(id.clone());
        match self.entries.get(id) {
            Some(e) if e.content_hash == content_hash && e.width == width => {
                Some((e.rows, e.tail_blank))
            }
            _ => None,
        }
    }

    /// Store a measured row count. Call after a `lookup` miss.
    pub fn insert(
        &mut self,
        id: MessageId,
        content_hash: u64,
        width: u16,
        rows: usize,
        tail_blank: bool,
    ) {
        self.seen.insert(id.clone());
        self.entries.insert(
            id,
            Entry {
                content_hash,
                width,
                rows,
                tail_blank,
            },
        );
    }

    /// Evict entries for messages that did not render this frame.
    /// Keeps the cache from growing without bound across a long
    /// session with many `/clear`s.
    pub fn end_frame(&mut self) {
        if self.entries.len() <= self.seen.len() {
            return;
        }
        self.entries.retain(|id, _| self.seen.contains(id));
    }

    /// Current number of cached entries. For tests and diagnostics.
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

/// Stable content hash used by the cache. `DefaultHasher` is fine
/// here: the value is never persisted, never compared across
/// processes, and a collision only costs a re-measure.
pub fn hash_content(s: &str) -> u64 {
    let mut h = DefaultHasher::new();
    s.hash(&mut h);
    h.finish()
}

thread_local! {
    /// The process-wide render cache. The TUI runs on a single thread,
    /// so a thread-local avoids threading `&mut RenderCache` through
    /// every widget signature. Tests on other threads get their own.
    static RENDER_CACHE: RefCell<RenderCache> = RefCell::new(RenderCache::new());
}

/// Run `f` with mutable access to the current thread's render cache.
/// Panics if `f` re-enters `with_cache` (the `RefCell` would already
/// be borrowed); callers must not nest.
pub fn with_cache<R>(f: impl FnOnce(&mut RenderCache) -> R) -> R {
    RENDER_CACHE.with(|c| f(&mut c.borrow_mut()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hash_content_is_stable() {
        assert_eq!(hash_content("hello"), hash_content("hello"));
        assert_ne!(hash_content("hello"), hash_content("world"));
    }

    #[test]
    fn empty_cache_is_empty() {
        let c = RenderCache::new();
        assert!(c.is_empty());
        assert_eq!(c.len(), 0);
    }

    #[test]
    fn lookup_after_insert_hits() {
        let mut c = RenderCache::new();
        let id = MessageId::new();
        c.begin_frame();
        assert!(c.lookup(&id, 42, 80).is_none());
        c.insert(id.clone(), 42, 80, 3, false);
        assert_eq!(c.lookup(&id, 42, 80), Some((3, false)));
        // Different width = miss.
        assert!(c.lookup(&id, 42, 81).is_none());
        // Different content hash = miss.
        assert!(c.lookup(&id, 43, 80).is_none());
    }

    #[test]
    fn end_frame_evicts_unseen() {
        let mut c = RenderCache::new();
        let a = MessageId::new();
        let b = MessageId::new();
        c.begin_frame();
        c.insert(a.clone(), 1, 80, 1, false);
        c.insert(b.clone(), 1, 80, 1, false);
        c.end_frame();
        assert_eq!(c.len(), 2);

        c.begin_frame();
        // Only touch `a` this frame.
        let _ = c.lookup(&a, 1, 80);
        c.end_frame();
        assert_eq!(c.len(), 1);
    }

    #[test]
    fn tail_blank_round_trips() {
        let mut c = RenderCache::new();
        let id = MessageId::new();
        c.begin_frame();
        c.insert(id.clone(), 1, 80, 2, true);
        assert_eq!(c.lookup(&id, 1, 80), Some((2, true)));
    }
}
