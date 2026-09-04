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
                // For now, use a simple embedding (in production, would use fastembed)
                let embedding = self.generate_simple_embedding(content);

                let episode = EpisodicMemoryType {
                    id: id.clone(),
                    content: content.to_string(),
                    embedding,
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
    pub async fn update(&self, memory_type: MemoryType, id: &MemoryId, content: &str) -> Result<()> {
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
                Err(KodError::MemoryStorage("Episodic memory update not supported".to_string()))
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
            MemoryType::LongTerm | MemoryType::Semantic => {
                self.long_term.remove(id).await
            }
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

    /// Retrieve context for a query
    pub async fn retrieve_context(&self, query: &str) -> Result<MemoryContext> {
        let mut context = MemoryContext {
            working_memory: self.short_term.get_recent(10),
            long_term: self.long_term.search(query).await?,
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

    /// Generate a simple embedding (placeholder for fastembed)
    fn generate_simple_embedding(&self, content: &str) -> Vec<f32> {
        // Simple hash-based embedding for testing
        // In production, this would use fastembed
        let mut embedding = vec![0.0; 128];

        for (i, byte) in content.bytes().enumerate() {
            let index = (i + byte as usize) % 128;
            embedding[index] += 1.0;
        }

        // Normalize
        let magnitude: f32 = embedding.iter().map(|x| x * x).sum::<f32>().sqrt();
        if magnitude > 0.0 {
            for value in embedding.iter_mut() {
                *value /= magnitude;
            }
        }

        embedding
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
