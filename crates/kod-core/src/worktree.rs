//! Per-agent git worktrees (D4-D1).
//!
//! When a swarm runs against a git repository, each agent gets its own
//! `git worktree` so writes from one agent cannot race or overwrite
//! another's. The manager wraps the `git` binary rather than `git2`
//! — the same choice `kod-tools/src/git.rs` makes: the CLI is what
//! every developer already has configured (aliases, credential
//! helpers, SSH agent), and read-only git tools that shell out are
//! trivially auditable.
//!
//! # Layout
//!
//! Worktrees live under `<repo>/.kod/worktrees/<slug>` on branch
//! `kod/agent-<slug>`, branched from HEAD at swarm start. The parent
//! `.kod/` is added to `.gitignore` on first creation — idempotent,
//! so a repo that already ignores it is left alone.
//!
//! # Failure modes
//!
//! Every operation is best-effort with a clear `Result`. A repo that
//! is not a git checkout, a disk cap exceeded, a merge conflict, or a
//! `git worktree add` failure all surface as errors the caller can
//! act on. The alternative — silently sharing the repo root when
//! worktrees are unavailable — is the pre-D4 behaviour and is exactly
//! what we are moving away from; the swarm runner decides whether to
//! fall back.

use kod_error::{KodError, Result};
use std::path::{Path, PathBuf};
use std::process::Stdio;

/// One worktree created by the manager.
#[derive(Debug, Clone)]
pub struct WorktreeInfo {
    /// Short slug used to build the path and branch (`agent-1`,
    /// `schema-writer`, ...). Sanitized by the caller or by
    /// [`sanitize_slug`].
    pub slug: String,
    pub path: PathBuf,
    pub branch: String,
}

/// Outcome of a merge sequence.
#[derive(Debug, Clone, Default)]
pub struct MergeReport {
    /// Branches that merged cleanly, in the order they were merged.
    pub merged: Vec<String>,
    /// Files that produced a conflict. The merge is aborted (not left
    /// in a half-applied state) so the caller can present the list and
    /// decide.
    pub conflicted: Vec<PathBuf>,
    /// Branches that failed to merge for reasons other than a
    /// conflict (dirty index, missing branch). Naming them keeps a
    /// partially-successful merge legible.
    pub failed: Vec<(String, String)>,
}

/// Manages a set of worktrees for one swarm run.
pub struct WorktreeManager {
    repo: PathBuf,
    base_commit: String,
    created: Vec<WorktreeInfo>,
    /// Branches whose work merged cleanly. Only these get `git branch
    /// -D` on cleanup; an unmerged branch holds the only copy of an
    /// agent's commits and deleting it loses the work.
    merged_ok: std::collections::HashSet<String>,
    /// When true, cleanup() also deletes unmerged branches. Set by the
    /// caller that has explicitly accepted the loss (a `--force`
    /// style flag, or after a manual reconcile).
    force_cleanup: bool,
    disk_cap_mb: u64,
    git_timeout_secs: u64,
}

impl WorktreeManager {
    /// Detect whether `repo` is a git checkout and capture the current
    /// HEAD. `Ok(None)` means "not a git repo"; the caller falls back
    /// to the shared workspace. `Ok(Some(mgr))` means worktrees are
    /// available.
    pub fn detect(repo: &Path) -> Result<Option<Self>> {
        let inside = run_git(repo, &["rev-parse", "--is-inside-work-tree"], 10);
        match inside {
            Ok(out) if out.trim() == "true" => {}
            _ => return Ok(None),
        }
        let head = match run_git(repo, &["rev-parse", "HEAD"], 10) {
            Ok(h) => h.trim().to_string(),
            Err(_) => return Ok(None),
        };
        Ok(Some(Self {
            repo: repo.to_path_buf(),
            base_commit: head,
            created: Vec::new(),
            merged_ok: std::collections::HashSet::new(),
            force_cleanup: false,
            // 512 MB default: enough for a mid-size Rust project's
            // target dir, small enough that a runaway agent cannot
            // fill a laptop's disk.
            disk_cap_mb: 512,
            git_timeout_secs: 60,
        }))
    }

    /// Override the per-worktree disk cap (MB).
    pub fn with_disk_cap_mb(mut self, mb: u64) -> Self {
        self.disk_cap_mb = mb.max(64);
        self
    }

    /// The base commit worktrees are branched from. Exposed so the
    /// merge step can verify it is merging onto the expected parent.
    pub fn base_commit(&self) -> &str {
        &self.base_commit
    }

