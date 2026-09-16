//! Read-only git tools: `git_status` and `git_diff`.
//!
//! Both shell out to the `git` binary rather than linking `git2`. The
//! binary is what every developer already has installed and configured
//! (aliases, `core.pager`, the SSH agent, credential helpers), while a
//! linked libgit2 would have to reimplement or bypass each of those.
//! Read-only tools that shell out are also trivially auditable — the
//! exact command line is a `tracing::debug!` away.
//!
//! Both tools are read-only by contract: they never write to the index,
//! the worktree, or the config. `git_status` runs `git status
//! --porcelain=v2 -b`; `git_diff` runs `git diff` with the caller's
//! options. A future `git_commit`/`git_branch` tool would need the
//! `git_operations` permission flag; these two still request it, so a
//! context that denies git is denied uniformly.

use crate::{Tool, ToolContext};
use kod_error::{KodError, Result};
use kod_types::{ToolCategory, ToolDefinition, ToolId, ToolPermissions, ToolResult};
use serde_json::Value;

/// Byte cap for git output rendered back to the model. A `git diff` on a
/// large refactor, or a `git status` in a repo with a stray `target/`,
/// can otherwise produce tens of thousands of lines. 64 KB is generous
/// for the useful case (a diff the model can actually read) and cheap to
/// bound the useless one.
const MAX_GIT_OUTPUT_BYTES: usize = 64 * 1024;

/// Run `git <args>` in `wd`. Returns the trimmed stdout on exit 0, an
/// error naming the failing args and the stderr otherwise. Timeout is
/// the caller's `context.timeout_secs`, matching the other tools.
async fn run_git(args: &[&str], wd: &std::path::Path, timeout_secs: u64) -> Result<String> {
    let mut cmd = tokio::process::Command::new("git");
    cmd.args(args)
        .current_dir(wd)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());
    // `GIT_PAGER` and `PAGER` off: the pager would consume the pipe and
    // the tool would see nothing. `git` also honors `-c core.pager=` but
    // the env var is what a user's shell sets.
    cmd.env("GIT_PAGER", "cat").env("PAGER", "cat");

    let fut = cmd.output();
    let effective = timeout_secs.max(1);
    let output = match tokio::time::timeout(
        std::time::Duration::from_secs(effective),
        fut,
    )
    .await
    {
        Ok(Ok(o)) => o,
        Ok(Err(e)) => {
            return Err(KodError::ToolExecution {
                tool_name: "git".to_string(),
                reason: format!(
                    "could not run `git {}`: {}. Is git installed and on PATH?",
                    args.join(" "),
                    e
                ),
            });
        }
        Err(_) => {
            return Err(KodError::ToolExecution {
                tool_name: "git".to_string(),
                reason: format!(
                    "`git {}` did not finish within {}s",
                    args.join(" "),
                    effective
                ),
            });
        }
    };

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let stderr = stderr.trim();
        let detail = if stderr.is_empty() {
            format!("exit status {:?}", output.status.code())
        } else {
            stderr.to_string()
        };
        return Err(KodError::ToolExecution {
            tool_name: "git".to_string(),
            reason: format!("`git {}` failed: {}", args.join(" "), detail),
        });
    }

    let raw = String::from_utf8_lossy(&output.stdout);
    let text = raw.trim_end_matches('\n').to_string();
    Ok(text)
}

/// Truncate at a UTF-8 boundary, appending a one-line notice when the
/// content was cut. Byte count, not tokens — this is the raw-output cap,
/// not the prompt cap.
fn truncate_git_output(s: &str) -> (String, bool) {
    if s.len() <= MAX_GIT_OUTPUT_BYTES {
        return (s.to_string(), false);
    }
    let mut end = MAX_GIT_OUTPUT_BYTES;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    let removed = s.len() - end;
    (
        format!(
            "{}\n… [truncated {} bytes — narrow with `path` or `staged`]",
            &s[..end],
            removed
        ),
        true,
    )
}

/// `git status --porcelain=v2 -b`.
pub struct GitStatusTool {
    pub definition: ToolDefinition,
}

