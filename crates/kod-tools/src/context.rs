//! Tool execution context and permissions.

use crate::path_lock::PathLockTable;
use kod_error::{KodError, Result};
use kod_types::ToolPermissions;
use std::path::{Path, PathBuf};

/// How the tool should invoke shell commands.
///
/// `Disabled` is the default: commands run in the process's normal
/// environment, no sandboxing. `Require` refuses to execute a command
/// at all if the platform primitive is not available, rather than
/// silently running unsandboxed — a caller that asked for a sandbox and
/// got a bare shell would have a worse problem than a failure.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SandboxMode {
    /// Run commands normally, no sandbox.
    #[default]
    Disabled,
    /// Run commands under the platform's sandbox primitive. Refuse to
    /// run if none is available. Linux: `bwrap` (bubblewrap). macOS:
    /// `sandbox-exec` with a workspace-confined profile.
    Require,
}

/// An OS-level sandbox invocation: the executable to spawn and the
/// arguments to prepend before the actual command.
#[derive(Debug, Clone)]
pub struct SandboxInvocation {
    /// Executable to spawn (`bwrap`, `sandbox-exec`, ...).
    pub program: String,
    /// Arguments to place before the command string. The last element
    /// is `--` on both platforms, separating sandbox args from the
    /// command the sandboxed process will run.
    pub args: Vec<String>,
}

/// Build the platform sandbox invocation for `mode`, rooted at `wd`.
///
/// Returns `Ok(None)` when mode is `Disabled`. Returns `Ok(Some(inv))`
/// when a working primitive is available. Returns `Err(...)` when mode
/// is `Require` and no primitive exists — the caller decides whether
/// to fail the command.
pub fn sandbox_invocation(
    mode: SandboxMode,
    wd: &Path,
) -> Result<Option<SandboxInvocation>> {
    if mode == SandboxMode::Disabled {
        return Ok(None);
    }
    let wd_str = wd.to_string_lossy().to_string();

    #[cfg(target_os = "linux")]
    {
        if which("bwrap") {
            let args = vec![
                "--ro-bind".into(), "/usr".into(), "/usr".into(),
                "--ro-bind".into(), "/lib".into(), "/lib".into(),
                "--ro-bind".into(), "/lib64".into(), "/lib64".into(),
                "--ro-bind".into(), "/bin".into(), "/bin".into(),
                "--ro-bind".into(), "/etc".into(), "/etc".into(),
                "--dev".into(), "/dev".into(),
                "--proc".into(), "/proc".into(),
                "--bind".into(), wd_str.clone(), wd_str.clone(),
                "--chdir".into(), wd_str,
                "--unshare-net".into(),
                "--die-with-parent".into(),
                "--".into(),
            ];
            return Ok(Some(SandboxInvocation {
                program: "bwrap".to_string(),
                args,
            }));
        }
        return Err(KodError::SandboxViolation(
            "sandbox=require but bubblewrap (bwrap) is not installed. \
             Install it (apt install bubblewrap, dnf install bubblewrap) \
             or run with sandbox=disabled."
                .to_string(),
        ));
    }

    #[cfg(target_os = "macos")]
    {
        if which("sandbox-exec") {
            // macOS sandbox profile: confine writes to the working
            // directory and standard temp locations, deny network,
            // inherit read access to the rest of the filesystem so
            // compilers and interpreters still work.
            //
            // Assembled via format! with the `{wd}` argument. Every
            // newline is a two-character `\n` escape in the source —
            // this string is the reason the previous attempt failed
            // when a shell heredoc collapsed the escapes.
            let profile = format!(
                "(version 1)\n\
                 (allow default)\n\
                 (deny network*)\n\
                 (deny file-write*)\n\
                 (allow file-write* (subpath \"{wd}\") (subpath \"/tmp\") (subpath \"/private/tmp\") (subpath \"/private/var/folders\"))\n",
                wd = wd_str,
            );
            return Ok(Some(SandboxInvocation {
                program: "sandbox-exec".to_string(),
                args: vec!["-p".into(), profile, "--".into()],
            }));
        }
        return Err(KodError::SandboxViolation(
            "sandbox=require but sandbox-exec is not available. It ships \
             with macOS by default; if it is missing, run with \
             sandbox=disabled."
                .to_string(),
        ));
    }

    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    {
        let _ = wd_str;
        Err(KodError::SandboxViolation(
            "sandbox=require is not supported on this platform. Run with \
             sandbox=disabled."
                .to_string(),
        ))
    }
}

