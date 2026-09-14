#!/usr/bin/env bash
set -uo pipefail

COMPILE_OK=true
INCOMPLETE=false
TARGET=crates/kod-core/src/router.rs

if [ ! -f Cargo.toml ] || [ ! -f "$TARGET" ]; then
    echo "ERROR: run from the kod workspace root ($TARGET missing)"
    exit 1
fi

echo "Diagnosing current classify_task ordering in $TARGET"
echo "Failing tests expected:"
echo "  test_classify_multi_step -> Complex (currently Testing)"
echo "  test_classify_research   -> Research (currently Documentation)"
echo "Fixing: move Complex before Testing, move Research before Documentation"

python3 - "$TARGET" << 'PYEOF'
import os
import sys

target = sys.argv[1]

with open(target, "r") as f:
    content = f.read()

start_marker = "        // 1. Debugging"
end_marker = "        // Default to simple"

start = content.find(start_marker)
if start == -1:
    print("ERROR: start marker not found (// 1. Debugging)")
    sys.exit(2)

end = content.find(end_marker, start)
if end == -1:
    print("ERROR: end marker not found (// Default to simple)")
    sys.exit(2)

new_block = '''        // Priority order (first match wins):
        //   1. Debugging   -- most specific failure vocabulary
        //   2. CodeMod     -- surgical action verbs
        //   3. Complex     -- broad planning verbs; a "design/build" request
        //                     that also mentions tests is still Complex
        //   4. Research    -- "investigate/find/search"; outranks docs because
        //                     "research the docs" is a research task
        //   5. Testing     -- specific testing verbs
        //   6. Documentation -- "document/readme/comment"; weakest signal
        //                       (comments show up in code snippets)
        //   7. Simple      -- default
        // Whole-word matching (not substring): "prefix" does not match
        // "fix", "remove" does not match "move", "testify" does not match
        // "test".

        // 1. Debugging
        if ["debug", "error", "traceback", "panic", "exception"]
            .iter()
            .copied()
            .any(|w| contains_word(&input_lower, w))
        {
            return Ok(TaskType::Debugging);
        }

        // 2. Code modification
        if [
            "refactor", "fix", "rename", "move", "extract", "inline", "modify", "update",
        ]
        .iter()
        .copied()
        .any(|w| contains_word(&input_lower, w))
        {
            return Ok(TaskType::CodeModification);
        }

        // 3. Complex -- broad planning verbs.
        if [
            "design",
            "architect",
            "implement",
            "create",
            "build",
            "complete",
            "analyze",
        ]
        .iter()
        .copied()
        .any(|w| contains_word(&input_lower, w))
        {
            return Ok(TaskType::Complex);
        }

        // 4. Research
        if ["research", "find", "search", "investigate", "look up"]
            .iter()
            .copied()
            .any(|w| contains_word(&input_lower, w))
        {
            return Ok(TaskType::Research);
        }

        // 5. Testing
        if ["test", "tests", "testing", "verify"]
            .iter()
            .copied()
            .any(|w| contains_word(&input_lower, w))
        {
            return Ok(TaskType::Testing);
        }

        // 6. Documentation
        if [
            "document", "documentation", "docs", "readme", "comment", "comments",
        ]
        .iter()
        .copied()
        .any(|w| contains_word(&input_lower, w))
        {
            return Ok(TaskType::Documentation);
        }

'''

patched = content[:start] + new_block + content[end:]

tmp = target + ".tmp"
with open(tmp, "w") as f:
    f.write(patched)
os.replace(tmp, target)
print("Reordered classify_task priority chain:", target)
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

echo "Running router tests only first"
if ! cargo test -p kod-core --test router 2>&1; then
    echo "Router tests failed. Paste the full output to get a surgical fix."
    exit 1
fi

echo "Running full workspace tests"
if ! cargo test --workspace 2>&1; then
    echo "Workspace tests failed. Paste the full output to get a surgical fix."
    exit 1
fi

echo "All tests passed. Committing."
git add -A
git commit -m "fix(router): order classification so multi-step and research win correctly

The previous reorder (specific intents before the broad Complex
bucket) broke two tests whose inputs deliberately contain keywords
from two categories at once:

  test_classify_multi_step: 'design/build ... test ...' expected
  Complex but got Testing, because Testing was checked before
  Complex.

  test_classify_research: 'research ... docs ...' expected Research
  but got Documentation, because Documentation was checked before
  Research.

Correct priority (verified by the existing tests):
  Debugging > CodeModification > Complex > Research > Testing
  > Documentation > Simple

Whole-word matching is kept from the previous patch: 'prefix' no
longer matches 'fix', 'remove' no longer matches 'move', 'testify'
no longer matches 'test'."
