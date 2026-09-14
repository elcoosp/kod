#!/usr/bin/env bash
set -uo pipefail

run_with_timeout() {
    local secs="$1"; shift
    if command -v timeout >/dev/null 2>&1; then
        timeout "$secs" "$@"; return $?
    fi
    if command -v gtimeout >/dev/null 2>&1; then
        gtimeout "$secs" "$@"; return $?
    fi
    "$@" &
    local pid=$!
    ( sleep "$secs"
      if kill -0 "$pid" 2>/dev/null; then
          kill -TERM "$pid" 2>/dev/null
          sleep 2
          kill -KILL "$pid" 2>/dev/null
      fi ) &
    local watchdog=$!
    wait "$pid"; local rc=$?
    kill "$watchdog" 2>/dev/null; wait "$watchdog" 2>/dev/null
    [ "$rc" -ge 128 ] && return 124
    return "$rc"
}

COMPILE_OK=true
INCOMPLETE=false
TARGET=crates/kod-core/src/engine.rs

if [ ! -f Cargo.toml ] || [ ! -f "$TARGET" ]; then
    echo "ERROR: run from the kod workspace root ($TARGET missing)"
    exit 1
fi

echo "Patching $TARGET: clone provider before awaits + regression test"

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

# --- 1. process(): clone provider out of the lock ----------------------
patch(
    '''        // If we have a provider, use it to generate a response
        let provider = self.provider.read().await;
        if let Some(provider) = provider.as_ref() {''',
    '''        // Clone the provider Arc out of the read lock before any long
        // await. Holding the read guard across the agentic loop below
        // made `set_provider` (used by the TUI's `/model` switch) block
        // until the current generation finished — the write acquired
        // only after the last read released, i.e. at the very end of
        // the response. Cloning is one atomic increment on the Arc, so
        // the read lock is held for microseconds.
        let provider: Option<Arc<dyn LlmProvider>> =
            self.provider.read().await.clone();
        if let Some(provider) = provider.as_ref() {''',
    "process(): clone provider",
)

# --- 2. process_streaming(): clone provider out of the lock ------------
patch(
    '''        let provider = self.provider.read().await;
        if let Some(provider) = provider.as_ref() {
            let response = self.router.process_input(input).await?;
            let task_type = response.task_type;
            let history = self.render_history().await;
            self.record_turn(true, input).await;
            let prompt = self
                .router
                .build_prompt(input, &task_type, &history)
                .await?;
            let definitions = self.tools.get_definitions().await;
            let mut pending = self.ground_prompt(prompt, &definitions);

            let options = GenerationOptions::default();
            let (final_text, tool_calls, tool_results, usage) = self
                .run_streaming_loop(provider, &mut pending, &definitions, &options, chunk_tx)
                .await?;''',
    '''        // See process(): clone out of the lock before any long await.
        let provider: Option<Arc<dyn LlmProvider>> =
            self.provider.read().await.clone();
        if let Some(provider) = provider.as_ref() {
            let response = self.router.process_input(input).await?;
            let task_type = response.task_type;
            let history = self.render_history().await;
            self.record_turn(true, input).await;
            let prompt = self
                .router
                .build_prompt(input, &task_type, &history)
                .await?;
            let definitions = self.tools.get_definitions().await;
            let mut pending = self.ground_prompt(prompt, &definitions);

            let options = GenerationOptions::default();
            let (final_text, tool_calls, tool_results, usage) = self
                .run_streaming_loop(provider, &mut pending, &definitions, &options, chunk_tx)
                .await?;''',
    "process_streaming(): clone provider",
)