impl GitStatusTool {
    pub fn new() -> Self {
        Self {
            definition: ToolDefinition {
                id: ToolId::new(),
                name: "git_status".to_string(),
                description: "Show the repository status: current branch, staged and unstaged changes, and untracked files. Read-only; never modifies the index or the worktree.".to_string(),
                category: ToolCategory::Git,
                parameters_schema: serde_json::json!({
                    "type": "object",
                    "properties": {},
                    "additionalProperties": false
                }),
                permissions: ToolPermissions {
                    read_files: false,
                    write_files: false,
                    execute_commands: false,
                    network_access: false,
                    git_operations: true,
                    allowed_paths: Vec::new(),
                    forbidden_paths: Vec::new(),
                },
            },
        }
    }
}

impl Default for GitStatusTool {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait::async_trait]
impl Tool for GitStatusTool {
    fn definition(&self) -> ToolDefinition {
        self.definition.clone()
    }

    async fn execute(&self, _params: &Value, context: &ToolContext) -> Result<ToolResult> {
        context.can_git_operation()?;

        let raw = match run_git(
            &["status", "--porcelain=v2", "-b"],
            &context.working_dir,
            context.timeout_secs,
        )
        .await
        {
            Ok(s) => s,
            Err(e) => return Ok(ToolResult::Error(e.to_string())),
        };

        let (rendered, truncated) = truncate_git_output(&raw);

        // Parse the branch line out of porcelain v2 so the model gets
        // the branch without having to parse it out of a header string.
        // The `# branch.head` line is stable across git 2.11+.
        let mut branch: Option<String> = None;
        let mut ahead: Option<u64> = None;
        let mut behind: Option<u64> = None;
        let mut changed_files: Vec<String> = Vec::new();
        for line in rendered.lines() {
            if let Some(rest) = line.strip_prefix("# branch.head ") {
                branch = Some(rest.trim().to_string());
            } else if let Some(rest) = line.strip_prefix("# branch.ab ") {
                for tok in rest.split_whitespace() {
                    if let Some(n) = tok.strip_prefix('+') {
                        ahead = n.parse().ok();
                    } else if let Some(n) = tok.strip_prefix('-') {
                        behind = n.parse().ok();
                    }
                }
            } else if !line.starts_with('#') {
                // Porcelain v2 entries: "1 XY ...", "2 XY ...", "? path",
                // "u XY ...". The status code and the path are separated
                // by spaces or a tab; taking the last whitespace-split
                // token is wrong for paths with spaces, so split once
                // from the left on the first non-`?` boundary instead.
                if let Some(rest) = line.strip_prefix("? ") {
                    changed_files.push(rest.to_string());
                } else if let Some(rest) = line.strip_prefix("1 ").or_else(|| line.strip_prefix("2 ")) {
                    // Format: "<XY> <sub> <mH> <mI> <mW> <hH> <hI> <path>"
                    // — path is after the 7th space-separated field.
                    let mut parts = rest.splitn(8, ' ');
                    let _xy = parts.next();
                    let _sub = parts.next();
                    let _mh = parts.next();
                    let _mi = parts.next();
                    let _mw = parts.next();
                    let _hh = parts.next();
                    let _hi = parts.next();
                    if let Some(path) = parts.next() {
                        changed_files.push(path.to_string());
                    }
                }
            }
        }

        Ok(ToolResult::Success(serde_json::json!({
            "branch": branch,
            "ahead": ahead,
            "behind": behind,
            "changed_files": changed_files,
            "changed_count": changed_files.len(),
            "raw": rendered,
            "truncated": truncated,
        })))
    }
}

/// `git diff [--staged] [--stat] [path]`.
pub struct GitDiffTool {
    pub definition: ToolDefinition,
}

