//! Memory manager - unified interface for all memory types.
//!
//! Coordinates short-term and long-term memory
//! to provide a single API for storing and retrieving context.

use crate::{long_term::LongTermMemory, short_term::ShortTermMemory};
use kod_error::{KodError, Result};
use kod_types::{MemoryContext, MemoryEntry, MemoryId, MemoryType};
use std::path::PathBuf;
use time::OffsetDateTime;

/// Episodic entries whose last touch (write or retrieval) is older
/// than this many days are archived by [`MemoryManager::consolidate`].
/// A durable `LongTerm` fact is never archived — only auto-extracted
/// `Episodic` entries age out. 60 days matches the design's D2.5.
pub const ARCHIVE_AFTER_DAYS: u32 = 60;

/// How many of the most recent long-term entries are examined for
/// near-duplicate fusion in one [`MemoryManager::consolidate`] pass.
/// The comparison is O(N²) in this window; 500 keeps it at ~125k
/// cosines — a fraction of a second even without SIMD — while still
/// covering the paraphrase-in-the-same-session case the fusion is
/// for.
const FUSE_WINDOW: usize = 500;

/// Cosine above which two entries are considered the same fact.
/// 0.95 is the design's threshold (D2.5). Paraphrases of a short
/// sentence typically land in 0.92–0.98; distinct facts on the same
/// topic usually stay below 0.9.
const FUSE_COSINE_THRESHOLD: f32 = 0.95;

/// Result of a [`MemoryManager::compact`] call: how many entries
/// were dropped from each layer. A single struct rather than a tuple
/// because more layers may grow a compaction path later.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct CompactionReport {
    /// Entries dropped from short-term memory.
    pub short_term_removed: usize,
}

/// Result of a [`MemoryManager::consolidate`] pass.
///
/// Two independent operations run in one call:
///
/// - **Archival**: episodic entries whose last touch is older than
///   `ARCHIVE_AFTER_DAYS` are dropped. This runs with or without an
///   embedder — an episode written months ago and never read again is
///   dropped on a fresh session with no model.
/// - **Fusion**: near-duplicate entries (cosine > 0.95) within the
///   same project are merged into a single survivor. This needs an
///   embedder; without one, `fused` is `0` and every duplicate stays
///   in place.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ConsolidationReport {
    /// Episodic entries archived (removed from the persistent store).
    pub archived: usize,
    /// Entries removed by near-duplicate fusion. `0` when no embedder
    /// is installed, or when no cluster exceeded the threshold.
    pub fused: usize,
}

/// Unified memory manager
pub struct MemoryManager {
    short_term: ShortTermMemory,
    long_term: LongTermMemory,
    context_window: usize,
    /// Optional embedding client (D2-B2). When `None` (or `dims()==0`),
    /// retrieval falls back to keyword + recency. Installed via
    /// `set_embedder` after construction; the CLI/TUI decide which
    /// endpoint to use.
    embedder: Option<std::sync::Arc<dyn crate::embedding::EmbeddingClient>>,
    /// In-memory vector index. Built lazily on the first retrieval
    /// that wants semantic scoring. Rebuilt from scratch when the
    /// embedder is swapped.
    vector_index: parking_lot::RwLock<Option<crate::vector_index::VectorIndex>>,
    /// Tunables for the hybrid scorer.
    scorer: crate::retrieval::HybridScorer,
}

impl MemoryManager {
    /// Create a new memory manager
    pub fn new(db_path: PathBuf, short_term_capacity: usize) -> Result<Self> {
        let long_term = LongTermMemory::new(&db_path)?;

        Ok(Self {
            short_term: ShortTermMemory::new(short_term_capacity),
            long_term,
            context_window: 4096, // Default context window
            embedder: None,
            vector_index: parking_lot::RwLock::new(None),
            scorer: crate::retrieval::HybridScorer::default(),
        })
    }

    /// Set the context window size (in tokens)
    pub fn set_context_window(&mut self, tokens: usize) {
        self.context_window = tokens;
    }