# --- 3. process_goal_streaming(): clone provider out of the lock -------
patch(
    '''        let provider = self.provider.read().await;
        if let Some(provider) = provider.as_ref() {
            let response = self.router.process_input(input).await?;
            let task_type = response.task_type;
            let history = self.render_history().await;
            self.record_turn(true, input).await;
            let prompt = self
                .router
                .build_prompt(input, &task_type, &history)
                .await?;
            let definitions = self.tools.get_definitions().await;
            let mut pending = self.ground_prompt(prompt, &definitions);
            pending.push_str(&format!(
                "\\n## Goal\\n\\n{goal}\\n\\nWork turn by turn toward this goal using tools. Do not ask the user for confirmation — act. When the goal is fully reached, end your reply with a line containing exactly GOAL MET and summarize what was done. If a tool errors, work around it and keep going.\\n"
            ));''',
    '''        // See process(): clone out of the lock before any long await.
        let provider: Option<Arc<dyn LlmProvider>> =
            self.provider.read().await.clone();
        if let Some(provider) = provider.as_ref() {
            let response = self.router.process_input(input).await?;
            let task_type = response.task_type;
            let history = self.render_history().await;
            self.record_turn(true, input).await;
            let prompt = self
                .router
                .build_prompt(input, &task_type, &history)
                .await?;
            let definitions = self.tools.get_definitions().await;
            let mut pending = self.ground_prompt(prompt, &definitions);
            pending.push_str(&format!(
                "\\n## Goal\\n\\n{goal}\\n\\nWork turn by turn toward this goal using tools. Do not ask the user for confirmation — act. When the goal is fully reached, end your reply with a line containing exactly GOAL MET and summarize what was done. If a tool errors, work around it and keep going.\\n"
            ));''',
    "process_goal_streaming(): clone provider",
)

# --- 4. Regression test, inserted before an unconditional anchor -------
if "test_set_provider_not_blocked_by_running_generation" in content:
    print("Regression test already present, skipping insert")
else:
    anchor = "    #[test]\n    fn test_tool_done_marker_roundtrip() {"
    if content.count(anchor) != 1:
        print("ERROR: anchor test_tool_done_marker_roundtrip not unique")
        sys.exit(2)

    new_test = '''    /// set_provider must complete promptly even while a streaming
    /// generation is in flight. Before the fix, process_streaming held
    /// the RwLock read guard across the whole agentic loop, so
    /// set_provider's write awaited the end of the generation — a
    /// `/model` switch mid-prompt looked like a hang.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn test_set_provider_not_blocked_by_running_generation() {
        use futures::Stream;
        use std::pin::Pin;
        use std::sync::Arc as StdArc;
        use std::time::Duration;
        use tokio::sync::Notify;

        /// Provider whose `stream_with_tools` signals `started` and then
        /// parks for `hold_for` before yielding `Done`. The signal is
        /// what makes the test deterministic: by the time `started`
        /// fires, the running `process_streaming` has definitely
        /// acquired the read guard and entered the streaming loop.
        struct SlowProvider {
            hold_for: Duration,
            started: StdArc<Notify>,
        }

        #[async_trait::async_trait]
        impl LlmProvider for SlowProvider {
            fn name(&self) -> &str {
                "slow"
            }
            async fn list_models(&self) -> kod_error::Result<Vec<String>> {
                Ok(vec![])
            }
            async fn generate(
                &self,
                _prompt: &str,
                _opts: &GenerationOptions,
            ) -> kod_error::Result<String> {
                Ok(String::new())
            }
            async fn generate_with_tools(
                &self,
                _prompt: &str,
                _tools: &[ToolDefinition],
                _opts: &GenerationOptions,
            ) -> kod_error::Result<GenerationResponse> {
                Ok(GenerationResponse::Text {
                    content: String::new(),
                    usage: None,
                })
            }
            fn stream(
                &self,
                _prompt: &str,
                _opts: &GenerationOptions,
            ) -> Pin<Box<dyn Stream<Item = kod_error::Result<StreamChunk>> + Send + '_>>
            {
                Box::pin(futures::stream::empty())
            }
            fn stream_with_tools<'a>(
                &'a self,
                _prompt: &'a str,
                _tools: &'a [ToolDefinition],
                _opts: &'a GenerationOptions,
            ) -> Pin<Box<dyn Stream<Item = kod_error::Result<StreamChunk>> + Send + 'a>>
            {
                let hold = self.hold_for;
                let started = self.started.clone();
                Box::pin(futures::stream::once(async move {
                    started.notify_one();
                    tokio::time::sleep(hold).await;
                    Ok(StreamChunk::Done)
                }))
            }
        }

        let temp = TempDir::new().unwrap();
        let db_path = temp.path().join("test.redb");
        let cfg = RouterConfig {
            working_dir: temp.path().to_path_buf(),
            enable_memory: false,
            enable_swarm: false,
            max_skills_per_query: 3,
        };
        let engine = Arc::new(KodEngine::new(cfg, db_path).unwrap());
        engine.start().await.unwrap();

        let started = StdArc::new(Notify::new());
        engine
            .set_provider(Arc::new(SlowProvider {
                hold_for: Duration::from_millis(1000),
                started: started.clone(),
            }))
            .await;

        let engine_for_gen = engine.clone();
        let (tx, _rx) = tokio::sync::mpsc::channel::<String>(4);
        let gen_task = tokio::spawn(async move {
            let _ = engine_for_gen.process_streaming("hello", &tx).await;
        });

        // Block until the streaming loop is definitely running and the
        // read guard is held.
        started.notified().await;

        // Swap providers. With the fix this returns immediately; without
        // it, it waits for the 1s stream to finish and the assertion
        // below fails.
        let start = std::time::Instant::now();
        engine
            .set_provider(Arc::new(SlowProvider {
                hold_for: Duration::from_millis(1),
                started: StdArc::new(Notify::new()),
            }))
            .await;
        let elapsed = start.elapsed();

        assert!(
            elapsed < Duration::from_millis(200),
            "set_provider blocked for {elapsed:?} — the read lock is \\
             still held across the agentic loop"
        );

        let _ = gen_task.await;
    }

'''
    content = content.replace(anchor, new_test + anchor, 1)
    print("Inserted regression test before test_tool_done_marker_roundtrip")

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

