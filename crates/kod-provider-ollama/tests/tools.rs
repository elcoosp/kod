use kod_provider_ollama::tools::{format_tools_for_ollama, parse_tool_calls};
use kod_types::{ToolCategory, ToolDefinition, ToolId, ToolPermissions};
use serde_json::{Value, json};

fn create_test_tool() -> ToolDefinition {
    ToolDefinition {
        id: ToolId::new(),
        name: "read_file".to_string(),
        description: "Read a file from the filesystem".to_string(),
        category: ToolCategory::FileSystem,
        parameters_schema: json!({
            "type": "object",
            "properties": {
                "path": {
                    "type": "string",
                    "description": "Path to the file"
                }
            },
            "required": ["path"]
        }),
        permissions: ToolPermissions::default(),
    }
}

#[test]
fn test_format_tools_for_ollama() {
    let tools = vec![create_test_tool()];
    let formatted = format_tools_for_ollama(&tools);

    assert_eq!(formatted.len(), 1);
    assert_eq!(formatted[0]["type"], "function");
    assert_eq!(formatted[0]["function"]["name"], "read_file");
    assert_eq!(
        formatted[0]["function"]["description"],
        "Read a file from the filesystem"
    );
}

#[test]
fn test_parse_tool_calls_single() {
    let response: Value = json!({
        "model": "llama3.2",
        "response": "I'll read the file.",
        "tool_calls": [
            {
                "function": {
                    "name": "read_file",
                    "arguments": {
                        "path": "/test.rs"
                    }
                }
            }
        ],
        "done": true
    });

    let (text, tool_calls) = parse_tool_calls(&response).unwrap();

    assert_eq!(text, "I'll read the file.");
    assert_eq!(tool_calls.len(), 1);
    assert_eq!(tool_calls[0].tool_name, "read_file");
    assert_eq!(tool_calls[0].arguments["path"], "/test.rs");
}

#[test]
fn test_parse_tool_calls_none() {
    let response: Value = json!({
        "model": "llama3.2",
        "response": "Just a text response.",
        "done": true
    });

    let (text, tool_calls) = parse_tool_calls(&response).unwrap();

    assert_eq!(text, "Just a text response.");
    assert!(tool_calls.is_empty());
}

#[test]
fn test_parse_tool_calls_multiple() {
    let response: Value = json!({
        "model": "llama3.2",
        "response": "I'll do multiple things.",
        "tool_calls": [
            {
                "function": {
                    "name": "read_file",
                    "arguments": {"path": "/a.rs"}
                }
            },
            {
                "function": {
                    "name": "write_file",
                    "arguments": {"path": "/b.rs", "content": "hello"}
                }
            }
        ],
        "done": true
    });

    let (_text, tool_calls) = parse_tool_calls(&response).unwrap();
    assert_eq!(tool_calls.len(), 2);
    assert_eq!(tool_calls[1].tool_name, "write_file");
}

#[test]
fn test_parse_string_arguments() {
    let response: Value = json!({
        "response": "",
        "tool_calls": [
            {
                "function": {
                    "name": "test",
                    "arguments": "{\"key\": \"value\"}"
                }
            }
        ]
    });

    let (_, tool_calls) = parse_tool_calls(&response).unwrap();
    assert_eq!(tool_calls[0].arguments["key"], "value");
}
