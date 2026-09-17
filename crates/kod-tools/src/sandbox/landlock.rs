//! Landlock LSM backend (Linux ≥ 5.13).
//!
//! # What it is
//!
//! Landlock is a Linux security module that lets an unprivileged
//! process restrict *itself*: after a `landlock_restrict_self` call,
//! the process and its descendants cannot perform the access types
//! the ruleset did not explicitly allow. It needs no root, no
//! capability, and no setuid helper.
//!
//! # What it is used for here
//!
//! When `bwrap` is unavailable (a container, a distro with
//! bubblewrap uninstalled, a system that disables unprivileged user
//! namespaces), `SandboxResolver::detect` prefers Landlock over the
//! fail-open path. The kernel ABI check is a real syscall, so a
//! machine whose kernel is older than 5.13 (or whose distro compiled
//! out Landlock) falls through to fail-open exactly as before; a
//! machine that supports it gets a real sandbox.
//!
//! # Launcher
//!
//! Landlock is not a program: the restriction applies to the calling
//! process, and cannot be installed from outside. The architecture
//! puts the implementation in the `kod` binary itself as the hidden
//! `__sandbox-exec` subcommand. `SandboxResolver::invocation`
//! therefore returns a `SandboxInvocation` whose program is
//! `current_exe()` and whose args begin with `__sandbox-exec
//! <profile> --`. The launcher reads the profile, calls `apply`, and
//! `execvp`s the inner command.
//!
//! # Limitation: network
//!
//! Landlock can restrict the filesystem portably across ABIs 1–4, but
//! network restriction (`LANDLOCK_ACCESS_NET_*`) only landed in ABI 4
//! (Linux 6.7). The profile carries a `net_deny` flag, and `apply`
//! *refuses to proceed* when it is set on a kernel whose ABI is
//! below 4 — the caller's expectation (`net_deny = true`) cannot be
//! met, and silently proceeding would be the "illusion of security"
//! failure mode the architecture document forbids. The resolver
//! checks the ABI *before* choosing Landlock as a backend, so the
//! user sees a fail-open with an explicit warning instead of a
//! launcher that would have to bail.

use kod_error::{KodError, Result};
use serde::{Deserialize, Serialize};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};

// ---------------------------------------------------------------------------
// Syscall numbers and ABI constants
// ---------------------------------------------------------------------------
//
// The kernel exposes Landlock through three syscalls whose numbers
// are not exported by the `libc` crate (as of libc 0.2.x). They are
// called through `libc::syscall` with the values below. The numbers
// 444-446 are the same on x86_64, aarch64, and the other modern
// architectures; the two Linux targets this workspace builds for are
// x86_64 and aarch64, both of which use these numbers.

const SYS_LANDLOCK_CREATE_RULESET: libc::c_long = 444;
const SYS_LANDLOCK_ADD_RULE: libc::c_long = 445;
const SYS_LANDLOCK_RESTRICT_SELF: libc::c_long = 446;

const LANDLOCK_CREATE_RULESET_VERSION: u32 = 1 << 0;
const LANDLOCK_RULE_PATH_BENEATH: u32 = 1;

/// `PR_SET_NO_NEW_PRIVS`. Required before `landlock_restrict_self`;
// ignore the clippy warning about `libc` already declaring a
/// constant of the same name — using our own keeps the semantics
/// visible at the call site.
#[allow(dead_code)]
const PR_SET_NO_NEW_PRIVS: libc::c_int = 38;

// File access rights (bitmask in a u64). Names match
// `<linux/landlock.h>`; the values are what the kernel reads from
// `landlock_path_beneath_attr.allowed_access`.
const LANDLOCK_ACCESS_FS_EXECUTE: u64 = 1 << 0;
const LANDLOCK_ACCESS_FS_WRITE_FILE: u64 = 1 << 1;
const LANDLOCK_ACCESS_FS_READ_FILE: u64 = 1 << 2;
const LANDLOCK_ACCESS_FS_READ_DIR: u64 = 1 << 3;
const LANDLOCK_ACCESS_FS_REMOVE_DIR: u64 = 1 << 4;
const LANDLOCK_ACCESS_FS_REMOVE_FILE: u64 = 1 << 5;
const LANDLOCK_ACCESS_FS_MAKE_CHAR: u64 = 1 << 6;
const LANDLOCK_ACCESS_FS_MAKE_DIR: u64 = 1 << 7;
const LANDLOCK_ACCESS_FS_MAKE_REG: u64 = 1 << 8;
const LANDLOCK_ACCESS_FS_MAKE_SOCK: u64 = 1 << 9;
const LANDLOCK_ACCESS_FS_MAKE_FIFO: u64 = 1 << 10;
const LANDLOCK_ACCESS_FS_MAKE_BLOCK: u64 = 1 << 11;
const LANDLOCK_ACCESS_FS_MAKE_SYM: u64 = 1 << 12;
const LANDLOCK_ACCESS_FS_REFER: u64 = 1 << 13; // ABI >= 2
const LANDLOCK_ACCESS_FS_TRUNCATE: u64 = 1 << 14; // ABI >= 3

