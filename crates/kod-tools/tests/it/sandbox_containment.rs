//! Sandbox containment (design §11.3, "non-contournement sandbox").
//!
//! # What this asserts
//!
//! With `SandboxMode::Require` on a host where a primitive is
//! available, a shell command that tries to escape the workspace must
//! fail at the OS level — not merely be rejected by the policy layer.
//! The design's defence in depth relies on this: an `Allow` policy
//! that lets `execute_command` through must still not let the child
//! process reach outside the working directory.
//!
//! # What this test does *not* do
//!
//! It does not install a sandbox primitive, and it does not fail on a
//! host without one. The test is `#[ignore]`-gated: a maintainer runs
//! it deliberately, on a host where they have set up bwrap or
//! sandbox-exec or Landlock and want to verify the containment
//! invariant holds end to end.
//!
//! To run:
//!
//! ```sh
//! cargo test -p kod-tools --test sandbox_containment -- --ignored --nocapture
//! ```
//!
//! On a host without a primitive, the test skips cleanly with an
//! explanatory message.

#![cfg(unix)]

use kod_tools::context::{Backend, SandboxMode, SandboxOpts, SandboxResolver};

/// The test workspace is a tempdir; the escape target is the parent.
fn tempdir() -> tempfile::TempDir {
    tempfile::TempDir::new().expect("tempdir")
}

/// `true` when the host has a sandbox primitive this test can exercise.
fn has_backend() -> Option<Backend> {
    let resolver = SandboxResolver::detect();
    if !resolver.has_any() {
        return None;
    }
    // Return the resolver's chosen backend via a throwaway invocation.
    let wd = std::env::temp_dir();
    resolver
        .invocation(SandboxMode::Require, &wd, SandboxOpts::default())
        .ok()
        .flatten()
        .map(|inv| inv.backend)
}

fn build_invocation(
    backend: Backend,
    wd: &std::path::Path,
    inner_cmd: &[&str],
) -> std::process::Command {
    let resolver = SandboxResolver::detect();
    let inv = resolver
        .invocation(SandboxMode::Require, wd, SandboxOpts::default())
        .expect("invocation")
        .expect("Require must yield Some when a backend is available");
    assert_eq!(inv.backend, backend, "resolver chose a different backend");

    let mut cmd = std::process::Command::new(&inv.program);
    cmd.args(&inv.args);
    // The invocation ends with `--`; the inner shell program follows.
    for arg in inner_cmd {
        cmd.arg(arg);
    }
    cmd.current_dir(wd);
    cmd
}

/// A shell command inside the sandbox that tries to write outside the
/// working directory must fail. `Require` + a primitive → the write is
/// refused at the OS level; the shell reports a non-zero status.
#[test]
fn write_outside_workspace_is_refused() {
    let Some(backend) = has_backend() else {
        eprintln!(
            "skipping: no sandbox primitive on this host. Install bwrap \
             (Linux) or use sandbox-exec (macOS) to exercise this test.",
        );
        return;
    };
    eprintln!("running with backend {backend:?}");

    let wd = tempdir();
    // The escape target must live somewhere the *default* SandboxOpts
    // profile does NOT allow. `SandboxOpts::default()` sets
    // `tmp_rw = true`, and the Seatbelt profile (and its bwrap
    // counterpart) deliberately allows writes under `$TMPDIR` — a
    // build tool that stages through `/tmp` would otherwise break.
    // Using `$TMPDIR` as the escape path therefore proved nothing:
    // the write succeeded because it was allowed by design, not
    // because the sandbox leaked. The escape target is now under
    // `$HOME`, which the default profile leaves denied.
    let home = std::env::var_os("HOME")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(std::env::temp_dir);
    let outside = home.join(format!("kod-sandbox-escape-{}", std::process::id()));
    let _ = std::fs::remove_file(&outside);

    // The command: try to create a file outside the sandbox. On a
    // properly configured primitive, this fails.
    let target = outside.to_string_lossy().to_string();
    let script = format!("echo escaped > {target}");

    let mut cmd = build_invocation(backend, wd.path(), &["sh", "-c", &script]);
    let out = cmd.output().expect("spawn sandboxed shell");
    eprintln!(
        "sandbox exit: {:?}\nstdout: {}\nstderr: {}",
        out.status.code(),
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr),
    );

    let escaped = outside.exists();
    let _ = std::fs::remove_file(&outside);

    assert!(
        !escaped,
        "sandbox did not contain the write: {target} was created despite \
         SandboxMode::Require on backend {backend:?}",
    );
    assert!(
        !out.status.success(),
        "the escape attempt should have failed; the shell exited 0",
    );
}

/// A shell command inside the sandbox that writes *inside* the working
/// directory must succeed. This is the counterpart: the sandbox is not
/// so tight that it blocks legitimate work.
#[test]
fn write_inside_workspace_succeeds() {
    let Some(backend) = has_backend() else {
        eprintln!("skipping: no sandbox primitive on this host.",);
        return;
    };
    eprintln!("running with backend {backend:?}");

    let wd = tempdir();
    let script = "echo ok > inside.txt";

    let mut cmd = build_invocation(backend, wd.path(), &["sh", "-c", script]);
    let out = cmd.output().expect("spawn sandboxed shell");
    eprintln!(
        "sandbox exit: {:?}\nstdout: {}\nstderr: {}",
        out.status.code(),
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr),
    );

    assert!(
        wd.path().join("inside.txt").exists(),
        "a write inside the working directory must succeed under \
         SandboxMode::Require on backend {backend:?}",
    );
    assert!(
        out.status.success(),
        "the inner command should have exited 0, got {:?}",
        out.status.code(),
    );
}

/// `.git` is read-only inside the workspace by default
/// (`SandboxOpts::git_readonly = true`). A command that tries to write
/// there must fail even though it is inside the working directory.
#[test]
fn write_to_dot_git_is_refused() {
    let Some(backend) = has_backend() else {
        eprintln!("skipping: no sandbox primitive on this host.");
        return;
    };
    eprintln!("running with backend {backend:?}");

    let wd = tempdir();
    // Create a `.git` directory so the ro-bind has a source.
    std::fs::create_dir_all(wd.path().join(".git")).unwrap();
    std::fs::write(
        wd.path().join(".git").join("HEAD"),
        "ref: refs/heads/main\n",
    )
    .unwrap();

    let script = "echo hijack > .git/config";

    let mut cmd = build_invocation(backend, wd.path(), &["sh", "-c", script]);
    let out = cmd.output().expect("spawn sandboxed shell");
    eprintln!(
        "sandbox exit: {:?}\nstderr: {}",
        out.status.code(),
        String::from_utf8_lossy(&out.stderr),
    );

    // The write into `.git` must not have succeeded; the file either
    // does not exist or its content is unchanged from what we wrote.
    let git_config = wd.path().join(".git").join("config");
    if git_config.exists() {
        let content = std::fs::read_to_string(&git_config).unwrap_or_default();
        assert!(
            !content.contains("hijack"),
            ".git/config was modified despite git_readonly; content: {content}",
        );
    }
}
