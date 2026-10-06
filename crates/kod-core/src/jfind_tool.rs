//! Delta §7.4: the `jfind` tool — semantic code search.
//!
//! Wraps [`kod_core_tools::jfind::search`] with a real [`Judge`] backed by the
//! engine's Jev client, and a `Tool` impl the model calls. The judge
//! asks Jev, for each candidate, "does this relate to the query?" and
//! maps the probability onto the cascade's `[0, 1]` score.
//!
//! # Without Jev
//!
//! When no Jev client is installed (the common local-model case), the
//! tool falls back to a lexical judge: a candidate scores by how many
//! query words it contains. That is not semantic search — it is a
//! keyword ranker — so the tool's result says which judge ran, and the
//! description tells the model `jfind` is a lexical fallback without
//! Jev.

use std::sync::Arc;

use async_trait::async_trait;
use kod_error::Result;
use kod_types::{ToolCategory, ToolDefinition, ToolId, ToolPermissions, ToolResult};
use serde_json::Value;

use crate::jev::JevDecider;
use kod_core_tools::jfind::{self, Judge, Query};
use kod_tools::{Tool, ToolContext};

/// A lexical fallback judge: score by query-word overlap. Used when no
/// Jev client is installed.
struct LexicalJudge;

#[async_trait]
impl Judge for LexicalJudge {
    async fn score_batch(&self, question: &str, candidates: &[String]) -> Vec<f32> {
        let words: Vec<String> = question
            .split_whitespace()
            .map(|w| w.to_lowercase())
            .collect();
        candidates
            .iter()
            .map(|c| {
                if words.is_empty() {
                    return 0.0;
                }
                let cl = c.to_lowercase();
                let hits = words.iter().filter(|w| cl.contains(w.as_str())).count();
                hits as f32 / words.len() as f32
            })
            .collect()
    }
}

/// A Jev-backed judge. One Jev batch call per `score_batch`, with a
/// per-candidate question.
struct JevJudge {
    client: Arc<dyn JevDecider>,
    /// A stable state label so the Jev cache keys on the query.
    state_label: String,
}

#[async_trait]
impl Judge for JevJudge {
    async fn score_batch(&self, question: &str, candidates: &[String]) -> Vec<f32> {
        if candidates.is_empty() {
            return Vec::new();
        }
        // Build one `(key, question)` pair per candidate. The batch
        // call returns `(key, probability)`.
        let questions: Vec<(String, String)> = candidates
            .iter()
            .enumerate()
            .map(|(i, c)| {
                (
                    i.to_string(),
                    format!(
                        "Does this code relate to the query {question:?}? \
                         Answer yes or no.\n\n{c}",
                    ),
                )
            })
            .collect();
        let state = crate::jev::build_state(&self.state_label, &[]);
        match self.client.evaluate_yes_no_batch(&state, &questions).await {
            Ok(answers) => {
                let mut out = vec![0.0f32; candidates.len()];
                for (key, p) in answers {
                    if let Ok(i) = key.parse::<usize>()
                        && i < out.len()
                    {
                        out[i] = p;
                    }
                }
                out
            }
            Err(e) => {
                tracing::warn!(error = %e, "jfind: Jev batch failed; scoring all zero");
                vec![0.0; candidates.len()]
            }
        }
    }
}

/// The `jfind` tool.
pub struct JfindTool {
    pub definition: ToolDefinition,
    judge: Arc<dyn Judge>,
    /// Whether the judge is Jev-backed (semantic) or the lexical
    /// fallback. Drives the result's `judge` field.
    judge_is_semantic: bool,
}

impl JfindTool {
    /// Build with a Jev client (semantic). `None` uses the lexical
    /// fallback.
    pub fn new(client: Option<Arc<dyn JevDecider>>) -> Self {
        let (judge, judge_is_semantic): (Arc<dyn Judge>, bool) = match client {
            Some(c) => (
                Arc::new(JevJudge {
                    client: c,
                    state_label: "jfind".to_string(),
                }),
                true,
            ),
            None => (Arc::new(LexicalJudge), false),
        };
        Self {
            definition: ToolDefinition {
                trust_level: kod_types::trust::TrustLevel::default(),
                id: ToolId::new(),
                name: "jfind".to_string(),
                description: "Semantic code search: describe what you are looking \
                    for in plain language and get back files + line ranges, not \
                    exact-string matches. Use it when `grep` cannot express the \
                    query (\"where is retry backoff computed\"). Without a judge \
                    model configured this falls back to a lexical ranker, and the \
                    result says so."
                    .to_string(),
                category: ToolCategory::FileSystem,
                parameters_schema: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "query": {
                            "type": "string",
                            "description": "Plain-language description of what to find"
                        },
                        "keywords": {
                            "type": "array",
                            "items": { "type": "string" },
                            "description": "Optional lexical keywords for the first (cheap) wave"
                        }
                    },
                    "required": ["query"]
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
                load_mode: Default::default(),
            },
            judge,
            judge_is_semantic,
        }
    }

    /// True when the semantic (Jev) judge is in use.
    pub fn is_semantic(&self) -> bool {
        self.judge_is_semantic
    }
}

