//! Tool definitions and calling types.

use crate::ids::ToolId;
use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolDefinition {
    pub id: ToolId,
    pub name: String,
    pub description: String,
    pub category: ToolCategory,
    pub parameters_schema: Value,
    pub permissions: ToolPermissions,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum ToolCategory {
    FileSystem,
    Git,
    Web,
    Code,
    System,
}

/// Git capability level. Replaces the pre-D3 `git_operations: bool`.
///
/// - `None`: no git tool runs. The default.
/// - `Read`: `git_status` / `git_diff` (and `git_branch list`) run;
///   nothing that mutates the index, worktree, or refs.
/// - `Write`: everything `Read` allows, plus `git_commit` and
///   `git_branch create`. The `.git` directory is not read-only at the
///   sandbox for these tools — they intentionally bypass the sandbox
///   and are the approved write path (AD-10 + the `[git]` policy
///   section).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum GitAccess {
    #[default]
    None,
    Read,
    Write,
}

impl GitAccess {
    pub fn is_at_least(self, required: GitAccess) -> bool {
        self >= required
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ToolPermissions {
    pub read_files: bool,
    pub write_files: bool,
    pub execute_commands: bool,
    pub network_access: bool,
    pub git_access: GitAccess,
    pub allowed_paths: Vec<String>,
    pub forbidden_paths: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolCall {
    /// Wire-level id the provider assigned to this call, when
    /// available (OpenAI's `tool_calls[i].id`, Anthropic's
    /// `tool_use.id`). The engine preserves it through the tool
    /// round so a `Role::Tool` message can be linked back to its
    /// originating call. `None` for a locally constructed call or a
    /// provider that does not emit ids.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    pub tool_name: String,
    pub arguments: Value,
}

impl ToolCall {
    /// Construct a call with no wire id. Convenience for tests,
    /// hand-built calls, and every site that predates AD-03.
    pub fn new(tool_name: impl Into<String>, arguments: Value) -> Self {
        Self {
            id: None,
            tool_name: tool_name.into(),
            arguments,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum ToolResult {
    Success(Value),
    Error(String),
    RequiresConfirmation {
        description: String,
        callback_id: String,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolExecution {
    pub call: ToolCall,
    pub result: ToolResult,
    pub execution_time_ms: u64,
    pub timestamp: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_tool_call_serialization() {
        let call = ToolCall {
            id: None,
            tool_name: "read_file".to_string(),
            arguments: serde_json::json!({"path": "/test.rs"}),
        };

        let json = serde_json::to_string(&call).unwrap();
        let deserialized: ToolCall = serde_json::from_str(&json).unwrap();
        assert_eq!(call.tool_name, deserialized.tool_name);
    }
}
