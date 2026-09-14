//! Tool execution context and permissions.

use kod_error::{KodError, Result};
use kod_types::ToolPermissions;
use std::path::{Path, PathBuf};

/// Context for tool execution
#[derive(Debug, Clone)]
pub struct ToolContext {
    /// Working directory for relative paths
    pub working_dir: PathBuf,

    /// Permissions for this execution
    pub permissions: ToolPermissions,

    /// Timeout for execution (in seconds)
    pub timeout_secs: u64,
}

impl ToolContext {
    /// Create a new context with default permissions
    pub fn new(working_dir: impl Into<PathBuf>) -> Self {
        Self {
            working_dir: working_dir.into(),
            permissions: ToolPermissions::default(),
            timeout_secs: 30,
        }
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

    /// Resolve `path` relative to the working directory, canonicalize it,
    /// and refuse anything that escapes the working directory.
    ///
    /// This is the single choke point where traversal is blocked. Even if
    /// `allowed_paths` is empty (which `is_path_allowed` treats as "allow
    /// everything"), a request like `read_file { "path": "../../etc/passwd" }`
    /// is rejected here because the canonical form lands outside
    /// `working_dir`.
    ///
    /// Files that do not exist yet (e.g. the target of a `write_file`
    /// creating a new file) are resolved by canonicalizing the deepest
    /// existing ancestor and re-appending the rest, so creation still
    /// works while traversal stays blocked.
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

        Ok(canonical)
    }

    /// Check if path is allowed by permissions
    pub fn is_path_allowed(&self, path: &Path) -> Result<bool> {
        // Check forbidden paths first
        for forbidden in &self.permissions.forbidden_paths {
            if Self::matches_pattern(path, forbidden) {
                return Ok(false);
            }
        }

        // If allowed_paths is empty, allow all (except forbidden)
        if self.permissions.allowed_paths.is_empty() {
            return Ok(true);
        }

        // Check allowed paths
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

        // Refuse a small set of unambiguously destructive commands.
        // These run through `sh -c` / `cmd /C`, so both shells' worst
        // offenders are listed. The check is a guardrail, not a sandbox:
        // `true; rm -rf /` slips past `starts_with`, and that is
        // acceptable — the real defense is that the whole tool is
        // behind ToolPermissions::execute_commands and the default is
        // off. This just stops the accidental "delete everything"
        // command from a model that read the wrong directory.
        let dangerous_patterns: &[&str] = &[
            // POSIX
            "rm -rf",
            "sudo",
            "chmod 777",
            "mkfs",
            "> /dev/sda",
            "> /dev/disk",
            // cmd.exe
            "format ",
            "del /f /q /s",
            "rd /s /q",
            "rmdir /s /q",
        ];
        for pattern in dangerous_patterns {
            if command.starts_with(pattern) {
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

    /// Match a path against a glob pattern
    fn matches_pattern(path: &Path, pattern: &str) -> bool {
        let glob = format!("{}/**", pattern);
        match globset::Glob::new(&glob) {
            Ok(glob) => glob.compile_matcher().is_match(path),
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
    }

    #[test]
    fn test_resolve_path() {
        let temp_dir = tempfile::TempDir::new().unwrap();
        let root = temp_dir.path().canonicalize().unwrap();
        std::fs::create_dir_all(root.join("subdir")).unwrap();
        std::fs::write(root.join("file.txt"), "x").unwrap();

        let context = ToolContext::new(&root);

        // Existing relative file resolves to a canonical path inside root.
        let resolved = context.resolve_path("file.txt").unwrap();
        assert!(resolved.starts_with(&root), "got {}", resolved.display());
        assert!(resolved.ends_with("file.txt"));

        // Existing subdir path also resolves.
        let resolved = context.resolve_path("subdir").unwrap();
        assert!(resolved.starts_with(&root), "got {}", resolved.display());
        assert!(resolved.ends_with("subdir"));

        // Non-existent file inside: allowed (write_file create path).
        let resolved = context.resolve_path("new_file.txt").unwrap();
        assert!(resolved.starts_with(&root), "got {}", resolved.display());
        assert!(resolved.ends_with("new_file.txt"));
    }

    #[test]
    fn test_can_read_default() {
        let context = ToolContext::new("/tmp");
        let result = context.can_read(Path::new("/tmp/test.txt"));
        // Default permissions don't allow reading
        assert!(result.is_err());
    }

    #[test]
    fn test_can_read_with_permissions() {
        let perms = ToolPermissions {
            read_files: true,
            ..Default::default()
        };
        let context = ToolContext::new("/tmp").with_permissions(perms);

        let result = context.can_read(Path::new("/tmp/test.txt"));
        assert!(result.is_ok());
    }

    #[test]
    fn test_can_write_with_permissions() {
        let perms = ToolPermissions {
            write_files: true,
            ..Default::default()
        };
        let context = ToolContext::new("/tmp").with_permissions(perms);

        let result = context.can_write(Path::new("/tmp/test.txt"));
        assert!(result.is_ok());
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

        // Allowed path
        let result = context.can_read(Path::new("/tmp/allowed/test.txt"));
        assert!(result.is_ok());

        // Forbidden path
        let result = context.can_read(Path::new("/tmp/allowed/forbidden/secret.txt"));
        assert!(result.is_err());
    }

    #[test]
    fn test_resolve_path_rejects_traversal() {
        let temp_dir = tempfile::TempDir::new().unwrap();
        let root = temp_dir.path().canonicalize().unwrap();
        std::fs::write(root.join("inside.txt"), "ok").unwrap();

        let context = ToolContext::new(&root);

        // A `..` climb must not escape the working directory.
        let escape = format!("{}/../outside.txt", root.display());
        let result = context.resolve_path(&escape);
        assert!(
            result.is_err(),
            "expected traversal rejection, got {:?}",
            result
        );

        // An absolute path outside the working directory must be rejected.
        let result = context.resolve_path("/etc/passwd");
        assert!(result.is_err(), "expected /etc/passwd rejection, got {:?}", result);

        // Legitimate inside path still works.
        let ok = context.resolve_path("inside.txt").unwrap();
        assert!(ok.starts_with(&root));
    }
}
