//! Memory tools exposed to the model (D2-B3a, channel 2).
//!
//! Two tools:
//!
//! - `memory_save(content, tags?)`: persist a long-term entry with
//!   the caller's cwd as the project key. The engine resolves the
//!   key from `ToolContext.working_dir` so the tool itself does not
//!   depend on anything engine-specific.
//! - `memory_search(query, k?)`: top-k entries by the hybrid
//!   retrieval (B2). When the model needs to know "what did I learn
//!   earlier?" it queries here instead of scrolling the transcript.
//!
//! Both tools hold an `Arc<TaskRouter>` — the router already owns the
//! `MemoryManager`, and exposing it as a shared handle avoids a
//! parallel ownership tree in the engine. The two thin wrappers on the
//! router (`store_long_term` / `search_long_term`) are the only
//! interface the tools touch.

use kod_tools::{Tool, ToolContext};
use kod_error::{KodError, Result};
use kod_types::{ToolCategory, ToolDefinition, ToolId, ToolPermissions, ToolResult};
use serde_json::Value;
use std::sync::Arc;

/// `memory_save` — record a durable fact for later recall.
pub struct MemorySaveTool {
    pub definition: ToolDefinition,
    router: Arc<crate::router::TaskRouter>,
}

impl MemorySaveTool {
    pub fn new(router: Arc<crate::router::TaskRouter>) -> Self {
        Self {
            definition: ToolDefinition {
                id: ToolId::new(),
                name: "memory_save".to_string(),
                description: "Save a fact, preference, or project decision to long-term \
                    memory. Use it for anything the user asks you to remember, or that \
                    you would otherwise have to ask again in a future session — a \
                    convention, a preference, a path, a project decision. Do not use it \
                    for transient state (which file you are editing now) — the session \
                    transcript covers that."
                    .to_string(),
                category: ToolCategory::System,
                parameters_schema: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "content": {
                            "type": "string",
                            "description": "The fact or preference to remember, one sentence."
                        },
                        "tags": {
                            "type": "array",
                            "items": { "type": "string" },
                            "description": "Optional short tags for later filtering \
                                (e.g. [\"preference\", \"rust\"])."
                        }
                    },
                    "required": ["content"],
                    "additionalProperties": false
                }),
                // Memory is neither filesystem nor network. A context
                // with default permissions can still use the tool —
                // the policy engine is what gates it, not the
                // ToolPermissions bitmask.
                permissions: ToolPermissions::default(),
            },
            router,
        }
    }
}

#[async_trait::async_trait]
impl Tool for MemorySaveTool {
    fn definition(&self) -> ToolDefinition {
        self.definition.clone()
    }

    async fn execute(&self, params: &Value, context: &ToolContext) -> Result<ToolResult> {
        let content = params
            .get("content")
            .and_then(|v| v.as_str())
            .map(|s| s.trim())
            .filter(|s| !s.is_empty())
            .ok_or_else(|| KodError::InvalidParameters {
                reason: "memory_save: 'content' is required and must not be empty"
                    .to_string(),
            })?;

        let tags: Vec<String> = params
            .get("tags")
            .and_then(|v| v.as_array())
            .map(|a| {
                a.iter()
                    .filter_map(|t| t.as_str())
                    .map(String::from)
                    .collect()
            })
            .unwrap_or_default();

        let project_key = Some(crate::router::TaskRouter::project_key_for(
            &context.working_dir,
        ));

        match self
            .router
            .store_long_term(content, tags.clone(), project_key)
            .await
        {
            Ok(id) => Ok(ToolResult::Success(serde_json::json!({
                "id": id.as_uuid().to_string(),
                "saved": content,
                "tags": tags,
            }))),
            Err(e) => Ok(ToolResult::Error(format!("memory_save failed: {e}"))),
        }
    }
}

/// `memory_search` — retrieve the top-k entries for a query.
pub struct MemorySearchTool {
    pub definition: ToolDefinition,
    router: Arc<crate::router::TaskRouter>,
}

impl MemorySearchTool {
    pub fn new(router: Arc<crate::router::TaskRouter>) -> Self {
        Self {
            definition: ToolDefinition {
                id: ToolId::new(),
                name: "memory_search".to_string(),
                description: "Search long-term memory for entries that match a query. \
                    Use it before asking the user to repeat something they may have \
                    told you in a previous session, and whenever 'what did I learn?' \
                    would be answered by your own notes."
                    .to_string(),
                category: ToolCategory::System,
                parameters_schema: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "query": {
                            "type": "string",
                            "description": "What to search for, one sentence."
                        },
                        "k": {
                            "type": "integer",
                            "description": "Maximum results to return. Default 8, cap 30."
                        }
                    },
                    "required": ["query"],
                    "additionalProperties": false
                }),
                permissions: ToolPermissions::default(),
            },
            router,
        }
    }
}

#[async_trait::async_trait]
impl Tool for MemorySearchTool {
    fn definition(&self) -> ToolDefinition {
        self.definition.clone()
    }

    async fn execute(&self, params: &Value, context: &ToolContext) -> Result<ToolResult> {
        let query = params
            .get("query")
            .and_then(|v| v.as_str())
            .map(|s| s.trim())
            .filter(|s| !s.is_empty())
            .ok_or_else(|| KodError::InvalidParameters {
                reason: "memory_search: 'query' is required and must not be empty"
                    .to_string(),
            })?;
        let k = params
            .get("k")
            .and_then(|v| v.as_u64())
            .map(|n| n as usize)
            .unwrap_or(8)
            .min(30);

        let _ = context; // memory_search does not use the cwd

        let entries = self.router.search_long_term(query, k).await;
        if entries.is_empty() {
            return Ok(ToolResult::Success(serde_json::json!({
                "count": 0,
                "results": [],
                "note": "no entries matched the query",
            })));
        }
        let results: Vec<serde_json::Value> = entries
            .iter()
            .map(|e| {
                serde_json::json!({
                    "id": e.id.as_uuid().to_string(),
                    "content": e.content,
                    "tags": e.metadata.tags,
                    "timestamp": e.timestamp.to_string(),
                })
            })
            .collect();
        Ok(ToolResult::Success(serde_json::json!({
            "count": results.len(),
            "results": results,
        })))
    }
}
