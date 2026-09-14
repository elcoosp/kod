//! Memory manager - unified interface for all memory types.
//!
//! Coordinates short-term, long-term, and episodic memory
//! to provide a single API for storing and retrieving context.

use crate::{episodic::EpisodicMemory, long_term::LongTermMemory, short_term::ShortTermMemory};
use kod_error::{KodError, Result};
use kod_types::{
    EpisodicMemory as EpisodicMemoryType, MemoryContext, MemoryEntry, MemoryId, MemoryType, Outcome,
};
use std::path::PathBuf;
use time::OffsetDateTime;

/// Unified memory manager
pub struct MemoryManager {
    short_term: ShortTermMemory,
    long_term: LongTermMemory,
    episodic: EpisodicMemory,
    context_window: usize,
}

impl MemoryManager {
    /// Create a new memory manager
    pub fn new(db_path: PathBuf, short_term_capacity: usize) -> Result<Self> {
        let long_term = LongTermMemory::new(&db_path)?;

        Ok(Self {
            short_term: ShortTermMemory::new(short_term_capacity),
            long_term,
            episodic: EpisodicMemory::new(),
            context_window: 4096, // Default context window
        })
    }

    /// Set the context window size (in tokens)
    pub fn set_context_window(&mut self, tokens: usize) {
        self.context_window = tokens;
    }

