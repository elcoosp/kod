#!/usr/bin/env bash
set -uo pipefail

COMPILE_OK=true
INCOMPLETE=false
TARGET=crates/kod-core/src/engine.rs

if [ ! -f Cargo.toml ] || [ ! -f "$TARGET" ]; then
    echo "ERROR: run from the kod workspace root ($TARGET missing)"
    exit 1
fi

echo "Patching $TARGET: derive mid-codepoint offset from MAX_TURN_CHARS"

python3 - "$TARGET" << 'PYEOF'
import os
import sys

target = sys.argv[1]
with open(target, "r") as f:
    content = f.read()

old = '''    /// record_turn runs on both sides of every prompt. A turn longer
    /// than MAX_TURN_CHARS whose 1500th byte falls inside a multibyte
    /// codepoint used to panic and abort the whole loop.
    #[tokio::test]
    async fn test_record_turn_does_not_panic_mid_multibyte() {
        let temp = TempDir::new().unwrap();
        let db_path = temp.path().join("test.redb");
        let cfg = RouterConfig {
            working_dir: temp.path().to_path_buf(),
            enable_memory: false,
            enable_swarm: false,
            max_skills_per_query: 3,
        };
        let engine = KodEngine::new(cfg, db_path).unwrap();
        engine.start().await.unwrap();

        // 1499 ASCII bytes, then 'é' (2 bytes) so byte offset 1500 is
        // the middle of the codepoint, then more content to exceed the
        // cap. MAX_TURN_CHARS is 1500.
        let mut prompt = "a".repeat(1499);
        prompt.push('é');
        prompt.push_str(&"x".repeat(100));
        assert!(prompt.len() > 1500);
        assert!(!prompt.is_char_boundary(1500));

        // Must not panic. The stored text ends at the last safe boundary
        // before the é, with the truncation marker appended.
        engine.record_turn(true, &prompt).await;

        let rendered = engine.render_history().await;
        assert!(rendered.contains("User:"), "history should carry the turn");
        assert!(
            rendered.contains("[truncated]"),
            "history should mark truncation"
        );
    }'''

new = '''    /// record_turn runs on both sides of every prompt. A turn longer
    /// than MAX_TURN_CHARS whose boundary byte falls inside a multibyte
    /// codepoint used to panic and abort the whole loop.
    ///
    /// The boundary offset is derived from `MAX_TURN_CHARS` rather than
    /// hardcoded, so raising the cap in the future does not silently
    /// turn this test into a no-op.
    #[tokio::test]
    async fn test_record_turn_does_not_panic_mid_multibyte() {
        let temp = TempDir::new().unwrap();
        let db_path = temp.path().join("test.redb");
        let cfg = RouterConfig {
            working_dir: temp.path().to_path_buf(),
            enable_memory: false,
            enable_swarm: false,
            max_skills_per_query: 3,
        };
        let engine = KodEngine::new(cfg, db_path).unwrap();
        engine.start().await.unwrap();

        // MAX_TURN_CHARS - 1 ASCII bytes, then 'é' (2 bytes) so byte
        // offset MAX_TURN_CHARS is the middle of the codepoint, then
        // enough extra content to exceed the cap and force truncation.
        let mut prompt = "a".repeat(MAX_TURN_CHARS - 1);
        prompt.push('é');
        prompt.push_str(&"x".repeat(100));
        assert!(prompt.len() > MAX_TURN_CHARS);
        assert!(
            !prompt.is_char_boundary(MAX_TURN_CHARS),
            "the test must place the cap inside the é codepoint; \\
             MAX_TURN_CHARS={} fell on a boundary",
            MAX_TURN_CHARS
        );

        // Must not panic. The stored text ends at the last safe boundary
        // before the é, with the truncation marker appended.
        engine.record_turn(true, &prompt).await;

        let rendered = engine.render_history().await;
        assert!(rendered.contains("User:"), "history should carry the turn");
        assert!(
            rendered.contains("[truncated]"),
            "history should mark truncation"
        );
    }'''

n = content.count(old)
if n != 1:
    print(f"ERROR: expected 1 occurrence of the test, found {n}")
    sys.exit(2)
content = content.replace(old, new, 1)

tmp = target + ".tmp"
with open(tmp, "w") as f:
    f.write(content)
os.replace(tmp, target)
print("Patched", target)
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

echo "Running kod-core tests"
if ! cargo test -p kod-core 2>&1; then
    echo "kod-core tests failed. Paste the full output for a surgical fix."
    exit 1
fi

echo "Running workspace tests"
if ! cargo test --workspace 2>&1; then
    echo "Workspace tests failed. Paste the full output for a surgical fix."
    exit 1
fi

echo "Running clippy with -D warnings"
if ! cargo clippy --workspace --all-targets -- -D warnings 2>&1; then
    echo "Clippy failed. Paste the full output for a surgical fix."
    exit 1
fi

echo "All checks passed. Committing."
git add -A
git commit -m "test(core): derive record_turn boundary from MAX_TURN_CHARS

test_record_turn_does_not_panic_mid_multibyte hardcoded byte offset
1500, which was MAX_TURN_CHARS when the test was written. The
history-budget change bumped MAX_TURN_CHARS to 4000, so the test's
1601-byte prompt no longer exceeded the cap, record_turn skipped
truncation, and the '[truncated]' assertion failed — a false
negative that looked like a real regression.

Place the mid-codepoint byte at MAX_TURN_CHARS - 1 and assert
!is_char_boundary(MAX_TURN_CHARS), so the test tracks whichever
value the cap holds and cannot silently become a no-op on the next
bump. The explicit is_char_boundary assertion also names the case
that would make the test meaningless."
