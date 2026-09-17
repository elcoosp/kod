//! Tool execution context and permissions.

use crate::path_lock::PathLockTable;
use kod_error::{KodError, Result};
use kod_types::ToolPermissions;
use std::path::{Path, PathBuf};

/// How the tool should invoke shell commands.
///
/// - `Disabled`: commands run in the process's normal environment.
///   The pre-D3 behaviour.
/// - `Auto`: use the best available platform primitive (`bwrap` on
///   Linux, `sandbox-exec` on macOS), silently skipping sandboxing
///   when none is installed. The default — the caller does not have to
///   know whether a sandbox is available on the host.
/// - `Require`: use a primitive or refuse to run. Fails loudly when
///   nothing is available.
///
/// The engine surfaces the effective backend through
/// [`SandboxResolver::backend_name`] so a caller can show
/// `sandbox: bwrap | sandbox-exec | off` in the UI. The
/// `Disabled` value is retained because the `--preset yolo` path and
/// several existing tests opt out explicitly.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SandboxMode {
    /// Run commands normally, no sandbox.
    Disabled,
    /// Use a sandbox if available, otherwise fall through to no
    /// sandbox. The default.
    #[default]
    Auto,
    /// Require a sandbox primitive. Fail loudly if unavailable.
    Require,
}

/// Options that shape the sandbox invocation.
///
/// Defaults are the safe choice: `.git` read-only, network denied,
/// `$TMPDIR` writable. A caller that wants a looser sandbox for one
/// command overrides the specific flag.
#[derive(Debug, Clone, Copy)]
pub struct SandboxOpts {
    /// Mount `.git` read-only so `git status`/`git diff` work but
    /// `git checkout .` cannot destroy history (D3-C4).
    pub git_readonly: bool,
    /// Deny network in the sandbox. Requires bwrap
    /// (`--unshare-net`); Seatbelt's profile gets `(deny network*)`.
    pub net_deny: bool,
    /// Allow writes to the OS temp dir. On by default — a build tool
    /// that stages through `/tmp` would otherwise break.
    pub tmp_rw: bool,
}

impl Default for SandboxOpts {
    fn default() -> Self {
        Self {
            git_readonly: true,
            net_deny: true,
            tmp_rw: true,
        }
    }
}

/// Which platform primitive a `SandboxInvocation` used. Named so the
/// engine's UI (and `kod doctor`) can surface it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Backend {
    Bwrap,
    Landlock,
    Seatbelt,
}

impl Backend {
    pub fn name(self) -> &'static str {
        match self {
            Backend::Bwrap => "bwrap",
            Backend::Landlock => "landlock",
            Backend::Seatbelt => "sandbox-exec",
        }
    }
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
    /// Which backend produced this invocation. Surfaced by the
    /// engine UI and `kod doctor`.
    pub backend: Backend,
}

/// A resolver that knows which sandbox primitives are available on
/// this host. Built once at startup; cheap to clone.
///
/// `detect` probes `which(bwrap)` / `which(sandbox-exec)` once. The
/// engine, `kod doctor`, and the sandbox invocation itself all read
/// the result — a single source of truth about "is a sandbox
/// available here?" avoids the doctor saying one thing and the tool
/// loop doing another.
#[derive(Debug, Clone)]
pub struct SandboxResolver {
    available: Vec<Backend>,
}

impl SandboxResolver {
    /// Probe the host for available primitives. Cheap; safe to call
    /// from a startup path.
    pub fn detect() -> Self {
        let mut available = Vec::new();
        #[cfg(target_os = "linux")]
        {
            if which("bwrap") {
                available.push(Backend::Bwrap);
            } else if crate::sandbox::landlock::probe_abi().is_some() {
                // bwrap is preferred when present (it is a real
                // namespace isolation and gets network denial on
                // every supported kernel). Landlock is the fallback
                // for a machine without bubblewrap — a container, a
                // stripped distro, a locked-down CI image — and its
                // availability is checked by the same real syscall
                // `apply` will perform.
                available.push(Backend::Landlock);
            }
        }
        #[cfg(target_os = "macos")]
        {
            if which("sandbox-exec") {
                available.push(Backend::Seatbelt);
            }
        }
        Self { available }
    }

    /// A resolver with no backends (a stripped container, a test). The
    /// `invocation` method returns `None` for `Auto` and errors for
    /// `Require`, matching what a host without primitives would do.
    pub fn empty() -> Self {
        Self { available: Vec::new() }
    }

    pub fn has_any(&self) -> bool {
        !self.available.is_empty()
    }

