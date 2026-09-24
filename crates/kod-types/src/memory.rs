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
    /// Set when a later entry replaces this one. A superseded entry is
    /// kept on disk — deleting it loses the audit trail — but excluded
    /// from retrieval. `Some(id)` names the replacement.
    #[serde(default)]
    pub superseded_by: Option<MemoryId>,
    /// Entries this one contradicts. Both stay active: a contradiction
    /// is a fact to surface, not one to silently resolve by picking a
    /// winner. The pair is for a caller to see and a user to settle.
    #[serde(default)]
    pub contradicts: Vec<MemoryId>,
}

impl MemoryEntry {
    /// Whether this entry should be offered to retrieval.
    ///
    /// A superseded entry is not: it was replaced on purpose. A
    /// contradicted one is — surfacing both sides of a disagreement
    /// is the point.
    pub fn is_active(&self) -> bool {
        self.superseded_by.is_none()
    }
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
        
            superseded_by: None,
            contradicts: Vec::new(),
        };

        let json = serde_json::to_string(&entry).unwrap();
        let deserialized: MemoryEntry = serde_json::from_str(&json).unwrap();
        assert_eq!(entry.id, deserialized.id);
    }
}

#[cfg(test)]
mod coverage_memory_metadata {
    //! `MemoryMetadata` gained `project_key` and
    //! `last_retrieved_at_ms` after the initial release. Both are
    //! `#[serde(default)]` so an existing redb store reads
    //! cleanly; a regression that dropped either attribute would
    //! make an existing install fail to open its own database.
    use super::*;

    #[test]
    fn legacy_json_defaults_the_newer_fields() {
        // The three original fields (session_id, tags, embedding)
        // are required; only project_key and last_retrieved_at_ms
        // carry a per-field `#[serde(default)]`. A pre-D2.5 entry
        // therefore parses cleanly, with the two new fields
        // defaulting.
        let legacy = r#"{"session_id":null,"tags":[],"embedding":null}"#;
        let m: MemoryMetadata = serde_json::from_str(legacy).unwrap();
        assert!(m.session_id.is_none());
        assert!(m.tags.is_empty());
        assert!(m.embedding.is_none());
        assert!(m.project_key.is_none());
        assert!(m.last_retrieved_at_ms.is_none());
    }

    #[test]
    fn project_key_round_trips_when_set() {
        let m = MemoryMetadata {
            project_key: Some("abc123".into()),
            ..Default::default()
        };
        let json = serde_json::to_string(&m).unwrap();
        assert!(json.contains("abc123"));
        let parsed: MemoryMetadata = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.project_key.as_deref(), Some("abc123"));
    }

    #[test]
    fn last_retrieved_at_ms_round_trips_when_set() {
        let m = MemoryMetadata {
            last_retrieved_at_ms: Some(1_700_000_000_000),
            ..Default::default()
        };
        let json = serde_json::to_string(&m).unwrap();
        let parsed: MemoryMetadata = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.last_retrieved_at_ms, Some(1_700_000_000_000));
    }

    #[test]
    fn a_legacy_entry_parses_without_the_new_fields() {
        // A pre-D2.5 entry has only session_id, tags, and embedding.
        // The two new fields must default cleanly.
        let legacy = r#"{"session_id":null,"tags":[],"embedding":null}"#;
        let m: MemoryMetadata = serde_json::from_str(legacy).unwrap();
        assert!(m.project_key.is_none());
        assert!(m.last_retrieved_at_ms.is_none());
    }

    #[test]
    fn memory_context_default_is_all_empty() {
        let c = MemoryContext::default();
        assert!(c.working_memory.is_empty());
        assert!(c.long_term.is_empty());
        assert_eq!(c.total_tokens, 0);
    }
}

#[cfg(test)]
mod coverage_memory_type {
    //! `MemoryType` selects the storage layer a write goes to.
    //! The three variants have distinct semantics — short-term
    //! FIFO, long-term durable, episodic aging — and a regression
    //! that swapped one for another in the serializer would send
    //! a fact to the wrong layer.
    use super::*;

    #[test]
    fn every_variant_round_trips_through_json() {
        for v in [
            MemoryType::ShortTerm,
            MemoryType::LongTerm,
            MemoryType::Episodic,
        ] {
            let json = serde_json::to_string(&v).unwrap();
            let parsed: MemoryType = serde_json::from_str(&json).unwrap();
            assert_eq!(v, parsed, "roundtrip mismatch for {json}");
        }
    }

    #[test]
    fn variant_names_appear_in_the_serialized_form() {
        // The default derive serializes as the variant name. A
        // caller (a log viewer, a filter) relies on the exact
        // spelling.
        let json = serde_json::to_string(&MemoryType::Episodic).unwrap();
        assert!(json.contains("Episodic"), "got: {json}");
    }

    #[test]
    fn entry_with_every_field_populated_round_trips() {
        let e = MemoryEntry {
            id: MemoryId::new(),
            memory_type: MemoryType::Episodic,
            content: "café".to_string(),
            timestamp: OffsetDateTime::now_utc(),
            relevance: 0.85,
            metadata: MemoryMetadata {
                session_id: Some(SessionId::new()),
                tags: vec!["a".into(), "b".into()],
                embedding: Some(vec![0.1, 0.2, 0.3]),
                project_key: Some("proj".into()),
                last_retrieved_at_ms: Some(1_700_000_000_000),
            },
        
            superseded_by: None,
            contradicts: Vec::new(),
        };
        let json = serde_json::to_string(&e).unwrap();
        let parsed: MemoryEntry = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.memory_type, e.memory_type);
        assert_eq!(parsed.content, e.content);
        assert_eq!(parsed.metadata.tags, e.metadata.tags);
        assert_eq!(parsed.metadata.embedding, e.metadata.embedding);
        assert_eq!(parsed.metadata.project_key, e.metadata.project_key);
        assert_eq!(
            parsed.metadata.last_retrieved_at_ms,
            e.metadata.last_retrieved_at_ms,
        );
    }

    #[test]
    fn memory_context_default_is_all_empty() {
        let c = MemoryContext::default();
        assert!(c.working_memory.is_empty());
        assert!(c.long_term.is_empty());
        assert_eq!(c.total_tokens, 0);
    }
}
