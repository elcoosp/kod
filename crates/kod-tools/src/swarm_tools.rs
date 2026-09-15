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
//! Cost when no swarm is running is a single `HashMap` allocation.

use crate::{Tool, ToolContext};
use kod_error::{KodError, Result};
use kod_types::{ToolCategory, ToolDefinition, ToolId, ToolPermissions, ToolResult};
use serde_json::Value;
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::RwLock;

/// The shared blackboard: a key → value map behind an `Arc<RwLock<…>>`.
/// Clone the `Arc` to share the same store.
pub type SwarmKnowledge = Arc<RwLock<HashMap<String, String>>>;

pub fn new_knowledge() -> SwarmKnowledge {
    Arc::new(RwLock::new(HashMap::new()))
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
                fact. Writing to an existing key overwrites it."
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
        let mut guard = self.knowledge.write().await;
        let prev = guard.insert(key.to_string(), value.to_string());
        Ok(ToolResult::Success(serde_json::json!({
            "key": key,
            "value": value,
            "overwrote": prev.is_some(),
            "total_keys": guard.len(),
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
                every fact with its key. Use this before starting work that \
                might depend on what another agent has already found."
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
        let guard = self.knowledge.read().await;
        match params.get("key").and_then(|v| v.as_str()) {
            Some(key) => match guard.get(key) {
                Some(value) => Ok(ToolResult::Success(serde_json::json!({
                    "key": key,
                    "value": value,
                    "found": true,
                }))),
                None => Ok(ToolResult::Success(serde_json::json!({
                    "key": key,
                    "found": false,
                    "total_keys": guard.len(),
                }))),
            },
            None => {
                let mut entries: Vec<serde_json::Value> = guard
                    .iter()
                    .map(|(k, v)| serde_json::json!({"key": k, "value": v}))
                    .collect();
                entries.sort_by(|a, b| {
                    a["key"]
                        .as_str()
                        .unwrap_or("")
                        .cmp(b["key"].as_str().unwrap_or(""))
                });
                Ok(ToolResult::Success(serde_json::json!({
                    "count": entries.len(),
                    "entries": entries,
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
        let k = new_knowledge();
        let note = SwarmNoteTool::new(k.clone());
        let read = SwarmReadTool::new(k.clone());

        note.execute(&serde_json::json!({"key": "a", "value": "first"}), &ctx())
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
            }
            other => panic!("expected success, got {:?}", other),
        }
    }

    #[tokio::test]
    async fn note_overwrites_and_reports() {
        let k = new_knowledge();
        let note = SwarmNoteTool::new(k.clone());
        note.execute(&serde_json::json!({"key": "a", "value": "v1"}), &ctx())
            .await
            .unwrap();
        let r = note
            .execute(&serde_json::json!({"key": "a", "value": "v2"}), &ctx())
            .await
            .unwrap();
        match r {
            ToolResult::Success(v) => assert_eq!(v["overwrote"], true),
            other => panic!("expected success, got {:?}", other),
        }
    }

    #[tokio::test]
    async fn read_all_is_sorted_by_key() {
        let k = new_knowledge();
        let note = SwarmNoteTool::new(k.clone());
        let read = SwarmReadTool::new(k.clone());
        for (key, val) in [("c", "3"), ("a", "1"), ("b", "2")] {
            note.execute(&serde_json::json!({"key": key, "value": val}), &ctx())
                .await
                .unwrap();
        }
        let r = read.execute(&serde_json::json!({}), &ctx()).await.unwrap();
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
        let k = new_knowledge();
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
}
