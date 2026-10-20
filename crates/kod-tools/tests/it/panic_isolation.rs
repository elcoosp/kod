//! A tool that panics must not take the process (or the turn) down.
//!
//! `ToolRegistry::execute_tool` awaits `Tool::execute` directly. In a
//! parallel round the calls are collected and awaited with
//! `futures::future::join_all(...).await` in the engine's own task —
//! there is no spawn boundary between the registry and the caller. A
//! panic inside any one tool therefore unwinds through the registry
//! and out of the engine's task, killing the turn. The swarm runner
//! wraps agent bodies in `catch_unwind` (swarm_runner.rs:1630); the
//! registry does not.
//!
//! A model can invoke ~40 tools per round and the tool surface is
//! open (MCP servers, user plugins). One tool with a `panic!` on a
//! malformed argument should produce a tool error the model can see,
//! not a dead session.
//!
//! These tests pin the isolation contract:
//!
//! 1. `panicking_tool_returns_an_error_not_an_unwind`
//! 2. `panicking_tool_in_a_parallel_batch_does_not_kill_siblings`
//! 3. `panicking_tool_error_names_the_tool`

use async_trait::async_trait;
use kod_tools::{Tool, ToolContext, ToolRegistry};
use kod_types::{ToolCategory, ToolDefinition, ToolId, ToolPermissions, ToolResult};
use std::sync::Arc;

/// A tool whose `execute` panics unconditionally. The whole point of
/// this file.
struct PanickingTool;

#[async_trait]
impl Tool for PanickingTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            id: ToolId::new(),
            name: "panics".to_string(),
            description: "always panics".to_string(),
            category: ToolCategory::System,
            parameters_schema: serde_json::json!({ "type": "object" }),
            permissions: ToolPermissions::default(),
            trust_level: Default::default(),
            load_mode: Default::default(),
        }
    }

    async fn execute(
        &self,
        _params: &serde_json::Value,
        _context: &ToolContext,
    ) -> kod_error::Result<ToolResult> {
        panic!("panicking tool: deliberate unwind for the isolation test");
    }
}

/// A well-behaved tool, so a parallel batch has a sibling that must
/// survive.
struct OkTool;

#[async_trait]
impl Tool for OkTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            id: ToolId::new(),
            name: "works".to_string(),
            description: "returns success".to_string(),
            category: ToolCategory::System,
            parameters_schema: serde_json::json!({ "type": "object" }),
            permissions: ToolPermissions::default(),
            trust_level: Default::default(),
            load_mode: Default::default(),
        }
    }

    async fn execute(
        &self,
        _params: &serde_json::Value,
        _context: &ToolContext,
    ) -> kod_error::Result<ToolResult> {
        Ok(ToolResult::Success(serde_json::json!({ "ok": true })))
    }
}

fn ctx() -> ToolContext {
    ToolContext::new(std::path::PathBuf::from("/tmp"))
}

#[tokio::test]
async fn panicking_tool_returns_an_error_not_an_unwind() {
    let reg = ToolRegistry::new();
    reg.register(Box::new(PanickingTool)).await;

    let out = reg
        .execute_tool("panics", &serde_json::json!({}), &ctx())
        .await;

    match out {
        Ok(ToolResult::Error(msg)) => {
            assert!(
                !msg.is_empty(),
                "an isolated panic must produce a non-empty error the model can read",
            );
        }
        Ok(ToolResult::Success(_)) => {
            panic!("a panicking tool must never report Success");
        }
        Ok(other) => panic!("unexpected ToolResult variant: {other:?}"),
        Err(e) => {
            panic!("a panicking tool must return a ToolResult::Error, not Err: {e:?}");
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn panicking_tool_in_a_parallel_batch_does_not_kill_siblings() {
    // Mirror the engine's parallel round: `join_all` over several
    // `execute_tool` futures, awaited in one task. If the registry
    // does not isolate the panic, this `join_all` unwinds and neither
    // future's result is observed.
    let reg = Arc::new(ToolRegistry::new());
    reg.register(Box::new(PanickingTool)).await;
    reg.register(Box::new(OkTool)).await;

    let names = ["works", "panics", "works"];
    let futs: Vec<_> = names
        .iter()
        .map(|n| {
            let reg = Arc::clone(&reg);
            let name = n.to_string();
            async move {
                let r = reg
                    .execute_tool(&name, &serde_json::json!({}), &ctx())
                    .await;
                (name, r)
            }
        })
        .collect();
    let done = futures::future::join_all(futs).await;

    // Every future resolved. The two `works` calls are Success; the
    // `panics` call is an Error, not an unwind, not a cancellation.
    assert_eq!(done.len(), 3);
    let successes = done
        .iter()
        .filter(|(_, r)| matches!(r, Ok(ToolResult::Success(_))))
        .count();
    let errors = done
        .iter()
        .filter(|(_, r)| matches!(r, Ok(ToolResult::Error(_))))
        .count();
    assert_eq!(
        successes, 2,
        "the well-behaved tools must survive a sibling's panic; got {done:?}",
    );
    assert_eq!(
        errors, 1,
        "the panicking tool must yield exactly one ToolResult::Error; got {done:?}",
    );
}

#[tokio::test]
async fn panicking_tool_error_names_the_tool() {
    let reg = ToolRegistry::new();
    reg.register(Box::new(PanickingTool)).await;

    let out = reg
        .execute_tool("panics", &serde_json::json!({}), &ctx())
        .await
        .expect("registry must not propagate the panic as Err");
    match out {
        ToolResult::Error(msg) => {
            assert!(
                msg.contains("panics"),
                "the error must name the tool so the model can correct its call; got {msg:?}",
            );
        }
        other => panic!("expected ToolResult::Error, got {other:?}"),
    }
}
