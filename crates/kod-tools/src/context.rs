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

    /// Resolve a path relative to working directory
    pub fn resolve_path(&self, path: &str) -> PathBuf {
        let path = Path::new(path);

        if path.is_absolute() {
            path.to_path_buf()
        } else {
            self.working_dir.join(path)
        }
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

        // Check for dangerous commands
        let dangerous_patterns = ["rm -rf", "sudo", "chmod 777", "> /dev/sda"];
        for pattern in &dangerous_patterns {
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
        let context = ToolContext::new("/tmp");

        let resolved = context.resolve_path("/abs/path");
        assert_eq!(resolved, PathBuf::from("/abs/path"));

        let resolved = context.resolve_path("relative/path");
        assert_eq!(resolved, PathBuf::from("/tmp/relative/path"));
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
}