    /// Install an embedding client (D2-B2). A second call replaces the
    /// first and invalidates any cached vector index — vectors from a
    /// different model are incomparable.
    pub fn set_embedder(
        &mut self,
        embedder: std::sync::Arc<dyn crate::embedding::EmbeddingClient>,
    ) {
        self.embedder = Some(embedder);
        *self.vector_index.write() = None;
    }

    /// The installed embedder's name, if any. `"none"` when none is
    /// installed.
    pub fn embedder_name(&self) -> &'static str {
        match &self.embedder {
            Some(e) if e.dims() > 0 => "configured",
            Some(_) => "unusable",
            None => "none",
        }
    }

    /// Replace the retrieval scoring weights.
    pub fn set_scorer(&mut self, scorer: crate::retrieval::HybridScorer) {
        self.scorer = scorer;
    }

    /// Rebuild the vector index from the long-term store, using the
    /// installed embedder. Best-effort: entries without a stored
    /// embedding are skipped (embedding them is B3's job — the write
    /// path). A missing embedder leaves the index empty.
    ///
    /// Called lazily from `retrieve_context` when semantic scoring is
    /// requested and the index has not been built yet.
    async fn rebuild_index(&self) -> Result<()> {
        let Some(embedder) = self.embedder.as_ref() else {
            *self.vector_index.write() = None;
            return Ok(());
        };
        let dim = embedder.dims();
        if dim == 0 {
            *self.vector_index.write() = None;
            return Ok(());
        }
        let all = self.long_term.get_all().await?;
        let mut idx = crate::vector_index::VectorIndex::new(dim);
        for entry in &all {
            if let Some(v) = &entry.metadata.embedding
                && v.len() == dim
            {
                // Ignore insert errors: a malformed embedding is
                // skipped rather than aborting the rebuild.
                let _ = idx.insert(entry.id.clone(), v.clone());
            }
        }
        *self.vector_index.write() = Some(idx);
        Ok(())
    }

    /// Store content in the specified memory type
    pub async fn store(&self, memory_type: MemoryType, content: &str) -> Result<MemoryId> {
        self.store_with_metadata(memory_type, content, Default::default())
            .await
    }

    /// Store content with caller-supplied metadata (D2-B3a). Used by
    /// the memory tools to attach tags and the project_key the hybrid
    /// retrieval filters on. `store()` is the no-metadata shorthand.
    pub async fn store_with_metadata(
        &self,
        memory_type: MemoryType,
        content: &str,
        metadata: kod_types::MemoryMetadata,
    ) -> Result<MemoryId> {
        let id = MemoryId::new();

        match memory_type {
            MemoryType::ShortTerm => {
                let entry = MemoryEntry {
                    id: id.clone(),
                    memory_type,
                    content: content.to_string(),
                    timestamp: OffsetDateTime::now_utc(),
                    relevance: 1.0,
                    metadata,
                };
                self.short_term.store(entry);
                // Compact down to 80% once the cap is reached.
                let cap = self.short_term.capacity();
                if cap > 0 && self.short_term.len() >= cap {
                    let target = cap * 4 / 5;
                    self.compact(target);
                }
            }
            MemoryType::LongTerm => {
                let entry = MemoryEntry {
                    id: id.clone(),
                    memory_type,
                    content: content.to_string(),
                    timestamp: OffsetDateTime::now_utc(),
                    relevance: 0.8,
                    metadata,
                };
                self.long_term.store(entry).await?;
            }
            MemoryType::Episodic => {
                let entry = MemoryEntry {
                    id: id.clone(),
                    memory_type,
                    content: content.to_string(),
                    timestamp: OffsetDateTime::now_utc(),
                    relevance: 0.7,
                    metadata,
                };
                self.long_term.store(entry).await?;
            }
        }

        Ok(id)
    }

    /// Get a short-term memory entry
    pub fn get_short_term(&self, id: &MemoryId) -> Option<MemoryEntry> {
        self.short_term.get(id)
    }

    /// Get a long-term memory entry
    pub async fn get_long_term(&self, id: &MemoryId) -> Result<Option<MemoryEntry>> {
        self.long_term.get(id).await
    }

    /// Update memory content
    pub async fn update(
        &self,
        memory_type: MemoryType,
        id: &MemoryId,
        content: &str,
    ) -> Result<()> {
        match memory_type {
            MemoryType::ShortTerm => {
                // Short-term memory doesn't support update, so remove and re-add
                if let Some(mut entry) = self.short_term.get(id) {
                    entry.content = content.to_string();
                    self.short_term.store(entry);
                    Ok(())
                } else {
                    Err(KodError::MemoryStorage("Entry not found".to_string()))
                }
            }
            MemoryType::LongTerm | MemoryType::Episodic => {
                if let Some(mut entry) = self.long_term.get(id).await? {
                    entry.content = content.to_string();
                    self.long_term.store(entry).await?;
                    Ok(())
                } else {
                    Err(KodError::MemoryStorage("Entry not found".to_string()))
                }
            }
        }
    }

    /// Remove memory entry
    pub async fn remove(&self, memory_type: MemoryType, id: &MemoryId) -> Result<()> {
        match memory_type {
            MemoryType::ShortTerm => {
                self.short_term.remove(id);
                Ok(())
            }
            MemoryType::LongTerm | MemoryType::Episodic => self.long_term.remove(id).await,
        }
    }

    /// Search across all memory types
    pub async fn search(&self, query: &str) -> Result<Vec<MemoryEntry>> {
        let mut results = Vec::new();

        // Search short-term
        results.extend(self.short_term.search(query));

        // Search long-term
        results.extend(self.long_term.search(query).await?);

        // Sort by relevance
        results.sort_by(|a, b| {
            b.relevance
                .partial_cmp(&a.relevance)
                .unwrap_or(std::cmp::Ordering::Equal)
        });

        Ok(results)
    }

    /// Retrieve context for a query (D2-B2 hybrid scoring).
    ///
    /// Short-term memory: recent N (unchanged).
    /// Long-term memory: top-20 by the hybrid scorer. Semantic
    /// component is cosine against the query embedding when an
    /// embedder is installed; the keyword + recency components always
    /// contribute. Without an embedder, weight is redistributed to
    /// keyword and the behaviour reduces to a weighted keyword search
    /// — the pre-D2 result, with stopwords and stemming.
    pub async fn retrieve_context(&self, query: &str) -> Result<MemoryContext> {
        let working_memory = self.short_term.get_recent(10);
        let long_term = self.retrieve_long_term_hybrid(query).await?;
        let mut context = MemoryContext {
            working_memory,
            long_term,
            ..Default::default()
        };
        self.limit_context_size(&mut context);
        Ok(context)
    }

    /// The hybrid retrieval itself, exposed for tests and for a caller
    /// that wants the top-k list without wrapping it in a
    /// `MemoryContext`.
    pub async fn retrieve_long_term_hybrid(&self, query: &str) -> Result<Vec<MemoryEntry>> {
        let all = self.long_term.get_all().await?;
        if all.is_empty() {
            return Ok(Vec::new());
        }
        let query_terms = crate::retrieval::QueryTerms::build(query, &all);

        // Ensure the vector index is populated when an embedder is
        // installed. `None` means "no semantic component".
        let semantic_available = self
            .embedder
            .as_ref()
            .map(|e| e.dims() > 0)
            .unwrap_or(false);
        if semantic_available && self.vector_index.read().is_none() {
            // Best-effort rebuild: a failure here just means the
            // hybrid falls back to keyword+recency for this call.
            if let Err(e) = self.rebuild_index().await {
                tracing::warn!(error = %e, "vector index rebuild failed; semantic scoring disabled for this call");
            }
        }

        // Compute cosine per entry when the index is available and the
        // embedder can produce a query vector. The semantic path is a
        // single embed call for the query, then a top-k search.
        let cosines: std::collections::HashMap<MemoryId, f32> = if semantic_available {
            let embedder = self.embedder.as_ref().unwrap();
            match embedder
                .embed(std::slice::from_ref(&query.to_string()))
                .await
            {
                Ok(mut v) if !v.is_empty() => {
                    let q_vec = v.remove(0);
                    let idx_guard = self.vector_index.read();
                    match idx_guard.as_ref() {
                        Some(idx) => {
                            let hits = idx.search(&q_vec, 200, |_| true).unwrap_or_default();
                            hits.into_iter().collect()
                        }
                        None => Default::default(),
                    }
                }
                Ok(_) => Default::default(),
                Err(e) => {
                    tracing::warn!(error = %e, "query embedding failed; keyword+recency only");
                    Default::default()
                }
            }
        } else {
            Default::default()
        };

        let now = time::OffsetDateTime::now_utc();
        let mut scored: Vec<(f32, MemoryEntry)> = all
            .into_iter()
            .map(|entry| {
                let cos = cosines.get(&entry.id).copied();
                let s = self.scorer.score(&query_terms, &entry, cos, now);
                (s, entry)
            })
            .collect();
        scored.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap_or(std::cmp::Ordering::Equal));

        // Apply a minimum relevance threshold so an unrelated query
        // does not return the entire store in a stable-but-meaningless
        // order. 0.15 lets recency+keyword-only entries through but
        // filters obvious noise.
        const MIN_SCORE: f32 = 0.15;
        let top: Vec<MemoryEntry> = scored
            .into_iter()
            .filter(|(s, _)| *s >= MIN_SCORE)
            .take(20)
            .map(|(_, e)| e)
            .collect();

        // Write back `last_retrieved_at_ms` (design D2.5). Best-effort
        // and batched: one redb write transaction for the top-k, on the
        // retrieval hot path. A failure is logged and the results are
        // returned unchanged — the writeback is an archival heuristic,
        // not a correctness requirement.
        if !top.is_empty() {
            let now_ms = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_millis() as u64)
                .unwrap_or(0);
            let mut updated: Vec<MemoryEntry> = Vec::with_capacity(top.len());
            for e in &top {
                let mut e = e.clone();
                e.metadata.last_retrieved_at_ms = Some(now_ms);
                updated.push(e);
            }
            if let Err(err) = self.long_term.store_batch(updated).await {
                tracing::debug!(
                    error = %err,
                    "could not persist last_retrieved_at_ms; consolidation                      will fall back to the entry timestamp",
                );
            }
        }

        Ok(top)
    }

    /// Archive episodic entries that have not been touched in
    /// [`ARCHIVE_AFTER_DAYS`]. Returns a count of what was removed and
    /// (a follow-up) how many duplicate pairs were fused.
    ///
    /// "Touched" means: written, or returned by a retrieval since the
    /// `last_retrieved_at_ms` writeback landed. A `LongTerm` entry is
    /// never archived — it is a durable fact the user asked to
    /// remember; only `Episodic` entries (auto-extracted at end of
    /// session) are age-eligible.
    ///
    /// Duplicate fusion (cosine > 0.95) needs an embedder; without one
    /// the pass reports `fused: 0` and leaves every pair in place. The
    /// archival half runs regardless.
    pub async fn consolidate(&self) -> Result<ConsolidationReport> {
        let all = self.long_term.get_all().await?;
        if all.is_empty() {
            return Ok(ConsolidationReport::default());
        }

        // --- Pass 1: archive old episodic entries. ---
        let now = OffsetDateTime::now_utc();
        let cutoff = now - time::Duration::days(ARCHIVE_AFTER_DAYS as i64);
        let mut archived = 0usize;
        let mut remaining: Vec<MemoryEntry> = Vec::with_capacity(all.len());
        for entry in all {
            if entry.memory_type == MemoryType::Episodic {
                let last_touch = entry
                    .metadata
                    .last_retrieved_at_ms
                    .and_then(|ms| OffsetDateTime::from_unix_timestamp((ms / 1000) as i64).ok())
                    .unwrap_or(entry.timestamp);
                if last_touch < cutoff && self.long_term.remove(&entry.id).await.is_ok() {
                    archived += 1;
                    continue;
                }
            }
            remaining.push(entry);
        }

        // --- Pass 2: fuse near-duplicates. ---
        let fused = self.fuse_duplicates(&remaining).await?;

        Ok(ConsolidationReport { archived, fused })
    }

    /// Fuse near-duplicate long-term entries (design D2.5).
    ///
    /// Considers the most recent `FUSE_WINDOW` entries per project,
    /// embeds them in one batch, and unions any pair whose cosine
    /// exceeds `FUSE_COSINE_THRESHOLD`. Each cluster keeps one entry
    /// (the most recently touched); the rest are deleted and their
    /// tags are merged into the survivor.
    ///
    /// Returns the number of entries deleted by fusion. `0` when no
    /// embedder is installed, or when no cluster exceeded the
    /// threshold — the design's honest report, not a guess.
    async fn fuse_duplicates(&self, entries: &[MemoryEntry]) -> Result<usize> {
        let Some(embedder) = self.embedder.as_ref() else {
            return Ok(0);
        };
        let dim = embedder.dims();
        if dim == 0 {
            return Ok(0);
        }

        // Group by project_key so a fact from project A never fuses
        // with a fact from project B. `None` is its own group (entries
        // written before the field existed, or by a caller that
        // deliberately left it empty).
        let mut by_project: std::collections::HashMap<Option<String>, Vec<&MemoryEntry>> =
            std::collections::HashMap::new();
        for e in entries {
            by_project
                .entry(e.metadata.project_key.clone())
                .or_default()
                .push(e);
        }

        let mut to_delete: Vec<MemoryId> = Vec::new();
        let mut tags_to_merge: std::collections::HashMap<MemoryId, Vec<String>> =
            std::collections::HashMap::new();

        for (_key, mut group) in by_project {
            if group.len() < 2 {
                continue;
            }
            // Sort by most recent touch descending, then truncate.
            group.sort_by(|a, b| {
                let ta = a
                    .metadata
                    .last_retrieved_at_ms
                    .unwrap_or_else(|| (a.timestamp.unix_timestamp() * 1000) as u64);
                let tb = b
                    .metadata
                    .last_retrieved_at_ms
                    .unwrap_or_else(|| (b.timestamp.unix_timestamp() * 1000) as u64);
                tb.cmp(&ta)
            });
            group.truncate(FUSE_WINDOW);

            // Embed the group in one batch. Any embed error skips
            // fusion for this project — the archive half already ran,
            // so the pass is not lost.
            let texts: Vec<String> = group.iter().map(|e| e.content.clone()).collect();
            let vectors = match embedder.embed(&texts).await {
                Ok(v) => v,
                Err(e) => {
                    tracing::warn!(
                        error = %e,
                        count = texts.len(),
                        "fuse_duplicates: embed failed; skipping project",
                    );
                    continue;
                }
            };
            if vectors.len() != group.len() {
                tracing::warn!(
                    got = vectors.len(),
                    expected = group.len(),
                    "fuse_duplicates: embed returned wrong count; skipping project",
                );
                continue;
            }

            // Normalize each vector so cosine == dot product.
            let normed: Vec<Vec<f32>> = vectors
                .into_iter()
                .map(|mut v| {
                    let n: f32 = v.iter().map(|x| x * x).sum::<f32>().sqrt();
                    if n > 0.0 {
                        for x in &mut v {
                            *x /= n;
                        }
                    }
                    v
                })
                .collect();

            // Union-find over "cosine > threshold".
            let n = group.len();
            let mut parent: Vec<usize> = (0..n).collect();
            fn find(p: &mut [usize], mut i: usize) -> usize {
                while p[i] != i {
                    p[i] = p[p[i]];
                    i = p[i];
                }
                i
            }
            for i in 0..n {
                for j in (i + 1)..n {
                    let cos: f32 = normed[i]
                        .iter()
                        .zip(normed[j].iter())
                        .map(|(a, b)| a * b)
                        .sum();
                    if cos >= FUSE_COSINE_THRESHOLD {
                        let ri = find(&mut parent, i);
                        let rj = find(&mut parent, j);
                        if ri != rj {
                            parent[rj] = ri;
                        }
                    }
                }
            }

            // Group indices by root, keep the first (most recent) of
            // each cluster, delete the rest.
            let mut clusters: std::collections::HashMap<usize, Vec<usize>> =
                std::collections::HashMap::new();
            for i in 0..n {
                let r = find(&mut parent, i);
                clusters.entry(r).or_default().push(i);
            }
            for (_root, indices) in clusters {
                if indices.len() < 2 {
                    continue;
                }
                let survivor = group[indices[0]];
                // Union tags of the cluster into the survivor.
                let mut union_tags: Vec<String> = survivor.metadata.tags.clone();
                for &idx in &indices[1..] {
                    for t in &group[idx].metadata.tags {
                        if !union_tags.contains(t) {
                            union_tags.push(t.clone());
                        }
                    }
                    to_delete.push(group[idx].id.clone());
                }
                tags_to_merge.insert(survivor.id.clone(), union_tags);
            }
        }

        if to_delete.is_empty() {
            return Ok(0);
        }

        // Persist the survivor tag unions first so a crash between
        // the two writes does not lose them.
        if !tags_to_merge.is_empty() {
            let mut updated: Vec<MemoryEntry> = Vec::new();
            for e in entries {
                if let Some(tags) = tags_to_merge.get(&e.id) {
                    let mut e = e.clone();
                    e.metadata.tags = tags.clone();
                    updated.push(e);
                }
            }
            if !updated.is_empty()
                && let Err(err) = self.long_term.store_batch(updated).await
            {
                tracing::warn!(
                    error = %err,
                    "fuse_duplicates: could not persist merged tags;                      skipping deletion to keep metadata consistent",
                );
                return Ok(0);
            }
        }

        let mut actually_deleted = 0usize;
        for id in &to_delete {
            if self.long_term.remove(id).await.is_ok() {
                actually_deleted += 1;
            }
        }
        Ok(actually_deleted)
    }

    /// Proactively trim short-term memory to `target` entries,
    /// leaving headroom below capacity. Returns how many entries were
    /// dropped.
    ///
    /// `store` already evicts the oldest entry when capacity is
    /// exceeded; this method is for callers (and for `store` itself,
    /// see the short-term branch) that want to compact further ahead
    /// of the next write so the working set sits below the boundary.
    /// `target` is clamped to the capacity.
    pub fn compact(&self, target: usize) -> CompactionReport {
        let removed = self.short_term.retain_recent(target);
        CompactionReport {
            short_term_removed: removed,
        }
    }

    /// Get all short-term memories
    pub fn get_all_short_term(&self) -> Vec<MemoryEntry> {
        self.short_term.get_all()
    }

    /// Get all long-term memories
    pub async fn get_all_long_term(&self) -> Result<Vec<MemoryEntry>> {
        self.long_term.get_all().await
    }

    /// Explicitly close the underlying long-term database.
    ///
    /// Consumes the manager: the caller (typically `KodEngine::shutdown`)
    /// has decided the session is over and no further reads or writes
    /// should be possible. Dropping the manager here releases its own
    /// `LongTermMemory` handle; redb's `Database` fsyncs and closes
    /// when the last reference drops (see `LongTermMemory::close`).
    ///
    /// Idempotency is by construction. A caller that needs to reuse
    /// the store after a shutdown creates a fresh manager.
    pub fn close(self) {
        // Bind to a local and drop explicitly so the intent is obvious
        // even if a future field is added: every owned subsystem is
        // released on this line.
        let Self {
            short_term,
            long_term,
            ..
        } = self;
        drop(short_term);
        long_term.close();
    }

    /// Clear all memories
    pub async fn clear_all(&self) -> Result<()> {
        self.short_term.clear();
        self.long_term.clear().await?;
        Ok(())
    }

    /// Clear short-term memory only
    pub fn clear_short_term(&self) {
        self.short_term.clear();
    }

    /// Limit context size to fit within context window
    fn limit_context_size(&self, context: &mut MemoryContext) {
        // Rough estimation: 1 token ≈ 4 characters
        let max_chars = self.context_window * 4;
        let mut total_chars = 0;

        // Limit working memory
        let mut working = Vec::new();
        for entry in context.working_memory.drain(..) {
            total_chars += entry.content.len();
            if total_chars <= max_chars {
                working.push(entry);
            } else {
                break;
            }
        }
        context.working_memory = working;

        // Limit long-term
        let mut long_term = Vec::new();
        for entry in context.long_term.drain(..) {
            total_chars += entry.content.len();
            if total_chars <= max_chars {
                long_term.push(entry);
            } else {
                break;
            }
        }
        context.long_term = long_term;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    /// `retrieve_context` on a natural-language prompt must pull a
    /// long-term fact that shares content words with it. Regression:
    /// the previous whole-query substring search matched only when the
    /// user's entire prompt appeared verbatim inside a memory entry —
    /// which never happened in practice, so long-term memory
    /// contributed nothing to any prompt.
    #[tokio::test]
    async fn test_retrieve_context_matches_by_word_overlap() {
        let temp_dir = TempDir::new().unwrap();
        let db_path = temp_dir.path().join("test.redb");
        let manager = MemoryManager::new(db_path, 100).unwrap();

        manager
            .store(MemoryType::LongTerm, "The user's project is called KOD.")
            .await
            .unwrap();
        manager
            .store(MemoryType::LongTerm, "The user prefers dark mode.")
            .await
            .unwrap();
        manager
            .store(MemoryType::LongTerm, "The build uses Cargo and rustc.")
            .await
            .unwrap();

        // The prompt shares "project" with the first fact. Under the
        // hybrid scorer (D2-B2) a single overlap gives a modest score
        // but the entry should still make the top-k.
        let ctx = manager
            .retrieve_context("tell me about the project")
            .await
            .unwrap();
        assert!(
            ctx.long_term.iter().any(|e| e.content.contains("KOD")),
            "expected the KOD fact in the results: {:?}",
            ctx.long_term.iter().map(|e| &e.content).collect::<Vec<_>>()
        );

        // A query with hits across two facts: both should appear.
        let ctx = manager
            .retrieve_context("does the project use dark mode?")
            .await
            .unwrap();
        assert!(
            ctx.long_term
                .iter()
                .any(|e| e.content.contains("dark mode")),
            "expected the dark-mode fact: {:?}",
            ctx.long_term.iter().map(|e| &e.content).collect::<Vec<_>>()
        );

        // A query with no content-word overlap returns nothing past
        // the min-score threshold.
        let ctx = manager.retrieve_context("xyzzy plugh").await.unwrap();
        assert!(
            ctx.long_term.is_empty(),
            "unrelated query should return nothing: {:?}",
            ctx.long_term.iter().map(|e| &e.content).collect::<Vec<_>>()
        );
    }

    /// Short words (<= 3 chars) are filtered out, so a prompt made
    /// entirely of stopwords returns no long-term memory rather than
    /// matching every entry.
    #[tokio::test]
    async fn test_retrieve_context_ignores_short_words() {
        let temp_dir = TempDir::new().unwrap();
        let db_path = temp_dir.path().join("test.redb");
        let manager = MemoryManager::new(db_path, 100).unwrap();

        manager
            .store(MemoryType::LongTerm, "Any old fact.")
            .await
            .unwrap();

        let ctx = manager.retrieve_context("the and you").await.unwrap();
        assert!(
            ctx.long_term.is_empty(),
            "short words must not match: {:?}",
            ctx.long_term.iter().map(|e| &e.content).collect::<Vec<_>>()
        );
    }

    #[test]
    fn test_compact_reports_zero_on_empty() {
        let temp_dir = TempDir::new().unwrap();
        let db_path = temp_dir.path().join("test.redb");
        let manager = MemoryManager::new(db_path, 10).unwrap();
        let report = manager.compact(5);
        assert_eq!(report.short_term_removed, 0);
        assert_eq!(manager.get_all_short_term().len(), 0);
    }

    #[tokio::test]
    async fn test_store_auto_compacts_at_capacity() {
        let temp_dir = TempDir::new().unwrap();
        let db_path = temp_dir.path().join("test.redb");
        let manager = MemoryManager::new(db_path, 10).unwrap();

        // Fill to capacity (10) and add one more. The store path
        // compacts to 80% (8) the moment capacity is reached, then the
        // extra store lands, giving 9.
        for i in 0..11 {
            manager
                .store(MemoryType::ShortTerm, &format!("turn {i}"))
                .await
                .unwrap();
        }
        let len = manager.get_all_short_term().len();
        assert!(len <= 10, "short-term must not exceed capacity, got {len}");
        assert!(
            len < 10,
            "store should have compacted below capacity, got {len}"
        );
    }

    #[tokio::test]
    async fn test_compact_trims_short_term_and_reports() {
        let temp_dir = TempDir::new().unwrap();
        let db_path = temp_dir.path().join("test.redb");
        let manager = MemoryManager::new(db_path, 20).unwrap();
        for i in 0..20 {
            manager
                .store(MemoryType::ShortTerm, &format!("entry {i}"))
                .await
                .unwrap();
        }

        // Direct compaction to 5.
        let report = manager.compact(5);
        assert!(
            report.short_term_removed > 0,
            "compaction from >5 to 5 must report removals, got {report:?}"
        );
        assert_eq!(manager.get_all_short_term().len(), 5);

        // The retained entries are the newest: entry 15 through 19.
        let all = manager.get_all_short_term();
        assert_eq!(all[0].content, "entry 15");
        assert_eq!(all[4].content, "entry 19");
    }

    #[tokio::test]
    async fn test_manager_basic_operations() {
        let temp_dir = TempDir::new().unwrap();
        let db_path = temp_dir.path().join("test.redb");

        let manager = MemoryManager::new(db_path, 10).unwrap();

        // Store in different types
        let short_id = manager
            .store(MemoryType::ShortTerm, "Short term")
            .await
            .unwrap();
        let long_id = manager
            .store(MemoryType::LongTerm, "Long term")
            .await
            .unwrap();

        // Retrieve
        assert!(manager.get_short_term(&short_id).is_some());
        assert!(manager.get_long_term(&long_id).await.unwrap().is_some());
    }
}

