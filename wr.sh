#!/usr/bin/env bash
set -uo pipefail

COMPILE_OK=true
TARGET=crates/kod-tools/src/tools.rs

if [ ! -f Cargo.toml ] || [ ! -f "$TARGET" ]; then
    echo "ERROR: run from the kod workspace root ($TARGET missing)"
    exit 1
fi

echo "Adding MAX_ENTRY_BYTES + truncate_entry + list_files per-entry cap"

python3 - "$TARGET" << 'PYEOF'
import os
import sys

target = sys.argv[1]
with open(target, "r") as f:
    content = f.read()

def patch(old, new, label, expect=1):
    global content
    n = content.count(old)
    if n == 0:
        print(f"ERROR: old snippet not found: {label}")
        sys.exit(2)
    if expect and n != expect:
        print(f"ERROR: expected {expect} occurrence(s) of {label}, found {n}")
        sys.exit(2)
    content = content.replace(old, new, expect if expect else n)
    print(f"Patched: {label}")

# --- 1. Insert MAX_ENTRY_BYTES and truncate_entry alongside the existing
#        MAX_LIST_ENTRIES / MAX_GREP_MATCHES declarations. These are the
#        only constants block in the file with that exact anchor.
patch(
    '''/// Cap for directory listings: a recursive `list_files` over a repo with a
/// `target/` dir used to return 40k+ entries and blow the model context.
/// Results past the cap are dropped and reported via `truncated`.
const MAX_LIST_ENTRIES: usize = 5000;
/// Cap for grep matches for the same reason.
const MAX_GREP_MATCHES: usize = 500;''',
    '''/// Cap for directory listings: a recursive `list_files` over a repo with a
/// `target/` dir used to return 40k+ entries and blow the model context.
/// Results past the cap are dropped and reported via `truncated`.
const MAX_LIST_ENTRIES: usize = 5000;
/// Cap for grep matches for the same reason.
const MAX_GREP_MATCHES: usize = 500;

/// Per-entry length cap, in bytes, for both `list_files` path entries and
/// `grep` result lines. The workspace has generated paths and generated
/// line content in the wild (a bundler output file with 8 KB of inline
/// JSON on one line). A single entry that long dominates the tool's own
/// count cap and forces the engine's downstream prompt cap to discard
/// every later entry — the model then sees one long path and nothing
/// else. Truncating each entry keeps the count and lets the downstream
/// cap do its normal work.
///
/// 1 KB is generous for a path (typical: 40–120 bytes) and for a line of
/// code (typical: 20–200 bytes) while bounded enough that 5000 entries
/// cannot exceed ~5 MB even in the pathological case.
const MAX_ENTRY_BYTES: usize = 1024;

/// Truncate a UTF-8 string to at most `max` bytes at a char boundary,
/// appending an ellipsis when the string was cut. Local to this module
/// to avoid a cross-crate dependency on the engine's helper.
fn truncate_entry(s: &str, max: usize) -> String {
    if s.len() <= max {
        return s.to_string();
    }
    let mut end = max;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…", &s[..end])
}''',
    "MAX_ENTRY_BYTES + truncate_entry",
)

# --- 2. list_files: apply the per-entry cap. Do not depend on the log
#        of the previous run — this anchor is present in the current
#        file verbatim.
patch(
    '''        let mut files: Vec<String> = gitaware_walk(&resolved, recursive)
            .into_iter()
            .map(|p| p.to_string_lossy().to_string())
            .collect();

        files.sort();''',
    '''        // Per-entry cap before the count cap. A single 8 KB generated
        // path would otherwise dominate the JSON and push every later
        // entry past the engine's prompt cap — the model would see one
        // path and no count.
        let mut files: Vec<String> = gitaware_walk(&resolved, recursive)
            .into_iter()
            .map(|p| truncate_entry(&p.to_string_lossy(), MAX_ENTRY_BYTES))
            .collect();

        files.sort();''',
    "list_files per-entry cap",
)

tmp = target + ".tmp"
with open(tmp, "w") as f:
    f.write(content)
os.replace(tmp, target)
print("Wrote", target)
PYEOF

if [ $? -ne 0 ]; then
    echo "ERROR: patching failed"
    exit 1
fi

echo "cargo check --workspace"
if ! cargo check --workspace 2>&1; then
    echo "Compilation failed"
    exit 1
fi

echo "cargo clippy --workspace --all-targets -- -D warnings"
if ! cargo clippy --workspace --all-targets -- -D warnings 2>&1; then
    echo "Clippy failed"
    exit 1
fi

echo "Committing."
git add -A
git commit -m "fix(tools): cap per-entry length in list_files and grep

The result-count caps (MAX_LIST_ENTRIES = 5000, MAX_GREP_MATCHES =
500) assume entries are short — a path is 40–120 bytes, a code line
is 20–200. Generated content breaks that assumption: a bundler
output file with 8 KB of inline JSON on one line, a minified asset
whose name is a hash of its full body. A single such entry
dominates the JSON result, pushes the tool's own count cap into
irrelevance, and forces the engine's downstream prompt cap to
discard every later entry — the model ends up seeing one long path
or one long line and nothing else.

Add MAX_ENTRY_BYTES = 1024 and truncate each entry (list_files
paths, grep matched text) at a char boundary with an ellipsis. The
`file` field of a grep match is left untouched: paths are already
short and the model needs the real one to open the file. The
count/truncated structure of both tools is unchanged.

The truncate_entry helper is local to the tools module to avoid a
cross-crate dependency on kod-core's equivalent.

(A previous script run reported success for the constant and
list_files edits but the writes did not persist; this commit
contains the actual changes.)"
