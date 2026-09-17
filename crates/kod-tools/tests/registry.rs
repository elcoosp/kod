use async_trait::async_trait;
use kod_tools::registry::ToolRegistry;
use kod_tools::{Tool, ToolContext, ToolResult};
use kod_types::{ToolCategory, ToolDefinition, ToolId, ToolPermissions};
use serde_json::json;

struct EchoTool;

#[async_trait]
impl Tool for EchoTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            id: ToolId::new(),
            name: "echo".to_string(),
            description: "Echo back the input".to_string(),
            category: ToolCategory::System,
            parameters_schema: json!({
                "type": "object",
                "properties": {
                    "message": {
                        "type": "string",
                        "description": "Message to echo"
                    }
                },
                "required": ["message"]
            }),
            permissions: ToolPermissions {
                read_files: false,
                write_files: false,
                execute_commands: false,
                network_access: false,
                git_access: kod_types::GitAccess::None,
                allowed_paths: Vec::new(),
                forbidden_paths: Vec::new(),
            },
        }
    }

    async fn execute(
        &self,
        params: &serde_json::Value,
        context: &ToolContext,
    ) -> Result<ToolResult, kod_error::KodError> {
        let message = params["message"].as_str().unwrap_or("");
        Ok(ToolResult::Success(json!({
            "echo": message,
            "working_dir": context.working_dir.display().to_string(),
        })))
    }
}

#[tokio::test]
async fn test_register_and_get_tool() {
    let registry = ToolRegistry::new();

    let tool = EchoTool;
    let tool_name = tool.definition().name.clone();

    registry.register(Box::new(tool)).await;

    assert!(registry.has(&tool_name).await);
    assert!(!registry.has("nonexistent").await);
}

#[tokio::test]
async fn test_list_tools_by_category() {
    let registry = ToolRegistry::new();
    registry.register(Box::new(EchoTool)).await;

    let system_tools = registry.list_by_category(ToolCategory::System).await;
    assert_eq!(system_tools.len(), 1);

    let file_tools = registry.list_by_category(ToolCategory::FileSystem).await;
    assert_eq!(file_tools.len(), 0);
}

#[tokio::test]
async fn test_list_all_tools() {
    let registry = ToolRegistry::new();
    registry.register(Box::new(EchoTool)).await;

    let all = registry.list_all().await;
    assert_eq!(all.len(), 1);
    assert_eq!(all[0], "echo");
}

#[tokio::test]
async fn test_remove_tool() {
    let registry = ToolRegistry::new();

    let tool = EchoTool;
    let tool_name = tool.definition().name.clone();

    registry.register(Box::new(tool)).await;
    assert!(registry.has(&tool_name).await);

    let removed = registry.remove(&tool_name).await;
    assert!(removed);
    assert!(!registry.has(&tool_name).await);
}

#[tokio::test]
async fn test_tool_count() {
    let registry = ToolRegistry::new();
    assert_eq!(registry.count().await, 0);

    registry.register(Box::new(EchoTool)).await;
    assert_eq!(registry.count().await, 1);
}

#[tokio::test]
async fn test_get_tool_definitions_for_llm() {
    let registry = ToolRegistry::new();
    registry.register(Box::new(EchoTool)).await;

    let definitions = registry.get_definitions_for_llm().await;
    assert_eq!(definitions.len(), 1);

    // Should be in OpenAI function calling format
    assert_eq!(definitions[0]["type"], "function");
    assert_eq!(definitions[0]["function"]["name"], "echo");
    assert!(definitions[0]["function"]["parameters"].is_object());
}
