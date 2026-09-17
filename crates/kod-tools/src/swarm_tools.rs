//! Swarm coordination tools: a shared key-value blackboard.
//!
//! Agents currently cannot see each other except through the filesystem.
//! A coordinator-owned key-value store gives them a hint channel: agent
//! A records "schema.sql is missing deleted_at" and agent B reads it.
//! The store lives on the engine, so every agent running under the same
//! `KodEngine` sees the same blackboard.
//!
//! Two tools:
//!
//! - `swarm_note(key, value)` — record a fact.
//! - `swarm_read(key?)` — read one fact, or every fact.
//!
//! # Bounds (D4-D3a)
//!
//! The blackboard is a *run-scoped* store, not an audit log. Left
//! unbounded it would grow with every note an agent writes, and a
//! long run (or a stuck agent looping on `swarm_note`) could grow it
//! without limit. The [`SwarmKnowledge`] type therefore holds a
//! `max_entries` cap; a write that pushes the map over the cap evicts
//! the oldest entry by `written_at_ms`. Reads report each entry's
//! `age_secs` so a caller can tell a fresh note from a stale one.
//!
//! A TTL (evict entries older than N seconds) is a follow-up: it
//! needs a background task, and today's cap + age reporting covers
//! the size risk without one.

use crate::{Tool, ToolContext};
use kod_error::{KodError, Result};
use kod_types::{ToolCategory, ToolDefinition, ToolId, ToolPermissions, ToolResult};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::RwLock;

/// Default cap on the number of live entries in one blackboard.
/// 512 is generous for a swarm run (each agent writes a handful of
/// notes) and small enough that a runaway loop stays bounded.
pub const DEFAULT_MAX_ENTRIES: usize = 512;

/// One note on the blackboard.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct KnowledgeEntry {
    pub value: String,
    /// Unix milliseconds when the note was last written.
    pub written_at_ms: u64,
}

/// Shared key-value store with a size cap.
///
/// `Clone` on the struct clones the inner `Arc`; every clone shares
/// the same store. The tools take a `SwarmKnowledge` by value in
/// their constructors, so a caller that has the engine's handle (as
/// `&SwarmKnowledge`) clones it the same way the pre-D4 alias did.
#[derive(Clone)]
pub struct SwarmKnowledge {
    inner: Arc<RwLock<HashMap<String, KnowledgeEntry>>>,
    max_entries: usize,
}

impl SwarmKnowledge {
    /// New store with the default cap.
    pub fn new() -> Self {
        Self::with_capacity(DEFAULT_MAX_ENTRIES)
    }

    /// New store with an explicit cap. A cap below 1 is treated as 1.
    pub fn with_capacity(max_entries: usize) -> Self {
        Self {
            inner: Arc::new(RwLock::new(HashMap::new())),
            max_entries: max_entries.max(1),
        }
    }

    /// Number of live entries. Read-only, used by tests and by a
    /// future `/swarm-status` display.
    pub async fn len(&self) -> usize {
        self.inner.read().await.len()
    }

    pub async fn is_empty(&self) -> bool {
        self.inner.read().await.is_empty()
    }

    /// Record a value. Overwrites any existing key. When the write
    /// pushes the store over the cap, the oldest entry (by
    /// `written_at_ms`) is evicted. Returns the previous value for
    /// the same key, if any — callers use this to report "overwrote".
    async fn note(&self, key: &str, value: &str) -> Option<String> {
        let now_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0);
        let mut guard = self.inner.write().await;
        let previous = guard
            .insert(
                key.to_string(),
                KnowledgeEntry {
                    value: value.to_string(),
                    written_at_ms: now_ms,
                },
            )
            .map(|e| e.value);
        // Evict oldest only if a *new* key was inserted (previous is
        // None) and we are now over the cap. An overwrite of an
        // existing key cannot grow the map.
        if previous.is_none() && guard.len() > self.max_entries
            && let Some(oldest_key) = guard
                .iter()
                .min_by_key(|(_, e)| e.written_at_ms)
                .map(|(k, _)| k.clone())
        {
            guard.remove(&oldest_key);
        }
        previous
    }

    /// Read one entry, if present.
    async fn get(&self, key: &str) -> Option<KnowledgeEntry> {
        self.inner.read().await.get(key).cloned()
    }

    /// Every entry, sorted by key.
    async fn entries(&self) -> Vec<(String, KnowledgeEntry)> {
        let guard = self.inner.read().await;
        let mut v: Vec<(String, KnowledgeEntry)> = guard
            .iter()
            .map(|(k, e)| (k.clone(), e.clone()))
            .collect();
        v.sort_by(|a, b| a.0.cmp(&b.0));
        v
    }
}