/// `true` if `program` is on PATH.
fn which(program: &str) -> bool {
    let path = match std::env::var_os("PATH") {
        Some(p) => p,
        None => return false,
    };
    for dir in std::env::split_paths(&path) {
        let candidate = dir.join(program);
        if candidate.is_file() {
            return true;
        }
    }
    false
}

/// Context for tool execution
#[derive(Debug, Clone)]
pub struct ToolContext {
    /// Working directory for relative paths
    pub working_dir: PathBuf,

    /// Permissions for this execution
    pub permissions: ToolPermissions,

    /// Timeout for execution (in seconds)
    pub timeout_secs: u64,

    /// Per-path advisory locks, shared by every `ToolContext` derived
    /// from the same engine. A context that has no table (a bare
    /// `ToolContext::new` in a unit test, or any context built before
    /// this field existed) writes without coordination — which is the
    /// right default for a single-writer tool call and the wrong one
    /// for a swarm, so the engine installs a table explicitly.
    pub lock_table: Option<std::sync::Arc<PathLockTable>>,

    /// Identity this context writes under — used as the "holder" string
    /// on a path lock, so a blocked waiter's error message and any
    /// debug log can attribute the write.
    pub holder: String,

    /// How long a write waits for a contended path lock before failing.
    pub lock_timeout: std::time::Duration,

    /// Whether `execute_command` runs through the platform's sandbox
    /// primitive. See [`SandboxMode`]. Default `Disabled`.
    pub sandbox: SandboxMode,
}

impl ToolContext {
    /// Create a new context with default permissions
    pub fn new(working_dir: impl Into<PathBuf>) -> Self {
        Self {
            working_dir: working_dir.into(),
            permissions: ToolPermissions::default(),
            timeout_secs: 30,
            lock_table: None,
            holder: "session".to_string(),
            lock_timeout: std::time::Duration::from_secs(2),
            sandbox: SandboxMode::Disabled,
        }
    }

    /// Install a shared lock table and set the writer identity.
    pub fn with_locks(
        mut self,
        table: std::sync::Arc<PathLockTable>,
        holder: impl Into<String>,
    ) -> Self {
        self.lock_table = Some(table);
        self.holder = holder.into();
        self
    }

    /// Override the lock wait.
    pub fn with_lock_timeout(mut self, d: std::time::Duration) -> Self {
        self.lock_timeout = d;
        self
    }

    /// Enable or disable sandboxed shell execution.
    pub fn with_sandbox(mut self, mode: SandboxMode) -> Self {
        self.sandbox = mode;
        self
    }

    /// Set permissions
    pub fn with_permissions(mut self, permissions: ToolPermissions) -> Self {
        self.permissions = permissions;
        self
    }

    /// Set timeout
    pub fn with_timeout(mut self, timeout_secs: u64) -> Self {
        self.timeout_secs = timeout_secs;
        self
    }