    /// Store content in the specified memory type
    pub async fn store(&self, memory_type: MemoryType, content: &str) -> Result<MemoryId> {
        let id = MemoryId::new();

        match memory_type {
            MemoryType::ShortTerm => {
                let entry = MemoryEntry {
                    id: id.clone(),
                    memory_type,
                    content: content.to_string(),
                    timestamp: OffsetDateTime::now_utc(),
                    relevance: 1.0,
                    metadata: Default::default(),
                };
                self.short_term.store(entry);
            }
            MemoryType::LongTerm => {
                let entry = MemoryEntry {
                    id: id.clone(),
                    memory_type,
                    content: content.to_string(),
                    timestamp: OffsetDateTime::now_utc(),
                    relevance: 0.8,
                    metadata: Default::default(),
                };
                self.long_term.store(entry).await?;
            }
            MemoryType::Episodic => {
                // `embedding` is reserved for a real embedding model
                // (fastembed is already in the workspace deps but not
                // wired up yet). Until then, episodic retrieval is
                // keyword-based — see `retrieve_context` — and the field
                // stays empty rather than being filled with a placeholder
                // that would silently make `find_similar` and
                // `semantic_search` return arbitrary results on the
                // caller's behalf.
                let episode = EpisodicMemoryType {
                    id: id.clone(),
                    content: content.to_string(),
                    embedding: Vec::new(),
                    task_type: "general".to_string(),
                    outcome: Outcome::Success,
                    timestamp: OffsetDateTime::now_utc(),
                };
                self.episodic.store(episode).await?;
            }
            MemoryType::Semantic => {
                // Semantic memory would be stored differently (graph DB)
                // For now, treat as long-term
                let entry = MemoryEntry {
                    id: id.clone(),
                    memory_type,
                    content: content.to_string(),
                    timestamp: OffsetDateTime::now_utc(),
                    relevance: 0.9,
                    metadata: Default::default(),
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
            MemoryType::LongTerm | MemoryType::Semantic => {
                if let Some(mut entry) = self.long_term.get(id).await? {
                    entry.content = content.to_string();
                    self.long_term.store(entry).await?;
                    Ok(())
                } else {
                    Err(KodError::MemoryStorage("Entry not found".to_string()))
                }
            }
            MemoryType::Episodic => {
                // Episodic memory update would need embedding regeneration
                // For now, return error
                Err(KodError::MemoryStorage(
                    "Episodic memory update not supported".to_string(),
                ))
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
            MemoryType::LongTerm | MemoryType::Semantic => self.long_term.remove(id).await,
            MemoryType::Episodic => {
                self.episodic.remove(id).await?;
                Ok(())
            }
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

    /// Retrieve context for a query.
    ///
    /// - Working memory is the most recent short-term entries (recency).
    /// - Long-term memory is matched by word overlap — see
    ///   [`MemoryManager::search_long_term_relevant`].
    /// - Episodic memory is filtered by case-insensitive substring match
    ///   on content — it is *not* embedding-based today, because the
    ///   manager does not yet compute real embeddings. When a real
    ///   embedding model is wired in, this method should switch to
    ///   `EpisodicMemory::find_similar`.
    pub async fn retrieve_context(&self, query: &str) -> Result<MemoryContext> {
        let mut context = MemoryContext {
            working_memory: self.short_term.get_recent(10),
            long_term: self.search_long_term_relevant(query).await?,
            ..Default::default()
        };

        // Get similar episodic memories
        let all_episodic = self.episodic.get_all().await?;
        let query_lower = query.to_lowercase();
        context.episodic = all_episodic
            .into_iter()
            .filter(|e| e.content.to_lowercase().contains(&query_lower))
            .collect();

        // Limit total context size
        self.limit_context_size(&mut context);

        Ok(context)
    }

    /// Retrieve long-term entries that share content words with `query`.
    ///
    /// The previous retrieve path called `long_term.search(query)`,
    /// which is a whole-query substring test: an entry matched only if
    /// its content literally contained the user's entire prompt. Real
    /// prompts are sentences ("tell me about the project"); real
    /// memory entries are short facts ("The user's project is called
    /// KOD"). The strict substring matched nothing, so the memory
    /// layer contributed nothing to any prompt — indistinguishable
    /// from a disconnected manager. This method is the retrieval half
    /// of the fix that wired the manager into `build_prompt`.
    ///
    /// Words of four or more characters, lowercased and deduplicated,
    /// form the query's content-word set. An entry scores by how many
    /// of those words its lowercased content contains; entries with
    /// zero overlap are dropped. The top 20 are returned, ties broken
    /// by the entry's own `relevance` so a fact explicitly marked
    /// important keeps its edge.
    ///
    /// Four characters is the shortest length that is unlikely to be
    /// an article, preposition, or pronoun — "the", "and", "is",
    /// "you" are all three or fewer. The filter is deliberately crude;
    /// a proper stopword list and stemming belong with the embedding
    /// work that will replace this heuristic.
    async fn search_long_term_relevant(&self, query: &str) -> Result<Vec<MemoryEntry>> {
        let words: Vec<String> = {
            let mut seen = std::collections::HashSet::new();
            query
                .to_lowercase()
                .split(|c: char| !c.is_alphanumeric())
                .filter(|w| w.len() >= 4)
                .filter(|w| seen.insert(w.to_string()))
                .map(|w| w.to_string())
                .collect()
        };
        if words.is_empty() {
            return Ok(Vec::new());
        }
        let all = self.long_term.get_all().await?;
        let mut scored: Vec<(usize, MemoryEntry)> = all
            .into_iter()
            .filter_map(|entry| {
                let lower = entry.content.to_lowercase();
                let hits = words
                    .iter()
                    .filter(|w| lower.contains(w.as_str()))
                    .count();
                if hits > 0 {
                    Some((hits, entry))
                } else {
                    None
                }
            })
            .collect();
        scored.sort_by(|a, b| {
            b.0.cmp(&a.0).then_with(|| {
                b.1.relevance
                    .partial_cmp(&a.1.relevance)
                    .unwrap_or(std::cmp::Ordering::Equal)
            })
        });
        Ok(scored.into_iter().take(20).map(|(_, e)| e).collect())
    }

    /// Get all short-term memories
    pub fn get_all_short_term(&self) -> Vec<MemoryEntry> {
        self.short_term.get_all()
    }

    /// Get all long-term memories
    pub async fn get_all_long_term(&self) -> Result<Vec<MemoryEntry>> {
        self.long_term.get_all().await
    }

    /// Get all episodic memories
    pub async fn get_all_episodic(&self) -> Result<Vec<EpisodicMemoryType>> {
        self.episodic.get_all().await
    }

    /// Clear all memories
    pub async fn clear_all(&self) -> Result<()> {
        self.short_term.clear();
        self.long_term.clear().await?;
        self.episodic.clear().await?;
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

        // Limit episodic (previously uncapped — could blow the window)
        let mut episodic = Vec::new();
        for entry in context.episodic.drain(..) {
            total_chars += entry.content.len();
            if total_chars <= max_chars {
                episodic.push(entry);
            } else {
                break;
            }
        }
        context.episodic = episodic;
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
            .store(
                MemoryType::LongTerm,
                "The user's project is called KOD.",
            )
            .await
            .unwrap();
        manager
            .store(MemoryType::LongTerm, "The user prefers dark mode.")
            .await
            .unwrap();
        manager
            .store(
                MemoryType::LongTerm,
                "The build uses Cargo and rustc.",
            )
            .await
            .unwrap();

        // The prompt shares only "project" with the first fact. The
        // other two share no content word with it.
        let ctx = manager
            .retrieve_context("tell me about the project")
            .await
            .unwrap();
        assert_eq!(
            ctx.long_term.len(),
            1,
            "expected exactly one match, got {:?}",
            ctx.long_term.iter().map(|e| &e.content).collect::<Vec<_>>()
        );
        assert!(ctx.long_term[0].content.contains("KOD"));

        // A query with two hits across two facts returns both, ranked
        // by hit count.
        let ctx = manager
            .retrieve_context("does the project use dark mode?")
            .await
            .unwrap();
        // "project" hits fact 1, "dark" and "mode" hit fact 2. Both
        // included; the ordering is by hit count so fact 2 is first.
        assert_eq!(ctx.long_term.len(), 2);
        assert!(ctx.long_term[0].content.contains("dark mode"));

        // A query with no content-word overlap returns nothing.
        let ctx = manager
            .retrieve_context("xyzzy plugh")
            .await
            .unwrap();
        assert!(ctx.long_term.is_empty());
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
