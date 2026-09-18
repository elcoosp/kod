//! `search_files`: grep with before/after context and line numbers.
//!
//! Distinct from `grep` (which returns one matching line per hit and no
//! surrounding text) so an agent reading a diff or scanning a function
//! gets the two lines of context that make a match meaningful. The tool
//! reuses the same gitaware walk and size / entry caps.

use crate::{Tool, ToolContext};
use kod_error::{KodError, Result};
use kod_types::{ToolCategory, ToolDefinition, ToolId, ToolPermissions, ToolResult};
use regex::Regex;
use serde_json::Value;

const MAX_MATCHES: usize = 200;
const MAX_LINE_CHARS: usize = 400;
const MAX_CONTEXT_LINES: u32 = 5;

pub struct SearchFilesTool {
    pub definition: ToolDefinition,
}

impl SearchFilesTool {
    pub fn new() -> Self {
        Self {
            definition: ToolDefinition {
                id: ToolId::new(),
                name: "search_files".to_string(),
                description: "Search file contents with a regex, returning each match \
                    with surrounding lines of context and a file:line header. Use this \
                    when grep's single-line output is not enough — reading a function \
                    around a hit, checking a match's branch, etc."
                    .to_string(),
                category: ToolCategory::FileSystem,
                parameters_schema: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "path": {
                            "type": "string",
                            "description": "File or directory to search"
                        },
                        "pattern": {
                            "type": "string",
                            "description": "Rust regex pattern"
                        },
                        "context": {
                            "type": "integer",
                            "description": "Lines of context before and after each match. Default 2, max 5."
                        },
                        "case_insensitive": {
                            "type": "boolean",
                            "description": "Match case-insensitively. Default false."
                        }
                    },
                    "required": ["path", "pattern"],
                    "additionalProperties": false
                }),
                permissions: ToolPermissions {
                    read_files: true,
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

impl Default for SearchFilesTool {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait::async_trait]
impl Tool for SearchFilesTool {
    fn definition(&self) -> ToolDefinition {
        self.definition.clone()
    }

    async fn execute(&self, params: &Value, context: &ToolContext) -> Result<ToolResult> {
        let path = params["path"]
            .as_str()
            .ok_or_else(|| KodError::InvalidParameters {
                reason: "Missing 'path' parameter".to_string(),
            })?;
        let pattern = params["pattern"]
            .as_str()
            .ok_or_else(|| KodError::InvalidParameters {
                reason: "Missing 'pattern' parameter".to_string(),
            })?;
        let context_lines = params["context"].as_u64().unwrap_or(2) as u32;
        let context_lines = context_lines.min(MAX_CONTEXT_LINES);
        let case_insensitive = params["case_insensitive"].as_bool().unwrap_or(false);

        let resolved = context.resolve_path(path)?;
        context.can_read(&resolved)?;

        let folded = if case_insensitive {
            format!("(?i){}", pattern)
        } else {
            pattern.to_string()
        };
        let regex = match Regex::new(&folded) {
            Ok(r) => r,
            Err(e) => {
                return Ok(ToolResult::Error(format!(
                    "invalid regex {:?}: {}",
                    pattern, e
                )));
            }
        };

        // Collect the files to search. A single file searches just
        // itself; a directory walks (respecting .gitignore).
        let files: Vec<std::path::PathBuf> = if resolved.is_file() {
            vec![resolved.clone()]
        } else if resolved.is_dir() {
            crate::tools::gitaware_walk(&resolved, true)
                .into_iter()
                .filter(|p| p.is_file())
                .collect()
        } else {
            return Ok(ToolResult::Error(format!(
                "not found: {}",
                resolved.display()
            )));
        };

        let mut hits: Vec<Value> = Vec::new();
        let mut truncated = false;

        'outer: for file in files {
            let text = match std::fs::read_to_string(&file) {
                Ok(t) => t,
                Err(_) => continue,
            };
            let lines: Vec<&str> = text.lines().collect();
            for (idx, line) in lines.iter().enumerate() {
                if regex.is_match(line) {
                    let start = idx.saturating_sub(context_lines as usize);
                    let end = (idx + context_lines as usize + 1).min(lines.len());
                    let mut block = String::new();
                    for (j, l) in lines[start..end].iter().enumerate() {
                        let real_line = start + j + 1;
                        let marker = if start + j == idx { '>' } else { ' ' };
                        let truncated_line: String = if l.chars().count() > MAX_LINE_CHARS {
                            let s: String = l.chars().take(MAX_LINE_CHARS).collect();
                            format!("{}…", s)
                        } else {
                            l.to_string()
                        };
                        block.push_str(&format!(
                            "{} {:>5} | {}\n",
                            marker, real_line, truncated_line
                        ));
                    }
                    hits.push(serde_json::json!({
                        "file": file.to_string_lossy(),
                        "line": idx + 1,
                        "context": block.trim_end(),
                    }));
                    if hits.len() >= MAX_MATCHES {
                        truncated = true;
                        break 'outer;
                    }
                }
            }
        }

        Ok(ToolResult::Success(serde_json::json!({
            "pattern": pattern,
            "context_lines": context_lines,
            "count": hits.len(),
            "truncated": truncated,
            "results": hits,
        })))
    }
}
