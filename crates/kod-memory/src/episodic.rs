//! Episodic memory - vector-based storage for semantic search.
//!
//! Stores task experiences with embeddings for finding similar
//! past experiences. Initially in-memory, with optional persistence.

use kod_error::Result;
use kod_types::{EpisodicMemory as EpisodicMemoryType, MemoryId, Outcome};
use parking_lot::RwLock;
use std::collections::HashMap;

/// In-memory episodic storage with vector similarity search
#[derive(Debug, Default)]
pub struct EpisodicMemory {
    episodes: RwLock<HashMap<MemoryId, EpisodicMemoryType>>,
}

impl EpisodicMemory {
    pub fn new() -> Self {
        Self {
            episodes: RwLock::new(HashMap::new()),
        }
    }

    /// Store an episode
    pub async fn store(&self, episode: EpisodicMemoryType) -> Result<()> {
        self.episodes.write().insert(episode.id.clone(), episode);
        Ok(())
    }

    /// Get an episode by ID
    pub async fn get(&self, id: &MemoryId) -> Result<Option<EpisodicMemoryType>> {
        Ok(self.episodes.read().get(id).cloned())
    }

    /// Remove an episode
    pub async fn remove(&self, id: &MemoryId) -> Result<Option<EpisodicMemoryType>> {
        Ok(self.episodes.write().remove(id))
    }

    /// Find similar episodes based on embedding similarity
    pub async fn find_similar(
        &self,
        query_embedding: &[f32],
        limit: usize,
    ) -> Result<Vec<EpisodicMemoryType>> {
        let episodes = self.episodes.read();

        let mut scored: Vec<(f32, &EpisodicMemoryType)> = episodes
            .values()
            .map(|episode| {
                let similarity = cosine_similarity(query_embedding, &episode.embedding);
                (similarity, episode)
            })
            .collect();

        // Sort by similarity (descending)
        scored.sort_by(|a, b| {
            b.0.partial_cmp(&a.0)
                .unwrap_or(std::cmp::Ordering::Equal)
        });

        Ok(scored
            .into_iter()
            .take(limit)
            .map(|(_, episode)| episode.clone())
            .collect())
    }

    /// Semantic search combining text query and embedding
    pub async fn semantic_search(
        &self,
        text_query: &str,
        query_embedding: &[f32],
        min_similarity: f32,
    ) -> Result<Vec<EpisodicMemoryType>> {
        let text_lower = text_query.to_lowercase();
        let episodes = self.episodes.read();

        let mut results: Vec<EpisodicMemoryType> = Vec::new();

        for episode in episodes.values() {
            // Calculate combined score
            let text_match = episode.content.to_lowercase().contains(&text_lower);
            let embedding_similarity = cosine_similarity(query_embedding, &episode.embedding);

            // Include if either text matches or embedding is similar enough
            if text_match || embedding_similarity >= min_similarity {
                results.push(episode.clone());
            }
        }

        // Sort by relevance (embedding similarity as proxy)
        results.sort_by(|a, b| {
            let sim_a = cosine_similarity(query_embedding, &a.embedding);
            let sim_b = cosine_similarity(query_embedding, &b.embedding);
            sim_b.partial_cmp(&sim_a).unwrap_or(std::cmp::Ordering::Equal)
        });

        Ok(results)
    }

    /// Get episodes by task type
    pub async fn get_by_task_type(&self, task_type: &str) -> Result<Vec<EpisodicMemoryType>> {
        let episodes = self.episodes.read();

        Ok(episodes
            .values()
            .filter(|e| e.task_type == task_type)
            .cloned()
            .collect())
    }

    /// Get episodes by outcome
    pub async fn get_by_outcome(&self, outcome: Outcome) -> Result<Vec<EpisodicMemoryType>> {
        let episodes = self.episodes.read();

        Ok(episodes
            .values()
            .filter(|e| e.outcome == outcome)
            .cloned()
            .collect())
    }

    /// Get all episodes
    pub async fn get_all(&self) -> Result<Vec<EpisodicMemoryType>> {
        Ok(self.episodes.read().values().cloned().collect())
    }

    /// Count total episodes
    pub async fn count(&self) -> Result<usize> {
        Ok(self.episodes.read().len())
    }

    /// Clear all episodes
    pub async fn clear(&self) -> Result<()> {
        self.episodes.write().clear();
        Ok(())
    }
}

/// Calculate cosine similarity between two vectors
fn cosine_similarity(a: &[f32], b: &[f32]) -> f32 {
    if a.len() != b.len() {
        return 0.0;
    }

    let dot_product: f32 = a.iter().zip(b.iter()).map(|(x, y)| x * y).sum();
    let magnitude_a: f32 = a.iter().map(|x| x * x).sum::<f32>().sqrt();
    let magnitude_b: f32 = b.iter().map(|x| x * x).sum::<f32>().sqrt();

    if magnitude_a == 0.0 || magnitude_b == 0.0 {
        0.0
    } else {
        dot_product / (magnitude_a * magnitude_b)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_cosine_similarity() {
        let a = vec![1.0, 0.0, 0.0];
        let b = vec![1.0, 0.0, 0.0];
        assert!((cosine_similarity(&a, &b) - 1.0).abs() < 1e-6);

        let c = vec![0.0, 1.0, 0.0];
        assert!((cosine_similarity(&a, &c) - 0.0).abs() < 1e-6);

        let d = vec![1.0, 1.0, 0.0];
        assert!((cosine_similarity(&a, &d) - std::f32::consts::FRAC_1_SQRT_2).abs() < 1e-3);
    }

    #[test]
    fn test_zero_vectors() {
        let a = vec![0.0, 0.0, 0.0];
        let b = vec![1.0, 0.0, 0.0];
        assert_eq!(cosine_similarity(&a, &b), 0.0);
    }
}