    /// The worktrees created so far.
    pub fn created(&self) -> &[WorktreeInfo] {
        &self.created
    }

    /// Create a new worktree for `slug`. The slug is sanitized into a
    /// kebab-case token; the resulting path is
    /// `<repo>/.kod/worktrees/<slug>` and the branch is
    /// `kod/agent-<slug>`.
    pub fn create(&mut self, slug: &str) -> Result<WorktreeInfo> {
        let slug = sanitize_slug(slug);
        if slug.is_empty() {
            return Err(KodError::InvalidParameters {
                reason: "worktree: slug is empty after sanitization".to_string(),
            });
        }
        // Reject a duplicate slug: two agents with the same name would
        // collide on both the path and the branch.
        if self.created.iter().any(|w| w.slug == slug) {
            return Err(KodError::InvalidState(format!(
                "worktree: slug {slug:?} already exists in this manager"
            )));
        }

        // Ensure `.kod/` is ignored so the worktrees do not show up as
        // untracked files in the parent repo.
        self.ensure_gitignored()?;

        let worktrees_dir = self.repo.join(".kod").join("worktrees");
        std::fs::create_dir_all(&worktrees_dir).map_err(KodError::Io)?;
        let path = worktrees_dir.join(&slug);
        let branch = format!("kod/agent-{slug}");

        // `git worktree add -b <branch> <path> <base>` creates the
        // branch and checks it out in the new worktree.
        run_git_owned(
            &self.repo,
            &[
                "worktree",
                "add",
                "-b",
                &branch,
                path.to_string_lossy().as_ref(),
                &self.base_commit,
            ],
            self.git_timeout_secs,
        )
        .map_err(|e| {
            KodError::Internal(format!(
                "git worktree add {branch} {path}: {e}",
                branch = branch,
                path = path.display()
            ))
        })?;

        // Best-effort disk cap check: refuse if the fresh worktree
        // already exceeds the cap (a very large checkout). Real
        // enforcement during the run is a follow-up; today this is a
        // clear signal at create time.
        let used_mb = directory_size_mb(&path).unwrap_or(0);
        if used_mb > self.disk_cap_mb {
            // Roll back the worktree we just created; leaving a
            // half-state would be worse than a clean failure.
            let _ = run_git_owned(
                &self.repo,
                &[
                    "worktree",
                    "remove",
                    "--force",
                    path.to_string_lossy().as_ref(),
                ],
                self.git_timeout_secs,
            );
            let _ = run_git_owned(
                &self.repo,
                &["branch", "-D", &branch],
                self.git_timeout_secs,
            );
            return Err(KodError::Internal(format!(
                "worktree {slug:?} exceeds disk cap: {} MB > {} MB",
                used_mb, self.disk_cap_mb
            )));
        }

        let info = WorktreeInfo { slug, path, branch };
        self.created.push(info.clone());
        Ok(info)
    }

