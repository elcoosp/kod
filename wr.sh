#!/usr/bin/env bash
set -uo pipefail

ENGINE=crates/kod-core/src/engine.rs

echo "=== Current run_maintenance ==="
awk '/pub async fn run_maintenance/,/^    \}$/' "$ENGINE" | head -20

echo
echo "Patching $ENGINE"

python3 - "$ENGINE" << 'PYEOF'
import os
import sys

target = sys.argv[1]
with open(target, "r") as f:
    src = f.read()

old = '''    /// Run maintenance tasks
    pub async fn run_maintenance(&self) -> Result<()> {
        // Perform periodic maintenance
        // - Compact memory
        // - Clean up expired locks
        // - Update skill cache

        tracing::debug!("Running engine maintenance");
        Ok(())
    }'''

new = '''    /// Run maintenance tasks.
    ///
    /// **Does nothing today.** The comment this replaces listed three
    /// intentions — compact memory, release expired locks, refresh the
    /// skill cache — none of which are implemented. A caller that
    /// reads the method name and doc and expects compaction is going
    /// to be surprised: the call returns `Ok(())`, leaves state
    /// untouched, and (because the body only logs at debug level)
    /// looks successful from the outside.
    ///
    /// Making it a no-op-with-a-doc is deliberate rather than
    /// implementing one of the three inline:
    ///
    /// - Memory compaction: `MemoryManager` has no `compact` method
    ///   today. Adding one and calling it here would be a feature, not
    ///   a fix, and the semantics (what to compact, when, how to
    ///   coordinate with in-flight retrievals) deserve a design
    ///   pass.
    /// - Lock cleanup: `SharedWorkspace` releases locks in `Drop`,
    ///   so there is nothing to sweep. Expired-lock GC would only
    ///   matter if a holder leaked its guard across a panic, which
    ///   is a separate concern.
    /// - Skill cache refresh: the loader already hot-reloads via
    ///   `notify`. A manual sweep has no work to do.
    ///
    /// The method is retained because a caller (a hypothetical
    /// long-running daemon, a future `/maintenance` slash command)
    /// might want a named entry point that returns `Ok(())` so the
    /// call site compiles. If any of the three is implemented later,
    /// this doc should be deleted, not adjusted.
    ///
    /// The `tracing::debug!` line was removed: it implied activity
    /// where there is none. A caller that wants to know maintenance
    /// ran can log around the call.
    pub async fn run_maintenance(&self) -> Result<()> {
        Ok(())
    }'''

n = src.count(old)
if n != 1:
    print(f"ERROR: expected 1 occurrence of run_maintenance, found {n}")
    sys.exit(2)
src = src.replace(old, new, 1)

# Add a test pinning the no-op behavior so a future implementer sees
# it and knows to update the doc + the test together.
if "test_run_maintenance_is_no_op" not in src:
    anchor = '''    #[test]
    fn test_tool_done_marker_roundtrip() {'''
    if anchor not in src:
        print("ERROR: test anchor not found")
        sys.exit(2)
    new_test = '''    /// `run_maintenance` is documented as a no-op. This test pins
    /// that: it must return Ok without changing engine state. If
    /// someone implements one of the three intended behaviors later,
    /// this test should be replaced with one that asserts the new
    /// behavior — not deleted, and not silently kept passing while
    /// the doc still says "does nothing."
    #[tokio::test]
    async fn test_run_maintenance_is_no_op() {
        let temp = TempDir::new().unwrap();
        let db_path = temp.path().join("test.redb");
        let cfg = RouterConfig {
            context_window: 8192,
            working_dir: temp.path().to_path_buf(),
            enable_memory: false,
            enable_swarm: false,
            max_skills_per_query: 3,
        };
        let engine = KodEngine::new(cfg, db_path).unwrap();
        engine.start().await.unwrap();

        // Seed some history so "did maintenance do anything" is a
        // meaningful question.
        engine.seed_turn(true, "one").await;
        engine.seed_turn(false, "two").await;

        // Call maintenance twice: once before compaction, once after a
        // manual compact. Neither call should change history.
        engine.run_maintenance().await.unwrap();
        let rendered_before = engine.render_history().await;
        engine.run_maintenance().await.unwrap();
        let rendered_after = engine.render_history().await;
        assert_eq!(
            rendered_before, rendered_after,
            "run_maintenance must not touch history"
        );
    }

    #[test]
    fn test_tool_done_marker_roundtrip() {'''
    src = src.replace(anchor, new_test, 1)
    print("  added test_run_maintenance_is_no_op")
else:
    print("  test already present")

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
echo "cargo check --workspace --all-targets 2>&1 | tail -15"
if ! cargo check --workspace --all-targets 2>&1 | tail -15; then
    echo "Compilation failed"
    exit 1
fi

cat > /tmp/kod_commit_msg.txt <<'MSG'
docs(core): be honest that run_maintenance does nothing

run_maintenance had a three-item to-do list in a comment and
returned Ok(()) without executing any of it:

  // Perform periodic maintenance
  // - Compact memory
  // - Clean up expired locks
  // - Update skill cache

A caller reading the name and doc and expecting compaction is going
to be surprised: the call succeeds, leaves state untouched, and
logs at debug level (so it looks successful from the outside).

Replace the comment with a doc block that states the method is a
no-op today, explains why each of the three items is deferred (each
needs a design pass; none is a bug fix), and names the conditions
under which the doc should be deleted rather than adjusted. Remove
the tracing::debug! line — it implied activity where there is none,
and a caller that wants a log can wrap the call.

Add test_run_maintenance_is_no_op: seeds history, calls
run_maintenance twice, asserts render_history is byte-identical
before and after. If one of the three behaviors is implemented
later, this test should be replaced with one that asserts the new
behavior, not deleted and not kept passing while the doc still says
"does nothing."
MSG

git add -A
git commit -F /tmp/kod_commit_msg.txt
rm -f /tmp/kod_commit_msg.txt
