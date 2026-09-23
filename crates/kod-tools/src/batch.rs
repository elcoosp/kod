//! `batch`: run several tool calls in one model round.
//!
//! A model that wants to read three files currently issues three tool
//! calls across three rounds — each round a full provider round-trip
//! with the growing context. `batch` collapses that: one call, N
//! sub-calls, N results in the original order.
//!
//! Execution is **sequential** in this first landing. The notebook's
//! design (§4.5) runs read-only sub-calls concurrently, but the engine
//! already runs an all-read-only round in parallel at a higher level
//! (`run_tool_calls` detects a read-only round and parallelizes it).
//! A `batch` of read-only calls therefore gets its parallelism for
//! free when the *outer* round is read-only, and a `batch` that mixes
//! reads and writes would have to serialize anyway. Sequential is the
//! correct default; concurrency here would duplicate a mechanism the
//! engine already has.
//!
//! The sub-call budget is fixed: `MAX_INVOCATIONS`. Beyond that the
//! tool errors rather than silently truncating — a model that asked
//! for 30 calls and got 10 results would believe 20 more ran.

use crate::{Tool, ToolContext, ToolRegistry};
use kod_error::{KodError, Result};
use kod_types::{ToolCategory, ToolDefinition, ToolId, ToolPermissions, ToolResult};
use serde_json::Value;
use std::sync::Weak;

/// Hard cap on sub-calls in one `batch`. Ten is the notebook's number:
/// enough to fold a multi-file read into one round, few enough that the
/// combined output stays inside a normal tool-result budget.
pub const MAX_INVOCATIONS: usize = 10;

pub struct BatchTool {
    pub definition: ToolDefinition,
    registry: Weak<ToolRegistry>,
}

impl BatchTool {
    pub fn new(registry: Weak<ToolRegistry>) -> Self {
        Self {
            definition: ToolDefinition {
                trust_level: kod_types::trust::TrustLevel::default(),
                id: ToolId::new(),
                name: "batch".to_string(),
                description: format!(
                    "Run up to {MAX_INVOCATIONS} tool calls in one step. \
                     Each invocation names a tool and its parameters; results \
                     come back in the same order. Use it to read several files, \
                     or run several independent searches, without paying a \
                     round-trip per call."
                ),
                category: ToolCategory::System,
                parameters_schema: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "invocations": {
                            "type": "array",
                            "maxItems": MAX_INVOCATIONS,
                            "items": {
                                "type": "object",
                                "properties": {
                                    "tool": { "type": "string" },
                                    "parameters": { "type": "object" }
                                },
                                "required": ["tool"]
                            }
                        }
                    },
                    "required": ["invocations"]
                }),
                permissions: ToolPermissions {
                    // A batch inherits the union of its sub-calls'
                    // permissions at execution time; the tool-level
                    // bitmask only needs to permit dispatch. The
                    // policy engine and each sub-tool's own
                    // `can_read`/`can_write` are the real gates.
                    read_files: true,
                    write_files: false,
                    execute_commands: false,
                    network_access: false,
                    git_access: kod_types::GitAccess::None,
                    allowed_paths: Vec::new(),
                    forbidden_paths: Vec::new(),
                },
            },
            registry,
        }
    }
}

impl Default for BatchTool {
    fn default() -> Self {
        // A tool with no registry can be constructed (the engine always
        // supplies one), but any call fails cleanly rather than
        // panicking.
        Self::new(Weak::new())
    }
}

#[async_trait::async_trait]
impl Tool for BatchTool {
    fn definition(&self) -> ToolDefinition {
        self.definition.clone()
    }

