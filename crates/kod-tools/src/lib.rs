//! Tool system for agent-environment interaction.
//!
//! Provides the Tool trait, registry, execution context, and built-in tools
//! that can be called by the AI agent.
//!
//! # Example
//!
//! ```rust,no_run
//! use kod_tools::{ToolRegistry, Tool, ToolContext, ToolResult};
//! use kod_types::ToolDefinition;
//! use async_trait::async_trait;
//!
//! struct EchoTool;
//!
//! #[async_trait]
//! impl Tool for EchoTool {
//!     fn definition(&self) -> ToolDefinition {
//!         // ...
//!         # unimplemented!()
//!     }
//!
//!     async fn execute(
//!         &self,
//!         _params: &serde_json::Value,
//!         _context: &ToolContext,
//!     ) -> Result<ToolResult, kod_error::KodError> {
//!         // ...
//!         # unimplemented!()
//!     }
//! }
//! #
//! # // Suppress unused warnings
//! # fn main() {}
//! ```

pub mod ask;
pub mod check;
pub mod context;
pub mod git;
pub mod patch;
pub mod path_lock;
pub mod executor;
pub mod registry;
pub mod search;
pub mod swarm_tools;
pub mod todo;
pub mod tools;
pub mod web;
pub use check::CheckTool;
pub use ask::{AskUserTool, QUESTION_MARKER, QuestionRequest, parse_question, question_marker};
pub use search::SearchFilesTool;
pub use todo::{TodoItem, TodoList, TodoStatus, TodoTool, new_list as new_todo_list};
pub use git::{GitDiffTool, GitStatusTool};
pub use web::WebFetchTool;
pub use tools::{
    ExecuteCommandTool, FileInfoTool, GrepTool, ListFilesTool, PatchFileTool, ReadFileTool,
    WriteFileTool,
};

pub use context::ToolContext;
pub use path_lock::{LockError, PathLockGuard, PathLockTable};
pub use executor::ToolExecutor;
pub use swarm_tools::{SwarmKnowledge, SwarmNoteTool, SwarmReadTool, new_knowledge};
pub use registry::ToolRegistry;

// Re-export tool trait and result for convenience
pub use kod_types::{ToolDefinition, ToolResult};

/// Trait that all tools must implement.
#[async_trait::async_trait]
pub trait Tool: Send + Sync {
    /// Get the tool definition (name, description, schema, permissions)
    fn definition(&self) -> ToolDefinition;

    /// Execute the tool with the given parameters
    async fn execute(
        &self,
        params: &serde_json::Value,
        context: &ToolContext,
    ) -> Result<ToolResult, kod_error::KodError>;
}

/// Extension trait for sync tool registration
/// Requires a boxed tool that can be used across async boundaries.
pub type BoxedTool = Box<dyn Tool>;

#[cfg(test)]
mod tests {
    use super::*;
    use kod_types::{ToolCategory, ToolId, ToolPermissions};

    struct TestTool;

    #[async_trait::async_trait]
    impl Tool for TestTool {
        fn definition(&self) -> ToolDefinition {
            ToolDefinition {
                id: ToolId::new(),
                name: "test".to_string(),
                description: "Test tool".to_string(),
                category: ToolCategory::System,
                parameters_schema: serde_json::json!({}),
                permissions: ToolPermissions::default(),
            }
        }

        async fn execute(
            &self,
            _params: &serde_json::Value,
            _context: &ToolContext,
        ) -> kod_error::Result<ToolResult> {
            Ok(ToolResult::Success(serde_json::json!({"test": true})))
        }
    }

    #[tokio::test]
    async fn test_tool_definition() {
        let tool = TestTool;
        let def = tool.definition();
        assert_eq!(def.name, "test");
        assert_eq!(def.category, ToolCategory::System);
    }

    #[tokio::test]
    async fn test_tool_execute() {
        let tool = TestTool;
        let context = ToolContext::new("/tmp");
        let result = tool
            .execute(&serde_json::json!({}), &context)
            .await
            .unwrap();
        match result {
            ToolResult::Success(data) => {
                assert_eq!(data["test"], true);
            }
            _ => panic!("Expected success"),
        }
    }
}
