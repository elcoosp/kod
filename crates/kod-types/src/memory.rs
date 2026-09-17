//! Memory types for the multi-layer memory system.

use crate::ids::{MemoryId, SessionId};
use serde::{Deserialize, Serialize};
use time::OffsetDateTime;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum MemoryType {
    ShortTerm,
    LongTerm,
    /// A session-scoped episode: a fact extracted from a transcript
    /// (D2.4, extraction channel), tagged with the session it came
    /// from. Stored in the same persistent backend as `LongTerm` —
    /// the variant exists so a caller can filter by kind, and so
    /// the extraction path has a natural home for the `auto-*` tags
    /// it produces. Retrieval surfaces episodic entries alongside
    /// long-term ones via the hybrid scorer.
    Episodic,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MemoryEntry {
    pub id: MemoryId,
    pub memory_type: MemoryType,
    pub content: String,
    pub timestamp: OffsetDateTime,
    pub relevance: f32,
    pub metadata: MemoryMetadata,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct MemoryMetadata {
    pub session_id: Option<SessionId>,
    pub tags: Vec<String>,
    pub embedding: Option<Vec<f32>>,
    /// FNV-1a hash of the canonical working directory. Used by the
    /// hybrid retrieval to scope a "global" memory store per project
    /// (D2-B2). `None` for legacy entries and for entries written
    /// outside a project scope.
    #[serde(default)]
    pub project_key: Option<String>,
    /// Unix milliseconds of the last successful retrieval that
    /// returned this entry. Drives the archival heuristic — an
    /// untouched entry older than 60 days is a candidate for
    /// `kod memory forget` (D2-B5, not in this PR).
    #[serde(default)]
    pub last_retrieved_at_ms: Option<u64>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct MemoryContext {
    pub working_memory: Vec<MemoryEntry>,
    pub long_term: Vec<MemoryEntry>,
    pub total_tokens: usize,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_memory_entry_serialization() {
        let entry = MemoryEntry {
            id: MemoryId::new(),
            memory_type: MemoryType::ShortTerm,
            content: "User prefers dark mode".to_string(),
            timestamp: OffsetDateTime::now_utc(),
            relevance: 0.9,
            metadata: MemoryMetadata::default(),
        };

        let json = serde_json::to_string(&entry).unwrap();
        let deserialized: MemoryEntry = serde_json::from_str(&json).unwrap();
        assert_eq!(entry.id, deserialized.id);
    }
}
