//! Serialize commits across agents sharing one checkout.
//!
//! When two swarm agents share a working tree (the `Shared` isolation
//! policy) and both run `git commit`, they race on `.git/index.lock`.
//! The second gets a failure that has nothing to do with its work,
//! and the first's commit may contain the second's staged changes —
//! `git add -A` stages the whole tree, not the caller's paths.
//!
//! Two things fix that, and this module provides both:
//!
//! 1. A **repo-keyed lock** so only one commit runs at a time. The key
//!    is the git *common* directory, so worktrees of one repo share a
//!    lock (they share `.git`) and unrelated repos do not.
//! 2. A **pathspec-scoped commit** — `git commit --only -- <paths>`
//!    commits exactly the named paths regardless of what else is
//!    staged, so agent A cannot swallow agent B's in-flight work.
//!
//! No agent runs `git commit` today, so nothing takes this lock yet.
//! The module is here because the hazard is real the moment one does,
//! and the fix is small enough that discovering it under a race would
//! be the expensive path.

use std::path::{Path, PathBuf};
use std::sync::Arc;

/// A per-repo commit lock.
pub struct CommitLock {
    /// The git common directory this lock is keyed to.
    key: PathBuf,
    inner: Arc<tokio::sync::Mutex<()>>,
}

impl CommitLock {
    /// A lock for the repo containing `path`.
    ///
    /// `None` when `path` is not in a git repo — there is nothing to
    /// serialize, and refusing is more honest than locking a
    /// filesystem path that means nothing.
    pub fn for_path(path: &Path) -> Option<Self> {
        Some(Self {
            key: git_common_dir(path)?,
            inner: Arc::new(tokio::sync::Mutex::new(())),
        })
    }

    /// The directory this lock is keyed to, for a log line.
    pub fn key(&self) -> &Path {
        &self.key
    }

    /// Run `f` while holding the lock.
    pub async fn with_lock<F, Fut, T>(&self, f: F) -> T
    where
        F: FnOnce() -> Fut,
        Fut: std::future::Future<Output = T>,
    {
        let _guard = self.inner.lock().await;
        f().await
    }

    /// Whether two locks would serialize against each other.
    pub fn shares_repo_with(&self, other: &Self) -> bool {
        self.key == other.key
    }
}

/// The git common directory for `path`.
///
/// Resolved by walking up for a `.git` entry. A `.git` *directory* is
/// the common dir. A `.git` *file* (a worktree) contains
/// `gitdir: <path>`, and the common dir is that path's `.git` parent
/// — so all worktrees of one repo resolve to the same key.
pub fn git_common_dir(path: &Path) -> Option<PathBuf> {
    let mut cur = path.to_path_buf();
    loop {
        let candidate = cur.join(".git");
        if candidate.is_dir() {
            return Some(candidate);
        }
        if candidate.is_file() {
            let text = std::fs::read_to_string(&candidate).ok()?;
            let dir = text.trim().strip_prefix("gitdir:")?.trim();
            let resolved = if Path::new(dir).is_absolute() {
                PathBuf::from(dir)
            } else {
                cur.join(dir)
            };
            // `<common>/.git/worktrees/<name>` → `<common>/.git`
            let mut p = resolved;
            while p.file_name().is_some_and(|n| n != ".git") {
                if !p.pop() {
                    return None;
                }
            }
            return p.file_name().is_some().then_some(p);
        }
        if !cur.pop() {
            return None;
        }
    }
}

/// The `git commit --only` invocation for a set of paths.
///
/// `--only` commits exactly the named paths, leaving everything else
/// staged or dirty. This is the half of the fix that keeps agent A's
/// commit from containing agent B's changes.
pub fn commit_args(message: &str, paths: &[String]) -> Vec<String> {
    if paths.is_empty() {
        // No paths: a plain commit. The lock still serializes it.
        return vec!["commit".into(), "-m".into(), message.to_string()];
    }
    let mut args: Vec<String> = vec![
        "commit".into(),
        "--only".into(),
        "-m".into(),
        message.to_string(),
        "--".into(),
    ];
    for p in paths {
        args.push(p.clone());
    }
    args
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_plain_directory_is_not_a_repo() {
        let tmp = tempfile::TempDir::new().unwrap();
        assert!(git_common_dir(tmp.path()).is_none());
    }

    #[test]
    fn a_dot_git_directory_is_the_key() {
        let tmp = tempfile::TempDir::new().unwrap();
        std::fs::create_dir_all(tmp.path().join(".git")).unwrap();
        let key = git_common_dir(tmp.path()).expect("a .git dir is a repo");
        assert!(key.ends_with(".git"));
    }

    #[test]
    fn a_worktree_git_file_resolves_to_the_common_dir() {
        // Mimic a worktree: `.git` is a file pointing at
        // `<main>/.git/worktrees/wt`.
        let main = tempfile::TempDir::new().unwrap();
        let common = main.path().join(".git");
        std::fs::create_dir_all(common.join("worktrees").join("wt")).unwrap();

        let wt = tempfile::TempDir::new().unwrap();
        std::fs::write(
            wt.path().join(".git"),
            format!("gitdir: {}", common.join("worktrees").join("wt").display()),
        )
        .unwrap();

        let key = git_common_dir(wt.path()).expect("a worktree resolves");
        assert_eq!(key, common, "the worktree and main share one key");
    }

    #[test]
    fn nested_paths_resolve_upward() {
        let tmp = tempfile::TempDir::new().unwrap();
        std::fs::create_dir_all(tmp.path().join(".git")).unwrap();
        let deep = tmp.path().join("a").join("b");
        std::fs::create_dir_all(&deep).unwrap();
        assert!(git_common_dir(&deep).is_some(), "walk up to find .git");
    }

    #[test]
    fn two_locks_in_one_repo_share_a_key() {
        let tmp = tempfile::TempDir::new().unwrap();
        std::fs::create_dir_all(tmp.path().join(".git")).unwrap();
        let a = CommitLock::for_path(tmp.path()).unwrap();
        let b = CommitLock::for_path(tmp.path()).unwrap();
        assert!(a.shares_repo_with(&b));
    }

    #[test]
    fn locks_in_different_repos_do_not_share() {
        let a_dir = tempfile::TempDir::new().unwrap();
        let b_dir = tempfile::TempDir::new().unwrap();
        std::fs::create_dir_all(a_dir.path().join(".git")).unwrap();
        std::fs::create_dir_all(b_dir.path().join(".git")).unwrap();
        let a = CommitLock::for_path(a_dir.path()).unwrap();
        let b = CommitLock::for_path(b_dir.path()).unwrap();
        assert!(!a.shares_repo_with(&b));
    }

    #[test]
    fn a_scoped_commit_lists_the_paths_after_double_dash() {
        let args = commit_args("msg", &["src/a.rs".into(), "src/b.rs".into()]);
        let dash = args.iter().position(|a| a == "--").expect("-- present");
        assert_eq!(&args[dash + 1..], &["src/a.rs", "src/b.rs"]);
        assert!(args.contains(&"--only".to_string()));
    }

    #[test]
    fn an_empty_path_list_is_a_plain_commit() {
        let args = commit_args("msg", &[]);
        assert!(!args.contains(&"--only".to_string()));
        assert!(args.contains(&"msg".to_string()));
    }
}
