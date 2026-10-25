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
                load_mode: Default::default(),
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

        // `[policy.read_protection]` is installed by default and
        // `read_file` honours it, but this tool called only
        // `can_read` (the permission bitmask) and walked straight
        // past the protection. An explicitly-named protected file is
        // refused under `Refuse`, exactly as `read_file` refuses it;
        // under `Redact` the *displayed* lines are redacted below.
        let protection = context.read_protection.as_ref();
        let protected = |p: &std::path::Path| -> Option<kod_config::ReadMode> {
            let rp = protection?;
            if rp.matches(p) { Some(rp.mode) } else { None }
        };
        if resolved.is_file()
            && let Some(kod_config::ReadMode::Refuse) = protected(&resolved)
        {
            return Ok(ToolResult::Error(format!(
                "search_files refused: {} matches a read-protection pattern.",
                resolved.display(),
            )));
        }

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
        // PERF-04: search_files is the third hot walk caller. The
        // pre-fix code deep-cloned the cached `Vec<PathBuf>` on every
        // call; `Arc` makes a hit cheap. The `is_file()` filter moves
        // into the loop body below (inline `continue`) because we
        // cannot filter an `Arc<Vec<PathBuf>>` without re-collecting
        // — which would defeat the point.
        let files: std::sync::Arc<Vec<std::path::PathBuf>> = if resolved.is_file() {
            std::sync::Arc::new(vec![resolved.clone()])
        } else if resolved.is_dir() {
            crate::tools::gitaware_walk_arc(&resolved, true)
        } else {
            return Ok(ToolResult::Error(format!(
                "not found: {}",
                resolved.display()
            )));
        };

        // PERF-05: run the search loop on a blocking thread. Same
        // offload as grep — the loop does open/read/scan per file,
        // synchronous filesystem work that parks a tokio worker.
        let files_c = files.clone();
        let regex_c = regex.clone();
        let rp_owned = context.read_protection.clone();
        let redactor_owned = context.redactor.clone();
        // Bug-hunt 2026-10-24: same fix as `grep` / `list_files`
        // (34961905) -- `can_read` was applied to the root the
        // caller named, not to every file the walk descends into.
        // Clone the two lists (cheap: policy globs, not content)
        // so the walk closure can apply `is_path_allowed_by`.
        let forbidden_owned = context.permissions.forbidden_paths.clone();
        let allowed_owned = context.permissions.allowed_paths.clone();
        let context_lines_c = context_lines;
        let (hits, truncated) = tokio::task::spawn_blocking(move || {
            let mut hits: Vec<Value> = Vec::new();
            let mut truncated = false;
            let protected = |p: &std::path::Path| -> Option<kod_config::ReadMode> {
                let rp = rp_owned.as_ref()?;
                if rp.matches(p) { Some(rp.mode) } else { None }
            };
            'outer: for file in files_c.iter() {
                if !crate::context::is_path_allowed_by(
                    &forbidden_owned,
                    &allowed_owned,
                    file.as_path(),
                ) {
                    continue;
                }
                if !file.is_file() {
                    continue;
                }
                let file_mode = protected(file);
                if let Some(kod_config::ReadMode::Refuse) = file_mode {
                    continue;
                }
                if let Ok(md) = std::fs::metadata(file)
                    && md.len() > 8 * 1024 * 1024
                {
                    continue;
                }
                let text = match std::fs::read_to_string(file) {
                    Ok(t) => t,
                    Err(_) => continue,
                };
                let redact_line: Option<&kod_types::redact::Redactor> =
                    match (file_mode, redactor_owned.as_ref()) {
                        (Some(kod_config::ReadMode::Redact), Some(r)) => Some(r.as_ref()),
                        _ => None,
                    };
                let lines: Vec<&str> = text.lines().collect();
                for (idx, line) in lines.iter().enumerate() {
                    if regex_c.is_match(line) {
                        let start = idx.saturating_sub(context_lines_c as usize);
                        let end = (idx + context_lines_c as usize + 1).min(lines.len());
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
                            let truncated_line = match redact_line {
                                Some(r) => r.redact(&truncated_line).0,
                                None => truncated_line,
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
            (hits, truncated)
        })
        .await
        .map_err(|e| KodError::Internal(format!("search worker panicked: {e}")))?;

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

    /// A directory-wide grep must not surface a read-protected
    /// file's content. `read_file` honours `[policy.read_protection]`
    /// (redact/refuse), but `SearchFilesTool` called only `can_read`
    /// (the permission bitmask) and walked straight past the
    /// protection — so `grep pattern=KEY path=.` returned the secret
    /// line from a `.env` that `read_file` would have redacted.
    /// `read_protection` is installed by default (CLI/TUI call
    /// `set_read_protection`), so this was live.
    #[tokio::test]
    async fn grep_does_not_leak_a_protected_path() {
        let tmp = tempfile::TempDir::new().unwrap();
        std::fs::write(
            tmp.path().join(".env"),
            "API_KEY=sk-abcdef1234567890ABCDEFGH\nOTHER=ok\n",
        )
        .unwrap();
        std::fs::write(tmp.path().join("normal.rs"), "let x = 1;\n").unwrap();

        let mut ctx = ctx(tmp.path());
        ctx.redactor = Some(std::sync::Arc::new(kod_types::redact::Redactor::default()));
        ctx.read_protection = Some(kod_config::ReadProtection {
            enabled: true,
            mode: kod_config::ReadMode::Redact,
            deny: vec!["**/.env".to_string()],
        });

        let tool = SearchFilesTool::new();
        // A pattern that matches the secret line.
        let r = tool
            .execute(
                &serde_json::json!({"path": ".", "pattern": "API_KEY"}),
                &ctx,
            )
            .await
            .unwrap();
        let body = format!("{r:?}");
        assert!(
            !body.contains("sk-abcdef"),
            "grep leaked a protected file's secret: {body}"
        );
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
        std::fs::write(tmp.path().join("a.txt"), "1\n2\n3\nneedle\n5\n6\n7\n").unwrap();
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

    /// Bug-hunt 2026-10-24: `search_files` uses the same walk shape as
    /// `grep`/`list_files` (fixed at 34961905). The walk descends into
    /// subdirectories but only checks `read_protection` per file --
    /// not `forbidden_paths` / `allowed_paths`. With the union wiring
    /// at c4c5f0df the field is populated, so `search_files` was
    /// reading files under a forbidden path.
    #[tokio::test]
    async fn search_files_skips_files_under_forbidden_paths() {
        let temp = tempfile::TempDir::new().unwrap();
        std::fs::create_dir_all(temp.path().join("secrets")).unwrap();
        std::fs::write(temp.path().join("secrets/creds.txt"), "SECRET_MARKER\n").unwrap();
        std::fs::write(temp.path().join("readme.txt"), "not a secret\n").unwrap();

        let ctx = ToolContext::new(temp.path());
        let mut perms = ctx.permissions.clone();
        perms.read_files = true;
        perms.forbidden_paths = vec!["**/secrets/**".to_string()];
        let mut ctx = ctx;
        ctx.permissions = perms;

        let r = SearchFilesTool::new()
            .execute(
                &serde_json::json!({"path": ".", "pattern": "SECRET_MARKER"}),
                &ctx,
            )
            .await
            .unwrap();
        let v = match &r {
            ToolResult::Success(v) => v,
            other => panic!("search_files failed: {other:?}"),
        };
        let results = v
            .get("results")
            .and_then(|x| x.as_array())
            .expect("search_files output has `results` array");
        for hit in results {
            let file = hit.get("file").and_then(|x| x.as_str()).unwrap_or("");
            assert!(
                !file.contains("secrets"),
                "search_files must not read files under `forbidden`; got hit in {file:?}",
            );
        }
        assert!(
            results.is_empty(),
            "no match expected: readme.txt does not contain the pattern \
             and secrets/creds.txt must be skipped; got {results:?}",
        );
    }
}
