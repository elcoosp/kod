//! Tool calling support for Ollama.
//!
//! Ollama supports tool calling (function calling) by providing
//! tool definitions in the request and parsing tool_calls in responses.

use kod_error::{KodError, Result};
use kod_types::{ToolCall, ToolDefinition};
use serde_json::{json, Value};

/// Format tool definitions for the Ollama API
pub fn format_tools_for_ollama(tools: &[ToolDefinition]) -> Vec<Value> {
    tools
        .iter()
        .map(|tool| {
            json!({
                "type": "function",
                "function": {
                    "name": tool.name,
                    "description": tool.description,
                    "parameters": tool.parameters_schema,
                }
            })
        })
        .collect()
}

/// Parse a response from Ollama that may contain tool calls
pub fn parse_tool_calls(response: &Value) -> Result<(String, Vec<ToolCall>)> {
    let text = response["response"]
        .as_str()
        .unwrap_or("")
        .to_string();

    let mut tool_calls = Vec::new();

    if let Some(calls_array) = response["tool_calls"].as_array() {
        for call in calls_array {
            let function = &call["function"];

            let name = function["name"]
                .as_str()
                .ok_or_else(|| KodError::Provider("Tool call missing name".to_string()))?;

            let arguments = function["arguments"].clone();

            // Handle both object and string arguments
            let arguments = if arguments.is_string() {
                // Some models return arguments as a JSON string
                let arg_str = arguments.as_str().unwrap();
                serde_json::from_str(arg_str)
                    .unwrap_or_else(|_| json!({}))
            } else if arguments.is_null() {
                json!({})
            } else {
                arguments
            };

            tool_calls.push(ToolCall {
                tool_name: name.to_string(),
                arguments,
            });
        }
    }

    Ok((text, tool_calls))
}

/// Build a tool result message to send back to Ollama
pub fn build_tool_result_message(
    tool_name: &str,
    result: &Value,
) -> Value {
    json!({
        "role": "tool",
        "tool_name": tool_name,
        "content": result,
    })
}

/// Build a chat-format request with tool results
pub fn build_chat_request_with_tools(
    model: &str,
    messages: Vec<Value>,
    tools: &[ToolDefinition],
) -> Value {
    let tools_json = format_tools_for_ollama(tools);

    json!({
        "model": model,
        "messages": messages,
        "tools": tools_json,
        "stream": false,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use kod_types::{ToolCategory, ToolId, ToolPermissions};

    fn test_tool() -> ToolDefinition {
        ToolDefinition {
            id: ToolId::new(),
            name: "get_weather".to_string(),
            description: "Get current weather".to_string(),
            category: ToolCategory::Web,
            parameters_schema: json!({
                "type": "object",
                "properties": {
                    "location": {
                        "type": "string",
                        "description": "City name"
                    }
                },
                "required": ["location"]
            }),
            permissions: ToolPermissions::default(),
        }
    }

    #[test]
    fn test_format_tools() {
        let tools = vec![test_tool()];
        let formatted = format_tools_for_ollama(&tools);

        assert_eq!(formatted[0]["function"]["name"], "get_weather");
        assert!(formatted[0]["function"]["parameters"].is_object());
    }

    #[test]
    fn test_parse_empty_response() {
        let response = json!({"response": "Hello", "done": true});
        let (text, calls) = parse_tool_calls(&response).unwrap();

        assert_eq!(text, "Hello");
        assert!(calls.is_empty());
    }

    #[test]
    fn test_parse_with_tool_calls() {
        let response = json!({
            "response": "Checking weather",
            "tool_calls": [
                {
                    "function": {
                        "name": "get_weather",
                        "arguments": {"location": "Paris"}
                    }
                }
            ]
        });

        let (text, calls) = parse_tool_calls(&response).unwrap();
        assert_eq!(text, "Checking weather");
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].tool_name, "get_weather");
    }

    #[test]
    fn test_parse_string_arguments() {
        // Some models return arguments as JSON string
        let response = json!({
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

        let (_, calls) = parse_tool_calls(&response).unwrap();
        assert_eq!(calls[0].arguments["key"], "value");
    }
}
