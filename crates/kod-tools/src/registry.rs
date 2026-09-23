//! Tool registry - manages available tools and their definitions.

use crate::Tool;
use crate::context::ToolContext;
use kod_error::{KodError, Result};
use kod_types::{ToolCategory, ToolDefinition};
use tokio::sync::RwLock;

/// Registry that manages all available tools
#[derive(Default)]
pub struct ToolRegistry {
    /// Indexed by tool name. Insertion order is not preserved; callers
    /// that need a stable listing should sort. The previous
    /// `Vec<Box<dyn Tool>>` allowed duplicate names (the first match won
    /// on lookup) and made every `execute_tool` O(n) in the number of
    /// tools.
    // H-R10: values are `Arc<dyn Tool>` so `execute_tool` can clone
    // the handle out and drop the lock before awaiting — the pre-fix
    // shape held the read lock across the entire tool call, so a
    // 120 s `check` blocked any `register` (MCP hot-reload).
    tools: RwLock<std::collections::HashMap<String, std::sync::Arc<dyn Tool>>>,
}

impl ToolRegistry {
    pub fn new() -> Self {
        Self {
            tools: RwLock::new(std::collections::HashMap::new()),
        }
    }

    /// Register (or replace) a tool. Idempotent by name: registering a
    /// tool whose name is already present replaces the previous
    /// instance. This is what makes MCP tool hot-reload safe (D6.1):
    /// a re-spawned server re-registers its tools, and the old
    /// implementations are dropped.
    pub async fn register(&self, tool: Box<dyn Tool>) {
        let name = tool.definition().name;
        let mut tools = self.tools.write().await;
        tools.insert(name, std::sync::Arc::from(tool));
    }

    /// Remove a tool by name. Returns true if a tool was removed.
    pub async fn remove(&self, name: &str) -> bool {
        let mut tools = self.tools.write().await;
        tools.remove(name).is_some()
    }

    /// Check if a tool exists by name.
    pub async fn has(&self, name: &str) -> bool {
        self.tools.read().await.contains_key(name)
    }

    /// List all tool names, sorted for stable presentation.
    pub async fn list_all(&self) -> Vec<String> {
        let tools = self.tools.read().await;
        let mut names: Vec<String> = tools.keys().cloned().collect();
        names.sort();
        names
    }

    /// List tools by category, sorted.
    pub async fn list_by_category(&self, category: ToolCategory) -> Vec<String> {
        let tools = self.tools.read().await;
        let mut names: Vec<String> = tools
            .values()
            .filter(|t| t.definition().category == category)
            .map(|t| t.definition().name.clone())
            .collect();
        names.sort();
        names
    }

    /// Get tool count
    pub async fn count(&self) -> usize {
        self.tools.read().await.len()
    }

    /// Get tool definitions formatted for LLM function calling
    pub async fn get_definitions_for_llm(&self) -> Vec<serde_json::Value> {
        let tools = self.tools.read().await;

        // H-R10: sort by name. HashMap iteration order is not
        // stable across process restarts, so the LLM's tool list
        // would shuffle from one run to the next — prompt
        // instability, prefix-cache misses.
        let mut entries: Vec<_> = tools.values().collect();
        entries.sort_by_key(|t| t.definition().name);
        entries
            .into_iter()
            .map(|tool| {
                let def = tool.definition();
                serde_json::json!({
                    "type": "function",
                    "function": {
                        "name": def.name,
                        "description": def.description,
                        "parameters": def.parameters_schema,
                    }
                })
            })
            .collect()
    }

    /// Get tool permissions by name
    pub async fn get_permissions(&self, name: &str) -> Option<kod_types::ToolPermissions> {
        let tools = self.tools.read().await;
        tools.get(name).map(|t| t.definition().permissions)
    }

    /// Get all tool definitions. Sorted by name for reproducibility:
    /// HashMap iteration order varies between runs and would make prompt
    /// diffs unstable.
    pub async fn get_definitions(&self) -> Vec<ToolDefinition> {
        let tools = self.tools.read().await;
        let mut defs: Vec<ToolDefinition> = tools.values().map(|t| t.definition()).collect();
        defs.sort_by(|a, b| a.name.cmp(&b.name));
        // P3-d: every schema gets an optional `intent` field. One
        // injection point keeps the wording identical across all
        // tools, and an MCP proxy that brings its own schema gets the
        // same field without the adapter knowing.
        for def in &mut defs {
            inject_intent_field(&mut def.parameters_schema);
        }
        defs
    }

    /// Execute a tool by name
    pub async fn execute_tool(
        &self,
        name: &str,
        params: &serde_json::Value,
        context: &ToolContext,
    ) -> Result<kod_types::ToolResult> {
        // Resolve a model-supplied alias to the name kod registered
        // (`shell_exec` → `execute_command`). A name that is already
        // real passes through unchanged.
        let resolved = crate::aliases::resolve_tool_name(name);

        // H-R10: clone the Arc out and drop the read lock before
        // awaiting the tool body. Registration (MCP hot-reload) no
        // longer blocks on a running tool call.
        let tool = { self.tools.read().await.get(resolved).cloned() };
        match tool {
            Some(t) => t.execute(params, context).await,
            None => Err(KodError::ToolNotFound {
                // The error names what the model sent, not the
                // resolved form — the model needs to see its own
                // call to correct it.
                tool_name: name.to_string(),
            }),
        }
    }
}

/// Add an optional `intent` string property to a tool's parameters
/// schema, if it is not already there.
///
/// The wording is terse on purpose: this rides on every tool schema of
/// every request, so a sentence here is a sentence per tool per turn.
/// `intent` is surfaced to the UI and to the swarm file-touch bus — a
/// peer seeing "agent-1 edited lines 18-25" also sees *why*.
pub fn inject_intent_field(schema: &mut serde_json::Value) {
    let Some(props) = schema.get_mut("properties").and_then(|p| p.as_object_mut()) else {
        return;
    };
    props.entry("intent").or_insert_with(|| {
        serde_json::json!({
            "type": "string",
            "description": "Short label: why this call is being made.",
        })
    });
}