impl Default for SwarmKnowledge {
    fn default() -> Self {
        Self::new()
    }
}

/// Current Unix milliseconds.
fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// Age of an entry in whole seconds, given its `written_at_ms`.
fn age_secs(written_at_ms: u64) -> u64 {
    let now = now_ms();
    now.saturating_sub(written_at_ms) / 1000
}

/// Record a fact for the other agents in the swarm.
pub struct SwarmNoteTool {
    knowledge: SwarmKnowledge,
}

impl SwarmNoteTool {
    pub fn new(knowledge: SwarmKnowledge) -> Self {
        Self { knowledge }
    }
}

#[async_trait::async_trait]
impl Tool for SwarmNoteTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            id: ToolId::new(),
            name: "swarm_note".to_string(),
            description: "Record a short fact for the other agents in this \
                swarm. Use it to share findings that affect shared code — a \
                schema missing a column, a file another agent should look at, \
                a decision that changes an interface. The key is a short \
                identifier (e.g. `schema.missing_column`); the value is the \
                fact. Writing to an existing key overwrites it. The store is \
                capped (oldest notes are dropped when it fills)."
                .to_string(),
            category: ToolCategory::System,
            parameters_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "key": {
                        "type": "string",
                        "description": "Short identifier for the fact"
                    },
                    "value": {
                        "type": "string",
                        "description": "The fact to record"
                    }
                },
                "required": ["key", "value"]
            }),
            permissions: ToolPermissions::default(),
        }
    }

    async fn execute(&self, params: &Value, _context: &ToolContext) -> Result<ToolResult> {
        let key = params["key"]
            .as_str()
            .ok_or_else(|| KodError::InvalidParameters {
                reason: "Missing 'key' parameter".to_string(),
            })?;
        let value = params["value"]
            .as_str()
            .ok_or_else(|| KodError::InvalidParameters {
                reason: "Missing 'value' parameter".to_string(),
            })?;
        let prev = self.knowledge.note(key, value).await;
        let total = self.knowledge.len().await;
        Ok(ToolResult::Success(serde_json::json!({
            "key": key,
            "value": value,
            "overwrote": prev.is_some(),
            "total_keys": total,
        })))
    }
}

/// Read a fact, or every fact.
pub struct SwarmReadTool {
    knowledge: SwarmKnowledge,
}

impl SwarmReadTool {
    pub fn new(knowledge: SwarmKnowledge) -> Self {
        Self { knowledge }
    }
}

