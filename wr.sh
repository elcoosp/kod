#!/usr/bin/env bash
set -uo pipefail

ROUTER=crates/kod-core/src/router.rs

if [ ! -f Cargo.toml ] || [ ! -f "$ROUTER" ]; then
    echo "ERROR: run from the kod workspace root ($ROUTER missing)"
    exit 1
fi

echo "=== Pre-state ==="
grep -n "memory_used\|swarm_used" "$ROUTER"

echo
echo "Patching router.rs (memory_used + swarm_used together)"

python3 - "$ROUTER" << 'PYEOF'
import os
import re
import sys

target = sys.argv[1]
with open(target, "r") as f:
    src = f.read()

def patch(old, new, label, expect=1):
    global src
    n = src.count(old)
    if n == 0:
        print(f"  MISS: {label}")
        return False
    if expect and n != expect:
        print(f"  ERROR: expected {expect} occurrence(s) of {label}, found {n}")
        sys.exit(2)
    src = src.replace(old, new, expect if expect else n)
    print(f"  patched: {label}")
    return True

# ----------------------------------------------------------------------
# 1. TaskResponse struct doc for swarm_used. Anchor on the struct
#    definition field list; if already documented, skip.
# ----------------------------------------------------------------------
if "Always `false` today. The swarm is registered" not in src:
    patch(
        '''    pub skills_used: Vec<String>,
    pub memory_used: bool,
    pub swarm_used: bool,
    pub execution_time_ms: u64,
    pub usage: Option<kod_provider::TokenUsage>,
}''',
        '''    /// Names of the skills whose instructions were injected into the
    /// prompt. Empty when no skill matched.
    pub skills_used: Vec<String>,
    /// True iff at least one memory entry was included in the prompt.
    pub memory_used: bool,
    /// True iff the swarm handled part of this task.
    ///
    /// Always `false` today. The swarm is registered in the router
    /// (when `enable_swarm` is set) but `handle_complex` returns a
    /// placeholder string rather than routing the task to any agent —
    /// so nothing has ever been dispatched through the swarm, and the
    /// field cannot honestly be `true`. The field is kept so the
    /// response shape is stable for the swarm-dispatch implementation,
    /// but it does not currently carry a signal.
    ///
    /// The previous computation — `matches!(task_type, Complex) &&
    /// self.swarm.is_some()` — was the same kind of tautology that
    /// `memory_used` used to be: a fact about the router's inputs
    /// (how it classified the task, whether it owns a swarm object)
    /// dressed up as a fact about what happened.
    pub swarm_used: bool,
    /// Wall-clock time from `process_input` entry to response.
    pub execution_time_ms: u64,
    /// Token usage the provider reported, when it did.
    pub usage: Option<kod_provider::TokenUsage>,
}''',
        "TaskResponse field docs",
    )
else:
    print("  TaskResponse doc already present")

# ----------------------------------------------------------------------
# 2. Insert `let memory_used = ...;` before the `Ok(TaskResponse {`
#    construction. Anchor on the skills_used line + the construction.
# ----------------------------------------------------------------------
if "let memory_used = memory_context" not in src:
    patch(
        '''        // 3. Find relevant skills
        let skills_used = self.find_relevant_skills(input).await?;''',
        '''        // 3. Find relevant skills
        let skills_used = self.find_relevant_skills(input).await?;

        // Did memory actually contribute to this prompt? The flag used
        // to be `memory_context.is_some()`, which is true whenever the
        // router has a manager — i.e. always, since enable_memory
        // defaults on. The observable meaning to a caller is "at least
        // one memory entry was included", and that is what this
        // reports.
        let memory_used = memory_context
            .as_ref()
            .map(|c| {
                !c.working_memory.is_empty()
                    || !c.long_term.is_empty()
                    || !c.episodic.is_empty()
            })
            .unwrap_or(false);''',
        "compute memory_used",
    )
else:
    print("  memory_used computation already present")

# ----------------------------------------------------------------------
# 3. Replace the response-construction fields. Anchor on the exact
#    block; the diagnostic confirmed this shape.
# ----------------------------------------------------------------------
patch(
    '''            skills_used,
            memory_used: memory_context.is_some(),
            swarm_used: matches!(task_type, TaskType::Complex | TaskType::MultiStep)
                && self.swarm.is_some(),
            execution_time_ms,''',
    '''            skills_used,
            memory_used,
            // No dispatch path uses the swarm today: `handle_complex`
            // returns a placeholder string, and nothing else consults
            // the router's `swarm` field for work routing. Report the
            // honest answer — the swarm did not handle this task.
            swarm_used: false,
            execution_time_ms,''',
    "response fields use computed values",
)