    /// Merge every created worktree back into the base branch, in
    /// creation order. On the first conflict the merge is aborted
    /// (`git merge --abort`) and the conflicted files are reported;
    /// subsequent branches are skipped so the caller sees the state
    /// they need to resolve.
    ///
    /// The caller is expected to be on the base branch (typically
    /// `main`) with a clean index; a dirty index fails the first
    /// merge with a `failed` entry naming the branch.
    pub fn merge_all(&mut self) -> Result<MergeReport> {
        // H-D3: verify the repo is still on the base commit with a
        // clean index before merging anything. A swarm runs for
        // minutes; the user may have switched branches, stashed,
        // committed, or the runner itself may have touched
        // `.gitignore`. The pre-fix shape ran `git merge` in whatever
        // checkout happened to be current and reported the result as
        // if it had merged onto the expected parent.
        let mut report = MergeReport::default();
        // H-D3: base must still be an *ancestor* of HEAD. The
        // pre-fix check demanded HEAD == base exactly, which refused
        // a legitimate case (a user commits a quick fix on `main`
        // mid-swarm — the branches still share base as their
        // ancestor, so `git merge --no-ff` is a valid operation).
        let ancestor = run_git_owned(
            &self.repo,
            &["merge-base", "--is-ancestor", &self.base_commit, "HEAD"],
            self.git_timeout_secs,
        );
        // `--is-ancestor` exits non-zero when the relation does not
        // hold; `run_git_owned` returns Err on non-zero. Ok means
        // "still descended from base".
        if ancestor.is_err() {
            let head = run_git_owned(&self.repo, &["rev-parse", "HEAD"], self.git_timeout_secs)
                .unwrap_or_default();
            let msg = format!(
                "refusing to merge: repo HEAD ({}) is not descended from \
                 the swarm's base commit ({}). Run `git checkout {}` and \
                 retry, or drop the worktrees with `kod worktree gc`.",
                head.trim(),
                self.base_commit,
                self.base_commit,
            );
            for info in &self.created {
                report.failed.push((info.branch.clone(), msg.clone()));
            }
            return Ok(report);
        }
        // Clean index: `git status --porcelain` must be empty. A dirty
        // index turns `git merge` into a partial merge that leaves the
        // repo in an ambiguous state.
        // H-D3: dirty-index check, excluding `.gitignore`. The
        // manager appends `.kod/` to it in `create()`, so a bare
        // `git status --porcelain` right after `create()` always
        // showed a modified file and the check refused every merge.
        // A user's own edit to `.gitignore` is a small price to pay
        // for making the merge path actually usable.
        let status = run_git_owned(
            &self.repo,
            &["status", "--porcelain", "--", ":(exclude).gitignore"],
            self.git_timeout_secs,
        )
        .unwrap_or_default();
        if !status.trim().is_empty() {
            let msg = format!(
                "refusing to merge: working tree has uncommitted changes:\n{}",
                status.trim(),
            );
            for info in &self.created {
                report.failed.push((info.branch.clone(), msg.clone()));
            }
            return Ok(report);
        }
        for info in &self.created {
            let result = run_git_owned(
                &self.repo,
                &[
                    "merge",
                    "--no-ff",
                    "-m",
                    &format!("kod: merge {}", info.branch),
                    &info.branch,
                ],
                self.git_timeout_secs,
            );
            match result {
                Ok(_) => {
                    self.merged_ok.insert(info.branch.clone());
                    report.merged.push(info.branch.clone());
                }
                Err(e) => {
                    let msg = e.to_string();
                    // Order matters: `git merge --abort` clears the
                    // unmerged-paths state, so the conflicted file
                    // list must be captured BEFORE the abort. The
                    // previous shape aborted first and then read an
                    // empty list, hiding the conflict.
                    if msg.to_lowercase().contains("conflict") {
                        let conflicted = self.conflicted_files().unwrap_or_default();
                        // Abort the merge so the caller sees a clean
                        // index, not a half-merged repo.
                        let _ =
                            run_git_owned(&self.repo, &["merge", "--abort"], self.git_timeout_secs);
                        if conflicted.is_empty() {
                            // Git mentioned a conflict but did not
                            // list unmerged paths (a submodule edge
                            // case, or output we do not recognise).
                            // Report as a generic failure naming the
                            // branch.
                            report.failed.push((info.branch.clone(), msg));
                        } else {
                            report.conflicted.extend(conflicted);
                            // A conflict stops the batch; the caller
                            // needs to reconcile before continuing.
                            break;
                        }
                    } else {
                        // Non-conflict failure: nothing to abort.
                        let _ =
                            run_git_owned(&self.repo, &["merge", "--abort"], self.git_timeout_secs);
                        report.failed.push((info.branch.clone(), msg));
                    }
                }
            }
        }
        Ok(report)
    }

    /// The files listed as conflicted by git. Used after a failed
    /// merge to fill the report. Empty when there is no merge in
    /// progress.
    fn conflicted_files(&self) -> Result<Vec<PathBuf>> {
        let out = run_git_owned(
            &self.repo,
            &["diff", "--name-only", "--diff-filter=U"],
            self.git_timeout_secs,
        )?;
        Ok(out
            .lines()
            .map(|l| l.trim())
            .filter(|l| !l.is_empty())
            .map(PathBuf::from)
            .collect())
    }

    /// Remove every worktree and delete the branches. Best-effort —
    /// the caller keeps a `kod/agent-*` branch only if the removal
    /// fails, which is worth logging but not worth aborting shutdown.
    pub fn cleanup(&mut self) {
        for info in self.created.drain(..) {
            let _ = run_git_owned(
                &self.repo,
                &[
                    "worktree",
                    "remove",
                    "--force",
                    info.path.to_string_lossy().as_ref(),
                ],
                self.git_timeout_secs,
            );
            // H-D2: only delete the branch when its work either
            // merged cleanly or the caller has explicitly accepted
            // the loss. An unmerged branch holds the only copy of an
            // agent's commits; `git branch -D` on it destroys them.
            let safe_to_delete = self.force_cleanup || self.merged_ok.contains(&info.branch);
            if safe_to_delete {
                let _ = run_git_owned(
                    &self.repo,
                    &["branch", "-D", &info.branch],
                    self.git_timeout_secs,
                );
            } else {
                tracing::warn!(
                    branch = %info.branch,
                    "worktree cleanup: branch kept (unmerged; not deleted)"
                );
            }
        }
        // Prune any dangling worktree metadata (a worktree removed by
        // hand leaves an entry in .git/worktrees/ that would trip the
        // next add).
        let _ = run_git_owned(&self.repo, &["worktree", "prune"], self.git_timeout_secs);
    }

