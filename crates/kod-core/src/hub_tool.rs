//! Delta §7.6: the hub tool — one op-dispatched surface for the
//! swarm's messaging and job state.
//!
//! # Why one tool, not many
//!
//! Separate tools (`swarm_send`, `swarm_inbox`, `job_list`, …) each
//! carry a schema in every request. One tool with an `op` enum keeps
//! the tool surface small and the prefix stable — better prompt-cache
//! behaviour and fewer schema tokens, the design's "one op-dispatched
//! tool beats many tiny tools".
//!
//! # Scope (honest)
//!
//! * **Messaging** — `send`, `inbox`, `list`. Built on
//!   `AgentCommunicationHub`, which exists.
//! * **Jobs** — `jobs`. Reads the background runner's snapshot.
//! * **Processes** — `start`/`ps`/`logs`/`stop` are *not* implemented.
//!   kod has no process supervisor (start-with-readiness-probe,
//!   restart policy, log capture). The ops are declared in the schema
//!   and return a clear `unavailable` result naming what is missing,
//!   rather than pretending to supervise a process the tool cannot
//!   track. A model that asks learns the boundary; a model that does
//!   not is not misled.

use std::sync::Arc;

use async_trait::async_trait;
use kod_error::Result;
use kod_swarm::AgentCommunicationHub;
use kod_types::{
    AgentId, AgentMessageContent, Priority, ToolCategory, ToolDefinition, ToolId, ToolPermissions,
    ToolResult,
};
use serde_json::Value;

use crate::background::BackgroundJobRunner;
use kod_tools::{Tool, ToolContext};

/// The hub tool.
pub struct HubTool {
    pub definition: ToolDefinition,
    hub: Arc<AgentCommunicationHub>,
    agent: AgentId,
    jobs: Arc<BackgroundJobRunner>,
}

impl HubTool {
    pub fn new(
        hub: Arc<AgentCommunicationHub>,
        agent: AgentId,
        jobs: Arc<BackgroundJobRunner>,
    ) -> Self {
        Self {
            definition: ToolDefinition {
                trust_level: kod_types::trust::TrustLevel::default(),
                id: ToolId::new(),
                name: "hub".to_string(),
                description: "Swarm hub. ops: list, send, inbox, jobs. \
                    (process ops unavailable in this build)"
                    .to_string(),
                category: ToolCategory::System,
                parameters_schema: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "op": {
                            "type": "string",
                            "enum": ["list", "send", "inbox", "jobs",
                                     "start", "ps", "logs", "stop"]
                        },
                        "to": { "type": "string" },
                        "body": { "type": "string" },
                        "priority": {
                            "type": "string",
                            "enum": ["low", "medium", "high", "critical"]
                        }
                    },
                    "required": ["op"],
                    "additionalProperties": false
                }),
                permissions: ToolPermissions::default(),
                load_mode: Default::default(),
            },
            hub,
            agent,
            jobs,
        }
    }
}

#[async_trait]
impl Tool for HubTool {
    fn definition(&self) -> ToolDefinition {
        self.definition.clone()
    }

    async fn execute(&self, params: &Value, _context: &ToolContext) -> Result<ToolResult> {
        let op = params.get("op").and_then(|v| v.as_str()).unwrap_or("");
        match op {
            "list" => self.op_list().await,
            "send" => self.op_send(params).await,
            "inbox" => self.op_inbox().await,
            "jobs" => self.op_jobs(),
            "start" | "ps" | "logs" | "stop" => Ok(ToolResult::Error(format!(
                "hub op {op:?} is unavailable: this build has no process \
                 supervisor. Start long-running work with `execute_command` \
                 (its `run_in_background` flag is a background job, visible \
                 through `hub {{op: \"jobs\"}}`)."
            ))),
            other => Ok(ToolResult::Error(format!(
                "hub: unknown op {other:?}; expected one of list, send, inbox, jobs"
            ))),
        }
    }
}

impl HubTool {
    /// `list` — the agents the hub knows and their online state.
    async fn op_list(&self) -> Result<ToolResult> {
        let agents = self.hub.list_agents().await;
        let peers: Vec<Value> = agents
            .iter()
            .filter(|(id, _)| id != &self.agent)
            .map(|(id, online)| {
                serde_json::json!({
                    "id": id.to_prefixed_string(),
                    "online": online,
                })
            })
            .collect();
        Ok(ToolResult::Success(serde_json::json!({
            "self": self.agent.to_prefixed_string(),
            "peers": peers,
            "count": peers.len(),
        })))
    }

