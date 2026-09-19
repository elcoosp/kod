//! `todo`: a structured task list the agent maintains across turns.
//!
//! Long tasks lose their shape when the model has to hold the plan in
//! its own context. A todo list externalises it: the model writes down
//! the steps, marks them complete as it goes, and reads the current
//! state when it needs to orient. The list lives on the engine, so a
//! swarm's agents share one — and every turn of the same session sees
//! the same list.
//!
//! The storage is deliberately in-memory: a todo list is a working
//! document for one session, not a durable artefact. If it needs to
//! survive a restart, the caller can persist it elsewhere (the session
//! log already records every tool call, including `todo`).

use crate::{Tool, ToolContext};
use kod_error::{KodError, Result};
use kod_types::{ToolCategory, ToolDefinition, ToolId, ToolPermissions, ToolResult};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::sync::Arc;
use tokio::sync::RwLock;

/// One task in the list.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TodoItem {
    pub id: u64,
    pub text: String,
    pub status: TodoStatus,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TodoStatus {
    Pending,
    InProgress,
    Completed,
    Cancelled,
}

/// The shared list. Clone the `Arc` to share it.
pub type TodoList = Arc<RwLock<Vec<TodoItem>>>;

pub fn new_list() -> TodoList {
    Arc::new(RwLock::new(Vec::new()))
}

pub struct TodoTool {
    pub definition: ToolDefinition,
    list: TodoList,
    next_id: Arc<std::sync::atomic::AtomicU64>,
}

impl TodoTool {
    pub fn new(list: TodoList) -> Self {
        Self {
            definition: ToolDefinition {
                trust_level: kod_types::trust::TrustLevel::default(),
                id: ToolId::new(),
                name: "todo".to_string(),
                description: "Maintain a task list for the current session. Call it to \
                    record a plan (action \"add\"), mark progress (action \"update\"), or \
                    review the current state (action \"list\"). Use this for tasks with \
                    three or more steps; a one-step task does not need a list."
                    .to_string(),
                category: ToolCategory::System,
                parameters_schema: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "action": {
                            "type": "string",
                            "enum": ["add", "update", "list", "clear"],
                            "description": "What to do with the list."
                        },
                        "text": {
                            "type": "string",
                            "description": "The task text. Required for `add`."
                        },
                        "id": {
                            "type": "integer",
                            "description": "Task id. Required for `update`."
                        },
                        "status": {
                            "type": "string",
                            "enum": ["pending", "in_progress", "completed", "cancelled"],
                            "description": "New status. Required for `update`."
                        }
                    },
                    "required": ["action"],
                    "additionalProperties": false
                }),
                permissions: ToolPermissions::default(),
            },
            list,
            next_id: Arc::new(std::sync::atomic::AtomicU64::new(1)),
        }
    }
}

#[async_trait::async_trait]
impl Tool for TodoTool {
    fn definition(&self) -> ToolDefinition {
        self.definition.clone()
    }

