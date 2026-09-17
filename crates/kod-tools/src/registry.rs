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
    tools: RwLock<std::collections::HashMap<String, Box<dyn Tool>>>,
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
        tools.insert(name, tool);
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

    /// Deprecated alias for [`ToolRegistry::has`]. The name `get` was
    /// misleading: the method returns `bool`, not the tool. New code
    /// should call `has`.
    #[deprecated(note = "use `has` — the method returns bool, not the tool")]
    pub async fn get(&self, name: &str) -> bool {
        self.has(name).await
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

        tools
            .values()
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
        defs
    }

    /// Execute a tool by name
    pub async fn execute_tool(
        &self,
        name: &str,
        params: &serde_json::Value,
        context: &ToolContext,
    ) -> Result<kod_types::ToolResult> {
        // Clone the tool's `Arc`? Tools live behind `Box<dyn Tool>` in the
        // map; we hold the read lock across the await. That is safe
        // because `register` is the only writer and it runs at startup
        // (and on MCP reload, which is serialized). If a future caller
        // needs concurrent registration during execution, wrap the map
        // in `Arc<dyn Tool>` values and clone the handle out before
        // awaiting.
        let tools = self.tools.read().await;
        match tools.get(name) {
            Some(t) => t.execute(params, context).await,
            None => Err(KodError::ToolNotFound {
                tool_name: name.to_string(),
            }),
        }
    }
}