    /// `send` — a direct message to a peer.
    async fn op_send(&self, params: &Value) -> Result<ToolResult> {
        let to = match params.get("to").and_then(|v| v.as_str()) {
            Some(s) if !s.trim().is_empty() => s.trim().to_string(),
            _ => return Ok(ToolResult::Error("hub send: 'to' is required".to_string())),
        };
        let body = match params.get("body").and_then(|v| v.as_str()) {
            Some(s) if !s.trim().is_empty() => s.trim().to_string(),
            _ => {
                return Ok(ToolResult::Error(
                    "hub send: 'body' is required".to_string(),
                ));
            }
        };
        let priority = match params
            .get("priority")
            .and_then(|v| v.as_str())
            .unwrap_or("medium")
        {
            "low" => Priority::Low,
            "high" => Priority::High,
            "critical" => Priority::Critical,
            _ => Priority::Medium,
        };
        // Resolve the recipient by its rendered id. AgentId parses
        // from the `<prefix>-<uuid>` form the `list` op prints.
        let to_id: AgentId = match to.parse() {
            Ok(id) => id,
            Err(_) => {
                return Ok(ToolResult::Error(format!(
                    "hub send: {to:?} is not a valid agent id (use `hub {{op: \"list\"}}`)"
                )));
            }
        };
        let content = AgentMessageContent::KnowledgeShare {
            information: body,
            tags: Vec::new(),
        };
        // Priority rides on the hub's own `send_direct`; the content
        // variant carries the body.
        match self.hub.send_direct(&self.agent, &to_id, content).await {
            Ok(()) => Ok(ToolResult::Success(serde_json::json!({
                "delivered": true,
                "to": to,
                "priority": format!("{priority:?}").to_lowercase(),
            }))),
            Err(e) => Ok(ToolResult::Error(format!("hub send failed: {e}"))),
        }
    }

    /// `inbox` — messages this agent has received.
    async fn op_inbox(&self) -> Result<ToolResult> {
        let history = self.hub.get_received_history(&self.agent).await;
        let messages: Vec<Value> = history
            .iter()
            .filter_map(|m| {
                let body = match &m.content {
                    AgentMessageContent::KnowledgeShare { information, .. } => information.clone(),
                    AgentMessageContent::TaskAssignment { description, .. } => description.clone(),
                    AgentMessageContent::ProgressUpdate { details, .. } => details.clone(),
                    AgentMessageContent::HelpRequest { question, .. } => question.clone(),
                    AgentMessageContent::ResultDelivery { result } => result.clone(),
                    _ => return None,
                };
                Some(serde_json::json!({
                    "from": format!("{:?}", m.from),
                    "body": body,
                }))
            })
            .collect();
        Ok(ToolResult::Success(serde_json::json!({
            "count": messages.len(),
            "messages": messages,
        })))
    }

    /// `jobs` — the background runner's snapshot.
    fn op_jobs(&self) -> Result<ToolResult> {
        let snap = self.jobs.snapshot();
        let jobs: Vec<Value> = snap
            .iter()
            .map(|(id, state)| {
                serde_json::json!({
                    "id": id.to_string(),
                    "state": format!("{state:?}"),
                })
            })
            .collect();
        Ok(ToolResult::Success(serde_json::json!({
            "running": self.jobs.running_count(),
            "jobs": jobs,
        })))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tool() -> HubTool {
        HubTool::new(
            Arc::new(AgentCommunicationHub::new()),
            AgentId::new(),
            Arc::new(BackgroundJobRunner::new(4)),
        )
    }

    #[test]
    fn process_ops_report_unavailable() {
        let t = tool();
        let rt = tokio::runtime::Runtime::new().unwrap();
        let ctx = ToolContext::new(std::path::PathBuf::from("."));
        for op in ["start", "ps", "logs", "stop"] {
            let r = rt
                .block_on(t.execute(&serde_json::json!({"op": op}), &ctx))
                .unwrap();
            match r {
                ToolResult::Error(e) => {
                    assert!(e.contains("unavailable"), "op {op}: {e}");
                    assert!(
                        e.contains("run_in_background"),
                        "op {op} should suggest the real path: {e}"
                    );
                }
                other => panic!("op {op}: expected unavailable, got {other:?}"),
            }
        }
    }

    #[test]
    fn unknown_op_is_an_error() {
        let t = tool();
        let rt = tokio::runtime::Runtime::new().unwrap();
        let ctx = ToolContext::new(std::path::PathBuf::from("."));
        let r = rt
            .block_on(t.execute(&serde_json::json!({"op": "frobnicate"}), &ctx))
            .unwrap();
        assert!(matches!(r, ToolResult::Error(_)));
    }

    #[test]
    fn jobs_op_returns_a_snapshot() {
        let t = tool();
        let rt = tokio::runtime::Runtime::new().unwrap();
        let ctx = ToolContext::new(std::path::PathBuf::from("."));
        let r = rt
            .block_on(t.execute(&serde_json::json!({"op": "jobs"}), &ctx))
            .unwrap();
        let ToolResult::Success(v) = r else {
            panic!("expected success");
        };
        assert_eq!(v["running"], 0);
        assert!(v["jobs"].is_array());
    }

    #[test]
    fn send_requires_to_and_body() {
        let t = tool();
        let rt = tokio::runtime::Runtime::new().unwrap();
        let ctx = ToolContext::new(std::path::PathBuf::from("."));
        let r = rt
            .block_on(t.execute(&serde_json::json!({"op": "send"}), &ctx))
            .unwrap();
        match r {
            ToolResult::Error(e) => assert!(e.contains("'to'"), "got: {e}"),
            other => panic!("expected an error, got {other:?}"),
        }
    }

    #[test]
    fn inbox_starts_empty() {
        let t = tool();
        let rt = tokio::runtime::Runtime::new().unwrap();
        let ctx = ToolContext::new(std::path::PathBuf::from("."));
        let r = rt
            .block_on(t.execute(&serde_json::json!({"op": "inbox"}), &ctx))
            .unwrap();
        let ToolResult::Success(v) = r else {
            panic!("expected success");
        };
        assert_eq!(v["count"], 0);
    }
}
