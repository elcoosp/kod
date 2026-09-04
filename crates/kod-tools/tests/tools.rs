use kod_error::KodError;
use kod_tools::{
    FileInfoTool, ListFilesTool, ReadFileTool, Tool, ToolContext, ToolResult, WriteFileTool,
};
use kod_types::ToolPermissions;
use serde_json::json;
use tempfile::TempDir;

#[tokio::test]
async fn test_read_file_tool() {
    let temp_dir = TempDir::new().unwrap();
    let file_path = temp_dir.path().join("test.txt");
    std::fs::write(&file_path, "Hello, world!").unwrap();

    let perms = ToolPermissions {
        read_files: true,
        ..Default::default()
    };
    let context = ToolContext::new(temp_dir.path()).with_permissions(perms);

    let tool = ReadFileTool::new();
    let params = json!({ "path": "test.txt" });

    let result = tool.execute(&params, &context).await.unwrap();

    match result {
        ToolResult::Success(data) => {
            assert_eq!(data["content"], "Hello, world!");
        }
        _ => panic!("Expected success"),
    }
}

#[tokio::test]
async fn test_write_file_tool() {
    let temp_dir = TempDir::new().unwrap();
    let file_path = temp_dir.path().join("output.txt");

    let perms = ToolPermissions {
        write_files: true,
        ..Default::default()
    };
    let context = ToolContext::new(temp_dir.path()).with_permissions(perms);

    let tool = WriteFileTool::new();
    let params = json!({ "path": "output.txt", "content": "Written content" });

    let result = tool.execute(&params, &context).await.unwrap();

    match result {
        ToolResult::Success(data) => {
            assert_eq!(data["written"], 15);
        }
        _ => panic!("Expected success"),
    }

    let content = std::fs::read_to_string(&file_path).unwrap();
    assert_eq!(content, "Written content");
}

#[tokio::test]
async fn test_list_files_tool() {
    let temp_dir = TempDir::new().unwrap();
    std::fs::write(temp_dir.path().join("a.txt"), "a").unwrap();
    std::fs::write(temp_dir.path().join("b.txt"), "b").unwrap();

    let perms = ToolPermissions {
        read_files: true,
        ..Default::default()
    };
    let context = ToolContext::new(temp_dir.path()).with_permissions(perms);

    let tool = ListFilesTool::new();
    let params = json!({ "path": "." });

    let result = tool.execute(&params, &context).await.unwrap();

    match result {
        ToolResult::Success(data) => {
            let files = data["files"].as_array().unwrap();
            assert_eq!(files.len(), 2);
        }
        _ => panic!("Expected success"),
    }
}

#[tokio::test]
async fn test_file_info_tool() {
    let temp_dir = TempDir::new().unwrap();
    let file_path = temp_dir.path().join("test.txt");
    std::fs::write(&file_path, "content").unwrap();

    let perms = ToolPermissions {
        read_files: true,
        ..Default::default()
    };
    let context = ToolContext::new(temp_dir.path()).with_permissions(perms);

    let tool = FileInfoTool::new();
    let params = json!({ "path": "test.txt" });

    let result = tool.execute(&params, &context).await.unwrap();

    match result {
        ToolResult::Success(data) => {
            assert!(data["size"].as_u64().unwrap() > 0);
            assert_eq!(data["is_file"], true);
        }
        _ => panic!("Expected success"),
    }
}

#[tokio::test]
async fn test_permission_denied() {
    let temp_dir = TempDir::new().unwrap();
    let context = ToolContext::new(temp_dir.path()); // default permissions: no read

    let tool = ReadFileTool::new();
    let params = json!({ "path": "test.txt" });

    let result = tool.execute(&params, &context).await;

    assert!(result.is_err());
    match result.unwrap_err() {
        KodError::PermissionDenied { action, .. } => {
            assert_eq!(action, "read");
        }
        e => panic!("Expected PermissionDenied, got: {:?}", e),
    }
}

#[tokio::test]
async fn test_missing_parameter() {
    let context = ToolContext::new("/tmp");
    let tool = ReadFileTool::new();

    let result = tool.execute(&json!({}), &context).await;

    assert!(result.is_err());
}
