//! Hub-backed swarm tools (D4.3).
//!
//! The swarm's blackboard is `kod_swarm::AgentCommunicationHub`.
//! The hub already carries a bounded per-agent history
//! (`MAX_HISTORY_PER_AGENT`) and a timestamp on every message, so
//! the previous approach of a second store alongside it was
//! redundant and worse.
//!
//! # Why the tools live here, not in kod-tools
//!
//! `kod-tools` defines the `Tool` trait. `kod-swarm` depends on
//! `kod-tools` (it uses the trait for its coordination tools). A
//! `Tool` implementation that holds an `AgentCommunicationHub`
//! would therefore force `kod-tools` to depend on `kod-swarm`,
//! closing a cycle. The composition root (`kod-core`) is the
//! correct home — the same pattern MCP tools use
//! (`mcp_adapters.rs`).
//!
//! # The tools
//!
//! - `swarm_note(key, value)`: broadcast a `KnowledgeShare` to every
//!   other online agent. The `key` becomes a tag on the message, the
//!   `value` the body.
//! - `swarm_read(key?)`: read the caller's received history. With a
//!   key, filter for entries tagged with it; without, list every
//!   `KnowledgeShare` the caller has received.
//!
//! Both hold `(Arc<AgentCommunicationHub>, AgentId)` — the hub to
//! talk to, and the identity to talk as. The engine registers the
//! pair under its own coordinator id; a swarm agent receives the
//! same tools under its own id via the tool registry it inherits
//! from the engine.

use crate::engine::KodEngine;
use kod_error::Result;
use kod_swarm::{AgentCommunicationHub, MessageContent};
use kod_tools::{Tool, ToolContext};
use kod_types::{AgentId, ToolCategory, ToolDefinition, ToolId, ToolPermissions, ToolResult};
use serde_json::Value;
use std::sync::Arc;

/// Register `agent` on the hub if it is not already there. The
/// registration is idempotent from the caller's point of view:
/// a hub that already knows the id is left alone, and the returned
/// error of a duplicate registration is swallowed. This is what
/// lets the tools survive a `clear_all()` the runner may have
/// performed at the start of a run.
async fn ensure_registered(hub: &AgentCommunicationHub, agent: &AgentId) {
    let already = hub.get_agent_history(agent).await;
    // An empty history is returned both for an unregistered agent
    // and for a registered-but-quiet one. There is no
    // `is_registered` query on the hub; the pragmatic probe is to
    // try registering and ignore the "already registered" error.
    let _ = already;
    let _ = hub.register_agent(agent.clone()).await;
}

/// The shared `ToolDefinition` skeleton for the two swarm tools.
fn base_definition(name: &str, description: &str, schema: Value) -> ToolDefinition {
    ToolDefinition {
        trust_level: kod_types::trust::TrustLevel::default(),
        id: ToolId::new(),
        name: name.to_string(),
        description: description.to_string(),
        category: ToolCategory::System,
        parameters_schema: schema,
        // The hub is neither filesystem nor network; a context with
        // default permissions can use the tools. The policy engine
        // is what gates them, not the ToolPermissions bitmask.
        permissions: ToolPermissions::default(),
        load_mode: Default::default(),
    }
}

// ---------------------------------------------------------------------------
// swarm_note
// ---------------------------------------------------------------------------

pub struct SwarmNoteTool {
    pub definition: ToolDefinition,
    hub: Arc<AgentCommunicationHub>,
    agent: AgentId,
}

impl SwarmNoteTool {
    pub fn new(hub: Arc<AgentCommunicationHub>, agent: AgentId) -> Self {
        Self {
            definition: base_definition(
                "swarm_note",
                "Record a short fact for the other agents in this \
                 swarm. Use it to share findings that affect shared \
                 code — a schema missing a column, a file another \
                 agent should look at, a decision that changes an \
                 interface. The key is a short identifier \
                 (e.g. `schema.missing_column`); the value is the \
                 fact. The store is a bounded per-agent history; \
                 older entries are dropped when it fills.",
                serde_json::json!({
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
                    "required": ["key", "value"],
                    "additionalProperties": false
                }),
            ),
            hub,
            agent,
        }
    }
}