    /// The backend that would be used for a call, if any. Useful for
    /// `sandbox: bwrap` badges.
    pub fn backend_name(&self) -> Option<&'static str> {
        self.available.first().map(|b| b.name())
    }

    /// The best available backend, or `None`.
    fn best(&self) -> Option<Backend> {
        self.available.first().copied()
    }

    /// Build an invocation for `mode`, rooted at `wd`, with `opts`.
    ///
    /// - `Disabled`: `Ok(None)`.
    /// - `Auto` + a backend: `Ok(Some(inv))`.
    /// - `Auto` + no backend: `Ok(None)` (silent passthrough; the
    ///   caller checks `has_any` for a UI warning).
    /// - `Require` + a backend: `Ok(Some(inv))`.
    /// - `Require` + no backend: `Err(SandboxViolation)` naming the
    ///   install command for the current platform.
    pub fn invocation(
        &self,
        mode: SandboxMode,
        wd: &Path,
        opts: SandboxOpts,
    ) -> Result<Option<SandboxInvocation>> {
        if mode == SandboxMode::Disabled {
            return Ok(None);
        }
        let Some(backend) = self.best() else {
            return match mode {
                SandboxMode::Require => Err(KodError::SandboxViolation(
                    missing_backend_message(),
                )),
                _ => Ok(None),
            };
        };
        match backend {
            Backend::Bwrap => Ok(Some(bwrap_invocation(wd, opts))),
            Backend::Seatbelt => Ok(Some(seatbelt_invocation(wd, opts))),
            Backend::Landlock => landlock_invocation(wd, opts).map(Some),
        }
    }
}

/// Build the `kod __sandbox-exec <profile> -- …` invocation.
///
/// The profile is written to a temp file the launcher reads and
/// unlinks. The file name is derived from the parent's pid so two
/// concurrent `kod` processes cannot collide; the parent leaves it
/// for the launcher to remove.
#[cfg(target_os = "linux")]
fn landlock_invocation(wd: &Path, opts: SandboxOpts) -> Result<SandboxInvocation> {
    // Locate the `kod` binary. The invocation runs it back; the
    // resolver has no other way to reach the launcher.
    let kod = std::env::current_exe().map_err(|e| {
        KodError::SandboxViolation(format!(
            "could not determine the kod binary path for the landlock launcher: {e}"
        ))
    })?;

    // Build the profile.
    let mut profile = crate::sandbox::landlock::LandlockProfile {
        ro_paths: vec![
            PathBuf::from("/usr"),
            PathBuf::from("/lib"),
            PathBuf::from("/lib64"),
            PathBuf::from("/bin"),
            PathBuf::from("/etc"),
        ],
        rw_paths: vec![wd.to_path_buf()],
        net_deny: opts.net_deny,
    };
    if opts.git_readonly {
        let git = wd.join(".git");
        if git.is_dir() {
            profile.ro_paths.push(git);
        }
    }
    if opts.tmp_rw {
        if let Some(tmp) = std::env::var_os("TMPDIR") {
            profile.rw_paths.push(PathBuf::from(tmp));
        }
        profile.rw_paths.push(PathBuf::from("/tmp"));
    }

    // The launcher refuses net_deny on ABI < 4. Detect that here so
    // the caller (and thus the user) sees a clean fail-open with a
    // warning, instead of spawning a launcher that would just error.
    if opts.net_deny {
        let abi = crate::sandbox::landlock::probe_abi().unwrap_or(0);
        if abi < 4 {
            return Err(KodError::SandboxViolation(format!(
                "landlock cannot deny network on this kernel (ABI {abi}); \
                 install bubblewrap or accept network access"
            )));
        }
    }

    let profile_path = std::env::temp_dir().join(format!(
        "kod-sandbox-{}.json",
        std::process::id(),
    ));
    std::fs::write(&profile_path, profile.to_json()).map_err(|e| {
        KodError::SandboxViolation(format!(
            "could not write sandbox profile to {}: {e}",
            profile_path.display()
        ))
    })?;

    Ok(SandboxInvocation {
        program: kod.to_string_lossy().to_string(),
        // The kernel's landlock_restrict_self is per-process; the
        // launcher is `kod` re-entering itself with this hidden
        // subcommand. `--` separates the profile from the inner
        // command — the caller appends the shell invocation after.
        args: vec![
            "__sandbox-exec".to_string(),
            profile_path.to_string_lossy().to_string(),
            "--".to_string(),
        ],
        backend: Backend::Landlock,
    })
}

#[cfg(not(target_os = "linux"))]
fn landlock_invocation(_wd: &Path, _opts: SandboxOpts) -> Result<SandboxInvocation> {
    Err(KodError::SandboxViolation(
        "landlock is a Linux-only backend".to_string(),
    ))
}

/// The default resolver for this host. Cheap after the first call.
pub fn default_resolver() -> SandboxResolver {
    SandboxResolver::detect()
}

