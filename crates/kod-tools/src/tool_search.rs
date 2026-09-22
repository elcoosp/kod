//! `tool_search`: find tools by what they do (P3).
//!
//! The doc's Thing 2 wants something between nothing and everything:
//! small snippets about what is available, plus the ability to dump
//! a full schema on demand. Skills already do this for instructions
//! (a capped name-and-description inventory, full body on match);
//! tools still ship every schema on every request.
//!
//! To be fair to kod, its tool block is unusually lean — ~1.4k
//! tokens of compact JSON for 23 hand-written schemas — so the waste
//! today is modest. The problem scales with MCP: every connected
//! server's tools arrive with whatever schemas they carry, and the
//! per-turn Jev top-3 trim that manages them is the cache-hostile
//! mutation P0's hysteresis works around.
//!
//! `tool_search` lets the model ask "which tool do I use for X" and
//! get back the matching schemas, instead of the engine shipping
//! all of them. It scores a query against each tool's name,
//! description, and category with the same term-overlap scorer the
//! relevance module uses, then returns the top N full schemas.

use crate::{Tool, ToolContext};
use kod_error::{KodError, Result};
use kod_types::{ToolCategory, ToolDefinition, ToolId, ToolPermissions, ToolResult};
use serde_json::Value;

/// Tools the search will match against. Filled by the engine at
/// construction from the live registry.
#[derive(Clone, Default)]
pub struct ToolInventory {
    pub definitions: Vec<ToolDefinition>,
}

impl ToolInventory {
    pub fn from_definitions(defs: Vec<ToolDefinition>) -> Self {
        Self { definitions: defs }
    }
}

pub struct ToolSearchTool {
    pub definition: ToolDefinition,
    /// Shared with the engine. The engine updates this whenever the
    /// registry changes (MCP server added, hot-reload) so the search
    /// always sees the current tool list.
    inventory: std::sync::Arc<std::sync::RwLock<ToolInventory>>,
}

impl ToolSearchTool {
    pub fn new(inventory: std::sync::Arc<std::sync::RwLock<ToolInventory>>) -> Self {
        Self {
            inventory,
            definition: ToolDefinition {
                trust_level: kod_types::trust::TrustLevel::default(),
                id: ToolId::new(),
                name: "tool_search".to_string(),
                description: "Find the tools that can do a task. Given a \
                    natural-language description of what you need, returns the \
                    full schemas of the most relevant tools. Use this when the \
                    tools you can see do not obviously cover what you need — \
                    especially when MCP servers are connected and their tools \
                    may not be in the default list."
                    .to_string(),
                category: ToolCategory::System,
                parameters_schema: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "query": {
                            "type": "string",
                            "description": "Natural-language description of the task"
                        },
                        "limit": {
                            "type": "integer",
                            "description": "Maximum results (default 5, max 20)",
                            "minimum": 1,
                            "maximum": 20
                        }
                    },
                    "required": ["query"]
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
            },
        }
    }
}

impl Default for ToolSearchTool {
    fn default() -> Self {
        Self::new(std::sync::Arc::new(std::sync::RwLock::new(
            ToolInventory::default(),
        )))
    }
}

#[async_trait::async_trait]
impl Tool for ToolSearchTool {
    fn definition(&self) -> ToolDefinition {
        self.definition.clone()
    }