    /// Add `.kod/` to the repo's `.gitignore` if it is not there yet.
    /// Idempotent. A repo without write access is left unchanged; the
    /// worktrees still get created, they just show as untracked in the
    /// parent.
    fn ensure_gitignored(&self) -> Result<()> {
        let gitignore = self.repo.join(".gitignore");
        let marker = ".kod/";
        if gitignore.is_file()
            && let Ok(content) = std::fs::read_to_string(&gitignore)
            && content.lines().any(|l| l.trim() == marker)
        {
            return Ok(());
        }
        use std::io::Write;
        let mut f = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&gitignore)
            .map_err(KodError::Io)?;
        // Ensure a newline before appending so we do not merge with a
        // missing final newline.
        let prefix = if gitignore.metadata().map(|m| m.len() > 0).unwrap_or(false) {
            "\n"
        } else {
            ""
        };
        writeln!(f, "{prefix}{marker}").map_err(KodError::Io)?;
        Ok(())
    }
}

impl Drop for WorktreeManager {
    fn drop(&mut self) {
        // H-D2: worktrees are removed unconditionally (they are just
        // directories under .kod/worktrees/), but branches are only
        // deleted for work that merged cleanly. A `cleanup()` is still
        // called so the directory does not leak; the branch-keep logic
        // inside it is what protects unmerged work.
        if !self.created.is_empty() {
            self.cleanup();
        }
    }
}

/// Sanitize a caller-supplied slug into a kebab-case token. Empty
/// after sanitization means the input was all separators.
pub fn sanitize_slug(s: &str) -> String {
    s.to_lowercase()
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect::<String>()
        .split('-')
        .filter(|p| !p.is_empty())
        .collect::<Vec<_>>()
        .join("-")
        .chars()
        .take(32)
        .collect()
}

/// Run `git <args>` in `repo`. Returns trimmed stdout on success. Used
/// for read-only queries; `run_git_owned` is the owned-args variant
/// for cases where the args include a temp String.
fn run_git(repo: &Path, args: &[&str], timeout_secs: u64) -> Result<String> {
    run_git_inner(repo, args, timeout_secs)
}

/// Same as `run_git` but takes owned `Vec<&str>` (so a caller can
/// borrow temporary Strings).
fn run_git_owned(repo: &Path, args: &[&str], timeout_secs: u64) -> Result<String> {
    run_git_inner(repo, args, timeout_secs)
}