    async fn execute(&self, params: &Value, context: &ToolContext) -> Result<ToolResult> {
        let invocations = match params.get("invocations").and_then(Value::as_array) {
            Some(v) => v,
            None => {
                return Ok(ToolResult::Error(
                    "batch: missing 'invocations' array".to_string(),
                ));
            }
        };
        if invocations.is_empty() {
            return Ok(ToolResult::Error(
                "batch: 'invocations' is empty".to_string(),
            ));
        }
        if invocations.len() > MAX_INVOCATIONS {
            return Ok(ToolResult::Error(format!(
                "batch: {} invocations exceeds the cap of {MAX_INVOCATIONS}; \
                 split into multiple batches",
                invocations.len(),
            )));
        }

        let Some(registry) = self.registry.upgrade() else {
            return Ok(ToolResult::Error(
                "batch: registry unavailable (engine shutting down?)".to_string(),
            ));
        };

        let mut results: Vec<Value> = Vec::with_capacity(invocations.len());
        for (i, inv) in invocations.iter().enumerate() {
            let name = match inv.get("tool").and_then(Value::as_str) {
                Some(n) => n,
                None => {
                    results.push(serde_json::json!({
                        "index": i,
                        "error": "missing 'tool' field",
                    }));
                    continue;
                }
            };
            if name == "batch" {
                // A nested batch would recurse without bound and its
                // failure mode (a stack of partial results) is worse
                // than a flat rejection.
                results.push(serde_json::json!({
                    "index": i,
                    "tool": name,
                    "error": "batch cannot be nested",
                }));
                continue;
            }
            let args = inv.get("parameters").cloned().unwrap_or(Value::Null);

            // Sub-calls run through the same execution path a top-level
            // call takes, so the policy gate, permission checks, path
            // locks, and file-touch hook all apply without the batch
            // tool re-implementing any of them.
            match registry.execute_tool(name, &args, context).await {
                Ok(ToolResult::Success(v)) => {
                    results.push(serde_json::json!({
                        "index": i,
                        "tool": name,
                        "success": v,
                    }));
                }
                Ok(ToolResult::Error(e)) => {
                    results.push(serde_json::json!({
                        "index": i,
                        "tool": name,
                        "error": e,
                    }));
                }
                Ok(ToolResult::RequiresConfirmation { description, callback_id }) => {
                    // A sub-call that needs approval cannot be approved
                    // interactively from inside a batch; surface it so
                    // the model can re-issue that one call on its own.
                    results.push(serde_json::json!({
                        "index": i,
                        "tool": name,
                        "requires_confirmation": description,
                        "callback_id": callback_id,
                    }));
                }
                Err(e) => {
                    results.push(serde_json::json!({
                        "index": i,
                        "tool": name,
                        "error": format!("{e}"),
                    }));
                }
            }
        }

        Ok(ToolResult::Success(serde_json::json!({
            "results": results,
        })))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn missing_invocations_is_a_tool_error() {
        let tool = BatchTool::default();
        let ctx = ToolContext::new(std::env::temp_dir());
        let r = tool.execute(&serde_json::json!({}), &ctx).await.unwrap();
        assert!(matches!(r, ToolResult::Error(_)));
    }

    #[tokio::test]
    async fn empty_invocations_is_a_tool_error() {
        let tool = BatchTool::default();
        let ctx = ToolContext::new(std::env::temp_dir());
        let r = tool
            .execute(&serde_json::json!({"invocations": []}), &ctx)
            .await
            .unwrap();
        assert!(matches!(r, ToolResult::Error(_)));
    }

    #[tokio::test]
    async fn over_the_cap_is_a_tool_error_naming_the_cap() {
        let tool = BatchTool::default();
        let ctx = ToolContext::new(std::env::temp_dir());
        let many: Vec<Value> = (0..MAX_INVOCATIONS + 1)
            .map(|_| serde_json::json!({"tool": "read_file"}))
            .collect();
        let r = tool
            .execute(&serde_json::json!({"invocations": many}), &ctx)
            .await
            .unwrap();
        match r {
            ToolResult::Error(msg) => {
                assert!(msg.contains(&MAX_INVOCATIONS.to_string()), "got: {msg}");
            }
            other => panic!("expected Error, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn no_registry_fails_cleanly() {
        let tool = BatchTool::default();
        let ctx = ToolContext::new(std::env::temp_dir());
        let r = tool
            .execute(
                &serde_json::json!({"invocations": [{"tool": "read_file"}]}),
                &ctx,
            )
            .await
            .unwrap();
        match r {
            ToolResult::Error(msg) => assert!(msg.contains("registry")),
            other => panic!("expected Error, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn nested_batch_is_rejected_per_index() {
        let registry = std::sync::Arc::new(ToolRegistry::new());
        let tool = BatchTool::new(std::sync::Arc::downgrade(&registry));
        let ctx = ToolContext::new(std::env::temp_dir());
        let r = tool
            .execute(
                &serde_json::json!({"invocations": [{"tool": "batch"}]}),
                &ctx,
            )
            .await
            .unwrap();
        match r {
            ToolResult::Success(v) => {
                let results = v.get("results").unwrap().as_array().unwrap();
                assert_eq!(results.len(), 1);
                assert!(results[0]["error"].as_str().unwrap().contains("nested"));
            }
            other => panic!("expected Success, got {other:?}"),
        }
    }
}
