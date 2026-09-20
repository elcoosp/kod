//! `plan_update`: apply a structured update to the session's plan
//! (Tier 2.1).
//!
//! The engine intercepts calls to this tool the same way it
//! intercepts `ask_user` — the tool itself cannot reach the engine
//! state, so `execute` returns an error if called directly. The
//! engine applies the update against its per-transcript plan and
//! returns the description as the tool result.

use crate::{Tool, ToolContext};
use kod_error::Result;
use kod_types::{ToolCategory, ToolDefinition, ToolId, ToolPermissions, ToolResult};
use serde_json::Value;

/// See the module docs.
pub struct PlanTool {
    pub definition: ToolDefinition,
}

impl PlanTool {
    pub fn new() -> Self {
        Self {
            definition: ToolDefinition {
                id: ToolId::new(),
                name: "plan_update".to_string(),
                description: "Update the session's plan. Use this to mark the \
                    current step done, add a note, insert a step, or remove \
                    a step. Keep the plan in sync with what you are actually \
                    doing so the user can see progress."
                    .to_string(),
                category: ToolCategory::System,
                parameters_schema: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "action": {
                            "type": "string",
                            "enum": ["advance", "annotate", "insert", "remove", "replace", "set_status"],
                            "description": "Which update to apply."
                        },
                        "step_id": {"type": "integer"},
                        "after_id": {"type": "integer"},
                        "text": {"type": "string"},
                        "note": {"type": "string"},
                        "status": {
                            "type": "string",
                            "enum": ["pending", "in_progress", "done", "blocked", "skipped"]
                        }
                    },
                    "required": ["action"],
                    "additionalProperties": false
                }),
                permissions: ToolPermissions::default(),
                trust_level: kod_types::trust::TrustLevel::ToolTrusted,
            },
        }
    }
}

impl Default for PlanTool {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait::async_trait]
impl Tool for PlanTool {
    fn definition(&self) -> ToolDefinition {
        self.definition.clone()
    }

    async fn execute(&self, _params: &Value, _context: &ToolContext) -> Result<ToolResult> {
        Ok(ToolResult::Error(
            "plan_update requires the engine; the engine intercepts this call".to_string(),
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn definition_has_the_expected_shape() {
        let t = PlanTool::new();
        assert_eq!(t.definition.name, "plan_update");
        assert_eq!(t.definition.category, ToolCategory::System);
        assert_eq!(
            t.definition.trust_level,
            kod_types::trust::TrustLevel::ToolTrusted,
        );
    }

    #[tokio::test]
    async fn execute_without_engine_errors() {
        let t = PlanTool::new();
        let ctx = ToolContext::new(std::path::PathBuf::from("/tmp"));
        let r = t.execute(&serde_json::json!({}), &ctx).await.unwrap();
        assert!(matches!(r, ToolResult::Error(_)));
    }
}