    async fn execute(&self, params: &Value, _context: &ToolContext) -> Result<ToolResult> {
        let action = params["action"]
            .as_str()
            .ok_or_else(|| KodError::InvalidParameters {
                reason: "Missing 'action' parameter".to_string(),
            })?;

        match action {
            "add" => {
                let text = params["text"]
                    .as_str()
                    .ok_or_else(|| KodError::InvalidParameters {
                        reason: "'add' requires 'text'".to_string(),
                    })?
                    .trim()
                    .to_string();
                if text.is_empty() {
                    return Ok(ToolResult::Error("todo text must not be empty".to_string()));
                }
                let id = self
                    .next_id
                    .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                let item = TodoItem {
                    id,
                    text,
                    status: TodoStatus::Pending,
                };
                self.list.write().await.push(item.clone());
                Ok(ToolResult::Success(serde_json::json!({
                    "id": id,
                    "text": item.text,
                    "status": "pending",
                })))
            }
            "update" => {
                let id = params["id"]
                    .as_u64()
                    .ok_or_else(|| KodError::InvalidParameters {
                        reason: "'update' requires 'id'".to_string(),
                    })?;
                let status_str =
                    params["status"]
                        .as_str()
                        .ok_or_else(|| KodError::InvalidParameters {
                            reason: "'update' requires 'status'".to_string(),
                        })?;
                let status = match status_str {
                    "pending" => TodoStatus::Pending,
                    "in_progress" => TodoStatus::InProgress,
                    "completed" => TodoStatus::Completed,
                    "cancelled" => TodoStatus::Cancelled,
                    other => {
                        return Ok(ToolResult::Error(format!(
                            "unknown status {:?}; expected one of pending, in_progress, completed, cancelled",
                            other
                        )));
                    }
                };
                let mut list = self.list.write().await;
                match list.iter_mut().find(|it| it.id == id) {
                    Some(item) => {
                        item.status = status;
                        Ok(ToolResult::Success(serde_json::json!({
                            "id": id,
                            "status": status_str,
                        })))
                    }
                    None => Ok(ToolResult::Error(format!("no todo with id {}", id))),
                }
            }
            "list" => {
                let list = self.list.read().await;
                let items: Vec<Value> = list
                    .iter()
                    .map(|it| {
                        serde_json::json!({
                            "id": it.id,
                            "text": it.text,
                            "status": match it.status {
                                TodoStatus::Pending => "pending",
                                TodoStatus::InProgress => "in_progress",
                                TodoStatus::Completed => "completed",
                                TodoStatus::Cancelled => "cancelled",
                            }
                        })
                    })
                    .collect();
                Ok(ToolResult::Success(serde_json::json!({
                    "count": items.len(),
                    "items": items,
                })))
            }
            "clear" => {
                let mut list = self.list.write().await;
                let n = list.len();
                list.clear();
                Ok(ToolResult::Success(serde_json::json!({
                    "cleared": n,
                })))
            }
            other => Ok(ToolResult::Error(format!(
                "unknown action {:?}; expected add, update, list, or clear",
                other
            ))),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use kod_types::ToolPermissions;

    fn ctx() -> ToolContext {
        ToolContext::new("/tmp").with_permissions(ToolPermissions::default())
    }

    #[tokio::test]
    async fn add_then_list() {
        let list = new_list();
        let tool = TodoTool::new(list.clone());

        let r = tool
            .execute(
                &serde_json::json!({"action": "add", "text": "write the schema"}),
                &ctx(),
            )
            .await
            .unwrap();
        match r {
            ToolResult::Success(v) => {
                assert_eq!(v["id"], 1);
                assert_eq!(v["text"], "write the schema");
            }
            other => panic!("got {other:?}"),
        }

        let r = tool
            .execute(&serde_json::json!({"action": "list"}), &ctx())
            .await
            .unwrap();
        match r {
            ToolResult::Success(v) => {
                assert_eq!(v["count"], 1);
                assert_eq!(v["items"][0]["status"], "pending");
            }
            other => panic!("got {other:?}"),
        }
    }

    #[tokio::test]
    async fn update_changes_status() {
        let list = new_list();
        let tool = TodoTool::new(list.clone());
        tool.execute(&serde_json::json!({"action": "add", "text": "a"}), &ctx())
            .await
            .unwrap();
        let r = tool
            .execute(
                &serde_json::json!({"action": "update", "id": 1, "status": "completed"}),
                &ctx(),
            )
            .await
            .unwrap();
        match r {
            ToolResult::Success(v) => assert_eq!(v["status"], "completed"),
            other => panic!("got {other:?}"),
        }
    }

    #[tokio::test]
    async fn unknown_id_returns_error() {
        let list = new_list();
        let tool = TodoTool::new(list);
        let r = tool
            .execute(
                &serde_json::json!({"action": "update", "id": 99, "status": "completed"}),
                &ctx(),
            )
            .await
            .unwrap();
        match r {
            ToolResult::Error(msg) => assert!(msg.contains("99"), "got: {msg}"),
            other => panic!("got {other:?}"),
        }
    }

    #[tokio::test]
    async fn clear_empties_list() {
        let list = new_list();
        let tool = TodoTool::new(list.clone());
        for t in ["a", "b", "c"] {
            tool.execute(&serde_json::json!({"action": "add", "text": t}), &ctx())
                .await
                .unwrap();
        }
        let r = tool
            .execute(&serde_json::json!({"action": "clear"}), &ctx())
            .await
            .unwrap();
        match r {
            ToolResult::Success(v) => assert_eq!(v["cleared"], 3),
            other => panic!("got {other:?}"),
        }
    }
}

#[cfg(test)]
mod coverage_todo_lifecycle {
    //! The todo list is a working document the model reads back.
    //! A regression in ordering, id assignment, or validation
    //! produces a list whose IDs do not match what `update`
    //! expects, and the model loses its place with no error
    //! pointing at the cause.
    use super::*;
    use kod_types::ToolPermissions;

    fn ctx() -> ToolContext {
        ToolContext::new("/tmp").with_permissions(ToolPermissions::default())
    }

    #[tokio::test]
    async fn ids_are_monotonic_and_never_reused_after_clear() {
        // Even after `clear`, the next `add` must not reuse an id.
        // A model that has stale ids in its context could otherwise
        // update the wrong new item.
        let list = new_list();
        let tool = TodoTool::new(list.clone());
        tool.execute(&serde_json::json!({"action": "add", "text": "one"}), &ctx())
            .await
            .unwrap();
        tool.execute(&serde_json::json!({"action": "add", "text": "two"}), &ctx())
            .await
            .unwrap();
        tool.execute(&serde_json::json!({"action": "clear"}), &ctx())
            .await
            .unwrap();
        let r = tool
            .execute(&serde_json::json!({"action": "add", "text": "three"}), &ctx())
            .await
            .unwrap();
        match r {
            ToolResult::Success(v) => {
                assert_eq!(v["id"], 3, "id reused after clear: {v}");
            }
            other => panic!("got {other:?}"),
        }
    }

    #[tokio::test]
    async fn empty_text_is_rejected() {
        let list = new_list();
        let tool = TodoTool::new(list);
        let r = tool
            .execute(&serde_json::json!({"action": "add", "text": "   "}), &ctx())
            .await
            .unwrap();
        match r {
            ToolResult::Error(msg) => assert!(msg.contains("empty"), "got: {msg}"),
            other => panic!("expected error, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn missing_text_on_add_is_a_parameter_error() {
        let list = new_list();
        let tool = TodoTool::new(list);
        let err = tool
            .execute(&serde_json::json!({"action": "add"}), &ctx())
            .await
            .unwrap_err();
        assert!(err.to_string().contains("text"), "got: {err}");
    }

    #[tokio::test]
    async fn missing_status_on_update_is_a_parameter_error() {
        let list = new_list();
        let tool = TodoTool::new(list);
        tool.execute(&serde_json::json!({"action": "add", "text": "a"}), &ctx())
            .await
            .unwrap();
        let err = tool
            .execute(&serde_json::json!({"action": "update", "id": 1}), &ctx())
            .await
            .unwrap_err();
        assert!(err.to_string().contains("status"), "got: {err}");
    }

    #[tokio::test]
    async fn unknown_status_is_a_value_error_not_a_parameter_error() {
        // A bad status string is model input, not a missing
        // argument; the error must be recoverable (Error result)
        // and name the valid values.
        let list = new_list();
        let tool = TodoTool::new(list);
        tool.execute(&serde_json::json!({"action": "add", "text": "a"}), &ctx())
            .await
            .unwrap();
        let r = tool
            .execute(
                &serde_json::json!({"action": "update", "id": 1, "status": "in-progress"}),
                &ctx(),
            )
            .await
            .unwrap();
        match r {
            ToolResult::Error(msg) => {
                assert!(msg.contains("in_progress"), "hyphen form not suggested: {msg}");
            }
            other => panic!("expected error, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn unknown_action_is_reported_cleanly() {
        let list = new_list();
        let tool = TodoTool::new(list);
        let r = tool
            .execute(&serde_json::json!({"action": "delete", "id": 1}), &ctx())
            .await
            .unwrap();
        match r {
            ToolResult::Error(msg) => {
                assert!(msg.contains("add"), "expected a hint list: {msg}");
            }
            other => panic!("got {other:?}"),
        }
    }

    #[tokio::test]
    async fn list_returns_items_in_insertion_order() {
        let list = new_list();
        let tool = TodoTool::new(list);
        for t in ["alpha", "beta", "gamma"] {
            tool.execute(&serde_json::json!({"action": "add", "text": t}), &ctx())
                .await
                .unwrap();
        }
        let r = tool
            .execute(&serde_json::json!({"action": "list"}), &ctx())
            .await
            .unwrap();
        match r {
            ToolResult::Success(v) => {
                let items = v["items"].as_array().unwrap();
                assert_eq!(items[0]["text"], "alpha");
                assert_eq!(items[1]["text"], "beta");
                assert_eq!(items[2]["text"], "gamma");
            }
            other => panic!("got {other:?}"),
        }
    }

    #[tokio::test]
    async fn every_documented_status_is_accepted() {
        // The four statuses in the schema are the contract; a
        // regression that renamed one would silently break
        // `update` for that status.
        let list = new_list();
        let tool = TodoTool::new(list);
        tool.execute(&serde_json::json!({"action": "add", "text": "a"}), &ctx())
            .await
            .unwrap();
        for s in ["pending", "in_progress", "completed", "cancelled"] {
            let r = tool
                .execute(
                    &serde_json::json!({"action": "update", "id": 1, "status": s}),
                    &ctx(),
                )
                .await
                .unwrap();
            match r {
                ToolResult::Success(v) => assert_eq!(v["status"], s),
                other => panic!("status {s} rejected: {other:?}"),
            }
        }
    }

    #[tokio::test]
    async fn the_list_is_shared_across_tools() {
        // The `TodoList` is an `Arc<RwLock<Vec>>`; two tools
        // sharing the same list must see each other's writes.
        // This is what lets a swarm's agents share one plan.
        let list = new_list();
        let a = TodoTool::new(list.clone());
        let b = TodoTool::new(list.clone());
        a.execute(&serde_json::json!({"action": "add", "text": "from a"}), &ctx())
            .await
            .unwrap();
        let r = b
            .execute(&serde_json::json!({"action": "list"}), &ctx())
            .await
            .unwrap();
        match r {
            ToolResult::Success(v) => assert_eq!(v["count"], 1),
            other => panic!("got {other:?}"),
        }
    }
}
