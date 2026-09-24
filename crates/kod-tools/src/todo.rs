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
    /// Ids of todos that must reach `Completed` before this one may
    /// start. Empty for an unblocked item.
    ///
    /// Deliberately model-set, not harness-inferred: the harness
    /// cannot know that "write the parser" depends on "design the
    /// grammar", the model can. The tool's contribution is
    /// enforcement — `update` refuses to move a blocked item to
    /// `in_progress` and names the blockers, so the model gets a
    /// concrete continuation rather than a silent no-op.
    #[serde(default)]
    pub blocked_by: Vec<u64>,
    /// How much the harness trusts the completion. Model cannot set
    /// it — see [`ConfidenceState`].
    #[serde(default)]
    pub confidence: ConfidenceState,
    /// What the harness observed, appended as it happens.
    #[serde(default)]
    pub evidence: Vec<Evidence>,
}

impl TodoItem {
    /// Whether every blocker has reached `Completed`.
    ///
    /// A *cancelled* blocker does not unblock: cancelling a
    /// prerequisite means the plan is broken, and treating it as done
    /// would hide that. The item stays blocked until the model either
    /// revives the prerequisite or calls `unblock`.
    pub fn is_ready(&self, all: &[TodoItem]) -> bool {
        self.blocked_by.iter().all(|bid| {
            all.iter()
                .any(|it| it.id == *bid && it.status == TodoStatus::Completed)
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TodoStatus {
    Pending,
    InProgress,
    Completed,
    Cancelled,
}


/// How much the harness trusts a completed todo.
///
/// The model cannot set this. A todo that the model marks `completed`
/// with nothing to back it up is [`ConfidenceState::Speculative`] — the
/// model says it is done, and the model is the one that would be wrong.
/// Evidence the *harness* observed raises it: a file the agent
/// actually wrote, a check that actually passed.
///
/// This is the notebook's "the model can't self-report done" made
/// concrete. An enum, not a score, because the question is categorical
/// — do we have evidence or not — and a number would invite treating
/// 0.6 as nearly done.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum ConfidenceState {
    /// The model marked it complete; nothing corroborates it.
    #[default]
    Speculative,
    /// The harness observed work: files written under this todo's
    /// span, a check that passed.
    Corroborated,
    /// A check passed *and* the files it touched are the ones the
    /// todo named. The strongest signal the harness can produce
    /// without understanding the task.
    Verified,
}

impl ConfidenceState {
    /// Raise to at least `other`. Never lowers: evidence once seen is
    /// not unseen when a later observation is weaker.
    pub fn raise_to(self, other: Self) -> Self {
        // The derive gives Ord on declaration order, which is the
        // strength order here.
        if (other as u8) > (self as u8) {
            other
        } else {
            self
        }
    }
}

/// One thing the harness observed that bears on a todo.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Evidence {
    /// What was seen, in the harness's words: `"3 files written"`,
    /// `"cargo check passed"`.
    pub note: String,
    /// Milliseconds since the epoch, so the report can order them.
    pub at_ms: u64,
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


/// Record harness-observed evidence bearing on a todo.
///
/// Free function, not a method: the caller holds the `Arc<TodoList>`
/// the engine shares, and running through the tool would need an
/// `execute` call with fabricated parameters.
///
/// `raise` is the confidence the observation supports — a passing
/// check is [`ConfidenceState::Corroborated`], a check whose files
/// match the todo's own `expected_writes` is
/// [`ConfidenceState::Verified`]. Raising is monotone: a weaker
/// observation after a stronger one leaves the stronger standing.
///
/// Returns `true` when a matching todo was found and updated, so a
/// caller can log "this evidence landed" versus "the todo was already
/// gone."
pub fn note_evidence(
    list: &TodoList,
    todo_id: u64,
    note: String,
    raise: ConfidenceState,
) -> bool {
    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0);
    // `try_write` in a sync fn: the evidence path runs on the engine's
    // async runtime, and blocking there on a lock the tool holds while
    // the model is mid-call would be a stall for a log line. Dropping
    // evidence under contention is the right trade — the next
    // observation re-raises.
    let Ok(mut guard) = list.try_write() else {
        return false;
    };
    for item in guard.iter_mut() {
        if item.id == todo_id {
            item.confidence = item.confidence.raise_to(raise);
            item.evidence.push(Evidence { note, at_ms: now_ms });
            return true;
        }
    }
    false
}

/// The id of the single `in_progress` todo, if exactly one exists.
///
/// A caller tracking "what is the agent working on" reads here. Two
/// in-progress todos means the model is not sequencing its work; the
/// function returns `None` rather than guessing which one evidence
/// belongs to.
pub fn in_progress_todo(list: &TodoList) -> Option<u64> {
    let Ok(guard) = list.try_read() else {
        return None;
    };
    let mut found = None;
    for item in guard.iter() {
        if item.status == TodoStatus::InProgress {
            if found.is_some() {
                return None;
            }
            found = Some(item.id);
        }
    }
    found
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
                // Optional `blocked_by`: ids this item waits on. Ids
                // that do not exist yet are accepted — the model may
                // add the prerequisite next; the item is simply never
                // ready until they appear and complete.
                let blocked_by: Vec<u64> = params
                    .get("blocked_by")
                    .and_then(Value::as_array)
                    .map(|a| a.iter().filter_map(Value::as_u64).collect())
                    .unwrap_or_default();
                let item = TodoItem {
                    id,
                    text,
                    status: TodoStatus::Pending,
                    blocked_by: blocked_by.clone(),
                    // A fresh todo has no evidence, so it is
                    // Speculative — which is accurate, not a
                    // placeholder.
                    confidence: ConfidenceState::Speculative,
                    evidence: Vec::new(),
                };
                self.list.write().await.push(item.clone());
                Ok(ToolResult::Success(serde_json::json!({
                    "id": id,
                    "text": item.text,
                    "status": "pending",
                    "blocked_by": blocked_by,
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
                // Readiness gate: `in_progress` on a blocked item is
                // refused with the blocker ids named. Every other
                // transition is allowed — cancelling a blocked item is
                // legitimate, and completing one out of order is the
                // model's call.
                if status == TodoStatus::InProgress {
                    let all: Vec<TodoItem> = list.clone();
                    if let Some(item) = all.iter().find(|it| it.id == id)
                        && !item.is_ready(&all)
                    {
                        let pending: Vec<u64> = item
                            .blocked_by
                            .iter()
                            .filter(|bid| {
                                !all.iter().any(|it| {
                                    it.id == **bid && it.status == TodoStatus::Completed
                                })
                            })
                            .copied()
                            .collect();
                        return Ok(ToolResult::Error(format!(
                            "todo {id} is blocked by {pending:?}; complete those first \
                             (or `unblock` this item if the dependency no longer applies)",
                        )));
                    }
                }
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
            "unblock" => {
                // Clear an item's blockers. The model calls this when
                // a dependency no longer applies — the tool cannot
                // infer that.
                let id = params["id"]
                    .as_u64()
                    .ok_or_else(|| KodError::InvalidParameters {
                        reason: "'unblock' requires 'id'".to_string(),
                    })?;
                let mut list = self.list.write().await;
                match list.iter_mut().find(|it| it.id == id) {
                    Some(item) => {
                        let cleared = std::mem::take(&mut item.blocked_by);
                        Ok(ToolResult::Success(serde_json::json!({
                            "id": id,
                            "cleared": cleared,
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
                        // `ready` distinguishes a pending item that can
                        // start from one waiting on a blocker.
                        let ready = it.is_ready(&list);
                        serde_json::json!({
                            "id": it.id,
                            "text": it.text,
                            "status": match it.status {
                                TodoStatus::Pending => "pending",
                                TodoStatus::InProgress => "in_progress",
                                TodoStatus::Completed => "completed",
                                TodoStatus::Cancelled => "cancelled",
                            },
                            "blocked_by": it.blocked_by,
                            "ready": ready,
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
            .execute(
                &serde_json::json!({"action": "add", "text": "three"}),
                &ctx(),
            )
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
                assert!(
                    msg.contains("in_progress"),
                    "hyphen form not suggested: {msg}"
                );
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
        a.execute(
            &serde_json::json!({"action": "add", "text": "from a"}),
            &ctx(),
        )
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


#[cfg(test)]
mod semantic_todo_tests {
    use super::*;

    /// Add a todo and return the id the tool assigned. Ids are
    /// assigned by an internal counter, not by the caller, so a test
    /// that hardcodes 0 gets a blocker that points at nothing.
    async fn add(tool: &TodoTool, ctx: &ToolContext, text: &str, blocked_by: Vec<u64>) -> u64 {
        let params = if blocked_by.is_empty() {
            serde_json::json!({"action": "add", "text": text})
        } else {
            serde_json::json!({"action": "add", "text": text, "blocked_by": blocked_by})
        };
        match tool.execute(&params, ctx).await.unwrap() {
            ToolResult::Success(v) => v["id"].as_u64().unwrap(),
            other => panic!("add failed: {other:?}"),
        }
    }

    #[tokio::test]
    async fn blocked_item_refuses_to_start() {
        let tool = TodoTool::new(new_list());
        let ctx = ToolContext::new(std::env::temp_dir());
        let dep = add(&tool, &ctx, "design", vec![]).await;
        let child = add(&tool, &ctx, "build", vec![dep]).await;

        let r = tool
            .execute(
                &serde_json::json!({"action": "update", "id": child, "status": "in_progress"}),
                &ctx,
            )
            .await
            .unwrap();
        match r {
            ToolResult::Error(msg) => {
                assert!(msg.contains("blocked by"), "got: {msg}");
                assert!(msg.contains(&dep.to_string()), "message must name the blocker: {msg}");
            }
            other => panic!("expected refusal, got {other:?}"),
        }

        // Completing the prerequisite unblocks the dependent.
        tool.execute(
            &serde_json::json!({"action": "update", "id": dep, "status": "completed"}),
            &ctx,
        )
        .await
        .unwrap();
        let r = tool
            .execute(
                &serde_json::json!({"action": "update", "id": child, "status": "in_progress"}),
                &ctx,
            )
            .await
            .unwrap();
        assert!(matches!(r, ToolResult::Success(_)));
    }

    #[tokio::test]
    async fn unblock_clears_the_blockers() {
        let tool = TodoTool::new(new_list());
        let ctx = ToolContext::new(std::env::temp_dir());
        let dep = add(&tool, &ctx, "dep", vec![]).await;
        let child = add(&tool, &ctx, "child", vec![dep]).await;

        tool.execute(&serde_json::json!({"action": "unblock", "id": child}), &ctx)
            .await
            .unwrap();
        let r = tool
            .execute(
                &serde_json::json!({"action": "update", "id": child, "status": "in_progress"}),
                &ctx,
            )
            .await
            .unwrap();
        assert!(matches!(r, ToolResult::Success(_)), "unblocked item starts");
    }

    #[tokio::test]
    async fn list_marks_readiness() {
        let tool = TodoTool::new(new_list());
        let ctx = ToolContext::new(std::env::temp_dir());
        let dep = add(&tool, &ctx, "a", vec![]).await;
        let _child = add(&tool, &ctx, "b", vec![dep]).await;

        let r = tool
            .execute(&serde_json::json!({"action": "list"}), &ctx)
            .await
            .unwrap();
        match r {
            ToolResult::Success(v) => {
                let items = v["items"].as_array().unwrap();
                assert_eq!(items.len(), 2);
                assert_eq!(items[0]["ready"], true, "a has no blockers");
                assert_eq!(items[1]["ready"], false, "b waits on a");
            }
            other => panic!("expected success, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn cancelled_blocker_does_not_unblock() {
        let tool = TodoTool::new(new_list());
        let ctx = ToolContext::new(std::env::temp_dir());
        let dep = add(&tool, &ctx, "a", vec![]).await;
        let child = add(&tool, &ctx, "b", vec![dep]).await;

        tool.execute(
            &serde_json::json!({"action": "update", "id": dep, "status": "cancelled"}),
            &ctx,
        )
        .await
        .unwrap();
        let r = tool
            .execute(
                &serde_json::json!({"action": "update", "id": child, "status": "in_progress"}),
                &ctx,
            )
            .await
            .unwrap();
        assert!(
            matches!(r, ToolResult::Error(_)),
            "cancelled blocker must not count as done",
        );
    }
}