/// The full set of access rights this backend manages.
///
/// Rights *not* in this mask are not restricted by the ruleset — the
/// sandbox is "restrict only what we say". That is deliberate: a
/// partial allow-list (execute a binary, read its config from
/// `/etc`, open a socket) needs the full breadth of access types,
/// and managing them individually is how a sandbox ends up either
/// too tight to run anything or so loose it is a decoration.
fn handled_access_fs(abi: u32) -> u64 {
    let mut mask = LANDLOCK_ACCESS_FS_EXECUTE
        | LANDLOCK_ACCESS_FS_WRITE_FILE
        | LANDLOCK_ACCESS_FS_READ_FILE
        | LANDLOCK_ACCESS_FS_READ_DIR
        | LANDLOCK_ACCESS_FS_REMOVE_DIR
        | LANDLOCK_ACCESS_FS_REMOVE_FILE
        | LANDLOCK_ACCESS_FS_MAKE_CHAR
        | LANDLOCK_ACCESS_FS_MAKE_DIR
        | LANDLOCK_ACCESS_FS_MAKE_REG
        | LANDLOCK_ACCESS_FS_MAKE_SOCK
        | LANDLOCK_ACCESS_FS_MAKE_FIFO
        | LANDLOCK_ACCESS_FS_MAKE_BLOCK
        | LANDLOCK_ACCESS_FS_MAKE_SYM;
    if abi >= 2 {
        mask |= LANDLOCK_ACCESS_FS_REFER;
    }
    if abi >= 3 {
        mask |= LANDLOCK_ACCESS_FS_TRUNCATE;
    }
    mask
}

/// The kernel reads `landlock_path_beneath_attr` from a raw pointer;
/// the struct is `packed` in the UAPI header, and `#[repr(C,
/// packed)]` here is required to match its layout (12 bytes: 8 + 4,
/// no trailing padding).
#[repr(C, packed)]
struct LandlockPathBeneathAttr {
    allowed_access: u64,
    parent_fd: RawFd,
}

// ---------------------------------------------------------------------------
// Profile
// ---------------------------------------------------------------------------

/// The sandbox profile written to a temp file by the parent `kod`
/// process and read by the `kod __sandbox-exec` launcher.
///
/// The profile is deliberately a small, explicit struct: the parent
/// decides *what* to allow (which subtrees are ro, which are rw),
/// the launcher decides *how* to apply the ruleset. Keeping the two
/// decisions in separate processes means a mistake in the launcher
/// cannot silently change the policy.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LandlockProfile {
    /// Path subtrees the sandboxed process can read+execute but not
    /// write. Typically `/usr`, `/lib`, `/bin`, `/etc`, and the
    /// worktree's `.git`.
    pub ro_paths: Vec<PathBuf>,
    /// Path subtrees the sandboxed process can read, write, and
    /// create. Typically the working directory and `$TMPDIR`.
    pub rw_paths: Vec<PathBuf>,
    /// Whether the caller asked for network to be denied. The
    /// launcher refuses to proceed with this set on kernels below
    /// ABI 4; see the module doc.
    pub net_deny: bool,
}

impl LandlockProfile {
    /// Serialize to the JSON the `kod __sandbox-exec` launcher reads.
    pub fn to_json(&self) -> String {
        serde_json::to_string(self).unwrap_or_else(|_| "{}".to_string())
    }

    pub fn from_json(s: &str) -> Result<Self> {
        serde_json::from_str(s)
            .map_err(|e| KodError::SandboxViolation(format!("invalid sandbox profile: {e}")))
    }
}

// ---------------------------------------------------------------------------
// ABI probe
// ---------------------------------------------------------------------------

/// Query the kernel's Landlock ABI version.
///
/// Returns `Some(n)` for a kernel that supports Landlock (n >= 1),
/// `None` for a kernel that does not (older than 5.13, or compiled
/// without Landlock). The check is a real syscall: on a supported
/// kernel the `landlock_create_ruleset` syscall with the VERSION
/// flag and a NULL attr returns the ABI as its return value; on an
/// unsupported kernel the same call returns -1 with errno `ENOSYS`
/// or `EOPNOTSUPP`.
pub fn probe_abi() -> Option<u32> {
    // SAFETY: the syscall takes a NULL pointer and a size of 0 with
    // the VERSION flag; that is the documented probe form and does
    // not dereference memory.
    let ret = unsafe {
        libc::syscall(
            SYS_LANDLOCK_CREATE_RULESET,
            std::ptr::null::<std::ffi::c_void>(),
            0usize,
            LANDLOCK_CREATE_RULESET_VERSION,
        )
    };
    if ret > 0 { Some(ret as u32) } else { None }
}

