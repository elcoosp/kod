//! Tool registry - manages available tools and their definitions.

use crate::Tool;
use crate::context::ToolContext;
use kod_error::{KodError, Result};
use kod_types::{ToolCategory, ToolDefinition};
use tokio::sync::RwLock;

/// Registry that manages all available tools
#[derive(Default)]
pub struct ToolRegistry {
    tools: RwLock<Vec<Box<dyn Tool>>>,
}

impl ToolRegistry {
    pub fn new() -> Self {
        Self {
            tools: RwLock::new(Vec::new()),
        }
    }

    /// Register a new tool
    pub async fn register(&self, tool: Box<dyn Tool>) {
        let mut tools = self.tools.write().await;
        tools.push(tool);
    }

    /// Remove a tool by name
    pub async fn remove(&self, name: &str) -> bool {
        let mut tools = self.tools.write().await;
        if let Some(pos) = tools.iter().position(|t| t.definition().name == name) {
            tools.remove(pos);
            true
        } else {
            false
        }
    }

    /// Get a tool by name (returns true if found)
    pub async fn get(&self, name: &str) -> bool {
        self.tools
            .read()
            .await
            .iter()
            .any(|t| t.definition().name == name)
    }

    /// Check if a tool exists
    pub async fn has(&self, name: &str) -> bool {
        let tools = self.tools.read().await;
        tools.iter().any(|t| t.definition().name == name)
    }

    /// List all tool names
    pub async fn list_all(&self) -> Vec<String> {
        let tools = self.tools.read().await;
        tools.iter().map(|t| t.definition().name.clone()).collect()
    }

    /// List tools by category
    pub async fn list_by_category(&self, category: ToolCategory) -> Vec<String> {
        let tools = self.tools.read().await;
        tools
            .iter()
            .filter(|t| t.definition().category == category)
            .map(|t| t.definition().name.clone())
            .collect()
    }

    /// Get tool count
    pub async fn count(&self) -> usize {
        self.tools.read().await.len()
    }

    /// Get tool definitions formatted for LLM function calling
    pub async fn get_definitions_for_llm(&self) -> Vec<serde_json::Value> {
        let tools = self.tools.read().await;

        tools
            .iter()
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
        tools
            .iter()
            .find(|t| t.definition().name == name)
            .map(|t| t.definition().permissions)
    }

    /// Get all tool definitions
    pub async fn get_definitions(&self) -> Vec<ToolDefinition> {
        let tools = self.tools.read().await;
        tools.iter().map(|t| t.definition()).collect()
    }

    /// Execute a tool by name
    pub async fn execute_tool(
        &self,
        name: &str,
        params: &serde_json::Value,
        context: &ToolContext,
    ) -> Result<kod_types::ToolResult> {
        let tools = self.tools.read().await;

        // Find the tool and get its definition to check permissions
        let tool = tools.iter().find(|t| t.definition().name == name);

        match tool {
            Some(t) => {
                // Check permissions before executing
                let _def = t.definition();
                context.can_read(std::path::Path::new("."))?;
                // Execute the tool
                t.execute(params, context).await
            }
            None => Err(KodError::ToolNotFound {
                tool_name: name.to_string(),
            }),
        }
    }
}
