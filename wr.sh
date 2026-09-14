#!/usr/bin/env bash
set -uo pipefail

CTX=crates/kod-tools/src/context.rs

echo "=== Diagnostic: matches_pattern + is_path_allowed ==="
awk '/fn matches_pattern/,/^    \}$/' "$CTX"
echo "---"
awk '/fn is_path_allowed/,/^    \}$/' "$CTX"

echo
echo "Patching $CTX"

python3 - "$CTX" << 'PYEOF'
import os
import sys

target = sys.argv[1]
with open(target, "r") as f:
    src = f.read()

def patch(old, new, label, expect=1):
    global src
    n = src.count(old)
    if n == 0:
        print(f"  SKIP (anchor absent): {label}")
        return False
    if expect and n != expect:
        print(f"  ERROR: expected {expect} occurrence(s) of {label}, found {n}")
        sys.exit(2)
    src = src.replace(old, new, expect if expect else n)
    print(f"  patched: {label}")
    return True

# ----------------------------------------------------------------------
# 1. matches_pattern: also match the path itself, not only its children.
# ----------------------------------------------------------------------
patch(
    '''    /// Match a path against a glob pattern
    fn matches_pattern(path: &Path, pattern: &str) -> bool {
        let glob = format!("{}/**", pattern);
        match globset::Glob::new(&glob) {
            Ok(glob) => glob.compile_matcher().is_match(path),
            Err(_) => false,
        }
    }''',
    '''    /// Does `path` fall under `pattern`?
    ///
    /// A pattern matches the path it names *and* anything below it.
    /// The previous implementation only built `pattern/**`, so
    /// `/tmp/allowed/file.txt` matched the entry `/tmp/allowed` but
    /// `/tmp/allowed` itself did not — the directory could not be
    /// listed or read by the user who had just allowed it, only its
    /// contents could. The same gap reversed on the forbidden side: a
    /// forbidden directory was itself readable, and only its children
    /// were blocked.
    ///
    /// Patterns containing `*`, `?`, or `[` are honored verbatim: a
    /// user who wrote `/tmp/*` meant exactly that set, and appending
    /// `/**` would broaden it to `/tmp/*/**` and match unrelated
    /// paths. Everything else gets both the literal pattern and
    /// `pattern/**`.
    ///
    /// Failure to compile either glob returns `false` — a bad pattern
    /// matches nothing, so a permission that names an invalid glob
    /// does not silently allow or forbid everything.
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
    }''',
    "matches_pattern: also match the named path",
)

# ----------------------------------------------------------------------
# 2. Extend the existing forbidden-path test to cover the boundary.
# ----------------------------------------------------------------------
patch(
    '''    #[test]
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
    }''',
    '''    #[test]
    fn test_forbidden_path() {
        let perms = ToolPermissions {
            read_files: true,
            allowed_paths: vec!["/tmp/allowed".to_string()],
            forbidden_paths: vec!["/tmp/allowed/forbidden".to_string()],
            ..Default::default()
        };
        let context = ToolContext::new("/tmp").with_permissions(perms);

        // Child of an allowed directory: allowed.
        let result = context.can_read(Path::new("/tmp/allowed/test.txt"));
        assert!(result.is_ok());

        // The allowed directory itself: allowed. Regression: the
        // previous matches_pattern only built `pattern/**`, so the
        // directory named by an allowed_paths entry did not match
        // that entry — a user who allowed a directory could read its
        // children but not the directory.
        let result = context.can_read(Path::new("/tmp/allowed"));
        assert!(
            result.is_ok(),
            "allowed directory itself must be readable: {result:?}"
        );

        // Child of a forbidden directory: forbidden.
        let result = context.can_read(Path::new("/tmp/allowed/forbidden/secret.txt"));
        assert!(result.is_err());

        // The forbidden directory itself: forbidden. Regression: the
        // same gap in the other direction — a forbidden directory was
        // readable, only its contents were blocked.
        let result = context.can_read(Path::new("/tmp/allowed/forbidden"));
        assert!(
            result.is_err(),
            "forbidden directory itself must be rejected: {result:?}"
        );
    }

    /// A pattern with a wildcard is honored verbatim — the fix must
    /// not broaden `/tmp/*` to `/tmp/*/**` and match unrelated paths.
    #[test]
    fn test_wildcard_pattern_is_verbatim() {
        let perms = ToolPermissions {
            read_files: true,
            allowed_paths: vec!["/tmp/*".to_string()],
            ..Default::default()
        };
        let context = ToolContext::new("/").with_permissions(perms);

        // `/tmp/anything` matches `/tmp/*`.
        assert!(context.can_read(Path::new("/tmp/anything")).is_ok());
        // `/tmp/anything/deeper` does not match `/tmp/*` under
        // globset's default separator handling — `*` is a single
        // path segment.
        assert!(
            context.can_read(Path::new("/tmp/anything/deeper")).is_err(),
            "wildcard must not be silently broadened to match nested paths"
        );
        // And it definitely does not match unrelated paths.
        assert!(context.can_read(Path::new("/var/log")).is_err());
    }''',
    "extend forbidden-path test + wildcard test",
)

tmp = target + ".tmp"
with open(tmp, "w") as f:
    f.write(src)
os.replace(tmp, target)
print("Wrote", target)
PYEOF

if [ $? -ne 0 ]; then
    echo "ERROR: patching failed"
    exit 1
fi

echo
echo "cargo check --workspace --all-targets 2>&1 | tail -12"
if ! cargo check --workspace --all-targets 2>&1 | tail -12; then
    echo "Compilation failed"
    exit 1
fi

echo
echo "cargo clippy --workspace --all-targets -- -D warnings 2>&1 | tail -12"
if ! cargo clippy --workspace --all-targets -- -D warnings 2>&1 | tail -12; then
    echo "Clippy failed"
    exit 1
fi

cat > /tmp/kod_commit_msg.txt <<'MSG'
fix(tools): permission patterns match the named path, not just children

ToolContext::matches_pattern built `format!("{}/**", pattern)` for
every entry. Under globset, `/tmp/allowed/**` matches
`/tmp/allowed/file.txt` but not `/tmp/allowed` itself. Two
consequences, one on each side of the permission check:

- A user who wrote `allowed_paths = ["/tmp/allowed"]` could read
  files under that directory but not the directory itself. A tool
  that tried to stat, list, or read the allowed path directly got a
  permission error — surprising, and hard to diagnose because the
  files inside worked.
- A user who wrote `forbidden_paths = ["/tmp/secrets"]` could still
  read the `/tmp/secrets` directory (its own contents): only the
  children were blocked. The natural reading of "forbid this
  directory" is that the directory itself is off limits too.

Rebuild matches_pattern to try both the literal pattern and
`pattern/**`. Patterns that already contain `*`, `?`, or `[` are
honored verbatim: a user who wrote `/tmp/*` meant exactly that set,
and appending `/**` would broaden it to `/tmp/*/**` — the opposite
of the intent.

An invalid glob matches nothing (returns false), so a permission
that names a malformed pattern does not silently allow or forbid
everything. Same contract as before for the failure case.

Extends test_forbidden_path with the two boundary cases (allowed
directory itself readable; forbidden directory itself rejected) and
adds test_wildcard_pattern_is_verbatim, which pins that `/tmp/*`
does not silently become `/tmp/*/**`.
MSG

git add -A
git commit -F /tmp/kod_commit_msg.txt
rm -f /tmp/kod_commit_msg.txt