echo "Checking compilation"
if ! cargo check --workspace 2>&1; then
    echo "Compilation failed – will skip commit"
    COMPILE_OK=false
fi

if [ "$INCOMPLETE" = true ] || [ "$COMPILE_OK" = false ]; then
    echo "Skipping tests and commit due to incomplete files or compilation errors"
    exit 1
fi

echo "Running kod-core tests (180s wall clock)"
if ! run_with_timeout 180 cargo test -p kod-core 2>&1; then
    echo "kod-core tests failed or hung. Paste the full output for a surgical fix."
    exit 1
fi

echo "Running workspace tests (300s wall clock)"
if ! run_with_timeout 300 cargo test --workspace 2>&1; then
    echo "Workspace tests failed or hung. Paste the full output for a surgical fix."
    exit 1
fi

echo "Running clippy with -D warnings"
if ! cargo clippy --workspace --all-targets -- -D warnings 2>&1; then
    echo "Clippy failed. Paste the full output for a surgical fix."
    exit 1
fi

echo "All checks passed. Committing."
git add -A
git commit -m "fix(core): release provider lock before long-running generation

KodEngine::process, process_streaming, and process_goal_streaming each
did 'let provider = self.provider.read().await' and held the read
guard across the entire agentic loop. set_provider takes the write
lock, so a model switch issued while a generation was in flight
blocked until that generation completed — for a TUI user typing
'/model qwen3:0.6b' mid-prompt, this looked like a hang.

Clone the Arc<dyn LlmProvider> out of the guard instead. Cloning is a
single atomic increment; the read lock is held for microseconds, and
set_provider's write can proceed immediately.

Adds a regression test with a SlowProvider whose stream_with_tools
signals via Notify the moment the streaming loop is entered, then
sleeps for 1s. The test waits on the Notify (so the read guard is
definitely held), calls set_provider, and asserts it returns within
200ms. Without the fix it takes ~900ms and the assertion fails."