impl GitDiffTool {
    pub fn new() -> Self {
        Self {
            definition: ToolDefinition {
                id: ToolId::new(),
                name: "git_diff".to_string(),
                description: "Show a diff. By default, the unstaged changes in the worktree. With `staged: true`, the changes already staged for the next commit. `stat: true` returns the summary (`--stat`) instead of the full patch. Optionally restrict to `path`.".to_string(),
                category: ToolCategory::Git,
                parameters_schema: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "staged": {
                            "type": "boolean",
                            "description": "Diff the index (staged) instead of the worktree (unstaged). Default false."
                        },
                        "stat": {
                            "type": "boolean",
                            "description": "Return the --stat summary instead of the full patch. Default false."
                        },
                        "path": {
                            "type": "string",
                            "description": "Optional path (file or directory) to restrict the diff to."
                        }
                    },
                    "additionalProperties": false
                }),
                permissions: ToolPermissions {
                    read_files: true,
                    write_files: false,
                    execute_commands: false,
                    network_access: false,
                    git_operations: true,
                    allowed_paths: Vec::new(),
                    forbidden_paths: Vec::new(),
                },
            },
        }
    }
}

impl Default for GitDiffTool {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait::async_trait]
impl Tool for GitDiffTool {
    fn definition(&self) -> ToolDefinition {
        self.definition.clone()
    }