    /// Resolve `path` relative to the working directory, canonicalize
    /// it, and refuse anything that escapes the working directory.
    pub fn resolve_path(&self, path: &str) -> Result<PathBuf> {
        let raw = Path::new(path);
        let joined = if raw.is_absolute() {
            raw.to_path_buf()
        } else {
            self.working_dir.join(raw)
        };

        let canonical = match std::fs::canonicalize(&joined) {
            Ok(c) => c,
            Err(_) => {
                let parent = joined.parent().ok_or_else(|| KodError::InvalidParameters {
                    reason: format!("Path has no parent: {}", joined.display()),
                })?;
                let name = joined.file_name().ok_or_else(|| KodError::InvalidParameters {
                    reason: format!("Path has no file name: {}", joined.display()),
                })?;
                let canon_parent = std::fs::canonicalize(parent).map_err(KodError::Io)?;
                canon_parent.join(name)
            }
        };

        let root = std::fs::canonicalize(&self.working_dir).map_err(KodError::Io)?;
        if !canonical.starts_with(&root) {
            return Err(KodError::PermissionDenied {
                action: "resolve path".to_string(),
                reason: format!(
                    "Path escapes the working directory: {} -> {}",
                    path,
                    canonical.display()
                ),
            });
        }

        // Final-component symlink check.
        if let Ok(meta) = std::fs::symlink_metadata(&canonical)
            && meta.file_type().is_symlink()
        {
            match std::fs::canonicalize(&canonical) {
                Ok(target) => {
                    if !target.starts_with(&root) {
                        return Err(KodError::PermissionDenied {
                            action: "resolve path".to_string(),
                            reason: format!(
                                "Path is a symlink whose target escapes the \
                                 working directory: {} -> {}",
                                path,
                                target.display()
                            ),
                        });
                    }
                }
                Err(_) => {
                    return Err(KodError::PermissionDenied {
                        action: "resolve path".to_string(),
                        reason: format!(
                            "Path is a dangling symlink: {}. Refusing to read \
                             or write through it.",
                            canonical.display()
                        ),
                    });
                }
            }
        }

        Ok(canonical)
    }

    /// Check if path is allowed by permissions
    pub fn is_path_allowed(&self, path: &Path) -> Result<bool> {
        for forbidden in &self.permissions.forbidden_paths {
            if Self::matches_pattern(path, forbidden) {
                return Ok(false);
            }
        }

        if self.permissions.allowed_paths.is_empty() {
            return Ok(true);
        }

        for allowed in &self.permissions.allowed_paths {
            if Self::matches_pattern(path, allowed) {
                return Ok(true);
            }
        }

        Ok(false)
    }

    /// Check if path can be read
    pub fn can_read(&self, path: &Path) -> Result<()> {
        if !self.permissions.read_files {
            return Err(KodError::PermissionDenied {
                action: "read".to_string(),
                reason: "File reading not permitted".to_string(),
            });
        }
        if !self.is_path_allowed(path)? {
            return Err(KodError::PermissionDenied {
                action: "read".to_string(),
                reason: format!("Path not allowed: {}", path.display()),
            });
        }
        Ok(())
    }

    /// Check if path can be written
    pub fn can_write(&self, path: &Path) -> Result<()> {
        if !self.permissions.write_files {
            return Err(KodError::PermissionDenied {
                action: "write".to_string(),
                reason: "File writing not permitted".to_string(),
            });
        }
        if !self.is_path_allowed(path)? {
            return Err(KodError::PermissionDenied {
                action: "write".to_string(),
                reason: format!("Path not allowed: {}", path.display()),
            });
        }
        Ok(())
    }

    /// Check if command can be executed
    pub fn can_execute_command(&self, command: &str) -> Result<()> {
        if !self.permissions.execute_commands {
            return Err(KodError::PermissionDenied {
                action: "execute".to_string(),
                reason: "Command execution not permitted".to_string(),
            });
        }

        let trimmed = command.trim_start();
        let dangerous_patterns: &[&str] = &[
            "rm -rf",
            "rm -fr",
            "rm -r -f",
            "sudo",
            "chmod 777",
            "mkfs",
            "> /dev/sda",
            "> /dev/disk",
            "format ",
            "del /f /q /s",
            "rd /s /q",
            "rmdir /s /q",
        ];
        for pattern in dangerous_patterns {
            if trimmed.starts_with(pattern) {
                return Err(KodError::PermissionDenied {
                    action: "execute".to_string(),
                    reason: format!("Dangerous command pattern detected: {}", pattern),
                });
            }
        }

        Ok(())
    }