#[async_trait::async_trait]
impl Tool for SwarmNoteTool {
    fn definition(&self) -> ToolDefinition {
        self.definition.clone()
    }

    async fn execute(&self, params: &Value, _context: &ToolContext) -> Result<ToolResult> {
        let key = match params["key"].as_str() {
            Some(k) if !k.trim().is_empty() => k.trim().to_string(),
            _ => {
                return Ok(ToolResult::Error(
                    "swarm_note: 'key' is required and must not be empty".to_string(),
                ));
            }
        };
        let value = match params["value"].as_str() {
            Some(v) if !v.trim().is_empty() => v.trim().to_string(),
            _ => {
                return Ok(ToolResult::Error(
                    "swarm_note: 'value' is required and must not be empty".to_string(),
                ));
            }
        };
        ensure_registered(&self.hub, &self.agent).await;
        // The message body carries both the key and the value: the
        // key as a tag (so `swarm_read key=…` can filter), and both
        // as text (so a bare `swarm_read` is still legible).
        let body = format!("{key}: {value}");
        match self
            .hub
            .post_knowledge(&self.agent, &body, vec![key.clone()])
            .await
        {
            Ok(()) => Ok(ToolResult::Success(serde_json::json!({
                "key": key,
                "value": value,
                "shared_with": "every online agent",
            }))),
            Err(e) => Ok(ToolResult::Error(format!("swarm_note failed: {e}"))),
        }
    }
}

// ---------------------------------------------------------------------------
// swarm_read
// ---------------------------------------------------------------------------

pub struct SwarmReadTool {
    pub definition: ToolDefinition,
    hub: Arc<AgentCommunicationHub>,
    agent: AgentId,
}

impl SwarmReadTool {
    pub fn new(hub: Arc<AgentCommunicationHub>, agent: AgentId) -> Self {
        Self {
            definition: base_definition(
                "swarm_read",
                "Read facts other agents have recorded with \
                 `swarm_note`. Pass a `key` to filter to entries \
                 tagged with it; omit it to list every fact this \
                 agent has received.",
                serde_json::json!({
                    "type": "object",
                    "properties": {
                        "key": {
                            "type": "string",
                            "description": "Optional key to filter on"
                        }
                    },
                    "additionalProperties": false
                }),
            ),
            hub,
            agent,
        }
    }
}

#[async_trait::async_trait]
impl Tool for SwarmReadTool {
    fn definition(&self) -> ToolDefinition {
        self.definition.clone()
    }

    async fn execute(&self, params: &Value, _context: &ToolContext) -> Result<ToolResult> {
        let filter = params
            .get("key")
            .and_then(|v| v.as_str())
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty());

        let history = self.hub.get_received_history(&self.agent).await;
        // The hub carries more than KnowledgeShare; the tool is
        // only about the shared notes, so filter by variant.
        let mut notes: Vec<(String, Vec<String>, String)> = Vec::new();
        for msg in &history {
            if let MessageContent::KnowledgeShare { information, tags } = &msg.content {
                if let Some(want) = &filter
                    && !tags.iter().any(|t| t == want)
                {
                    continue;
                }
                notes.push((format!("{:?}", msg.from), tags.clone(), information.clone()));
            }
        }

        if notes.is_empty() {
            return Ok(ToolResult::Success(serde_json::json!({
                "count": 0,
                "entries": [],
                "note": if filter.is_some() {
                    "no entries match the key filter"
                } else {
                    "this agent has not received any shared notes yet"
                }
            })));
        }

        let entries: Vec<Value> = notes
            .iter()
            .map(|(from, tags, info)| {
                serde_json::json!({
                    "from": from,
                    "tags": tags,
                    "information": info,
                })
            })
            .collect();

        Ok(ToolResult::Success(serde_json::json!({
            "count": entries.len(),
            "entries": entries,
        })))
    }
}

