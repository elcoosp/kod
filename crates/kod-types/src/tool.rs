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

#[cfg(test)]
mod coverage_git_access_and_calls {
    //! `GitAccess`'s derived `Ord` is the whole reason
    //! `is_at_least` works: the variants are ordered None < Read <
    //! Write, and a reordering silently grants or denies every git
    //! tool. `ToolCall`'s serde shape is what makes a legacy
    //! transcript parse without a spurious `"id": null` and a
    //! modern one carry the wire id. Both are pinned here.
    use super::*;

    #[test]
    fn git_access_orders_none_read_write() {
        assert!(GitAccess::None < GitAccess::Read);
        assert!(GitAccess::Read < GitAccess::Write);
        assert!(GitAccess::None < GitAccess::Write);
    }

    #[test]
    fn git_access_is_at_least_matches_the_ordering() {
        assert!(GitAccess::Write.is_at_least(GitAccess::Read));
        assert!(GitAccess::Write.is_at_least(GitAccess::Write));
        assert!(GitAccess::Read.is_at_least(GitAccess::Read));
        assert!(!GitAccess::Read.is_at_least(GitAccess::Write));
        assert!(!GitAccess::None.is_at_least(GitAccess::Read));
        assert!(GitAccess::None.is_at_least(GitAccess::None));
    }

    #[test]
    fn git_access_default_is_the_safe_direction() {
        // Defaulting to Read would silently enable `git_status` for
        // every context built without permissions.
        assert_eq!(GitAccess::default(), GitAccess::None);
    }

    #[test]
    fn tool_permissions_default_is_all_off() {
        let p = ToolPermissions::default();
        assert!(!p.read_files);
        assert!(!p.write_files);
        assert!(!p.execute_commands);
        assert!(!p.network_access);
        assert_eq!(p.git_access, GitAccess::None);
        assert!(p.allowed_paths.is_empty());
        assert!(p.forbidden_paths.is_empty());
    }

    #[test]
    fn tool_call_new_sets_no_id() {
        let c = ToolCall::new("read_file", serde_json::json!({"path": "a"}));
        assert!(c.id.is_none());
        assert_eq!(c.tool_name, "read_file");
        assert_eq!(c.arguments["path"], "a");
    }

    #[test]
    fn tool_call_omits_id_from_json_when_none() {
        // `skip_serializing_if = "Option::is_none"` keeps a legacy
        // transcript's JSON free of a spurious null. A regression
        // that emitted `"id": null` would break every consumer that
        // checks for presence.
        let c = ToolCall::new("t", serde_json::json!({}));
        let json = serde_json::to_string(&c).unwrap();
        assert!(!json.contains("\"id\""), "id key present: {json}");
        let c = ToolCall {
            id: Some("call_1".into()),
            tool_name: "t".into(),
            arguments: serde_json::json!({}),
        };
        let json = serde_json::to_string(&c).unwrap();
        assert!(json.contains("\"id\":\"call_1\""), "id key missing: {json}");
    }

    #[test]
    fn tool_call_deserializes_without_id_key() {
        let json = r#"{"tool_name":"t","arguments":{}}"#;
        let c: ToolCall = serde_json::from_str(json).unwrap();
        assert!(c.id.is_none());
    }

    #[test]
    fn tool_result_variants_round_trip_through_json() {
        for r in [
            ToolResult::Success(serde_json::json!({"x": 1})),
            ToolResult::Error("boom".into()),
            ToolResult::RequiresConfirmation {
                description: "confirm".into(),
                callback_id: "cb".into(),
            },
        ] {
            let json = serde_json::to_string(&r).unwrap();
            let parsed: ToolResult = serde_json::from_str(&json).unwrap();
            let re = serde_json::to_string(&parsed).unwrap();
            assert_eq!(json, re);
        }
    }
}
