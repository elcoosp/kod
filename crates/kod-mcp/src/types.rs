//! Wire types for the MCP client.
//!
//! Only the shapes `initialize`, `tools/list`, and `tools/call` need.
//! Field names follow the MCP spec exactly (camelCase where the spec
//! uses camelCase — `inputSchema`, `isError`, `mimeType`); the
//! `#[serde(rename)]` attributes are the contract and are tested
//! against literal JSON fixtures in `client.rs`.

use serde::{Deserialize, Serialize};

/// `initialize` response payload: what the server calls itself.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ServerInfo {
    pub name: String,
    #[serde(default)]
    pub version: String,
}

/// One tool a server advertises. `input_schema` is a JSON Schema
/// object the spec calls `inputSchema`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct McpToolDef {
    pub name: String,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(rename = "inputSchema", default)]
    pub input_schema: serde_json::Value,
}

/// `tools/call` response payload.
///
/// `is_error` mirrors the spec's `isError`: a tool can return a
/// structured failure without the RPC layer itself failing.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct McpToolResult {
    #[serde(default)]
    pub content: Vec<McpContent>,
    #[serde(rename = "isError", default)]
    pub is_error: bool,
}

/// One content part inside a tool result.
///
/// `Resource` carries an optional inline `text` because the spec allows
/// a server to embed a resource body. A server that returns a resource
/// by URI only (no text) is legal; the adapter that turns this into a
/// `kod_types::ToolResult` reports it as a link, not as content.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum McpContent {
    Text {
        text: String,
    },
    Image {
        data: String,
        #[serde(rename = "mimeType")]
        mime_type: String,
    },
    Resource {
        uri: String,
        #[serde(default)]
        text: Option<String>,
    },
}

impl McpToolResult {
    /// Flatten every `Text` part into one string, newline-separated.
    /// Image and URI-only resource parts become short placeholders so
    /// the model knows something was returned but did not fit as text.
    pub fn render_text(&self) -> String {
        let mut parts: Vec<String> = Vec::new();
        for c in &self.content {
            match c {
                McpContent::Text { text } => parts.push(text.clone()),
                McpContent::Image { mime_type, .. } => {
                    parts.push(format!("[image: {mime_type}, base64 body omitted]"));
                }
                McpContent::Resource { uri, text } => match text {
                    Some(t) => parts.push(t.clone()),
                    None => parts.push(format!("[resource: {uri}]")),
                },
            }
        }
        parts.join("\n")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tool_def_parses_input_schema() {
        let raw = r#"{
            "name": "read_file",
            "description": "read a file",
            "inputSchema": {"type":"object","properties":{"path":{"type":"string"}}}
        }"#;
        let t: McpToolDef = serde_json::from_str(raw).unwrap();
        assert_eq!(t.name, "read_file");
        assert_eq!(t.description.as_deref(), Some("read a file"));
        assert_eq!(t.input_schema["type"], "object");
    }

    #[test]
    fn tool_def_tolerates_missing_description() {
        let raw = r#"{"name":"x","inputSchema":{}}"#;
        let t: McpToolDef = serde_json::from_str(raw).unwrap();
        assert_eq!(t.name, "x");
        assert!(t.description.is_none());
    }

    #[test]
    fn tool_result_flattens_text_parts() {
        let raw = r#"{
            "content":[
                {"type":"text","text":"first"},
                {"type":"text","text":"second"}
            ],
            "isError": false
        }"#;
        let r: McpToolResult = serde_json::from_str(raw).unwrap();
        assert!(!r.is_error);
        assert_eq!(r.render_text(), "first\nsecond");
    }

    #[test]
    fn tool_result_reports_image_and_resource_placeholders() {
        let raw = r#"{
            "content":[
                {"type":"image","data":"...","mimeType":"image/png"},
                {"type":"resource","uri":"file:///x"}
            ]
        }"#;
        let r: McpToolResult = serde_json::from_str(raw).unwrap();
        let rendered = r.render_text();
        assert!(rendered.contains("image/png"));
        assert!(rendered.contains("file:///x"));
    }

    #[test]
    fn server_info_parses() {
        let raw = r#"{"name":"example","version":"1.2.3"}"#;
        let s: ServerInfo = serde_json::from_str(raw).unwrap();
        assert_eq!(s.name, "example");
        assert_eq!(s.version, "1.2.3");
    }
}