/// Timeout is accepted but not enforced today: the worktree commands
/// this module runs are sub-second even on a cold repo, and the
/// `tokio::process` + kill machinery needed to enforce a bound would
/// add an async dependency to a synchronous module. The parameter is
/// kept so a future enforcement pass can wire it without an API
/// change.
fn run_git_inner(repo: &Path, args: &[&str], timeout_secs: u64) -> Result<String> {
    // S7: enforce the timeout the module has documented for years but
    // never applied. `std::process::Command::output` blocks forever on
    // a hung child (a stuck `git fetch`, a git waiting on a credential
    // helper, a lockfile contention pause). We spawn manually, poll
    // `try_wait` with a bounded sleep, and kill on deadline.
    //
    // The pipes are bounded by the OS and the worktree commands emit
    // tiny output (`git worktree add` prints at most a few lines), so
    // polling without a concurrent reader is safe: the child cannot
    // fill its pipe buffer before exiting.
    let mut cmd = std::process::Command::new("git");
    cmd.args(args)
        .current_dir(repo)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .env("GIT_PAGER", "cat")
        .env("PAGER", "cat")
        .env("GIT_EDITOR", "true")
        .env("GIT_MERGE_AUTOEDIT", "no");

    let mut child = cmd.spawn().map_err(KodError::Io)?;
    let effective = timeout_secs.max(1);
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(effective);
    let status = loop {
        match child.try_wait().map_err(KodError::Io)? {
            Some(s) => break s,
            None => {
                if std::time::Instant::now() >= deadline {
                    let _ = child.kill();
                    let _ = child.wait();
                    return Err(KodError::Internal(format!(
                        "git {}: did not finish within {}s (killed)",
                        args.join(" "),
                        effective,
                    )));
                }
                std::thread::sleep(std::time::Duration::from_millis(50));
            }
        }
    };

    // Drain both pipes (they are already closed because the child has
    // exited; reads return immediately).
    use std::io::Read;
    let mut stdout = Vec::new();
    let mut stderr = Vec::new();
    if let Some(mut p) = child.stdout.take() {
        let _ = p.read_to_end(&mut stdout);
    }
    if let Some(mut p) = child.stderr.take() {
        let _ = p.read_to_end(&mut stderr);
    }

    if !status.success() {
        let err = String::from_utf8_lossy(&stderr);
        let out = String::from_utf8_lossy(&stdout);
        let detail = if !err.trim().is_empty() {
            err.trim().to_string()
        } else if !out.trim().is_empty() {
            out.trim().to_string()
        } else {
            format!("exit status {:?}", status.code())
        };
        return Err(KodError::Internal(format!(
            "git {}: {}",
            args.join(" "),
            detail
        )));
    }
    Ok(String::from_utf8_lossy(&stdout)
        .trim_end_matches('\n')
        .to_string())
}