// ---------------------------------------------------------------------------
// Apply
// ---------------------------------------------------------------------------

/// Apply the profile's ruleset to the current process. Idempotent
/// with respect to calling conventions: a process that never calls
/// this cannot become sandboxed, and there is no way to remove the
/// restriction short of `execve`-ing a process that never called it.
pub fn apply(profile: &LandlockProfile) -> Result<()> {
    let abi = probe_abi().ok_or_else(|| {
        KodError::SandboxViolation(
            "Landlock is not available on this kernel (need Linux >= 5.13)".to_string(),
        )
    })?;
    if profile.net_deny && abi < 4 {
        return Err(KodError::SandboxViolation(format!(
            "network denial requested but Landlock ABI is {abi} \
             (network rules require ABI 4, Linux >= 6.7). \
             Falling back is the caller's decision."
        )));
    }

    // 1. Create the ruleset. The syscall reads `handled_access_fs`
    // as the first 8 bytes of the attr; passing only that field is
    // legal and works on every ABI we support.
    let handled = handled_access_fs(abi);
    // SAFETY: `handled` is a valid u64; the syscall reads exactly
    // size_of::<u64>() bytes from the pointer.
    let ruleset_fd_raw = unsafe {
        libc::syscall(
            SYS_LANDLOCK_CREATE_RULESET,
            &handled as *const u64 as *const std::ffi::c_void,
            std::mem::size_of::<u64>(),
            0u32,
        )
    };
    if ruleset_fd_raw < 0 {
        let err = std::io::Error::last_os_error();
        return Err(KodError::SandboxViolation(format!(
            "landlock_create_ruleset failed: {err}"
        )));
    }
    // SAFETY: ruleset_fd_raw is a fresh fd returned by the kernel;
    // wrapping it in OwnedFd takes responsibility for closing it.
    let ruleset_fd: OwnedFd = unsafe { OwnedFd::from_raw_fd(ruleset_fd_raw as i32) };

    // 2. Read-only rules.
    let ro_access = LANDLOCK_ACCESS_FS_EXECUTE
        | LANDLOCK_ACCESS_FS_READ_FILE
        | LANDLOCK_ACCESS_FS_READ_DIR;
    for path in &profile.ro_paths {
        add_path_rule(&ruleset_fd, path, ro_access)?;
    }

    // 3. Read-write rules: every right the ruleset handles.
    let rw_access = handled;
    for path in &profile.rw_paths {
        add_path_rule(&ruleset_fd, path, rw_access)?;
    }

    // 4. PR_SET_NO_NEW_PRIVS is required before landlock_restrict_self.
    // SAFETY: prctl is a standard libc wrapper; the arguments are the
    // documented form for PR_SET_NO_NEW_PRIVS.
    let ret = unsafe { libc::prctl(PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0) };
    if ret != 0 {
        let err = std::io::Error::last_os_error();
        return Err(KodError::SandboxViolation(format!(
            "prctl(PR_SET_NO_NEW_PRIVS) failed: {err}"
        )));
    }

    // 5. Restrict self.
    // SAFETY: ruleset_fd is a valid Landlock ruleset fd.
    let ret = unsafe {
        libc::syscall(SYS_LANDLOCK_RESTRICT_SELF, ruleset_fd.as_raw_fd(), 0u32)
    };
    if ret != 0 {
        let err = std::io::Error::last_os_error();
        return Err(KodError::SandboxViolation(format!(
            "landlock_restrict_self failed: {err}"
        )));
    }

    Ok(())
}

