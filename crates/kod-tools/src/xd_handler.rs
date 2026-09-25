//! `xd://` handler: lazy tool mounting (borrow from oh-my-pi, delta §6).
//!
//! # What this is
//!
//! A tool marked [`LoadMode::Discoverable`] is removed from the
//! provider's tools array — its schema no longer costs prompt
//! budget on every request. It stays reachable through the
//! internal-URL router:
//!
//! * `read xd://` lists every discoverable tool by name.
//! * `read xd://<tool>` returns the tool's full schema and
//!   description.
//! * `write xd://<tool>` runs it with the JSON body as arguments.
//!
//! The model sees an unfamiliar tool name for the first time only
//! when it reads the schema, and the schema is exactly what a
//! `write` call needs. A schema-mismatch error returns the schema
//! again, so a malformed call self-corrects in one turn.
//!
//! # Why a scheme, not a `tool_search` tool
//!
//! The design's §6: a bespoke search tool costs a schema of its own
//! and teaches the model a second vocabulary. `read`/`write`
//! already exist and are already in the tools array; the scheme
//! dispatch costs nothing.
//!
//! # What this does NOT do
//!
//! * Not the demotion. The engine removes discoverable tools from
//!   the array it builds; this handler only *serves* them.
//! * Not the registry. The handler reads through an
//!   `Arc<ToolRegistry>` — the same one the ordinary path uses, so
//!   a tool registered late is reachable immediately.

use crate::internal_url::{ProtocolError, ProtocolHandler, ResolveContext, ResolvedResource};
use crate::registry::ToolRegistry;
use std::sync::Arc;

/// The `xd://` scheme handler.
pub struct XdHandler {
    registry: Arc<ToolRegistry>,
}

impl XdHandler {
    pub fn new(registry: Arc<ToolRegistry>) -> Self {
        Self { registry }
    }
}