#[cfg(test)]
mod coverage_report_types {
    //! `CompactionReport` and `ConsolidationReport` are the two
    //! shapes a caller inspects after a maintenance pass. A
    //! regression in their `Default` or `PartialEq` would make
    //! `assert_eq!(r, Default::default())` lie, and a caller
    //! testing "nothing was pruned" would pass while the pass
    //! silently pruned everything.
    use super::*;

    #[test]
    fn compaction_report_default_is_zero() {
        let r = CompactionReport::default();
        assert_eq!(r.short_term_removed, 0);
    }

    #[test]
    fn consolidation_report_default_is_zero() {
        let r = ConsolidationReport::default();
        assert_eq!(r.archived, 0);
        assert_eq!(r.fused, 0);
    }

    #[test]
    fn reports_are_copy_and_comparable() {
        // `Copy` lets a caller pass a report by value multiple
        // times; `PartialEq` lets a test compare two runs.
        let a = CompactionReport {
            short_term_removed: 3,
        };
        let b = a;
        assert_eq!(a, b);
        let a = ConsolidationReport {
            archived: 2,
            fused: 1,
        };
        let b = a;
        assert_eq!(a, b);
    }

    #[tokio::test]
    async fn compact_on_a_fresh_manager_is_a_no_op_with_a_zero_report() {
        let tmp = tempfile::TempDir::new().unwrap();
        let m = MemoryManager::new(tmp.path().join("t.redb"), 10).unwrap();
        let r = m.compact(5);
        assert_eq!(r, CompactionReport::default());
    }

    #[tokio::test]
    async fn consolidate_on_a_fresh_store_is_a_no_op_with_a_zero_report() {
        let tmp = tempfile::TempDir::new().unwrap();
        let m = MemoryManager::new(tmp.path().join("t.redb"), 10).unwrap();
        let r = m.consolidate().await.unwrap();
        assert_eq!(r, ConsolidationReport::default());
    }

    #[test]
    fn archive_threshold_is_the_documented_sixty_days() {
        // The consolidation pass archives episodic entries older
        // than this. A regression that changed the constant would
        // silently shift the retention policy.
        assert_eq!(ARCHIVE_AFTER_DAYS, 60);
    }

    #[test]
    fn embedding_aware_manager_reports_its_embedder_name() {
        // A manager without an embedder reports "none"; the
        // accessor's contract is "name the effective state, never
        // panic on the absent case".
        let tmp = tempfile::TempDir::new().unwrap();
        let m = MemoryManager::new(tmp.path().join("t.redb"), 10).unwrap();
        assert_eq!(m.embedder_name(), "none");
    }
}