    async fn execute(&self, params: &Value, _context: &ToolContext) -> Result<ToolResult> {
        let query = params["query"]
            .as_str()
            .ok_or_else(|| KodError::InvalidParameters {
                reason: "Missing 'query' parameter".to_string(),
            })?;
        let limit = params["limit"].as_u64().unwrap_or(5).clamp(1, 20) as usize;

        let inv = self
            .inventory
            .read()
            .unwrap_or_else(|e| e.into_inner());
        if inv.definitions.is_empty() {
            return Ok(ToolResult::Success(serde_json::json!({
                "matches": [],
                "note": "no tools registered",
            })));
        }

        let terms: Vec<String> = query
            .split_whitespace()
            .map(|s| {
                s.trim_matches(|c: char| !c.is_alphanumeric() && c != '_')
                    .to_lowercase()
            })
            .filter(|s| s.len() >= 2)
            .collect();

        // Score each tool. A hit on the name weighs more than a hit
        // on the description, which weighs more than a hit on the
        // category label.
        let mut scored: Vec<(f64, &ToolDefinition)> = inv
            .definitions
            .iter()
            .map(|d| {
                let name = d.name.to_lowercase();
                let desc = d.description.to_lowercase();
                let cat = format!("{:?}", d.category).to_lowercase();
                let mut score = 0.0;
                for t in &terms {
                    if name.contains(t.as_str()) {
                        score += 3.0;
                    }
                    if desc.contains(t.as_str()) {
                        score += 1.0;
                    }
                    if cat.contains(t.as_str()) {
                        score += 0.5;
                    }
                }
                (score, d)
            })
            .filter(|(s, _)| *s > 0.0)
            .collect();

        // Descending score; ties keep registry order.
        scored.sort_by(|a, b| {
            b.0.partial_cmp(&a.0)
                .unwrap_or(std::cmp::Ordering::Equal)
        });

        let matches: Vec<Value> = scored
            .iter()
            .take(limit)
            .map(|(_, d)| {
                serde_json::json!({
                    "name": d.name,
                    "description": d.description,
                    "category": format!("{:?}", d.category),
                    "parameters_schema": d.parameters_schema,
                })
            })
            .collect();

        let total = inv.definitions.len();
        let returned = matches.len();
        Ok(ToolResult::Success(serde_json::json!({
            "matches": matches,
            "returned": returned,
            "searched": total,
            "note": if returned == 0 {
                "No tool matched. Try a broader query, or check whether an MCP server provides the capability."
            } else {
                "The schemas above are the ones you can call now; tools not listed remain callable but were not ranked as relevant."
            },
        })))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    fn tool(name: &str, desc: &str, cat: ToolCategory) -> ToolDefinition {
        ToolDefinition {
            id: ToolId::new(),
            name: name.to_string(),
            description: desc.to_string(),
            category: cat,
            parameters_schema: serde_json::json!({"type": "object"}),
            permissions: ToolPermissions::default(),
            trust_level: kod_types::trust::TrustLevel::default(),
        }
    }

    fn inventory(defs: Vec<ToolDefinition>) -> Arc<std::sync::RwLock<ToolInventory>> {
        Arc::new(std::sync::RwLock::new(ToolInventory::from_definitions(defs)))
    }

    #[tokio::test]
    async fn empty_inventory_reports_no_tools() {
        let t = ToolSearchTool::new(inventory(vec![]));
        let ctx = ToolContext::new(std::path::Path::new("."));
        let r = t
            .execute(&serde_json::json!({"query": "read"}), &ctx)
            .await
            .unwrap();
        if let ToolResult::Success(v) = r {
            assert_eq!(v["matches"].as_array().unwrap().len(), 0);
        } else {
            panic!("expected success");
        }
    }

    #[tokio::test]
    async fn a_name_match_ranks_above_a_description_match() {
        let t = ToolSearchTool::new(inventory(vec![
            tool("read_file", "Read a file from disk", ToolCategory::FileSystem),
            tool("write_file", "Read stdin and write to a file", ToolCategory::FileSystem),
        ]));
        let ctx = ToolContext::new(std::path::Path::new("."));
        let r = t
            .execute(&serde_json::json!({"query": "read", "limit": 2}), &ctx)
            .await
            .unwrap();
        if let ToolResult::Success(v) = r {
            let matches = v["matches"].as_array().unwrap();
            assert_eq!(matches[0]["name"], "read_file");
        } else {
            panic!("expected success");
        }
    }

    #[tokio::test]
    async fn limit_caps_the_returned_schemas() {
        // Query must use terms of >= 2 chars; the tokenizer drops
        // single letters on purpose (they match nearly everything).
        let t = ToolSearchTool::new(inventory(vec![
            tool("alpha", "search files", ToolCategory::System),
            tool("beta", "search files", ToolCategory::System),
            tool("gamma", "search files", ToolCategory::System),
        ]));
        let ctx = ToolContext::new(std::path::Path::new("."));
        let r = t
            .execute(&serde_json::json!({"query": "search", "limit": 2}), &ctx)
            .await
            .unwrap();
        if let ToolResult::Success(v) = r {
            assert_eq!(v["matches"].as_array().unwrap().len(), 2);
        }
    }

    #[tokio::test]
    async fn an_irrelevant_query_returns_no_matches() {
        let t = ToolSearchTool::new(inventory(vec![tool(
            "grep",
            "search file contents",
            ToolCategory::FileSystem,
        )]));
        let ctx = ToolContext::new(std::path::Path::new("."));
        let r = t
            .execute(&serde_json::json!({"query": "kubernetes"}), &ctx)
            .await
            .unwrap();
        if let ToolResult::Success(v) = r {
            assert_eq!(v["matches"].as_array().unwrap().len(), 0);
        }
    }

    #[tokio::test]
    async fn category_labels_match_a_broad_query() {
        let t = ToolSearchTool::new(inventory(vec![tool(
            "grep",
            "search",
            ToolCategory::FileSystem,
        )]));
        let ctx = ToolContext::new(std::path::Path::new("."));
        let r = t
            .execute(&serde_json::json!({"query": "filesystem"}), &ctx)
            .await
            .unwrap();
        if let ToolResult::Success(v) = r {
            assert_eq!(v["matches"].as_array().unwrap().len(), 1);
        }
    }
}
