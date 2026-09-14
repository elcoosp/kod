#!/usr/bin/env bash
set -uo pipefail

ENGINE=crates/kod-core/src/engine.rs

echo "=== Diagnostic: no-provider fallbacks in engine.rs ==="
grep -n "No provider\|fall back to router\|no_provider_error" "$ENGINE" || echo "  none found"

echo
echo "=== Diagnostic: MAX_TOOL_ROUNDS ==="
grep -n "MAX_TOOL_ROUNDS" "$ENGINE" | head -5

echo
echo "=== Diagnostic: ground_prompt block name ==="
grep -n "## Tool result" "$ENGINE"

echo
echo "Patching $ENGINE"

python3 - "$ENGINE" << 'PYEOF'
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
# 1. Add no_provider_error helper. Place it right after set_provider.
# ----------------------------------------------------------------------
if "fn no_provider_error()" not in src:
    patch(
        '''    /// Set the LLM provider
    pub async fn set_provider(&self, provider: Arc<dyn LlmProvider>) {
        *self.provider.write().await = Some(provider);
    }''',
        '''    /// Set the LLM provider
    pub async fn set_provider(&self, provider: Arc<dyn LlmProvider>) {
        *self.provider.write().await = Some(provider);
    }

    /// Error for a `process*` call made before a provider is installed.
    ///
    /// The router has a set of placeholder handlers that return
    /// strings like "Processing simple task: …". Those exist so the
    /// router's own unit tests can exercise the classification path
    /// without a provider, and they are fine in that role. As a
    /// user-visible answer from the engine, though, they are worse
    /// than an error: the call looks like it succeeded, the reply
    /// advertises no fix, and the operator has to guess that the
    /// engine was never wired to a model.
    ///
    /// Callers that do want the placeholder behavior (the router's
    /// own tests) use `TaskRouter` directly. The engine tells the
    /// truth.
    fn no_provider_error() -> KodError {
        KodError::InvalidState(
            "No LLM provider configured. Install one with \\
             `engine.set_provider(Arc::new(provider))` before calling \\
             process — kod-cli and kod-tui do this automatically from \\
             ~/.config/kod/config.toml."
                .to_string(),
        )
    }''',
        "no_provider_error helper",
    )
else:
    print("  no_provider_error already present")

# ----------------------------------------------------------------------
# 2. process() fallback → error.
# ----------------------------------------------------------------------
patch(
    '''        // No provider — fall back to router's built-in handlers
        let response = self.router.process_input(input).await?;
        Ok(response)
    }

    /// Process user input, streaming text chunks live to `chunk_tx`.''',
    '''        // No provider. Reject rather than return the router's
        // placeholder text — see `no_provider_error`.
        Err(Self::no_provider_error())
    }

    /// Process user input, streaming text chunks live to `chunk_tx`.''',
    "process() no-provider path",
)

# ----------------------------------------------------------------------
# 3. process_streaming() fallback → error. Anchor on the
#    distinguishing nearby doc comment.
# ----------------------------------------------------------------------
patch(
    '''        let response = self.router.process_input(input).await?;
        Ok(response)
    }

    /// Work toward `goal` across turns until the model declares it met.''',
    '''        Err(Self::no_provider_error())
    }

    /// Work toward `goal` across turns until the model declares it met.''',
    "process_streaming() no-provider path",
)

# ----------------------------------------------------------------------
# 4. process_goal_streaming() fallback → error.
# ----------------------------------------------------------------------
patch(
    '''        let response = self.router.process_input(input).await?;
        Ok(response)
    }

    /// Collected (non-streaming) agentic loop used by [`process`].''',
    '''        Err(Self::no_provider_error())
    }

    /// Collected (non-streaming) agentic loop used by [`process`].''',
    "process_goal_streaming() no-provider path",
)

# ----------------------------------------------------------------------
# 5. Fix the "## Tool result" / "## Tool results" mismatch in the
#    grounding prompt.
# ----------------------------------------------------------------------
patch(
    '''Call them when you need facts from this machine instead of guessing. Tool outputs return as `## Tool result` blocks — then answer the user.\\n",''',
    '''Call them when you need facts from this machine instead of guessing. Tool outputs return as `## Tool results` blocks — then answer the user.\\n",''',
    "## Tool results (plural) in ground_prompt",
)