#[async_trait]
impl Tool for JfindTool {
    fn definition(&self) -> ToolDefinition {
        self.definition.clone()
    }

    async fn execute(&self, params: &Value, context: &ToolContext) -> Result<ToolResult> {
        let query_text = params
            .get("query")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .trim()
            .to_string();
        if query_text.is_empty() {
            return Ok(ToolResult::Error("jfind: 'query' is required".to_string()));
        }
        let keywords: Vec<String> = params
            .get("keywords")
            .and_then(|v| v.as_array())
            .map(|a| {
                a.iter()
                    .filter_map(|v| v.as_str().map(str::to_string))
                    .collect()
            })
            .unwrap_or_default();
        let query = Query {
            text: query_text,
            grep_keywords: keywords,
        };
        // The file list + read closure come from the working dir.
        let root = context.working_dir.clone();
        let files = kod_tools::tools::gitaware_walk(&root, true);
        let read = |p: &std::path::Path| std::fs::read_to_string(p).ok();
        let hits = jfind::search(&root, &query, Arc::clone(&self.judge), &read, &files).await;
        let semantic = self.is_semantic();
        let results: Vec<Value> = hits
            .iter()
            .map(|h| {
                serde_json::json!({
                    "file": h.path.to_string_lossy(),
                    "start_line": h.start_line,
                    "end_line": h.end_line,
                    "score": h.score,
                })
            })
            .collect();
        Ok(ToolResult::Success(serde_json::json!({
            "count": results.len(),
            "results": results,
            "judge": if semantic { "jev" } else { "lexical" },
            "note": if semantic {
                "semantic search (Jev judge)"
            } else {
                "no judge model configured; this is a lexical ranker, not semantic search"
            },
        })))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_lexical_fallback_scores_by_overlap() {
        let j = LexicalJudge;
        let rt = tokio::runtime::Runtime::new().unwrap();
        let scores = rt.block_on(j.score_batch(
            "retry backoff",
            &["retry backoff logic".to_string(), "unrelated".to_string()],
        ));
        assert!(scores[0] > scores[1], "got: {scores:?}");
        assert!(scores[0] > 0.0);
    }

    #[test]
    fn a_tool_without_jev_reports_the_fallback() {
        let t = JfindTool::new(None);
        assert!(!t.is_semantic());
        assert_eq!(t.definition().name, "jfind");
    }

    #[test]
    fn jfind_requires_a_query() {
        let t = JfindTool::new(None);
        let tmp = tempfile::TempDir::new().unwrap();
        let mut ctx = ToolContext::new(tmp.path());
        ctx.permissions.read_files = true;
        let rt = tokio::runtime::Runtime::new().unwrap();
        let r = rt
            .block_on(t.execute(&serde_json::json!({}), &ctx))
            .unwrap();
        assert!(matches!(r, ToolResult::Error(_)));
    }

    #[test]
    fn jfind_finds_a_file_by_keyword() {
        let tmp = tempfile::TempDir::new().unwrap();
        std::fs::write(
            tmp.path().join("retry.rs"),
            "fn backoff() { /* retry backoff logic */ }\n",
        )
        .unwrap();
        let t = JfindTool::new(None);
        let mut ctx = ToolContext::new(tmp.path());
        ctx.permissions.read_files = true;
        let rt = tokio::runtime::Runtime::new().unwrap();
        let r = rt
            .block_on(t.execute(
                &serde_json::json!({"query": "retry backoff", "keywords": ["retry"]}),
                &ctx,
            ))
            .unwrap();
        let ToolResult::Success(v) = r else {
            panic!("expected success, got {r:?}");
        };
        assert_eq!(v["judge"], "lexical");
        assert!(v["count"].as_u64().unwrap() >= 1, "got: {v}");
    }
}