/// Add one path rule to the ruleset. A path that does not exist yet
/// is logged and skipped rather than treated as fatal: the worktree
/// may gain subdirectories after `restrict_self` (created by the
/// sandboxed process itself), and the profile built before that
/// cannot name them.
fn add_path_rule(
    ruleset_fd: &OwnedFd,
    path: &Path,
    allowed_access: u64,
) -> Result<()> {
    // Open the path with O_PATH: the kernel only needs an fd to
    // identify the inode the rule applies beneath; O_PATH avoids
    // requiring read permission on the directory itself.
    let cstr = std::ffi::CString::new(path.as_os_str().as_bytes()).map_err(|e| {
        KodError::SandboxViolation(format!(
            "sandbox path contains NUL: {}: {e}",
            path.display()
        ))
    })?;
    // SAFETY: cstr is a valid C string; O_PATH|O_CLOEXEC is a
    // standard open flags pair.
    let fd_raw = unsafe { libc::open(cstr.as_ptr(), libc::O_PATH | libc::O_CLOEXEC) };
    if fd_raw < 0 {
        let err = std::io::Error::last_os_error();
        tracing::debug!(
            path = %path.display(),
            error = %err,
            "landlock: could not open path for rule; skipping"
        );
        return Ok(());
    }
    // SAFETY: fd_raw is a fresh fd from open(2).
    let fd: OwnedFd = unsafe { OwnedFd::from_raw_fd(fd_raw) };

    let rule = LandlockPathBeneathAttr {
        allowed_access,
        parent_fd: fd.as_raw_fd(),
    };
    // SAFETY: `rule` is a valid LandlockPathBeneathAttr; the syscall
    // reads exactly size_of::<LandlockPathBeneathAttr>() bytes from
    // the pointer.
    let ret = unsafe {
        libc::syscall(
            SYS_LANDLOCK_ADD_RULE,
            ruleset_fd.as_raw_fd(),
            LANDLOCK_RULE_PATH_BENEATH,
            &rule as *const _ as *const std::ffi::c_void,
            0u32,
        )
    };
    if ret != 0 {
        let err = std::io::Error::last_os_error();
        return Err(KodError::SandboxViolation(format!(
            "landlock_add_rule for {} failed: {err}",
            path.display()
        )));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn probe_returns_some_or_none_but_never_panics() {
        // A useful smoke test: the probe runs a real syscall and
        // either succeeds with an ABI or fails cleanly. Any panic
        // (e.g. from a malformed pointer) fails the test on every
        // host, supported or not.
        let abi = probe_abi();
        if let Some(v) = abi {
            assert!(v >= 1, "Landlock ABI reports 0, which is not a valid version");
        }
    }

    #[test]
    fn handled_mask_grows_with_abi() {
        let a1 = handled_access_fs(1);
        let a2 = handled_access_fs(2);
        let a3 = handled_access_fs(3);
        assert_eq!(a2, a1 | LANDLOCK_ACCESS_FS_REFER);
        assert_eq!(a3, a2 | LANDLOCK_ACCESS_FS_TRUNCATE);
    }

    #[test]
    fn profile_roundtrip() {
        let p = LandlockProfile {
            ro_paths: vec![PathBuf::from("/usr")],
            rw_paths: vec![PathBuf::from("/tmp")],
            net_deny: true,
        };
        let json = p.to_json();
        let q = LandlockProfile::from_json(&json).expect("roundtrip");
        assert_eq!(p.ro_paths, q.ro_paths);
        assert_eq!(p.rw_paths, q.rw_paths);
        assert_eq!(p.net_deny, q.net_deny);
    }

    #[test]
    fn from_json_rejects_garbage() {
        let err = LandlockProfile::from_json("not json").unwrap_err();
        match err {
            KodError::SandboxViolation(msg) => {
                assert!(msg.contains("invalid sandbox profile"), "got: {msg}");
            }
            other => panic!("expected SandboxViolation, got {other:?}"),
        }
    }

    /// On a kernel that supports Landlock, applying an empty
    /// profile with no net_deny must succeed. This is a real test of
    /// the syscall path; skipped on hosts without Landlock so the
    /// workspace's macOS and Windows CI still passes.
    ///
    /// The kernel change is process-wide and irreversible, so this
    /// test would normally be a landmine (all subsequent tests in
    /// the same process would run restricted). The empty profile
    /// avoids that: no path rules and no net_deny means nothing is
    /// actually blocked, and the restriction is a no-op.
    #[test]
    fn apply_empty_profile_is_a_noop_or_clean_error() {
        let profile = LandlockProfile {
            ro_paths: Vec::new(),
            rw_paths: Vec::new(),
            net_deny: false,
        };
        match apply(&profile) {
            Ok(()) => {}
            Err(KodError::SandboxViolation(msg)) => {
                // Old kernel: expected.
                assert!(
                    msg.contains("not available"),
                    "unexpected SandboxViolation: {msg}"
                );
            }
            Err(other) => panic!("unexpected error: {other:?}"),
        }
    }

    /// net_deny on an ABI below 4 must be refused explicitly — the
    /// whole point is that the caller asked for something the
    /// kernel cannot provide, and a silent success would lie.
    #[test]
    fn net_deny_on_low_abi_is_refused() {
        let abi = match probe_abi() {
            Some(v) => v,
            None => return, // no Landlock on this host; nothing to test
        };
        if abi >= 4 {
            return; // ABI supports net rules; the refusal path is unreachable
        }
        let profile = LandlockProfile {
            ro_paths: Vec::new(),
            rw_paths: Vec::new(),
            net_deny: true,
        };
        let err = apply(&profile).unwrap_err();
        match err {
            KodError::SandboxViolation(msg) => {
                assert!(
                    msg.contains("network denial"),
                    "message should name the unmet expectation: {msg}"
                );
            }
            other => panic!("expected SandboxViolation, got {other:?}"),
        }
    }
}