# ----------------------------------------------------------------------
# 4. Tests. Add both after the existing build_prompt test.
# ----------------------------------------------------------------------
if "test_memory_used_flag_reflects_contribution" not in src:
    anchor = '''    /// `build_prompt` must consult the memory manager. Regression:'''
    if anchor not in src:
        print("  ERROR: test anchor not found")
        sys.exit(2)
    new_tests = '''    /// `TaskResponse::memory_used` must be true only when a memory
    /// entry actually reached the prompt.
    #[tokio::test]
    async fn test_memory_used_flag_reflects_contribution() {
        let temp_dir = TempDir::new().unwrap();
        let db_path = temp_dir.path().join("test.redb");
        let router = TaskRouter::new(
            RouterConfig {
                enable_memory: true,
                enable_swarm: false,
                max_skills_per_query: 3,
                working_dir: temp_dir.path().to_path_buf(),
                context_window: 8192,
            },
            db_path,
        )
        .unwrap();

        // Fresh manager: no entries -> flag false.
        let resp = router
            .process_input("what is the meaning of life?")
            .await
            .unwrap();
        assert!(!resp.memory_used, "empty memory must not report as used");

        // Add a fact sharing a content word with the next prompt.
        let manager = router.memory_manager.as_ref().unwrap();
        manager
            .store(
                kod_types::MemoryType::LongTerm,
                "The project is called KOD.",
            )
            .await
            .unwrap();
        let resp = router
            .process_input("tell me about the project")
            .await
            .unwrap();
        assert!(resp.memory_used, "matching entry must report as used");

        // Non-overlapping prompt -> flag false again.
        let resp = router
            .process_input("xyzzy plugh frobnicate")
            .await
            .unwrap();
        assert!(!resp.memory_used, "non-matching prompt must be false");
    }

    /// `TaskResponse::swarm_used` must be false regardless of task
    /// classification or swarm configuration, since nothing dispatches
    /// to the swarm today.
    #[tokio::test]
    async fn test_swarm_used_is_honest() {
        let temp_dir = TempDir::new().unwrap();
        let db_path = temp_dir.path().join("test.redb");
        let router = TaskRouter::new(
            RouterConfig {
                enable_memory: false,
                enable_swarm: true,
                max_skills_per_query: 3,
                working_dir: temp_dir.path().to_path_buf(),
                context_window: 8192,
            },
            db_path,
        )
        .unwrap();

        // A Complex-classified input on a router with swarm enabled:
        // the old tautology reported true here.
        let resp = router
            .process_input("design a distributed job queue")
            .await
            .unwrap();
        assert_eq!(resp.task_type, TaskType::Complex);
        assert!(!resp.swarm_used, "swarm_used must be false: no dispatch path");

        let resp = router.process_input("2 + 2").await.unwrap();
        assert!(!resp.swarm_used);
    }

    /// `build_prompt` must consult the memory manager. Regression:'''
    src = src.replace(anchor, new_tests, 1)
    print("  added memory_used + swarm_used tests")
else:
    print("  tests already present")

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
echo "=== Post-state ==="
grep -n "memory_used\|swarm_used" "$ROUTER" | head -20

echo
echo "cargo check --workspace --all-targets 2>&1 | tail -12"
if ! cargo check --workspace --all-targets 2>&1 | tail -12; then
    echo "Compilation failed"
    exit 1
fi

echo
echo "Committing."
git add -A
git commit -F - <<'MSG'
fix(core): memory_used and swarm_used report facts, not tautologies

Both fields on TaskResponse read as observables ("this prompt was
informed by memory", "the swarm handled this task") but were
computed from the router's inputs.

- memory_used was `memory_context.is_some()`, which is true
  whenever the router has a manager — always, since enable_memory
  defaults on. A caller reading the flag saw "yes" for every prompt
  regardless of whether any entry was retrieved. Compute it from
  the retrieved context: true iff working_memory, long_term, or
  episodic is non-empty.

- swarm_used was `matches!(task_type, Complex | MultiStep) &&
  self.swarm.is_some()`. That is "classified as Complex and a
  swarm object exists" — a fact about the router's configuration.
  handle_complex returns a placeholder string and does not route to
  any agent, so no task has ever been dispatched through the
  swarm; the field could not honestly be true. Set it to false and
  document the field as a placeholder for the swarm-dispatch
  implementation.

The struct's doc block now explains what each flag actually means
so the next reader does not have to reverse-engineer it from the
computation.

Adds two tests: test_memory_used_flag_reflects_contribution stores
a fact, prompts with a matching word, and asserts the flag flips;
test_swarm_used_is_honest runs a Complex-classified prompt on a
router with enable_swarm=true and asserts the flag stays false.
MSG
