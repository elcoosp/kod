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

/// Read a file for the judge, honouring `[policy.read_protection]`.
///
/// `read_file` and `grep` both consult this policy; `jfind` shipped the
/// raw content of every file in its candidate list to the judge, and
/// with a Jev client installed that judge is a network call. A
/// `Refuse`-mode match never reaches this function (the caller filters
/// it out), so the only case here is `Redact`: the file is read, and
/// the judge sees the redacted form.
///
/// With no redactor installed, the content cannot be made safe, so it
/// is not returned at all. Failing closed is the only correct choice —
/// returning the raw text would silently undo the policy.
fn read_for_judge(
    path: &std::path::Path,
    protection: Option<&kod_config::ReadProtection>,
    redactor: Option<&kod_types::redact::Redactor>,
) -> Option<String> {
    let text = std::fs::read_to_string(path).ok()?;
    match protection {
        Some(rp) if rp.matches(path) => redactor.map(|r| r.redact(&text).0),
        _ => Some(text),
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
        //
        // Bug-hunt 2026-10-24: same shape as `grep`/`list_files`/
        // `search_files` before their fixes -- the walk handed the
        // full list to the searcher without consulting the user's
        // `forbidden_paths`. `jfind::search` iterates the `list`
        // argument directly (never re-walks), so filtering the list
        // here is the whole fix: a forbidden file never reaches the
        // `read` closure and cannot contribute a `Hit`.
        let root = context.working_dir.clone();
        // `[policy.read_protection]` is installed by default and
        // protects `.env`, `*.pem`, `id_rsa*`, and friends. `read_file`
        // and `grep` honour it; this tool called only `is_path_allowed`
        // (the forbidden-glob check) and then read every remaining file
        // and shipped up to SKETCH_BYTES of it to the judge. With a Jev
        // client installed that judge is a network call, so a protected
        // file's content left the process.
        //
        // Two responses, matching `read_file`'s: a `Refuse`-mode match
        // is skipped entirely (never read, never a candidate), and a
        // `Redact`-mode match is read and redacted before the judge
        // sees it. The redactor is the same one `read_file` uses.
        let protection = context.read_protection.clone();
        let redactor = context.redactor.clone();
        let files: Vec<std::path::PathBuf> = kod_tools::tools::gitaware_walk(&root, true)
            .into_iter()
            .filter(|p| context.is_path_allowed(p).unwrap_or(true))
            .filter(|p| {
                !matches!(
                    protection.as_ref().map(|rp| (rp, rp.matches(p))),
                    Some((rp, true)) if rp.mode == kod_config::ReadMode::Refuse
                )
            })
            .collect();
        let read = |p: &std::path::Path| -> Option<String> {
            read_for_judge(p, protection.as_ref(), redactor.as_deref())
        };
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

    /// Bug-hunt 2026-10-24: `JfindTool::execute` walks the working
    /// dir with `gitaware_walk` and hands the full list to
    /// `jfind::search`. No per-file `forbidden_paths` check -- same
    /// shape as `grep`/`list_files`/`search_files` before their
    /// fixes.
    ///
    /// The fixture is built so the *only* thing the fix changes is
    /// whether `secrets/` contributes a hit:
    ///
    ///   - `secret-notes.txt` (content "secret data") -- positive
    ///     control, must appear in results with or without the fix.
    ///   - `secrets/secret.txt` (content "secret data") -- forbidden,
    ///     must appear WITHOUT the fix (proving the bug) and must
    ///     NOT appear WITH it.
    ///
    /// Both files satisfy every stage of jfind's cascade:
    ///   Wave 1 (lexical_rank): path contains "secret" -> candidate
    ///   Wave 2 (judge_filenames): filename contains "secret" ->
    ///     LexicalJudge score 1.0 > 0 -> kept
    ///   Wave 3 (content): "secret data" contains "secret" -> score
    ///     1.0 > CUTOFF(0.45) and > FILE_THRESHOLD(0.2) -> Hit
    #[tokio::test]
    async fn jfind_skips_files_under_forbidden_paths() {
        let temp = tempfile::TempDir::new().unwrap();
        std::fs::create_dir_all(temp.path().join("secrets")).unwrap();
        std::fs::write(temp.path().join("secrets/secret.txt"), "secret data\n").unwrap();
        std::fs::write(temp.path().join("secret-notes.txt"), "secret data\n").unwrap();

        let mut ctx = kod_tools::context::ToolContext::new(temp.path());
        ctx.permissions.forbidden_paths = vec!["**/secrets/**".to_string()];

        let tool = JfindTool::new(None);
        let r = tool
            .execute(
                &serde_json::json!({
                    "query": "secret",
                    "keywords": ["secret"],
                }),
                &ctx,
            )
            .await
            .unwrap();
        let v = match &r {
            ToolResult::Success(v) => v,
            other => panic!("jfind failed: {other:?}"),
        };
        let results = v
            .get("results")
            .and_then(|x| x.as_array())
            .expect("jfind output has `results` array");

        // Positive control: the non-forbidden file must produce a hit.
        let has_notes = results.iter().any(|h| {
            h.get("file")
                .and_then(|x| x.as_str())
                .map(|s| s.contains("secret-notes.txt"))
                .unwrap_or(false)
        });
        assert!(
            has_notes,
            "positive control failed: `secret-notes.txt` must be reported; got {results:?}",
        );

        // The bug: a hit under a forbidden path must not be reported.
        let has_forbidden = results.iter().any(|h| {
            h.get("file")
                .and_then(|x| x.as_str())
                .map(|s| s.contains("secrets/"))
                .unwrap_or(false)
        });
        assert!(
            !has_forbidden,
            "jfind must not report files under a forbidden path; got {results:?}",
        );
    }
    /// A `Refuse`-mode read-protection pattern must keep the file out
    /// of `jfind` entirely.
    ///
    /// `read_file` and `grep` both honour `[policy.read_protection]`;
    /// this tool did not. It checked `is_path_allowed` (forbidden
    /// globs) and then read every remaining file and shipped up to
    /// SKETCH_BYTES of it to the judge. With a Jev client installed
    /// that judge is a network call, so `.env` / `*.pem` / `id_rsa*`
    /// content left the process. The default policy protects exactly
    /// those paths, so this was live for every user who had not
    /// disabled read protection.
    #[tokio::test]
    async fn jfind_skips_refuse_mode_protected_files() {
        let temp = tempfile::TempDir::new().unwrap();
        std::fs::write(temp.path().join("secret-credentials.env"), "secret data\n").unwrap();
        std::fs::write(temp.path().join("secret-notes.txt"), "secret data\n").unwrap();

        let mut ctx = kod_tools::context::ToolContext::new(temp.path());
        ctx.read_protection = Some(kod_config::ReadProtection {
            enabled: true,
            mode: kod_config::ReadMode::Refuse,
            deny: vec!["**/*.env".to_string()],
        });

        let tool = JfindTool::new(None);
        let r = tool
            .execute(
                &serde_json::json!({
                    "query": "secret",
                    "keywords": ["secret"],
                }),
                &ctx,
            )
            .await
            .unwrap();
        let v = match &r {
            ToolResult::Success(v) => v,
            other => panic!("jfind failed: {other:?}"),
        };
        let results = v
            .get("results")
            .and_then(|x| x.as_array())
            .expect("jfind output has `results` array");

        // Positive control: the unprotected file still produces a hit.
        assert!(
            results.iter().any(|h| h
                .get("file")
                .and_then(|x| x.as_str())
                .map(|s| s.contains("secret-notes.txt"))
                .unwrap_or(false)),
            "the unprotected file must still be found; got {results:?}",
        );
        // The protected file must not appear at all.
        assert!(
            !results.iter().any(|h| h
                .get("file")
                .and_then(|x| x.as_str())
                .map(|s| s.contains("secret-credentials.env"))
                .unwrap_or(false)),
            "jfind must not report a Refuse-protected file; got {results:?}",
        );
    }

    /// `read_for_judge` must redact a `Redact`-mode protected file
    /// before returning it.
    ///
    /// This is the half of the fix that keeps `Redact` useful: `Refuse`
    /// skips the file entirely, `Redact` reads it and strips the secret
    /// before the judge sees it. Returning the raw text would leak the
    /// same content under a policy the user thought was safe.
    #[test]
    fn read_for_judge_redacts_a_protected_file() {
        let temp = tempfile::TempDir::new().unwrap();
        let path = temp.path().join("secret-config.env");
        std::fs::write(&path, "api_key = sk-abcdef0123456789abcdef0123456789\n").unwrap();

        let rp = kod_config::ReadProtection {
            enabled: true,
            mode: kod_config::ReadMode::Redact,
            deny: vec!["**/*.env".to_string()],
        };
        let redactor = kod_types::redact::Redactor::default();
        let out = read_for_judge(&path, Some(&rp), Some(&redactor))
            .expect("a Redact-mode file is still readable");
        assert!(
            !out.contains("sk-abcdef0123456789"),
            "the secret must be redacted; got {out:?}",
        );
    }

    /// No redactor installed plus a protected file: fail closed.
    #[test]
    fn read_for_judge_fails_closed_without_a_redactor() {
        let temp = tempfile::TempDir::new().unwrap();
        let path = temp.path().join("secret-config.env");
        std::fs::write(&path, "api_key = sk-abcdef0123456789abcdef0123456789\n").unwrap();

        let rp = kod_config::ReadProtection {
            enabled: true,
            mode: kod_config::ReadMode::Redact,
            deny: vec!["**/*.env".to_string()],
        };
        assert!(
            read_for_judge(&path, Some(&rp), None).is_none(),
            "without a redactor the content must not be returned at all",
        );
    }

    /// An unprotected file passes through unchanged.
    #[test]
    fn read_for_judge_passes_an_unprotected_file_through() {
        let temp = tempfile::TempDir::new().unwrap();
        let path = temp.path().join("notes.txt");
        std::fs::write(&path, "ordinary content\n").unwrap();

        let rp = kod_config::ReadProtection {
            enabled: true,
            mode: kod_config::ReadMode::Redact,
            deny: vec!["**/*.env".to_string()],
        };
        let redactor = kod_types::redact::Redactor::default();
        let out = read_for_judge(&path, Some(&rp), Some(&redactor)).unwrap();
        assert_eq!(out, "ordinary content\n");
    }
}
