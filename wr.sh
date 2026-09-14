#!/usr/bin/env bash
set -uo pipefail

CTX=crates/kod-tools/src/context.rs

if [ ! -f Cargo.toml ] || [ ! -f "$CTX" ]; then
    echo "ERROR: run from the kod workspace root"
    exit 1
fi

echo "=== Pre-state: end of resolve_path ==="
awk '/let root = std::fs::canonicalize\(&self.working_dir\)/,/^    \}$/' "$CTX" | head -20

echo
python3 - "$CTX" << 'PYEOF'
import os
import sys

path = sys.argv[1]
with open(path) as f:
    src = f.read()

old = '''        let root = std::fs::canonicalize(&self.working_dir).map_err(KodError::Io)?;
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

        Ok(canonical)'''

new = '''        let root = std::fs::canonicalize(&self.working_dir).map_err(KodError::Io)?;
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
        //
        // The initial `canonicalize` above resolves any symlink whose
        // target exists — including a symlink at the final component
        // pointing outside the workspace, which the containment check
        // catches. The case it does not catch is a *dangling* symlink:
        // canonicalize fails on a target that does not exist, the
        // fallback canonicalizes the parent (inside the workspace) and
        // re-appends the leaf, and the resulting lexical path passes
        // containment while the OS-level write would follow the
        // symlink and create the file at the target outside.
        //
        // Refuse a dangling leaf symlink. Resolving its target would
        // require recursively following read_link chains and is not a
        // destination whose containment can be verified; the safe
        // answer is "no". A symlink whose target *does* exist is
        // unaffected — that case was already safe.
        if let Ok(meta) = std::fs::symlink_metadata(&canonical)
            && meta.file_type().is_symlink()
        {
            match std::fs::canonicalize(&canonical) {
                Ok(target) => {
                    // Should be unreachable given the flow above
                    // (canonicalize on the target would have set
                    // `canonical` to it), but cheap insurance if the
                    // fallback path was taken for any other reason.
                    if !target.starts_with(&root) {
                        return Err(KodError::PermissionDenied {
                            action: "resolve path".to_string(),
                            reason: format!(
                                "Path is a symlink whose target escapes the \\
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
                            "Path is a dangling symlink: {}. Refusing to read \\
                             or write through it — the target does not exist \\
                             and cannot be verified as inside the working \\
                             directory.",
                            canonical.display()
                        ),
                    });
                }
            }
        }

        Ok(canonical)'''

n = src.count(old)
if n == 0:
    print("ERROR: resolve_path tail not found verbatim")
    sys.exit(2)
if n != 1:
    print(f"ERROR: expected 1 occurrence, found {n}")
    sys.exit(2)
src = src.replace(old, new, 1)

# ----------------------------------------------------------------------
# Tests.
# ----------------------------------------------------------------------
if "test_resolve_path_rejects_dangling_symlink" not in src:
    anchor = '''    #[test]
    fn test_resolve_path_rejects_traversal() {'''
    if anchor not in src:
        print("  WARN: test anchor not found; skipping test add")
    else:
        new_tests = '''    /// A dangling symlink inside the workspace must be rejected by
    /// resolve_path. Regression: the previous code canonicalized the
    /// parent (inside the workspace) and re-appended the leaf when
    /// the full canonicalize failed, so a dangling symlink passed the
    /// containment check while `File::create` would have created the
    /// file at the symlink's target outside the workspace.
    #[cfg(unix)]
    #[test]
    fn test_resolve_path_rejects_dangling_symlink() {
        let temp = tempfile::TempDir::new().unwrap();
        let root = temp.path().canonicalize().unwrap();
        // A symlink whose target does not exist.
        let link = root.join("dangling");
        std::os::unix::fs::symlink("/nonexistent-kod-test-target", &link).unwrap();

        let ctx = ToolContext::new(&root);
        let result = ctx.resolve_path("dangling");
        assert!(
            result.is_err(),
            "dangling symlink must be rejected, got: {result:?}"
        );
        let msg = result.unwrap_err().to_string();
        assert!(
            msg.contains("dangling") || msg.contains("symlink"),
            "error should name the problem: {msg}"
        );
    }

    /// A symlink whose target is inside the workspace and *exists*
    /// still resolves. The fix must not reject every symlink.
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
        assert!(
            result.is_ok(),
            "symlink to an inside file must resolve: {result:?}"
        );
        // Resolved form is the target, not the link.
        assert_eq!(result.unwrap(), root.join("real.txt"));
    }

    #[test]
    fn test_resolve_path_rejects_traversal() {'''
        src = src.replace(anchor, new_tests, 1)
        print("  patched: dangling-symlink + inside-symlink tests")

with open(path, "w") as f:
    f.write(src)
print("Wrote", path)
PYEOF

if [ $? -ne 0 ]; then
    echo "ERROR: patching failed"
    exit 1
fi

echo
echo "cargo check --workspace --all-targets 2>&1 | tail -8"
if ! cargo check --workspace --all-targets 2>&1 | tail -8; then
    echo "Compilation failed"
    exit 1
fi

echo
echo "cargo test -p kod-tools --quiet 2>&1 | tail -12"
if ! cargo test -p kod-tools --quiet 2>&1 | tail -12; then
    echo "kod-tools tests failed"
    exit 1
fi

cat > /tmp/kod_commit_msg.txt <<'MSG'
fix(tools): reject dangling symlinks in resolve_path

ToolContext::resolve_path canonicalized the target path and, when
that failed (the target does not exist), canonicalized the parent
and re-appended the leaf. The resulting lexical path passed the
working-directory containment check while the OS-level operation
would follow a symlink at the leaf and touch the target.

Concretely: with `dangling -> /outside/nonexistent` inside the
workspace, `write_file("dangling")` resolved to
`<workspace>/dangling`, passed containment, then `File::create`
followed the symlink and tried to create `/outside/nonexistent`.
If `/outside` is writable, the file is created outside the
workspace — a real escape from the containment guarantee that
resolve_path exists to enforce.

Refuse a dangling leaf symlink. Resolving its target would require
recursively following read_link chains and produces a destination
whose containment cannot be verified; refusing is the safe answer.
A symlink whose target exists is unaffected — canonicalize
resolves it, the containment check sees the target, and the
behavior is the same as before.

Adds two tests: a dangling symlink is rejected with a message
naming the problem; a symlink to an existing file inside the
workspace still resolves (the fix must not reject every symlink).
MSG

git add -A
git commit -F /tmp/kod_commit_msg.txt
rm -f /tmp/kod_commit_msg.txt