#[async_trait::async_trait]
impl Tool for SwarmReadTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            id: ToolId::new(),
            name: "swarm_read".to_string(),
            description: "Read facts other agents have recorded with \
                `swarm_note`. Pass a `key` to read one fact; omit it to list \
                every fact with its key. Each entry carries `age_secs` so you \
                can tell how fresh a fact is."
                .to_string(),
            category: ToolCategory::System,
            parameters_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "key": {
                        "type": "string",
                        "description": "Optional key to read a single fact"
                    }
                }
            }),
            permissions: ToolPermissions::default(),
        }
    }

    async fn execute(&self, params: &Value, _context: &ToolContext) -> Result<ToolResult> {
        match params.get("key").and_then(|v| v.as_str()) {
            Some(key) => match self.knowledge.get(key).await {
                Some(entry) => Ok(ToolResult::Success(serde_json::json!({
                    "key": key,
                    "value": entry.value,
                    "age_secs": age_secs(entry.written_at_ms),
                    "found": true,
                }))),
                None => Ok(ToolResult::Success(serde_json::json!({
                    "key": key,
                    "found": false,
                    "total_keys": self.knowledge.len().await,
                }))),
            },
            None => {
                let entries = self.knowledge.entries().await;
                let arr: Vec<serde_json::Value> = entries
                    .iter()
                    .map(|(k, e)| {
                        serde_json::json!({
                            "key": k,
                            "value": e.value,
                            "age_secs": age_secs(e.written_at_ms),
                        })
                    })
                    .collect();
                Ok(ToolResult::Success(serde_json::json!({
                    "count": arr.len(),
                    "entries": arr,
                })))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ctx() -> ToolContext {
        ToolContext::new("/tmp")
    }

    #[tokio::test]
    async fn note_then_read_one() {
        let k = SwarmKnowledge::new();
        let note = SwarmNoteTool::new(k.clone());
        let read = SwarmReadTool::new(k.clone());

        note.execute(
            &serde_json::json!({"key": "a", "value": "first"}),
            &ctx(),
        )
        .await
        .unwrap();

        let r = read
            .execute(&serde_json::json!({"key": "a"}), &ctx())
            .await
            .unwrap();
        match r {
            ToolResult::Success(v) => {
                assert_eq!(v["found"], true);
                assert_eq!(v["value"], "first");
                assert!(v["age_secs"].is_u64());
            }
            other => panic!("expected success, got {:?}", other),
        }
    }

    #[tokio::test]
    async fn note_overwrites_and_reports() {
        let k = SwarmKnowledge::new();
        let note = SwarmNoteTool::new(k.clone());
        note.execute(
            &serde_json::json!({"key": "a", "value": "v1"}),
            &ctx(),
        )
        .await
        .unwrap();
        let r = note
            .execute(
                &serde_json::json!({"key": "a", "value": "v2"}),
                &ctx(),
            )
            .await
            .unwrap();
        match r {
            ToolResult::Success(v) => assert_eq!(v["overwrote"], true),
            other => panic!("expected success, got {:?}", other),
        }
    }

    #[tokio::test]
    async fn read_all_is_sorted_by_key() {
        let k = SwarmKnowledge::new();
        let note = SwarmNoteTool::new(k.clone());
        let read = SwarmReadTool::new(k.clone());
        for (key, val) in [("c", "3"), ("a", "1"), ("b", "2")] {
            note.execute(
                &serde_json::json!({"key": key, "value": val}),
                &ctx(),
            )
            .await
            .unwrap();
        }
        let r = read
            .execute(&serde_json::json!({}), &ctx())
            .await
            .unwrap();
        match r {
            ToolResult::Success(v) => {
                assert_eq!(v["count"], 3);
                let entries = v["entries"].as_array().unwrap();
                assert_eq!(entries[0]["key"], "a");
                assert_eq!(entries[1]["key"], "b");
                assert_eq!(entries[2]["key"], "c");
            }
            other => panic!("expected success, got {:?}", other),
        }
    }

    #[tokio::test]
    async fn read_missing_key_reports_not_found() {
        let k = SwarmKnowledge::new();
        let read = SwarmReadTool::new(k.clone());
        let r = read
            .execute(&serde_json::json!({"key": "nope"}), &ctx())
            .await
            .unwrap();
        match r {
            ToolResult::Success(v) => assert_eq!(v["found"], false),
            other => panic!("expected success, got {:?}", other),
        }
    }

    /// Writing past the cap evicts the oldest entry.
    #[tokio::test]
    async fn cap_evicts_oldest() {
        let k = SwarmKnowledge::with_capacity(3);
        let note = SwarmNoteTool::new(k.clone());

        // Write four keys with distinct values; the last write
        // overflows and should evict the first key.
        for i in 1..=4 {
            note.execute(
                &serde_json::json!({
                    "key": format!("k{i}"),
                    "value": format!("v{i}"),
                }),
                &ctx(),
            )
            .await
            .unwrap();
            // Ensure the timestamp of each entry differs so eviction
            // is deterministic.
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
        assert_eq!(k.len().await, 3);

        // k1 gone, k2/k3/k4 present.
        assert!(k.get("k1").await.is_none());
        assert!(k.get("k2").await.is_some());
        assert!(k.get("k3").await.is_some());
        assert!(k.get("k4").await.is_some());
    }

    /// Overwriting an existing key does not evict anything, even at
    /// the cap.
    #[tokio::test]
    async fn overwrite_does_not_evict() {
        let k = SwarmKnowledge::with_capacity(2);
        let note = SwarmNoteTool::new(k.clone());
        note.execute(
            &serde_json::json!({"key": "a", "value": "1"}),
            &ctx(),
        )
        .await
        .unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        note.execute(
            &serde_json::json!({"key": "b", "value": "2"}),
            &ctx(),
        )
        .await
        .unwrap();
        // At cap: overwriting an existing key must not evict the
        // other one.
        note.execute(
            &serde_json::json!({"key": "a", "value": "1b"}),
            &ctx(),
        )
        .await
        .unwrap();
        assert_eq!(k.len().await, 2);
        assert!(k.get("a").await.is_some());
        assert!(k.get("b").await.is_some());
    }
}