    async fn execute(&self, params: &Value, context: &ToolContext) -> Result<ToolResult> {
        context.can_git_operation()?;

        let staged = params["staged"].as_bool().unwrap_or(false);
        let stat = params["stat"].as_bool().unwrap_or(false);
        let path = params["path"].as_str();

        // Resolve and validate a caller-supplied path through the same
        // resolver the filesystem tools use, so a path outside the
        // working directory is rejected before it reaches git. git
        // itself would refuse an out-of-tree path, but the error would
        // be a git message, not the tool's own permission message.
        let mut args: Vec<String> = vec!["diff".to_string()];
        if staged {
            args.push("--staged".to_string());
        }
        if stat {
            args.push("--stat".to_string());
        }
        // `--no-color` so the model does not get ANSI escapes it will
        // have to strip; `--no-ext-diff` so a user's configured
        // external diff does not turn the call into a GUI launch.
        args.push("--no-color".to_string());
        args.push("--no-ext-diff".to_string());
        if let Some(p) = path {
            let resolved = context.resolve_path(p)?;
            context.can_read(&resolved)?;
            // git wants a path relative to the repo root or a path git
            // understands; passing the canonical absolute path works
            // for both in-tree and sub-directory invocations.
            args.push(resolved.to_string_lossy().to_string());
        }

        let arg_refs: Vec<&str> = args.iter().map(|s| s.as_str()).collect();
        let raw = match run_git(&arg_refs, &context.working_dir, context.timeout_secs).await {
            Ok(s) => s,
            Err(e) => return Ok(ToolResult::Error(e.to_string())),
        };

        let (rendered, truncated) = truncate_git_output(&raw);

        Ok(ToolResult::Success(serde_json::json!({
            "staged": staged,
            "stat": stat,
            "path": path,
            "empty": rendered.is_empty(),
            "diff": rendered,
            "truncated": truncated,
        })))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use kod_types::ToolPermissions;

    /// Context with git operations enabled. The other permission flags
    /// stay at their (false) defaults so a test that accidentally reads
    /// a file fails loudly.
    fn git_ctx(dir: &std::path::Path) -> ToolContext {
        ToolContext::new(dir).with_permissions(ToolPermissions {
            read_files: true,
            git_operations: true,
            ..Default::default()
        })
    }

    /// A temp dir initialized as a git repo with one commit. Returns the
    /// dir (kept alive) so the test can query status/diff against a
    /// known-good repository.
    async fn init_repo() -> (tempfile::TempDir, std::path::PathBuf) {
        let tmp = tempfile::TempDir::new().unwrap();
        let path = tmp.path().to_path_buf();
        // Best-effort: `git init` requires `git` on PATH; if it is not
        // available, the tests that follow would fail anyway, but the
        // error here is the honest one.
        let _ = std::process::Command::new("git")
            .args(["init", "-q", "-b", "main"])
            .current_dir(&path)
            .status()
            .expect("git init failed");
        // Configure user so commit works in environments without a
        // global identity (CI, fresh containers).
        let _ = std::process::Command::new("git")
            .args(["config", "user.email", "test@example.com"])
            .current_dir(&path)
            .status();
        let _ = std::process::Command::new("git")
            .args(["config", "user.name", "Test"])
            .current_dir(&path)
            .status();
        std::fs::write(path.join("seed.txt"), "seed\n").unwrap();
        let _ = std::process::Command::new("git")
            .args(["add", "seed.txt"])
            .current_dir(&path)
            .status();
        let _ = std::process::Command::new("git")
            .args(["commit", "-q", "-m", "seed"])
            .current_dir(&path)
            .status();
        (tmp, path)
    }

    #[tokio::test]
    async fn git_status_clean_repo_reports_no_changes() {
        let (_tmp, repo) = init_repo().await;
        let tool = GitStatusTool::new();
        let result = tool
            .execute(&serde_json::json!({}), &git_ctx(&repo))
            .await
            .unwrap();
        match result {
            ToolResult::Success(v) => {
                assert_eq!(v["changed_count"], 0, "clean repo must report 0: {v}");
                assert_eq!(v["branch"], "main");
            }
            other => panic!("expected success, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn git_status_sees_untracked_file() {
        let (_tmp, repo) = init_repo().await;
        std::fs::write(repo.join("new.txt"), "hi\n").unwrap();
        let tool = GitStatusTool::new();
        let result = tool
            .execute(&serde_json::json!({}), &git_ctx(&repo))
            .await
            .unwrap();
        match result {
            ToolResult::Success(v) => {
                assert_eq!(v["changed_count"], 1);
                let files = v["changed_files"].as_array().unwrap();
                assert_eq!(files[0], "new.txt");
            }
            other => panic!("expected success, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn git_status_denied_without_permission() {
        let (_tmp, repo) = init_repo().await;
        let ctx = ToolContext::new(&repo).with_permissions(ToolPermissions {
            read_files: true,
            git_operations: false,
            ..Default::default()
        });
        let tool = GitStatusTool::new();
        let result = tool.execute(&serde_json::json!({}), &ctx).await;
        match result {
            Err(KodError::PermissionDenied { action, .. }) => {
                assert!(action.contains("git"), "got: {action}");
            }
            other => panic!("expected PermissionDenied, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn git_diff_shows_unstaged_change() {
        let (_tmp, repo) = init_repo().await;
        std::fs::write(repo.join("seed.txt"), "seed\nmore\n").unwrap();
        let tool = GitDiffTool::new();
        let result = tool
            .execute(&serde_json::json!({}), &git_ctx(&repo))
            .await
            .unwrap();
        match result {
            ToolResult::Success(v) => {
                assert_eq!(v["empty"], false);
                let diff = v["diff"].as_str().unwrap();
                assert!(diff.contains("+more"), "diff should show the addition: {diff}");
            }
            other => panic!("expected success, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn git_diff_stat_returns_summary() {
        let (_tmp, repo) = init_repo().await;
        std::fs::write(repo.join("seed.txt"), "seed\nmore\n").unwrap();
        let tool = GitDiffTool::new();
        let result = tool
            .execute(&serde_json::json!({ "stat": true }), &git_ctx(&repo))
            .await
            .unwrap();
        match result {
            ToolResult::Success(v) => {
                let diff = v["diff"].as_str().unwrap();
                assert!(
                    diff.contains("seed.txt"),
                    "stat should name the file: {diff}"
                );
            }
            other => panic!("expected success, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn git_diff_on_clean_repo_is_empty() {
        let (_tmp, repo) = init_repo().await;
        let tool = GitDiffTool::new();
        let result = tool
            .execute(&serde_json::json!({}), &git_ctx(&repo))
            .await
            .unwrap();
        match result {
            ToolResult::Success(v) => {
                assert_eq!(v["empty"], true);
                assert_eq!(v["diff"], "");
            }
            other => panic!("expected success, got {other:?}"),
        }
    }
}