// Silence an unused-import warning while the module is small.
#[allow(dead_code)]
fn _engine_marker(_: &KodEngine) {}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn note_broadcasts_a_knowledge_share() {
        let hub = Arc::new(AgentCommunicationHub::new());
        let sender = AgentId::new();
        let receiver = AgentId::new();
        // Take the receiver before the broadcast so its channel is
        // live; a channel that has already been dropped would still
        // record in history but the send would fail.
        hub.register_agent(sender.clone()).await.unwrap();
        hub.register_agent(receiver.clone()).await.unwrap();
        let _rx = hub.get_agent_receiver(&receiver).await.unwrap();

        let tool = SwarmNoteTool::new(hub.clone(), sender.clone());
        let ctx = ToolContext::new("/tmp");
        let result = tool
            .execute(
                &serde_json::json!({"key": "schema", "value": "missing column"}),
                &ctx,
            )
            .await
            .unwrap();
        match result {
            ToolResult::Success(v) => {
                assert_eq!(v["key"], "schema");
                assert_eq!(v["value"], "missing column");
            }
            other => panic!("expected Success, got {other:?}"),
        }

        // The receiver's history has the message.
        let received = hub.get_received_history(&receiver).await;
        assert_eq!(received.len(), 1);
        match &received[0].content {
            MessageContent::KnowledgeShare { information, tags } => {
                assert!(information.contains("missing column"));
                assert!(tags.contains(&"schema".to_string()));
            }
            other => panic!("expected KnowledgeShare, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn read_filters_by_key() {
        let hub = Arc::new(AgentCommunicationHub::new());
        let sender = AgentId::new();
        let reader = AgentId::new();
        hub.register_agent(sender.clone()).await.unwrap();
        hub.register_agent(reader.clone()).await.unwrap();
        let _rx = hub.get_agent_receiver(&reader).await.unwrap();

        let note = SwarmNoteTool::new(hub.clone(), sender.clone());
        let ctx = ToolContext::new("/tmp");
        note.execute(&serde_json::json!({"key": "alpha", "value": "first"}), &ctx)
            .await
            .unwrap();
        note.execute(&serde_json::json!({"key": "beta", "value": "second"}), &ctx)
            .await
            .unwrap();

        let read = SwarmReadTool::new(hub.clone(), reader.clone());

        // No filter: both entries.
        match read.execute(&serde_json::json!({}), &ctx).await.unwrap() {
            ToolResult::Success(v) => assert_eq!(v["count"], 2),
            other => panic!("expected Success, got {other:?}"),
        }

        // Key filter: one entry.
        match read
            .execute(&serde_json::json!({"key": "alpha"}), &ctx)
            .await
            .unwrap()
        {
            ToolResult::Success(v) => {
                assert_eq!(v["count"], 1);
                assert!(
                    v["entries"][0]["information"]
                        .as_str()
                        .unwrap()
                        .contains("first")
                );
            }
            other => panic!("expected Success, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn read_on_quiet_hub_reports_zero_cleanly() {
        let hub = Arc::new(AgentCommunicationHub::new());
        let reader = AgentId::new();
        let tool = SwarmReadTool::new(hub.clone(), reader);
        let ctx = ToolContext::new("/tmp");
        match tool.execute(&serde_json::json!({}), &ctx).await.unwrap() {
            ToolResult::Success(v) => {
                assert_eq!(v["count"], 0);
                assert!(v["note"].as_str().unwrap().contains("not received"));
            }
            other => panic!("expected Success, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn note_rejects_empty_key_or_value() {
        let hub = Arc::new(AgentCommunicationHub::new());
        let sender = AgentId::new();
        let tool = SwarmNoteTool::new(hub, sender);
        let ctx = ToolContext::new("/tmp");
        assert!(matches!(
            tool.execute(&serde_json::json!({"key": "", "value": "x"}), &ctx)
                .await
                .unwrap(),
            ToolResult::Error(_)
        ));
        assert!(matches!(
            tool.execute(&serde_json::json!({"key": "k", "value": ""}), &ctx)
                .await
                .unwrap(),
            ToolResult::Error(_)
        ));
    }
}
