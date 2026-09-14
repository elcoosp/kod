#!/usr/bin/env bash
set -uo pipefail

WS=crates/kod-swarm/src/workspace.rs

if [ ! -f Cargo.toml ] || [ ! -f "$WS" ]; then
    echo "ERROR: run from the kod workspace root"
    exit 1
fi

python3 - "$WS" << 'PYEOF'
import os
import sys

path = sys.argv[1]
with open(path) as f:
    src = f.read()

old = '''        if !canonical.starts_with(&root_canonical) {
            return Err(KodError::PermissionDenied {
                action: "access".to_string(),
                reason: format!("Path outside workspace: {}", canonical.display()),
            });
        }

        Ok(canonical)'''

new = '''        if !canonical.starts_with(&root_canonical) {
            return Err(KodError::PermissionDenied {
                action: "access".to_string(),
                reason: format!("Path outside workspace: {}", canonical.display()),
            });
        }

        // Final-component symlink check.
        //
        // `canonicalize` above resolves any symlink whose target
        // exists — including one at the final component pointing
        // outside the workspace, which the containment check catches.
        // The case it does not catch is a *dangling* symlink: the
        // initial canonicalize fails on a target that does not exist,
        // the fallback canonicalizes the deepest existing ancestor
        // (inside the workspace) and re-appends the leaf, and the
        // resulting lexical path passes containment while an OS-level
        // operation would follow the symlink to a target outside.
        //
        // Refuse a dangling leaf symlink. Resolving its target would
        // require recursively following read_link chains and produces
        // a destination whose containment cannot be verified; the
        // safe answer is "no." A symlink whose target exists is
        // unaffected — canonicalize resolves it, and the lock is
        // taken on the target's canonical form (which is the right
        // semantics for coordination: two agents reaching the same
        // file via two symlinks must take the same lock).
        if let Ok(meta) = std::fs::symlink_metadata(&canonical)
            && meta.file_type().is_symlink()
        {
            match std::fs::canonicalize(&canonical) {
                Ok(target) => {
                    // Should be unreachable given the flow above, but
                    // cheap insurance in case the fallback path was
                    // taken for any other reason.
                    if !target.starts_with(&root_canonical) {
                        return Err(KodError::PermissionDenied {
                            action: "access".to_string(),
                            reason: format!(
                                "Path is a symlink whose target escapes the \\
                                 workspace: {} -> {}",
                                path.display(),
                                target.display()
                            ),
                        });
                    }
                }
                Err(_) => {
                    return Err(KodError::PermissionDenied {
                        action: "access".to_string(),
                        reason: format!(
                            "Path is a dangling symlink: {}. Refusing to lock \\
                             or resolve — the target does not exist and cannot \\
                             be verified as inside the workspace.",
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
    fn test_resolve_path_rejects_absolute_outside_root() {'''
    if anchor not in src:
        print("  WARN: test anchor not found; skipping test add")
    else:
        new_tests = '''    /// A dangling symlink inside the workspace must be rejected.
    /// Regression: the fallback canonicalized the deepest existing
    /// ancestor (inside the workspace) and re-appended the leaf, so
    /// the lexical path passed containment while an OS-level write
    /// would have followed the symlink to a target outside.
    #[cfg(unix)]
    #[test]
    fn test_resolve_path_rejects_dangling_symlink() {
        let (_tmp, ws) = ws();
        let link = ws.root().join("dangling");
        std::os::unix::fs::symlink("/nonexistent-kod-swarm-test", &link).unwrap();

        let result = ws.resolve_path(std::path::Path::new("dangling"));
        assert!(
            result.is_err(),
            "dangling symlink must be rejected: {result:?}"
        );
        let msg = result.unwrap_err().to_string();
        assert!(
            msg.contains("dangling") || msg.contains("symlink"),
            "error should name the problem: {msg}"
        );
    }

    /// A symlink whose target exists inside the workspace still
    /// resolves, and resolves to the *target* form — so two agents
    /// reaching the same file via different symlinks take the same
    /// lock.
    #[cfg(unix)]
    #[test]
    fn test_resolve_path_accepts_symlink_to_inside_file() {
        let (_tmp, ws) = ws();
        std::fs::write(ws.root().join("real.txt"), "x").unwrap();
        std::os::unix::fs::symlink(
            ws.root().join("real.txt"),
            ws.root().join("link.txt"),
        )
        .unwrap();

        let via_link = ws
            .resolve_path(std::path::Path::new("link.txt"))
            .expect("symlink to inside file must resolve");
        let via_real = ws
            .resolve_path(std::path::Path::new("real.txt"))
            .expect("real file must resolve");
        assert_eq!(
            via_link, via_real,
            "both paths must resolve to the same canonical form \\
             (the lock key)"
        );
    }

    #[test]
    fn test_resolve_path_rejects_absolute_outside_root() {'''
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
echo "cargo test -p kod-swarm --quiet 2>&1 | tail -12"
if ! cargo test -p kod-swarm --quiet 2>&1 | tail -12; then
    echo "kod-swarm tests failed"
    exit 1
fi

cat > /tmp/kod_commit_msg.txt <<'MSG'
fix(swarm): reject dangling symlinks in SharedWorkspace::resolve_path

Same shape as the ToolContext::resolve_path fix. The workspace
canonicalized the target; when the target does not exist,
canonicalization of the deepest existing ancestor (inside the
workspace) plus re-append of the leaf produced a lexical path that
passed the containment check while an OS-level operation would have
followed a symlink at the leaf to a target outside the workspace.

Concretely: with `dangling -> /outside/nonexistent` inside the
workspace, `acquire_lock("dangling")` would resolve to
`<root>/dangling`, pass containment, and register a lock on the
lexical path — while any subsequent write through the same path
went to `/outside/nonexistent`. The lock protected nothing.

Refuse a dangling leaf symlink. A symlink whose target exists is
unaffected: canonicalize resolves it, the containment check sees
the target, and the lock is taken on the target's canonical form.
That last point is the correct semantics for coordination — two
agents reaching the same file via two different symlinks now take
the same lock, because both resolve to the same canonical path.

Adds two tests: a dangling symlink is rejected with a message
naming the problem; a symlink to an existing file inside the
workspace resolves to the *target* form (equal to resolving the
target directly), pinning the lock-key invariant.
MSG

git add -A
git commit -F /tmp/kod_commit_msg.txt
rm -f /tmp/kod_commit_msg.txt
