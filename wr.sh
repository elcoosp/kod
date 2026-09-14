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
TARGET=crates/kod-cli/src/commands.rs

if [ ! -f Cargo.toml ] || [ ! -f "$TARGET" ]; then
    echo "ERROR: run from the kod workspace root ($TARGET missing)"
    exit 1
fi

echo "Patching $TARGET: stream tokens to stdout in run_chat"

python3 - "$TARGET" << 'PYEOF'
import os
import sys

target = sys.argv[1]
with open(target, "r") as f:
    content = f.read()

old = '''    println!(
        "KOD Chat (model: {}) - Type 'quit' or Ctrl+C to exit",
        model_name
    );
    println!();

    let stdin = io::stdin();
    let mut input = String::new();
    print!("> ");
    let _ = io::stdout().flush();

    while let Ok(bytes) = stdin.lock().read_line(&mut input) {
        if bytes == 0 {
            break;
        }
        let input_line = input.trim();
        if input_line.is_empty() {
            print!("> ");
            let _ = io::stdout().flush();
            continue;
        }
        if input_line == "quit" || input_line == "exit" {
            break;
        }

        let response = engine.process(input_line).await?;

        if let Some(text) = response.text {
            println!();
            println!("{}", text);
            println!();
        }

        input.clear();
        print!("> ");
        let _ = io::stdout().flush();
    }

    // Shutdown
    engine.shutdown().await?;

    Ok(())
}'''

new = '''    println!(
        "KOD Chat (model: {}) - Type 'quit' or Ctrl+C to exit",
        model_name
    );
    println!();

    let stdin = io::stdin();
    let mut input = String::new();

    loop {
        print!("> ");
        let _ = io::stdout().flush();
        input.clear();

        match stdin.lock().read_line(&mut input) {
            Ok(0) => break, // EOF (Ctrl+D)
            Ok(_) => {}
            Err(e) => {
                eprintln!("Input error: {}", e);
                break;
            }
        }

        let input_line = input.trim();
        if input_line.is_empty() {
            continue;
        }
        if input_line == "quit" || input_line == "exit" {
            break;
        }

        // Stream tokens as they arrive. The engine's chunk channel also
        // carries `\\0kod-*` markers (tool start / args / done / thinking)
        // that the TUI uses to render its running indicator — the CLI has
        // no such indicator, so it drops them. If nothing streamed (a
        // tool-only reply whose summary is empty, or a provider whose
        // default stream_with_tools emits no Text), fall back to the
        // response's full text.
        let (tx, mut rx) = tokio::sync::mpsc::channel::<String>(64);
        let pump = tokio::spawn(async move {
            let mut streamed_any = false;
            while let Some(chunk) = rx.recv().await {
                if kod_core::engine::parse_tool_start(&chunk).is_some()
                    || kod_core::engine::parse_tool_args(&chunk).is_some()
                    || kod_core::engine::parse_tool_done(&chunk).is_some()
                    || kod_core::engine::is_thinking_marker(&chunk)
                {
                    continue;
                }
                print!("{}", chunk);
                let _ = io::stdout().flush();
                streamed_any = true;
            }
            streamed_any
        });

        let result = engine.process_streaming(input_line, &tx).await;
        drop(tx);
        let streamed_any = pump.await.unwrap_or(false);

        match result {
            Ok(resp) => {
                if streamed_any {
                    // Stream already printed the answer; finish the line
                    // and leave one blank line before the next prompt.
                    println!();
                    println!();
                } else if let Some(text) = resp.text
                    && !text.trim().is_empty()
                {
                    println!();
                    println!("{}", text);
                    println!();
                }
            }
            Err(e) => {
                eprintln!("Error: {}", e);
            }
        }
    }

    // Shutdown
    engine.shutdown().await?;

    Ok(())
}'''

n = content.count(old)
if n != 1:
    print(f"ERROR: expected 1 occurrence of the run_chat loop, found {n}")
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

echo "Running kod-cli tests (120s wall clock)"
if ! run_with_timeout 120 cargo test -p kod-cli 2>&1; then
    echo "kod-cli tests failed or hung. Paste the full output for a surgical fix."
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
git commit -m "feat(cli): stream tokens live in \`kod chat\`

\`kod chat\` awaited engine.process() and printed the whole reply at
once. For a 500-token answer, that meant several seconds of a frozen
prompt while the model generated — the same behaviour that made the
TUI feel sluggish before it learned to stream.

Route the chat loop through engine.process_streaming instead. A
background pump drains the chunk channel, prints each text chunk
immediately, and drops the engine's \\0kod-* control markers (tool
start / args / done / thinking) — those exist for the TUI's running
indicator, which the CLI does not render.

The pump reports whether it printed anything. When nothing streamed
(the summary was empty, or a provider whose default stream_with_tools
never emits Text), the loop falls back to the response's full text.
Otherwise the response text is skipped, since it was already shown.

Engine errors no longer abort the whole REPL — a failed prompt prints
its error and returns to the \`>\` prompt, so the user can try again
without losing the session."
