#!/usr/bin/env bash
set -uo pipefail

CTX=crates/kod-tools/src/context.rs

if [ ! -f Cargo.toml ] || [ ! -f "$CTX" ]; then
    echo "ERROR: run from the kod workspace root"
    exit 1
fi

echo "=== Pre-state: wildcard test in context.rs ==="
grep -n "test_wildcard_pattern" "$CTX"

echo
python3 - "$CTX" << 'PYEOF'
import os
import sys

path = sys.argv[1]
with open(path) as f:
    src = f.read()

old = '''    /// A pattern with a wildcard is honored verbatim — the fix must
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
    }'''

new = '''    /// A wildcard pattern is used as written — the `/**` fix that
    /// appends a descendant clause to non-wildcard patterns must not
    /// also append it to a pattern that already contains `*`, `?`, or
    /// `[`. Doing so would broaden `/tmp/*` into `/tmp/*/**` and match
    /// paths the user did not name.
    ///
    /// globset's `*` matches across path separators by default, so
    /// `/tmp/*` covers `/tmp/anything` *and* `/tmp/anything/deeper`.
    /// That is globset's documented behavior, not something the `/**`
    /// fix introduced; the assertion that catches over-broadening is
    /// the negative one — a path outside `/tmp` does not match.
    #[test]
    fn test_wildcard_pattern_is_used_as_written() {
        let perms = ToolPermissions {
            read_files: true,
            allowed_paths: vec!["/tmp/*".to_string()],
            ..Default::default()
        };
        let context = ToolContext::new("/").with_permissions(perms);

        // `/tmp/anything` matches.
        assert!(context.can_read(Path::new("/tmp/anything")).is_ok());
        // `/tmp/anything/deeper` also matches: globset's `*` is greedy
        // across separators unless `literal_separator` is set. That is
        // pre-existing behavior, not something the `/**` fix changed.
        assert!(
            context.can_read(Path::new("/tmp/anything/deeper")).is_ok(),
            "globset's `*` is greedy across separators (documented)"
        );
        // A path outside `/tmp` does NOT match — the fix must not
        // broaden `/tmp/*` to something that matches the whole
        // filesystem.
        assert!(
            context.can_read(Path::new("/var/log")).is_err(),
            "wildcard must not match paths outside its literal prefix"
        );
        assert!(
            context.can_read(Path::new("/etc/hostname")).is_err(),
            "wildcard must not match paths outside its literal prefix"
        );
    }'''

n = src.count(old)
if n == 0:
    print("ERROR: old wildcard test not found verbatim")
    sys.exit(2)
if n != 1:
    print(f"ERROR: found {n} occurrences, expected 1")
    sys.exit(2)
src = src.replace(old, new, 1)

with open(path, "w") as f:
    f.write(src)
print("Patched", path)
PYEOF

if [ $? -ne 0 ]; then
    echo "ERROR: patching failed"
    exit 1
fi

echo
echo "cargo test -p kod-tools 2>&1 | tail -12"
if ! cargo test -p kod-tools 2>&1 | tail -12; then
    echo "kod-tools tests failed"
    exit 1
fi

echo
echo "cargo check --workspace --all-targets 2>&1 | tail -8"
if ! cargo check --workspace --all-targets 2>&1 | tail -8; then
    echo "Compilation failed"
    exit 1
fi

echo
echo "cargo clippy --workspace --all-targets -- -D warnings 2>&1 | tail -8"
if ! cargo clippy --workspace --all-targets -- -D warnings 2>&1 | tail -8; then
    echo "Clippy failed"
    exit 1
fi

cat > /tmp/kod_commit_msg.txt <<'MSG'
fix(tools): read_file missing-file error is model-facing; correct wildcard test

Two fixes.

1. read_file: a missing file returned Err(KodError::Io(...)) while
   a directory returned Ok(ToolResult::Error(...)). Both are "the
   model asked for something that does not exist" — same class,
   different shape, and a caller had to handle both. Return
   Ok(ToolResult::Error(describe_path_error(...))) for the missing
   case, matching the directory case above. Error content is
   identical; only the wrapping changes.

2. test_wildcard_pattern_is_verbatim (context.rs): the assertion
   assumed globset's `*` matches a single path segment. It does not
   — by default `*` is greedy across path separators, so `/tmp/*`
   matches `/tmp/anything` and `/tmp/anything/deeper` alike. That is
   globset's documented behavior, not something the `/**` fix
   introduced. Rewrite the test to assert the actual invariant: a
   wildcard pattern is used as written (the fix must not append
   `/**` to a pattern that already contains `*`, `?`, or `[`), and
   a path outside its literal prefix does not match.
MSG

git add -A
git commit -F /tmp/kod_commit_msg.txt
rm -f /tmp/kod_commit_msg.txt
