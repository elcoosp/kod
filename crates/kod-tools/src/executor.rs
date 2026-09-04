//! Tool executor - executes tools with timeout and error handling.

use crate::Tool;
use crate::context::ToolContext;
use kod_error::{KodError, Result};
use kod_types::ToolResult;
use std::time::Duration;
use tokio::time::timeout;

/// Executes tool calls with timeout handling
pub struct ToolExecutor {
    context: ToolContext,
}

impl ToolExecutor {
    pub fn new(context: ToolContext) -> Self {
        Self { context }
    }

    /// Execute a single tool call
    pub async fn execute(&self, tool: &dyn Tool, params: &serde_json::Value) -> Result<ToolResult> {
        let tool_name = tool.definition().name.clone();
        let timeout_secs = self.context.timeout_secs;

        let result = timeout(
            Duration::from_secs(timeout_secs),
            tool.execute(params, &self.context),
        )
        .await
        .map_err(|_| KodError::ProviderTimeout {
            timeout_ms: timeout_secs * 1000,
        })?;

        result.map_err(|e| KodError::ToolExecution {
            tool_name,
            reason: e.to_string(),
        })
    }

    /// Execute multiple tool calls in parallel
    pub async fn execute_batch(
        &self,
        tools: &[(&dyn Tool, &serde_json::Value)],
    ) -> Vec<Result<ToolResult>> {
        let mut results = Vec::new();

        for (tool, params) in tools {
            let context = self.context.clone();
            let tool_name = tool.definition().name.clone();
            let timeout_secs = context.timeout_secs;

            let result = tokio::time::timeout(
                Duration::from_secs(timeout_secs),
                tool.execute(params, &context),
            )
            .await;

            match result {
                Ok(Ok(r)) => results.push(Ok(r)),
                Ok(Err(e)) => {
                    results.push(Err(KodError::ToolExecution {
                        tool_name: tool_name.clone(),
                        reason: e.to_string(),
                    }));
                }
                Err(_) => {
                    results.push(Err(KodError::ProviderTimeout {
                        timeout_ms: timeout_secs * 1000,
                    }));
                }
            }
        }

        results
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use kod_types::{ToolCategory, ToolDefinition, ToolId, ToolPermissions};

    struct SlowTool;

    #[async_trait]
    impl Tool for SlowTool {
        fn definition(&self) -> ToolDefinition {
            ToolDefinition {
                id: ToolId::new(),
                name: "slow".to_string(),
                description: "Slow tool".to_string(),
                category: ToolCategory::System,
                parameters_schema: serde_json::json!({}),
                permissions: ToolPermissions::default(),
            }
        }

        async fn execute(
            &self,
            _params: &serde_json::Value,
            _context: &ToolContext,
        ) -> Result<ToolResult> {
            tokio::time::sleep(Duration::from_secs(10)).await;
            Ok(ToolResult::Success(serde_json::json!({})))
        }
    }

    #[tokio::test]
    async fn test_executor_timeout() {
        let context = ToolContext::new("/tmp").with_timeout(1);
        let executor = ToolExecutor::new(context);

        let tool = SlowTool;
        let result = executor.execute(&tool, &serde_json::json!({})).await;

        assert!(result.is_err());
        match result.unwrap_err() {
            KodError::ProviderTimeout { .. } => {}
            e => panic!("Expected timeout, got: {:?}", e),
        }
    }
}