    /// Check if network access is allowed
    pub fn can_access_network(&self, url: &str) -> Result<()> {
        if !self.permissions.network_access {
            return Err(KodError::PermissionDenied {
                action: "network".to_string(),
                reason: format!("Network access not permitted: {}", url),
            });
        }
        Ok(())
    }

    /// Check if git operations are allowed
    pub fn can_git_operation(&self) -> Result<()> {
        if !self.permissions.git_operations {
            return Err(KodError::PermissionDenied {
                action: "git".to_string(),
                reason: "Git operations not permitted".to_string(),
            });
        }
        Ok(())
    }

    /// Does `path` fall under `pattern`?
    fn matches_pattern(path: &Path, pattern: &str) -> bool {
        let has_wildcard =
            pattern.contains('*') || pattern.contains('?') || pattern.contains('[');
        let mut builder = globset::GlobSetBuilder::new();
        match globset::Glob::new(pattern) {
            Ok(g) => {
                builder.add(g);
            }
            Err(_) => return false,
        }
        if !has_wildcard
            && let Ok(g) = globset::Glob::new(&format!("{}/**", pattern))
        {
            builder.add(g);
        }
        match builder.build() {
            Ok(set) => set.is_match(path),
            Err(_) => false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_context_creation() {
        let context = ToolContext::new("/tmp");
        assert_eq!(context.working_dir, PathBuf::from("/tmp"));
        assert_eq!(context.timeout_secs, 30);
        assert_eq!(context.sandbox, SandboxMode::Disabled);
    }

    #[test]
    fn test_with_sandbox_builder() {
        let c = ToolContext::new("/tmp").with_sandbox(SandboxMode::Require);
        assert_eq!(c.sandbox, SandboxMode::Require);
    }

    #[test]
    fn test_sandbox_disabled_returns_none() {
        let r = sandbox_invocation(SandboxMode::Disabled, Path::new("/tmp")).unwrap();
        assert!(r.is_none());
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn test_sandbox_require_linux() {
        // On a machine without bwrap this returns Err; with bwrap it
        // returns Some. Both are correct; a panic is not.
        match sandbox_invocation(SandboxMode::Require, Path::new("/tmp")) {
            Ok(Some(inv)) => {
                assert_eq!(inv.program, "bwrap");
                assert_eq!(inv.args.last().map(|s| s.as_str()), Some("--"));
            }
            Ok(None) => panic!("Require must not return None"),
            Err(e) => {
                assert!(e.to_string().contains("bubblewrap"), "got: {e}");
            }
        }
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn test_sandbox_require_macos() {
        let inv = sandbox_invocation(SandboxMode::Require, Path::new("/tmp"))
            .expect("sandbox-exec should be available on macOS")
            .expect("Require must return Some");
        assert_eq!(inv.program, "sandbox-exec");
        assert_eq!(inv.args.last().map(|s| s.as_str()), Some("--"));
        // The profile must contain the working dir literally, not the
        // placeholder.
        let profile = &inv.args[1];
        assert!(profile.contains("/tmp"), "profile missing wd: {profile}");
        assert!(!profile.contains("{wd}"), "profile has unexpanded {{wd}}: {profile}");
    }

    #[test]
    fn test_resolve_path() {
        let temp_dir = tempfile::TempDir::new().unwrap();
        let root = temp_dir.path().canonicalize().unwrap();
        std::fs::create_dir_all(root.join("subdir")).unwrap();
        std::fs::write(root.join("file.txt"), "x").unwrap();

        let context = ToolContext::new(&root);

        let resolved = context.resolve_path("file.txt").unwrap();
        assert!(resolved.starts_with(&root));

        let resolved = context.resolve_path("subdir").unwrap();
        assert!(resolved.starts_with(&root));

        let resolved = context.resolve_path("new_file.txt").unwrap();
        assert!(resolved.starts_with(&root));
    }

    #[test]
    fn test_can_read_default() {
        let context = ToolContext::new("/tmp");
        let result = context.can_read(Path::new("/tmp/test.txt"));
        assert!(result.is_err());
    }

    #[test]
    fn test_can_read_with_permissions() {
        let perms = ToolPermissions {
            read_files: true,
            ..Default::default()
        };
        let context = ToolContext::new("/tmp").with_permissions(perms);
        assert!(context.can_read(Path::new("/tmp/test.txt")).is_ok());
    }

    #[test]
    fn test_can_write_with_permissions() {
        let perms = ToolPermissions {
            write_files: true,
            ..Default::default()
        };
        let context = ToolContext::new("/tmp").with_permissions(perms);
        assert!(context.can_write(Path::new("/tmp/test.txt")).is_ok());
    }

    #[test]
    fn test_can_execute_command_rejects_leading_whitespace() {
        let perms = ToolPermissions {
            execute_commands: true,
            ..Default::default()
        };
        let ctx = ToolContext::new("/tmp").with_permissions(perms);

        assert!(ctx.can_execute_command("rm -rf /").is_err());
        assert!(ctx.can_execute_command("  rm -rf /").is_err());
        assert!(ctx.can_execute_command("\trm -rf /").is_err());
        assert!(ctx.can_execute_command("\nrm -rf /").is_err());
        assert!(ctx.can_execute_command("ls -la").is_ok());
    }

    #[test]
    fn test_forbidden_path() {
        let perms = ToolPermissions {
            read_files: true,
            allowed_paths: vec!["/tmp/allowed".to_string()],
            forbidden_paths: vec!["/tmp/allowed/forbidden".to_string()],
            ..Default::default()
        };
        let context = ToolContext::new("/tmp").with_permissions(perms);

        assert!(context.can_read(Path::new("/tmp/allowed/test.txt")).is_ok());
        assert!(context.can_read(Path::new("/tmp/allowed")).is_ok());
        assert!(context.can_read(Path::new("/tmp/allowed/forbidden/secret.txt")).is_err());
        assert!(context.can_read(Path::new("/tmp/allowed/forbidden")).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn test_resolve_path_rejects_dangling_symlink() {
        let temp = tempfile::TempDir::new().unwrap();
        let root = temp.path().canonicalize().unwrap();
        let link = root.join("dangling");
        std::os::unix::fs::symlink("/nonexistent-kod-test-target", &link).unwrap();

        let ctx = ToolContext::new(&root);
        let result = ctx.resolve_path("dangling");
        assert!(result.is_err(), "dangling symlink must be rejected");
    }

    #[cfg(unix)]
    #[test]
    fn test_resolve_path_accepts_symlink_to_inside_file() {
        let temp = tempfile::TempDir::new().unwrap();
        let root = temp.path().canonicalize().unwrap();
        std::fs::write(root.join("real.txt"), "hi").unwrap();
        let link = root.join("link.txt");
        std::os::unix::fs::symlink(root.join("real.txt"), &link).unwrap();

        let ctx = ToolContext::new(&root);
        let result = ctx.resolve_path("link.txt");
        assert!(result.is_ok());
        assert_eq!(result.unwrap(), root.join("real.txt"));
    }

    #[test]
    fn test_resolve_path_rejects_traversal() {
        let temp_dir = tempfile::TempDir::new().unwrap();
        let root = temp_dir.path().canonicalize().unwrap();
        std::fs::write(root.join("inside.txt"), "ok").unwrap();

        let context = ToolContext::new(&root);

        let escape = format!("{}/../outside.txt", root.display());
        assert!(context.resolve_path(&escape).is_err());
        assert!(context.resolve_path("/etc/passwd").is_err());

        let ok = context.resolve_path("inside.txt").unwrap();
        assert!(ok.starts_with(&root));
    }
}