/// Message shown when a primitive is missing and the mode requires one.
/// Mentions the platform's package manager so a user has a next step.
fn missing_backend_message() -> String {
    #[cfg(target_os = "linux")]
    {
        "sandbox=require but bubblewrap (bwrap) is not installed. \
         Install it (apt install bubblewrap, dnf install bubblewrap, \
         pacman -S bubblewrap, apk add bubblewrap) or run with \
         sandbox=disabled."
            .to_string()
    }
    #[cfg(target_os = "macos")]
    {
        "sandbox=require but sandbox-exec is not available. It ships \
         with macOS by default; if it is missing, run with \
         sandbox=disabled."
            .to_string()
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    {
        "sandbox=require is not supported on this platform. Run with \
         sandbox=disabled."
            .to_string()
    }
}

#[cfg(target_os = "linux")]
fn bwrap_invocation(wd: &Path, opts: SandboxOpts) -> SandboxInvocation {
    let wd_str = wd.to_string_lossy().to_string();
    let mut args: Vec<String> = vec![
        "--ro-bind".into(), "/usr".into(), "/usr".into(),
        "--ro-bind".into(), "/lib".into(), "/lib".into(),
        "--ro-bind".into(), "/lib64".into(), "/lib64".into(),
        "--ro-bind".into(), "/bin".into(), "/bin".into(),
        "--ro-bind".into(), "/etc".into(), "/etc".into(),
        "--dev".into(), "/dev".into(),
        "--proc".into(), "/proc".into(),
    ];
    // .git read-only: mount it RO *after* the workspace bind so the
    // narrower rule wins. Order matters in bwrap — later binds override
    // earlier ones for the same mount point.
    if opts.git_readonly {
        let git = format!("{wd_str}/.git");
        // Only bind when the .git directory actually exists; a bwrap
        // invocation with a bind on a non-existent source fails hard.
        if std::path::Path::new(&git).is_dir() {
            args.extend([
                "--ro-bind".into(), git.clone(), git.clone(),
            ]);
        }
    }
    args.extend([
        "--bind".into(), wd_str.clone(), wd_str.clone(),
        "--chdir".into(), wd_str,
    ]);
    if opts.net_deny {
        args.push("--unshare-net".into());
    }
    args.push("--die-with-parent".into());
    args.push("--".into());

    SandboxInvocation {
        program: "bwrap".to_string(),
        args,
        backend: Backend::Bwrap,
    }
}

#[cfg(target_os = "macos")]
fn seatbelt_invocation(wd: &Path, opts: SandboxOpts) -> SandboxInvocation {
    let wd_str = wd.to_string_lossy().to_string();
    // macOS sandbox profile: read access to the filesystem at large,
    // writes only under the working dir (with .git excluded when
    // git_readonly), temp dirs, and the standard macOS caches. Network
    // denied when net_deny.
    //
    // Seatbelt rules are evaluated top-to-bottom and the *last*
    // matching rule wins, so `(deny file-write* ...)` on .git must
    // come after the general `(allow file-write* ...)`.
    let mut profile = String::from("(version 1)\n(allow default)\n");
    if opts.net_deny {
        profile.push_str("(deny network*)\n");
    }
    // Deny-all-writes, then allow-list. The order matters: the last
    // matching rule wins, so the specific allows must come after the
    // broad deny.
    profile.push_str("(deny file-write*)\n");
    profile.push_str(&format!(
        "(allow file-write* (subpath \"{wd}\"))\n",
        wd = wd_str
    ));
    // stdout / stderr / /dev/null writes go through the file-write*
    // operation. Without this allow, an `echo hello` inside the
    // sandbox produces no output at all — the sandbox silently
    // discards the write. Explicit literals for the standard streams.
    profile.push_str("(allow file-write* (literal \"/dev/stdout\") (literal \"/dev/stderr\") (literal \"/dev/null\"))\n");
    profile.push_str("(allow file-write-data (literal \"/dev/stdout\") (literal \"/dev/stderr\") (literal \"/dev/null\"))\n");
    if opts.tmp_rw {
        profile.push_str("(allow file-write* (subpath \"/tmp\") (subpath \"/private/tmp\") (subpath \"/private/var/folders\"))\n");
    }
    if opts.git_readonly {
        profile.push_str(&format!(
            "(deny file-write* (subpath \"{wd}/.git\"))\n",
            wd = wd_str
        ));
    }

    SandboxInvocation {
        program: "sandbox-exec".to_string(),
        args: vec!["-p".into(), profile, "--".into()],
        backend: Backend::Seatbelt,
    }
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn unsupported_platform_invocation() -> SandboxInvocation {
    unreachable!("sandbox_invocation on unsupported platform")
}

/// Stub used on platforms where bwrap is unavailable. The resolver
/// only reaches this arm when `Backend::Bwrap` is in `available`,
/// which the `detect()` function only populates on Linux — the stub is
/// therefore unreachable at runtime, but the compiler still needs the
/// symbol to exist for the match in `SandboxResolver::invocation`.
#[cfg(not(target_os = "linux"))]
fn bwrap_invocation(_wd: &Path, _opts: SandboxOpts) -> SandboxInvocation {
    unreachable!("bwrap_invocation called on a non-Linux platform")
}


/// Stub for platforms without Seatbelt. Same reasoning as the bwrap
/// stub above.
#[cfg(not(target_os = "macos"))]
fn seatbelt_invocation(_wd: &Path, _opts: SandboxOpts) -> SandboxInvocation {
    unreachable!("seatbelt_invocation called on a non-macOS platform")
}

/// Backward-compatible free function that matches the pre-C3a shape.
/// Uses a one-shot resolver; prefer holding a `SandboxResolver` when
/// the caller invokes multiple times.
pub fn sandbox_invocation(
    mode: SandboxMode,
    wd: &Path,
) -> Result<Option<SandboxInvocation>> {
    SandboxResolver::detect().invocation(mode, wd, SandboxOpts::default())
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

    /// Domain allow-list for `web_fetch` (D3-C5). Empty means "no
    /// policy-imposed restriction — the SSRF filter still applies".
    /// Populated by the engine from `ToolPolicy.domains` when a
    /// `PolicyEngine` is installed. Subdomain matching: `docs.rs`
    /// accepts `docs.rs` and `*.docs.rs`.
    pub allowed_domains: Vec<String>,
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
            sandbox: SandboxMode::Auto,
            allowed_domains: Vec::new(),
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

    /// Check whether git operations at the given access level are
    /// allowed. The caller passes the level it actually needs:
    /// `GitAccess::Read` for a query, `GitAccess::Write` for a
    /// mutation. A context granted `Write` also satisfies `Read`
    /// (see the enum's `Ord`).
    pub fn can_git_operation(
        &self,
        required: kod_types::GitAccess,
    ) -> Result<()> {
        if !self.permissions.git_access.is_at_least(required) {
            return Err(KodError::PermissionDenied {
                action: "git".to_string(),
                reason: format!(
                    "git access {:?} is below the required {:?}",
                    self.permissions.git_access, required
                ),
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
        // The default is now Auto (D3-C3a). The engine overrides it
        // per-call via `with_sandbox(self.sandbox_setting())`, so a
        // bare `ToolContext::new()` is a testing convenience.
        assert_eq!(context.sandbox, SandboxMode::Auto);
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
        let resolver = SandboxResolver::detect();
        let inv = resolver
            .invocation(SandboxMode::Require, Path::new("/tmp"), SandboxOpts::default())
            .expect("sandbox-exec should be available on macOS")
            .expect("Require must return Some");
        assert_eq!(inv.program, "sandbox-exec");
        assert_eq!(inv.backend, Backend::Seatbelt);
        assert_eq!(inv.args.last().map(|s| s.as_str()), Some("--"));
        let profile = &inv.args[1];
        assert!(profile.contains("/tmp"), "profile missing wd: {profile}");
        assert!(!profile.contains("{wd}"), "profile has unexpanded {{wd}}: {profile}");
        // The .git read-only rule must be present by default.
        assert!(
            profile.contains(".git"),
            "default opts should protect .git: {profile}"
        );
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

    #[test]
    fn auto_with_empty_resolver_returns_none() {
        let resolver = SandboxResolver::empty();
        let r = resolver
            .invocation(
                SandboxMode::Auto,
                Path::new("/tmp"),
                SandboxOpts::default(),
            )
            .unwrap();
        assert!(r.is_none(), "Auto must gracefully fall through");
    }

    #[test]
    fn require_with_empty_resolver_errors() {
        let resolver = SandboxResolver::empty();
        let err = resolver
            .invocation(
                SandboxMode::Require,
                Path::new("/tmp"),
                SandboxOpts::default(),
            )
            .unwrap_err();
        assert!(matches!(err, KodError::SandboxViolation(_)));
    }

    #[test]
    fn disabled_ignores_the_resolver() {
        // Even if the resolver has a backend, Disabled must short-circuit.
        let resolver = SandboxResolver::detect();
        let r = resolver
            .invocation(
                SandboxMode::Disabled,
                Path::new("/tmp"),
                SandboxOpts::default(),
            )
            .unwrap();
        assert!(r.is_none());
    }

    #[test]
    fn empty_resolver_reports_no_backend() {
        let resolver = SandboxResolver::empty();
        assert!(!resolver.has_any());
        assert!(resolver.backend_name().is_none());
    }

}
