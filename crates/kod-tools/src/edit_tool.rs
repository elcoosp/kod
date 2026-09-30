//! Delta §7.1: the `edit` tool — line-addressed, tag-guarded edits.
//!
//! See [`crate::edit_hashline`] for the store and the two guards. This
//! file is the tool wrapper: it parses the input, consults the store
//! on the `ToolContext`, and applies or rejects.

use async_trait::async_trait;
use kod_error::{KodError, Result};
use kod_types::{ToolCategory, ToolDefinition, ToolId, ToolPermissions, ToolResult};
use serde_json::Value;

use crate::edit_hashline::parse;
use crate::{Tool, ToolContext};

/// The hashline edit tool.
pub struct EditHashlineTool {
    pub definition: ToolDefinition,
}

impl EditHashlineTool {
    pub fn new() -> Self {
        Self {
            definition: ToolDefinition {
                trust_level: kod_types::trust::TrustLevel::default(),
                id: ToolId::new(),
                name: "edit".to_string(),
                description: "Apply line-addressed edits to a file that was \
                    read this session. The input is a hashline block: a \
                    `[path#tag]` header (the tag comes from the `read_file` \
                    output), then `PUT N.=M:` / `CUT N.=M` / `PUT >$:` ops \
                    with `+`-prefixed payload rows. An edit whose tag does \
                    not match the file's current content, or whose anchor \
                    line was never read, is rejected — re-read and retry. \
                    All ops apply or none do."
                    .to_string(),
                category: ToolCategory::FileSystem,
                parameters_schema: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "input": {
                            "type": "string",
                            "description": "The hashline edit block, starting with [path#tag]"
                        }
                    },
                    "required": ["input"]
                }),
                permissions: ToolPermissions {
                    read_files: true,
                    write_files: true,
                    execute_commands: false,
                    network_access: false,
                    git_access: kod_types::GitAccess::None,
                    allowed_paths: Vec::new(),
                    forbidden_paths: Vec::new(),
                },
                load_mode: Default::default(),
            },
        }
    }
}

impl Default for EditHashlineTool {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl Tool for EditHashlineTool {
    fn definition(&self) -> ToolDefinition {
        self.definition.clone()
    }

    async fn execute(&self, params: &Value, context: &ToolContext) -> Result<ToolResult> {
        let input = params["input"]
            .as_str()
            .ok_or_else(|| KodError::InvalidParameters {
                reason: "missing 'input' parameter".to_string(),
            })?;

        let Some(store) = &context.edit_store else {
            return Ok(ToolResult::Error(
                "the edit tool is not available in this session (no edit store); \
                 use patch_file or write_file"
                    .to_string(),
            ));
        };

        let (path_str, tag, ops) = match parse(input) {
            Ok(v) => v,
            Err(e) => return Ok(ToolResult::Error(format!("edit: {e}"))),
        };
        if ops.is_empty() {
            return Ok(ToolResult::Error(
                "edit: no operations (need PUT or CUT rows)".to_string(),
            ));
        }

        let resolved = context.resolve_path(&path_str)?;
        context.can_write(&resolved)?;

        // Serialize access to the store: the whole apply is under the
        // mutex so a concurrent edit cannot interleave between the
        // tag check and the write.
        let mut guard = store
            .lock()
            .map_err(|_| KodError::Internal("edit store poisoned".to_string()))?;
        match guard.apply(&resolved, tag, &ops) {
            Ok((new_tag, _text)) => {
                context.note_file_touch(
                    &resolved,
                    crate::context::FileOp::Edit,
                    params["intent"].as_str(),
                );
                Ok(ToolResult::Success(serde_json::json!({
                    "path": resolved.to_string_lossy(),
                    "ops": ops.len(),
                    "new_tag": crate::edit_hashline::tag_hex(new_tag),
                })))
            }
            Err(e) => Ok(ToolResult::Error(format!("edit rejected: {e}"))),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn edit_without_a_store_reports_cleanly() {
        let tmp = tempfile::TempDir::new().unwrap();
        let ctx = ToolContext::new(tmp.path());
        let tool = EditHashlineTool::new();
        assert_eq!(tool.definition().name, "edit");
        let rt = tokio::runtime::Runtime::new().unwrap();
        let r = rt.block_on(tool.execute(
            &serde_json::json!({"input": "[a.txt#0000]\nPUT 1.=1:\n+x"}),
            &ctx,
        ));
        match r.unwrap() {
            ToolResult::Error(e) => assert!(e.contains("not available"), "got: {e}"),
            other => panic!("expected an error, got {other:?}"),
        }
    }

    #[test]
    fn edit_applies_against_a_recorded_snapshot() {
        let tmp = tempfile::TempDir::new().unwrap();
        let f = tmp.path().join("a.txt");
        std::fs::write(&f, "one\ntwo\n").unwrap();
        let store = std::sync::Arc::new(std::sync::Mutex::new(
            crate::edit_hashline::EditStore::new(),
        ));
        // Record the snapshot as a `read_file` would.
        let tag = {
            let mut g = store.lock().unwrap();
            g.record_snapshot(&f, "one\ntwo\n", vec![true, true, true])
        };
        let tag_hex = crate::edit_hashline::tag_hex(tag);
        let mut ctx = ToolContext::new(tmp.path()).with_edit_store(store);
        ctx.permissions.write_files = true;
        let tool = EditHashlineTool::new();
        let input = format!("[a.txt#{tag_hex}]\nPUT 2.=2:\n+TWO\n");
        let rt = tokio::runtime::Runtime::new().unwrap();
        let r = rt
            .block_on(tool.execute(&serde_json::json!({ "input": input }), &ctx))
            .unwrap();
        match r {
            ToolResult::Success(_) => {
                assert_eq!(std::fs::read_to_string(&f).unwrap(), "one\nTWO\n");
            }
            other => panic!("expected success, got {other:?}"),
        }
    }

    #[test]
    fn a_stale_tag_is_reported_to_the_model() {
        let tmp = tempfile::TempDir::new().unwrap();
        let f = tmp.path().join("a.txt");
        std::fs::write(&f, "one\ntwo\n").unwrap();
        let store = std::sync::Arc::new(std::sync::Mutex::new(
            crate::edit_hashline::EditStore::new(),
        ));
        {
            let mut g = store.lock().unwrap();
            g.record_snapshot(&f, "one\ntwo\n", vec![true, true, true]);
        }
        let mut ctx = ToolContext::new(tmp.path()).with_edit_store(store);
        ctx.permissions.write_files = true;
        let tool = EditHashlineTool::new();
        // A wrong tag.
        let input = "[a.txt#ffff]\nPUT 1.=1:\n+X\n";
        let rt = tokio::runtime::Runtime::new().unwrap();
        let r = rt
            .block_on(tool.execute(&serde_json::json!({ "input": input }), &ctx))
            .unwrap();
        match r {
            ToolResult::Error(e) => assert!(e.contains("stale"), "got: {e}"),
            other => panic!("expected a stale error, got {other:?}"),
        }
        assert_eq!(std::fs::read_to_string(&f).unwrap(), "one\ntwo\n");
    }
}