#[async_trait::async_trait]
impl ProtocolHandler for XdHandler {
    fn scheme(&self) -> &'static str {
        "xd"
    }

    async fn resolve(
        &self,
        url: &str,
        _ctx: &ResolveContext,
    ) -> Result<ResolvedResource, ProtocolError> {
        let path = url.strip_prefix("xd://").ok_or_else(|| ProtocolError::Malformed {
            url: url.to_string(),
            reason: "expected `xd://` or `xd://<tool>`".to_string(),
        })?;

        // `read xd://` — the mount list.
        if path.is_empty() {
            let mut names = self.registry.list_all().await;
            names.sort();
            let json = serde_json::to_string_pretty(&serde_json::json!({
                "discoverable_tools": names,
                "note": "read xd://<name> for the schema, \
                         write xd://<name> with JSON args to run it",
            }))
            .unwrap_or_default();
            return Ok(ResolvedResource {
                text: json,
                mime: Some("application/json".to_string()),
                immutable: false,
            });
        }

        // `read xd://<tool>` — the schema.
        let defs = self.registry.get_definitions().await;
        let Some(def) = defs.into_iter().find(|d| d.name == path) else {
            return Err(ProtocolError::NotFound {
                url: url.to_string(),
            });
        };
        let json = serde_json::to_string_pretty(&serde_json::json!({
            "name": def.name,
            "description": def.description,
            "parameters": def.parameters_schema,
        }))
        .unwrap_or_default();
        Ok(ResolvedResource {
            text: json,
            mime: Some("application/json".to_string()),
            immutable: false,
        })
    }

    async fn write(
        &self,
        url: &str,
        content: &str,
        ctx: &ResolveContext,
    ) -> Result<(), ProtocolError> {
        let name = url.strip_prefix("xd://").ok_or_else(|| ProtocolError::Malformed {
            url: url.to_string(),
            reason: "expected `xd://<tool>`".to_string(),
        })?;
        if name.is_empty() {
            return Err(ProtocolError::Malformed {
                url: url.to_string(),
                reason: "`write xd://` needs a tool name".to_string(),
            });
        }
        if !self.registry.has(name).await {
            return Err(ProtocolError::NotFound {
                url: url.to_string(),
            });
        }
        let args: serde_json::Value = serde_json::from_str(content).map_err(|e| {
            ProtocolError::Malformed {
                url: url.to_string(),
                reason: format!("arguments are not JSON: {e}"),
            }
        })?;
        // Build a ToolContext from the ResolveContext. The permissions
        // are the caller's own, so a tool reached through xd:// gets
        // exactly what the ordinary path would grant it.
        let mut tool_ctx = crate::context::ToolContext::new(ctx.working_dir.clone())
            .with_locks(
                Arc::new(crate::path_lock::PathLockTable::new()),
                ctx.holder.clone(),
            );
        if let Some(perms) = ctx.tool_permissions.clone() {
            tool_ctx = tool_ctx.with_permissions(perms);
        }
        match self.registry.execute_tool(name, &args, &tool_ctx).await {
            Ok(_) => Ok(()),
            Err(e) => Err(ProtocolError::Handler {
                url: url.to_string(),
                message: format!("{e}"),
            }),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // A minimal registry with one echo tool. Verifies the read
    // (list + schema) and write (execute) paths without an engine.
    struct EchoTool;

    #[async_trait::async_trait]
    impl crate::Tool for EchoTool {
        fn definition(&self) -> kod_types::ToolDefinition {
            kod_types::ToolDefinition {
                id: kod_types::ToolId::new(),
                name: "echo".to_string(),
                description: "Echoes its input".to_string(),
                category: kod_types::ToolCategory::System,
                parameters_schema: serde_json::json!({
                    "type": "object",
                    "properties": { "text": { "type": "string" } }
                }),
                permissions: kod_types::ToolPermissions::default(),
                trust_level: kod_types::trust::TrustLevel::default(),
                load_mode: kod_types::LoadMode::Discoverable,
            }
        }
        async fn execute(
            &self,
            _params: &serde_json::Value,
            _ctx: &crate::ToolContext,
        ) -> kod_error::Result<kod_types::ToolResult> {
            Ok(kod_types::ToolResult::Success(serde_json::json!({"ok": true})))
        }
    }

    async fn handler() -> XdHandler {
        let reg = Arc::new(ToolRegistry::new());
        reg.register(Box::new(EchoTool)).await;
        XdHandler::new(reg)
    }

    fn rctx() -> ResolveContext {
        ResolveContext::new("session", "/tmp")
    }

    #[tokio::test]
    async fn reading_the_root_lists_tools() {
        let h = handler().await;
        let r = h.resolve("xd://", &rctx()).await.unwrap();
        assert!(r.text.contains("echo"), "got: {}", r.text);
    }

    #[tokio::test]
    async fn reading_a_tool_returns_its_schema() {
        let h = handler().await;
        let r = h.resolve("xd://echo", &rctx()).await.unwrap();
        assert!(r.text.contains("Echoes its input"), "got: {}", r.text);
        assert!(r.text.contains("parameters"), "got: {}", r.text);
    }

    #[tokio::test]
    async fn reading_an_unknown_tool_is_not_found() {
        let h = handler().await;
        let e = h.resolve("xd://nope", &rctx()).await.unwrap_err();
        assert!(matches!(e, ProtocolError::NotFound { .. }));
    }

    #[tokio::test]
    async fn writing_a_tool_runs_it() {
        let h = handler().await;
        let r = h
            .write("xd://echo", r#"{"text": "hi"}"#, &rctx())
            .await;
        assert!(r.is_ok(), "expected Ok, got {r:?}");
    }

    #[tokio::test]
    async fn writing_an_unknown_tool_is_not_found() {
        let h = handler().await;
        let e = h
            .write("xd://nope", "{}", &rctx())
            .await
            .unwrap_err();
        assert!(matches!(e, ProtocolError::NotFound { .. }));
    }

    #[tokio::test]
    async fn writing_invalid_json_is_malformed() {
        let h = handler().await;
        let e = h
            .write("xd://echo", "not json", &rctx())
            .await
            .unwrap_err();
        assert!(matches!(e, ProtocolError::Malformed { .. }));
    }

    #[tokio::test]
    async fn writing_to_the_root_is_malformed() {
        let h = handler().await;
        let e = h.write("xd://", "{}", &rctx()).await.unwrap_err();
        assert!(matches!(e, ProtocolError::Malformed { .. }));
    }
}