/// Approximate directory size in MB. Walks shallowly (max depth 3)
/// and caps the entry count so a huge tree does not stall the check.
fn directory_size_mb(path: &Path) -> Option<u64> {
    let mut total: u64 = 0;
    let mut count = 0usize;
    let mut stack = vec![(path.to_path_buf(), 0usize)];
    while let Some((dir, depth)) = stack.pop() {
        if depth > 3 || count > 50_000 {
            break;
        }
        let entries = match std::fs::read_dir(&dir) {
            Ok(rd) => rd,
            Err(_) => continue,
        };
        for e in entries.flatten() {
            count += 1;
            let p = e.path();
            if let Ok(meta) = e.metadata() {
                if meta.is_file() {
                    total = total.saturating_add(meta.len());
                } else if meta.is_dir() {
                    stack.push((p, depth + 1));
                }
            }
        }
    }
    Some(total / (1024 * 1024))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn binary_on_path(name: &str) -> bool {
        std::env::var_os("PATH")
            .map(|p| std::env::split_paths(&p).any(|d| d.join(name).is_file()))
            .unwrap_or(false)
    }

    fn init_repo() -> Option<tempfile::TempDir> {
        if !binary_on_path("git") {
            return None;
        }
        let tmp = tempfile::TempDir::new().ok()?;
        for args in [
            vec!["init", "-q", "-b", "main"],
            vec!["config", "user.email", "t@example.com"],
            vec!["config", "user.name", "T"],
        ] {
            let _ = std::process::Command::new("git")
                .args(&args)
                .current_dir(tmp.path())
                .status();
        }
        std::fs::write(tmp.path().join("seed.txt"), "seed\n").ok()?;
        let _ = std::process::Command::new("git")
            .args(["add", "seed.txt"])
            .current_dir(tmp.path())
            .status();
        let _ = std::process::Command::new("git")
            .args(["commit", "-q", "-m", "seed"])
            .current_dir(tmp.path())
            .status();
        Some(tmp)
    }

    #[test]
    fn sanitize_slug_makes_kebab() {
        assert_eq!(sanitize_slug("Design Schema!"), "design-schema");
        assert_eq!(sanitize_slug("a  b   c"), "a-b-c");
        let long = "x".repeat(100);
        assert!(sanitize_slug(&long).len() <= 32);
    }

    #[test]
    fn detect_on_non_repo_is_none() {
        let tmp = tempfile::TempDir::new().unwrap();
        assert!(WorktreeManager::detect(tmp.path()).unwrap().is_none());
    }

    #[test]
    fn detect_on_repo_is_some() {
        let Some(tmp) = init_repo() else { return };
        let mgr = WorktreeManager::detect(tmp.path()).unwrap();
        assert!(mgr.is_some(), "fresh git repo should be detected");
        assert!(!mgr.unwrap().base_commit().is_empty());
    }

    #[test]
    fn create_adds_a_worktree_and_branch() {
        let Some(tmp) = init_repo() else { return };
        let mut mgr = WorktreeManager::detect(tmp.path()).unwrap().unwrap();
        let info = mgr.create("agent-1").unwrap();
        assert!(info.path.is_dir(), "worktree path should exist");
        assert!(
            info.path.join("seed.txt").is_file(),
            "seed should be checked out"
        );
        assert_eq!(info.branch, "kod/agent-agent-1");
    }

    #[test]
    fn create_rejects_duplicate_slug() {
        let Some(tmp) = init_repo() else { return };
        let mut mgr = WorktreeManager::detect(tmp.path()).unwrap().unwrap();
        mgr.create("agent-1").unwrap();
        assert!(mgr.create("agent-1").is_err());
    }

    #[test]
    fn cleanup_removes_worktrees_and_keeps_unmerged_branches() {
        // H-D2: the pre-fix behaviour deleted every branch on
        // cleanup, destroying any commits an agent had made that had
        // not yet merged. The new behaviour removes the worktree
        // directory (no leak) but keeps the branch when it is not in
        // `merged_ok` — the caller decides whether to `git branch -D`
        // or reconcile.
        let Some(tmp) = init_repo() else { return };
        let mut mgr = WorktreeManager::detect(tmp.path()).unwrap().unwrap();
        let info = mgr.create("agent-1").unwrap();
        let path = info.path.clone();
        let branch = info.branch.clone();
        mgr.cleanup();
        assert!(!path.exists(), "worktree path should be gone");
        // The branch survives because it never merged.
        let out = std::process::Command::new("git")
            .args(["branch", "--list", &branch])
            .current_dir(tmp.path())
            .output()
            .unwrap();
        assert!(
            !String::from_utf8_lossy(&out.stdout).trim().is_empty(),
            "unmerged branch must survive cleanup",
        );
    }

    #[test]
    fn merge_all_merges_clean_worktree() {
        let Some(tmp) = init_repo() else { return };
        let mut mgr = WorktreeManager::detect(tmp.path()).unwrap().unwrap();
        let info = mgr.create("agent-1").unwrap();
        // Make a change in the worktree and commit it.
        std::fs::write(info.path.join("new.txt"), "hi\n").unwrap();
        for args in [
            vec!["add", "new.txt"],
            vec!["commit", "-q", "-m", "agent work"],
        ] {
            let _ = std::process::Command::new("git")
                .args(&args)
                .current_dir(&info.path)
                .status();
        }
        let report = mgr.merge_all().unwrap();
        assert_eq!(report.merged.len(), 1);
        assert!(report.conflicted.is_empty());
        assert!(tmp.path().join("new.txt").is_file());
    }

    #[test]
    fn merge_all_reports_conflict_and_aborts() {
        let Some(tmp) = init_repo() else { return };
        let mut mgr = WorktreeManager::detect(tmp.path()).unwrap().unwrap();
        let info = mgr.create("agent-1").unwrap();
        // Both branches modify seed.txt differently.
        std::fs::write(info.path.join("seed.txt"), "worktree\n").unwrap();
        for args in [
            vec!["add", "seed.txt"],
            vec!["commit", "-q", "-m", "agent change"],
        ] {
            let _ = std::process::Command::new("git")
                .args(&args)
                .current_dir(&info.path)
                .status();
        }
        std::fs::write(tmp.path().join("seed.txt"), "parent\n").unwrap();
        for args in [
            vec!["add", "seed.txt"],
            vec!["commit", "-q", "-m", "parent change"],
        ] {
            let _ = std::process::Command::new("git")
                .args(&args)
                .current_dir(tmp.path())
                .status();
        }
        let report = mgr.merge_all().unwrap();
        assert!(!report.conflicted.is_empty(), "conflict should be reported");
        assert!(report.conflicted.iter().any(|p| p.ends_with("seed.txt")));
        // Merge was aborted: git status should not show "MERGING".
        let out = std::process::Command::new("git")
            .args(["status", "--porcelain"])
            .current_dir(tmp.path())
            .output()
            .unwrap();
        assert!(!String::from_utf8_lossy(&out.stdout).contains("UU"));
    }

    #[test]
    fn drop_cleans_up_leftover_worktrees() {
        let Some(tmp) = init_repo() else { return };
        let path_to_check;
        {
            let mut mgr = WorktreeManager::detect(tmp.path()).unwrap().unwrap();
            let info = mgr.create("agent-1").unwrap();
            path_to_check = info.path.clone();
            // mgr dropped here without explicit cleanup.
        }
        assert!(!path_to_check.exists(), "Drop should have cleaned up");
    }
}
