//! Memory types for the multi-layer memory system.

use crate::ids::{MemoryId, SessionId};
use serde::{Deserialize, Serialize};
use time::OffsetDateTime;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum MemoryType {
    ShortTerm,
    LongTerm,
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
