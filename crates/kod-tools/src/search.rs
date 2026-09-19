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
                trust_level: kod_types::trust::TrustLevel::default(),
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

#[cfg(test)]
mod coverage_search_files_tool {
    //! `search_files` had no tests at all before this module.
    //! Every behaviour the model depends on — context lines, the
    //! match marker, the case-insensitive flag, single-file vs
    //! directory, a missing path, an invalid regex — is pinned
    //! here so a regression in the grep-style walker surfaces
    //! immediately.
    use super::*;
    use kod_types::ToolPermissions;

    fn ctx(dir: &std::path::Path) -> ToolContext {
        ToolContext::new(dir).with_permissions(ToolPermissions {
            read_files: true,
            ..Default::default()
        })
    }

    #[tokio::test]
    async fn finds_a_match_with_context_lines_and_marker() {
        let tmp = tempfile::TempDir::new().unwrap();
        std::fs::write(
            tmp.path().join("a.rs"),
            "line one\nline two\nNEEDLE\nline four\nline five\n",
        )
        .unwrap();
        let tool = SearchFilesTool::new();
        let r = tool
            .execute(
                &serde_json::json!({"path": ".", "pattern": "NEEDLE", "context": 1}),
                &ctx(tmp.path()),
            )
            .await
            .unwrap();
        match r {
            ToolResult::Success(v) => {
                assert_eq!(v["count"], 1);
                let block = v["results"][0]["context"].as_str().unwrap();
                assert!(block.contains("NEEDLE"));
                assert!(block.contains("line two"), "context missing: {block}");
                assert!(block.contains("line four"), "context missing: {block}");
                assert!(block.contains('>'), "match marker missing: {block}");
            }
            other => panic!("got {other:?}"),
        }
    }

    #[tokio::test]
    async fn invalid_regex_is_a_tool_error_not_a_panic() {
        let tmp = tempfile::TempDir::new().unwrap();
        std::fs::write(tmp.path().join("a.txt"), "hello").unwrap();
        let tool = SearchFilesTool::new();
        let r = tool
            .execute(
                &serde_json::json!({"path": ".", "pattern": "[unterminated"}),
                &ctx(tmp.path()),
            )
            .await
            .unwrap();
        match r {
            ToolResult::Error(msg) => assert!(msg.contains("invalid regex"), "got: {msg}"),
            other => panic!("expected error, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn case_insensitive_flag_matches_uppercase_content() {
        let tmp = tempfile::TempDir::new().unwrap();
        std::fs::write(tmp.path().join("a.txt"), "HELLO\nworld\n").unwrap();
        let tool = SearchFilesTool::new();
        let r = tool
            .execute(
                &serde_json::json!({
                    "path": ".",
                    "pattern": "hello",
                    "case_insensitive": true
                }),
                &ctx(tmp.path()),
            )
            .await
            .unwrap();
        match r {
            ToolResult::Success(v) => assert_eq!(v["count"], 1),
            other => panic!("got {other:?}"),
        }
    }

    #[tokio::test]
    async fn no_matches_is_an_empty_success_not_an_error() {
        // "The pattern is not in the tree" is a legitimate answer,
        // not a failure. A regression that returned an error here
        // would make the model think the search itself broke.
        let tmp = tempfile::TempDir::new().unwrap();
        std::fs::write(tmp.path().join("a.txt"), "nothing here").unwrap();
        let tool = SearchFilesTool::new();
        let r = tool
            .execute(
                &serde_json::json!({"path": ".", "pattern": "zzz-no-match"}),
                &ctx(tmp.path()),
            )
            .await
            .unwrap();
        match r {
            ToolResult::Success(v) => {
                assert_eq!(v["count"], 0);
                assert_eq!(v["truncated"], false);
            }
            other => panic!("got {other:?}"),
        }
    }

    #[tokio::test]
    async fn a_single_file_path_is_searched_directly() {
        let tmp = tempfile::TempDir::new().unwrap();
        std::fs::write(tmp.path().join("a.txt"), "needle\n").unwrap();
        let tool = SearchFilesTool::new();
        let r = tool
            .execute(
                &serde_json::json!({"path": "a.txt", "pattern": "needle"}),
                &ctx(tmp.path()),
            )
            .await
            .unwrap();
        match r {
            ToolResult::Success(v) => assert_eq!(v["count"], 1),
            other => panic!("got {other:?}"),
        }
    }

    #[tokio::test]
    async fn a_missing_path_is_a_tool_error() {
        let tmp = tempfile::TempDir::new().unwrap();
        let tool = SearchFilesTool::new();
        let r = tool
            .execute(
                &serde_json::json!({"path": "no-such-file", "pattern": "x"}),
                &ctx(tmp.path()),
            )
            .await
            .unwrap();
        match r {
            ToolResult::Error(msg) => assert!(msg.contains("not found"), "got: {msg}"),
            other => panic!("got {other:?}"),
        }
    }

    #[tokio::test]
    async fn context_parameter_is_clamped_to_the_maximum() {
        // The schema says max 5 lines of context; a caller asking
        // for 500 must be clamped, not honored. Otherwise a single
        // call can drag an entire file into the prompt.
        let tmp = tempfile::TempDir::new().unwrap();
        std::fs::write(
            tmp.path().join("a.txt"),
            "1\n2\n3\nneedle\n5\n6\n7\n",
        )
        .unwrap();
        let tool = SearchFilesTool::new();
        let r = tool
            .execute(
                &serde_json::json!({
                    "path": ".",
                    "pattern": "needle",
                    "context": 500
                }),
                &ctx(tmp.path()),
            )
            .await
            .unwrap();
        match r {
            ToolResult::Success(v) => {
                assert_eq!(v["context_lines"], 5, "clamp ignored: {v}");
            }
            other => panic!("got {other:?}"),
        }
    }
}
