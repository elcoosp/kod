#!/usr/bin/env bash
set -uo pipefail

COMPILE_OK=true

echo "Scanning workspace for all RouterConfig literals"

FILES=$(grep -rl --include='*.rs' 'RouterConfig {' crates/ \
    | grep -v -E '(^|/)target/' \
    | sort -u)
echo "$FILES"

python3 - $FILES << 'PYEOF'
import re
import sys

files = sys.argv[1:]
total = 0
for path in files:
    with open(path, "r") as f:
        src = f.read()
    if "RouterConfig {" not in src:
        continue
    out = []
    i = 0
    added = 0
    while True:
        idx = src.find("RouterConfig {", i)
        if idx == -1:
            out.append(src[i:])
            break
        end_open = idx + len("RouterConfig {")
        out.append(src[i:end_open])
        nl = src.find("\n", end_open)
        if nl == -1:
            out.append(src[end_open:])
            break
        out.append(src[end_open:nl + 1])
        rest = src[nl + 1:]
        m = re.match(r"([ \t]*)\S", rest)
        indent = m.group(1) if m else "    "
        # Find matching close brace to know if context_window is present.
        depth = 1
        j = nl + 1
        while j < len(src) and depth > 0:
            c = src[j]
            if c == "{":
                depth += 1
            elif c == "}":
                depth -= 1
            j += 1
        body = src[nl + 1:j - 1] if depth == 0 else src[nl + 1:]
        if "context_window" in body:
            out.append(src[nl + 1:j] if depth == 0 else src[nl + 1:])
            i = j if depth == 0 else len(src)
            if depth == 0:
                continue
            break
        out.append(f"{indent}context_window: 8192,\n")
        added += 1
        i = nl + 1
    if added:
        with open(path, "w") as f:
            f.write("".join(out))
    print(f"{path}: +{added}")
    total += added

print(f"Total added: {total}")
PYEOF

if [ $? -ne 0 ]; then
    echo "ERROR: patching failed"
    exit 1
fi

echo "cargo check --workspace --all-targets"
if ! cargo check --workspace --all-targets 2>&1; then
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
git commit -m "fix(tests): add context_window to remaining RouterConfig literals

The prior pass covered kod-core's lib, benches, and tests but missed
the four literals in kod-cli/tests/integration_tests.rs. Scan the
whole workspace for \\`RouterConfig {\\` this time so no future
literals are missed."
