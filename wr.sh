#!/usr/bin/env bash
set -uo pipefail

COMPILE_OK=true
INCOMPLETE=false
CTX=crates/kod-tools/src/context.rs
TOOLS=crates/kod-tools/src/tools.rs

if [ ! -f Cargo.toml ] || [ ! -f "$CTX" ] || [ ! -f "$TOOLS" ]; then
    echo "ERROR: run from the kod workspace root ($CTX or $TOOLS missing)"
    exit 1
fi

echo "Patching $CTX and $TOOLS: enforce working-dir containment in resolve_path"

python3 - "$CTX" "$TOOLS" << 'PYEOF'
import os
import sys

ctx_path, tools_path = sys.argv[1], sys.argv[2]

def patch(path, old, new, label, count=1):
    with open(path, "r") as f:
        content = f.read()
    n = content.count(old)
    if n == 0:
        print(f"ERROR: old snippet not found in {path}: {label}")
        sys.exit(2)
    if count == 1 and n != 1:
        print(f"ERROR: expected 1 occurrence of {label} in {path}, found {n}")
        sys.exit(2)
    patched = content.replace(old, new, count)
    tmp = path + ".tmp"
    with open(tmp, "w") as f:
        f.write(patched)
    os.replace(tmp, path)
    print(f"Patched {path}: {label} ({n} occurrence(s))")

# --- Patch 1: resolve_path method in context.rs --------------------------
old_resolve = '''    /// Resolve a path relative to working directory
    pub fn resolve_path(&self, path: &str) -> PathBuf {
        let path = Path::new(path);

        if path.is_absolute() {
            path.to_path_buf()
        } else {
            self.working_dir.join(path)
        }
    }'''

new_resolve = '''    /// Resolve `path` relative to the working directory, canonicalize it,
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
    }'''

patch(ctx_path, old_resolve, new_resolve, "resolve_path method")

# --- Patch 2: test_resolve_path in context.rs ----------------------------
old_test = '''    #[test]
    fn test_resolve_path() {
        let context = ToolContext::new("/tmp");

        let resolved = context.resolve_path("/abs/path");
        assert_eq!(resolved, PathBuf::from("/abs/path"));

        let resolved = context.resolve_path("relative/path");
        assert_eq!(resolved, PathBuf::from("/tmp/relative/path"));
    }'''

new_test = '''    #[test]
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
    }'''

patch(ctx_path, old_test, new_test, "test_resolve_path")

# --- Patch 3: append traversal rejection test to context.rs tests --------
old_tail = '''        // Forbidden path
        let result = context.can_read(Path::new("/tmp/allowed/forbidden/secret.txt"));
        assert!(result.is_err());
    }
}'''

new_tail = '''        // Forbidden path
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
}'''

patch(ctx_path, old_tail, new_tail, "traversal rejection test")

# --- Patch 4: add `?` at all 5 resolve_path call sites in tools.rs -------
old_call = "let resolved = context.resolve_path(path);"
new_call = "let resolved = context.resolve_path(path)?;"
with open(tools_path, "r") as f:
    content = f.read()
n = content.count(old_call)
if n != 5:
    print(f"ERROR: expected 5 resolve_path call sites in {tools_path}, found {n}")
    sys.exit(2)
patched = content.replace(old_call, new_call)
tmp = tools_path + ".tmp"
with open(tmp, "w") as f:
    f.write(patched)
os.replace(tmp, tools_path)
print(f"Patched {tools_path}: 5 resolve_path call sites now propagate errors")

print("All patches applied.")
PYEOF

if [ $? -ne 0 ]; then
    echo "ERROR: patching failed"
    exit 1
fi

echo "Checking compilation"
if ! cargo check --workspace 2>&1; then
    echo "Compilation failed – will skip commit"
    COMPILE_OK=false
fi

if [ "$INCOMPLETE" = true ] || [ "$COMPILE_OK" = false ]; then
    echo "Skipping tests and commit due to incomplete files or compilation errors"
    exit 1
fi

echo "Running tests"
cargo test -p kod-tools
if [ $? -eq 0 ]; then
    echo "All tests passed. Committing."
    git add -A
    git commit -m "fix(tools): reject path traversal in ToolContext::resolve_path

ToolContext::resolve_path previously did a plain working_dir.join(path)
with no canonicalization. is_path_allowed then returned Ok(true)
whenever allowed_paths was empty, which is exactly what KodEngine
configures. Net effect: read_file { path: '../../etc/passwd' } and
write_file { path: '../../../tmp/x' } succeeded regardless of the
sandbox flags.

resolve_path now canonicalizes the target (falling back to
canonicalizing the parent for files that do not exist yet, so
write_file create still works) and refuses anything whose canonical
form does not start with the canonical working directory.

All five built-in tools (read_file, write_file, list_files, grep,
file_info) propagate the new Result. Adds two tests: relative and
absolute in-root paths succeed, traversal and absolute out-of-root
paths are rejected."
else
    echo "Tests failed. Fix errors then run the next script."
    exit 1
fi