# ----------------------------------------------------------------------
# 6. Reduce MAX_TOOL_ROUNDS from 150 to 40.
# ----------------------------------------------------------------------
patch(
    '''/// Max agentic tool rounds per `process()` call before forcing a summary.
const MAX_TOOL_ROUNDS: usize = 150;''',
    '''/// Max agentic tool rounds per `process()` call before forcing a
/// summary. A single agentic pass typically uses 3–15 rounds for a
/// non-trivial task; 40 is a generous safety margin that catches a
/// runaway loop (a small model that keeps re-calling `read_file` on
/// the same path, unable to recognize it is done) well before the
/// user has waited minutes for nothing.
const MAX_TOOL_ROUNDS: usize = 40;''',
    "MAX_TOOL_ROUNDS 150 -> 40",
)

# ----------------------------------------------------------------------
# 7. Tests: no-provider rejection at all three entry points.
# ----------------------------------------------------------------------
if "test_process_without_provider_errors" not in src:
    anchor = '''    #[test]
    fn test_tool_done_marker_roundtrip() {'''
    if anchor not in src:
        print("  ERROR: test anchor not found")
        sys.exit(2)
    new_tests = '''    /// All three process* entry points must reject a call made
    /// before a provider is installed. Regression: the previous
    /// fallback routed through the router's placeholder handlers and
    /// returned "Processing simple task: …" — a plausible-looking
    /// answer that hid the missing setup.
    #[tokio::test]
    async fn test_process_without_provider_errors() {
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

        let err = engine.process("hello?").await.unwrap_err();
        match err {
            KodError::InvalidState(msg) => assert!(
                msg.contains("No LLM provider"),
                "error should name the missing provider: {msg}"
            ),
            other => panic!("expected InvalidState, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn test_process_streaming_without_provider_errors() {
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

        let (tx, _rx) = tokio::sync::mpsc::channel::<String>(4);
        let err = engine
            .process_streaming("hello?", &tx)
            .await
            .unwrap_err();
        assert!(matches!(err, KodError::InvalidState(_)), "got {err:?}");
    }

    #[tokio::test]
    async fn test_process_goal_streaming_without_provider_errors() {
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

        let (tx, _rx) = tokio::sync::mpsc::channel::<String>(4);
        let err = engine
            .process_goal_streaming("work on it", "finish the task", &tx)
            .await
            .unwrap_err();
        assert!(matches!(err, KodError::InvalidState(_)), "got {err:?}");
    }

    #[test]
    fn test_tool_done_marker_roundtrip() {'''
    src = src.replace(anchor, new_tests, 1)
    print("  added no-provider tests")
else:
    print("  no-provider tests already present")

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
fix(core): reject process* calls made without a provider; small fixes

Three small honesty fixes on the engine's front door.

1. process, process_streaming, and process_goal_streaming each fell
   through to TaskRouter::process_input when no provider was set.
   That path returns placeholder strings like "Processing simple
   task: <input>", which the engine then handed to the caller as if
   it were a successful answer. The placeholder handlers exist so
   the router's own unit tests can exercise classification without
   a provider; as a user-visible engine reply, they hid the missing
   setup and advertised no fix. Reject with InvalidState and a
   message that names the step the caller forgot.

2. ground_prompt told the model to look for `## Tool result`
   blocks; run_tool_calls emits `## Tool results` (plural). A model
   reading the grounding text and then looking for the singular
   header never found it. Fix the spelling.

3. MAX_TOOL_ROUNDS was 150 — three orders of magnitude more than
   the 3–15 rounds a real agentic pass uses, and the loop exited
   silently at the cap. Reduce to 40, which still catches a runaway
   loop well before the user has waited minutes for nothing.

Adds three tests, one per process* entry point, asserting the
InvalidState error and its "No LLM provider" phrasing.
MSG

git add -A
git commit -F /tmp/kod_commit_msg.txt
rm -f /tmp/kod_commit_msg.txt
