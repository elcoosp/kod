//! `kod chat`, `kod agent`, `kod acp`.
//!
//! Interactive entry points. `run_chat` and `run_agent` share the
//! REPL shape; `run_chat_remote` and `run_agent_remote` are the
//! daemon-client variants; `run_acp` speaks the Agent Client
//! Protocol over stdin/stdout.

use super::*;

pub async fn run_chat_remote(socket: Option<std::path::PathBuf>) -> Result<()> {
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

    let socket_path = socket.unwrap_or_else(kod_core::serve::default_socket_path);
    if !socket_path.exists() {
        return Err(KodError::InvalidState(format!(
            "no daemon listening at {}. Start one with `kod serve`, \
             or drop --remote to run an embedded session.",
            socket_path.display(),
        )));
    }

    println!(
        "KOD Chat (remote: {}) - Type 'quit' or Ctrl+D to exit",
        socket_path.display()
    );
    println!();

    let stdin = io::stdin();
    let mut input = String::new();
    // Monotonic request ids; the daemon does not care what the id is,
    // only that the client can match responses back. A counter keeps
    // the stream human-readable in a debug log.
    let mut next_id: u64 = 1;

    loop {
        print!("> ");
        let _ = io::stdout().flush();
        input.clear();

        match stdin.lock().read_line(&mut input) {
            Ok(0) => break, // EOF (Ctrl+D)
            Ok(_) => {}
            Err(e) => {
                eprintln!("Input error: {e}");
                break;
            }
        }
        let line = input.trim();
        if line.is_empty() {
            continue;
        }
        if line == "quit" || line == "exit" {
            break;
        }

        // Fresh connection per prompt. A long-lived connection would
        // be marginally cheaper, but a server that dies mid-session
        // would leave the client printing nothing with no clear
        // reason; a fresh connect per turn surfaces "connection
        // refused" on the prompt that follows the daemon's death,
        // which is where the user looks for it.
        let stream = match tokio::net::UnixStream::connect(&socket_path).await {
            Ok(s) => s,
            Err(e) => {
                eprintln!(
                    "Could not reach the daemon at {}: {e}. \
                     It may have stopped; restart with `kod serve`, \
                     or drop --remote to run embedded.",
                    socket_path.display(),
                );
                break;
            }
        };
        let (read_half, mut write_half) = stream.into_split();

        let id = format!("chat-{next_id}");
        next_id += 1;
        let req = serde_json::json!({
            "v": 1,
            "id": id,
            "method": "process_streaming",
            "params": { "input": line, "transcript_key": "chat" },
        });
        let mut frame = match serde_json::to_string(&req) {
            Ok(s) => s,
            Err(e) => {
                eprintln!("could not serialize request: {e}");
                continue;
            }
        };
        frame.push('\n');
        if let Err(e) = write_half.write_all(frame.as_bytes()).await {
            eprintln!("could not send request: {e}");
            continue;
        }
        if let Err(e) = write_half.flush().await {
            eprintln!("could not flush request: {e}");
            continue;
        }

        let mut reader = BufReader::new(read_half).lines();
        let mut printed_any = false;
        let mut answered = false;
        loop {
            let line = match reader.next_line().await {
                Ok(Some(l)) => l,
                Ok(None) => break,
                Err(e) => {
                    eprintln!("daemon read error: {e}");
                    break;
                }
            };
            let v: serde_json::Value = match serde_json::from_str(&line) {
                Ok(v) => v,
                Err(_) => continue,
            };
            if v.get("id").and_then(|x| x.as_str()) != Some(id.as_str()) {
                continue;
            }
            match v.get("type").and_then(|t| t.as_str()) {
                Some("chunk") => {
                    let Some(data) = v.get("data").and_then(|d| d.as_str()) else {
                        continue;
                    };
                    // The daemon forwards every engine chunk
                    // verbatim, including the `\0kod-*` markers. The
                    // CLI drops them the same way the embedded path
                    // does — except for tool-args, which the
                    // embedded path surfaces as a short notice so
                    // the user sees activity between two stretches
                    // of text. Keep that parity here.
                    if let Some(brief) = kod_core::engine::parse_tool_args(data) {
                        print!("\n[{brief}]\n");
                        let _ = io::stdout().flush();
                        continue;
                    }
                    if is_control_marker(data) {
                        continue;
                    }
                    print!("{data}");
                    let _ = io::stdout().flush();
                    printed_any = true;
                }
                Some("done") => {
                    answered = true;
                    if printed_any {
                        println!();
                        println!();
                    }
                    break;
                }
                Some("error") => {
                    let msg = v
                        .get("data")
                        .and_then(|d| d.get("message"))
                        .and_then(|m| m.as_str())
                        .unwrap_or("(no message)");
                    eprintln!("Error: {msg}");
                    answered = true;
                    break;
                }
                _ => {}
            }
        }
        // A response stream that closes without a `done` or `error`
        // is the daemon dying mid-turn. Say so instead of looping
        // back to a prompt that would then fail on connect.
        if !answered {
            eprintln!("(daemon closed the connection before completing this turn)");
            break;
        }
    }

    Ok(())
}

pub async fn run_chat(
    model: Option<String>,
    sandbox: bool,
    system_prompt: Option<String>,
    cli_preset: Option<String>,
) -> Result<()> {
    // Load configuration
    let config = KodConfig::load_default()?;

    // S10: shared bootstrap. `Arc` because the approval forwarder
    // below needs the same handle `process_streaming` runs on.
    let engine = engine_from_config(
        &config,
        EngineBootstrapOptions {
            model_override: model.as_deref(),
            cli_preset: cli_preset.as_deref(),
            require_sandbox: sandbox,
            install_mcp: true,
            db_path: None,
            install_embedder: true,
            install_policy: true,
        },
    )
    .await?;

    // Start the engine
    engine.start().await?;

    // Session log: every tool call recorded as JSONL, `kod replay`-able.
    // `KOD_SESSION_LOG` overrides the default path; the default lives
    // under `~/.kod/sessions/` so a run in a project does not scatter
    // logs into the project tree.
    if sandbox {
        engine.set_sandbox_mode(kod_tools::context::SandboxMode::Require);
        // Surface the choice on stderr so the user sees it took effect.
        // A silent --sandbox that turned out to be unavailable would
        // only be visible on the first shell command, which is too
        // late to be useful.
        eprintln!("Sandbox: required (bwrap on Linux, sandbox-exec on macOS)");
    }
    if let Some(path) = std::env::var("KOD_SESSION_LOG")
        .ok()
        .map(std::path::PathBuf::from)
        .or_else(kod_core::session_log::default_session_path)
    {
        match kod_core::session_log::SessionRecorder::open(path.clone()) {
            Ok(recorder) => {
                engine.set_session_recorder(Arc::new(recorder));
                eprintln!("Session log: {}", path.display());
            }
            Err(e) => eprintln!("Could not open session log {}: {}", path.display(), e),
        }
    }

    // Tier 1.4 — open a turn-trace writer next to the session log.
    if let Some(log_path) = engine.session_log_path()
        && let Some(trace_path) = kod_core::TraceWriter::default_for_session(&log_path)
        && let Ok(w) = kod_core::TraceWriter::open(trace_path)
    {
        engine.set_turn_trace_writer(std::sync::Arc::new(w));
    }
    // Tier 3.4 — persist plans and decisions across restarts.
    if let Some(log_path) = engine.session_log_path()
        && let Some(state_path) = kod_core::StateStore::sibling_of(&log_path)
    {
        let store = kod_core::StateStore::open(state_path);
        engine.set_state_store(store).await;
    }

    // Install the Jev (TypeSafe AI) client when enabled in config.
    // A misconfigured enabled block is a loud startup error; a
    // disabled block (the default) is a silent no-op.
    match kod_core::install_jev_from_config(&engine, &config.jev) {
        Ok(true) => eprintln!("Jev: enabled"),
        Ok(false) => {}
        Err(e) => eprintln!("Jev configuration error (continuing without): {e}"),
    }

    // Load skills from every standard location so the router has the
    // same inventory the TUI session sees.
    let skills_dirs = config.skills_dirs()?;
    match engine.load_skills_from_dirs(&skills_dirs).await {
        Ok(n) if n > 0 => println!("Loaded {} skill file(s)", n),
        Ok(_) => {}
        Err(e) => eprintln!("Could not load skills: {}", e),
    }
    if config.skills.enable_hot_reload {
        for dir in &skills_dirs {
            if dir.is_dir()
                && let Err(e) = engine.enable_hot_reload(dir).await
            {
                eprintln!(
                    "Could not enable skill hot reload for {}: {}",
                    dir.display(),
                    e
                );
            }
        }
    }

    // The model name shown in the greeting. Prefer the caller's
    // `--model` override; otherwise the config's default endpoint.
    let model_name = model
        .clone()
        .unwrap_or_else(|| config.llm.default_endpoint().model.clone());
    println!(
        "KOD Chat (model: {}) - Type 'quit' or Ctrl+C to exit",
        model_name
    );
    if let Some(sys) = &system_prompt {
        let preview = if sys.len() > 120 {
            format!("{}…", kod_types::strutil::truncate_chars(sys, 120))
        } else {
            sys.clone()
        };
        println!("System prompt override: {}", preview);
    }
    println!();

    // H-C8: async stdin so the read does not pin a runtime worker
    // (the pre-fix shape blocked a worker for the duration of every
    // user think-time). Ctrl+C is intercepted by tokio so the
    // process does not die mid-tool with orphaned MCP children; it
    // cancels the current turn instead. Ctrl+D (EOF) still exits.
    let mut reader = tokio::io::BufReader::new(tokio::io::stdin());
    let mut input = String::new();

    loop {
        print!("> ");
        let _ = io::stdout().flush();
        input.clear();

        let read_result = tokio::select! {
            r = {
                use tokio::io::AsyncBufReadExt;
                reader.read_line(&mut input)
            } => r,
            _ = tokio::signal::ctrl_c() => {
                // Cancel the running turn and continue the REPL.
                engine.request_cancel();
                println!();
                continue;
            }
        };

        match read_result {
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
        // carries `\0kod-*` markers (tool start / args / done / thinking)
        // that the TUI uses to render its running indicator — the CLI has
        // no such indicator, so it drops them. If nothing streamed (a
        // tool-only reply whose summary is empty, or a provider whose
        // default stream_with_tools emits no Text), fall back to the
        // response's full text.
        let (tx, mut rx) = tokio::sync::mpsc::channel::<String>(64);
        // Approval channel: the pump sends (id, decision); the
        // forwarder calls engine.respond_to_approval. The split
        // exists because the pump owns its scope and cannot also
        // borrow the engine across the response loop.
        let (approval_tx, mut approval_rx) =
            tokio::sync::mpsc::channel::<(u64, kod_core::engine::ApprovalDecision)>(16);
        let engine_for_approvals = engine.clone();
        let approval_forwarder = tokio::spawn(async move {
            while let Some((id, decision)) = approval_rx.recv().await {
                let _ = engine_for_approvals.respond_to_approval(id, decision).await;
            }
        });
        // Clone so the outer scope retains its own sender: dropping it
        // after `process_streaming` closes the channel, and the pump's
        // clone is dropped with the task. Without the clone, the outer
        // `drop(approval_tx)` is a use-after-move.
        let (question_tx, mut question_rx) = tokio::sync::mpsc::channel::<(u64, String)>(16);
        let engine_for_questions = engine.clone();
        let question_forwarder = tokio::spawn(async move {
            while let Some((id, answer)) = question_rx.recv().await {
                let _ = engine_for_questions.respond_to_question(id, answer).await;
            }
        });
        let approval_tx_pump = approval_tx.clone();
        let question_tx_pump = question_tx.clone();
        let pump = tokio::spawn(async move {
            // `streamed_any` counts text chunks, not control markers.
            // It decides whether the caller still needs to print the
            // final text: if the reply was already streamed live, the
            // caller skips the duplicate. A tool notice is not a text
            // chunk — printing it must not suppress the summary.
            let mut streamed_any = false;
            while let Some(chunk) = rx.recv().await {
                // Tool-args marker: the engine has assembled a tool
                // call and knows what it is about to do. Print a
                // one-line notice so the user sees activity between
                // two stretches of streamed text rather than an
                // unexplained pause. Uses the same brief the TUI
                // shows in its running row (format_call_brief), so
                // the two surfaces speak the same vocabulary:
                // `[execute_command cargo test]`,
                // `[read_file path=src/main.rs]`.
                if let Some(brief) = kod_core::engine::parse_tool_args(&chunk) {
                    print!("\n[{brief}]\n");
                    let _ = io::stdout().flush();
                    continue;
                }
                // Other control markers (tool start, tool done,
                // thinking) carry information the CLI does not
                // render. Consume them without printing — the
                // preceding tool notice already covers the visible
                // activity, and the model's follow-up text will
                // arrive as ordinary streamed chunks.
                // Approval marker: engine wants yes/no before running
                // a write_file / patch_file. Print the diff, read a
                // line from stdin, forward the answer to the engine.
                // Any input error is treated as Deny.
                // Question marker: ask_user wants a text answer.
                if let Some((id, json)) = kod_core::engine::parse_question(&chunk) {
                    let request: kod_tools::ask::QuestionRequest = serde_json::from_str(json)
                        .unwrap_or_else(|_| kod_tools::ask::QuestionRequest {
                            question: "(unparseable question)".to_string(),
                            placeholder: None,
                        });
                    println!();
                    println!("── question ──");
                    println!("{}", request.question);
                    if let Some(hint) = &request.placeholder {
                        println!("(e.g. {})", hint);
                    }
                    print!("> ");
                    let _ = io::stdout().flush();
                    let mut answer = String::new();
                    let text = match io::stdin().read_line(&mut answer) {
                        Ok(_) => answer.trim_end().to_string(),
                        Err(_) => "(no answer)".to_string(),
                    };
                    let _ = question_tx_pump.send((id, text)).await;
                    continue;
                }

                if let Some((_batch_id, json)) = kod_core::engine::parse_tool_approval_batch(&chunk)
                {
                    let batch: kod_core::engine::ApprovalBatch = serde_json::from_str(json)
                        .unwrap_or_else(|_| kod_core::engine::ApprovalBatch { items: Vec::new() });
                    let total = batch.items.len();
                    for (n, item) in batch.items.iter().enumerate() {
                        let item_id = match item.id {
                            Some(i) => i,
                            None => {
                                println!("(approval item {}/{} has no id; skipping)", n + 1, total);
                                continue;
                            }
                        };
                        println!();
                        println!("── approval required ({}/{}) ──", n + 1, total);
                        println!("Tool:    {}", item.tool_name);
                        println!("Summary: {}", item.summary);
                        if let Some(diff) = &item.diff {
                            println!();
                            let mut lines = diff.lines();
                            for l in lines.by_ref().take(60) {
                                println!("{l}");
                            }
                            let extra = lines.count();
                            if extra > 0 {
                                println!("… and {extra} more lines of diff");
                            }
                        }
                        print!("Approve? [y/N/a=never] ");
                        let _ = io::stdout().flush();
                        let mut answer = String::new();
                        let answer_lower = match io::stdin().read_line(&mut answer) {
                            Ok(_) => answer.trim().to_lowercase(),
                            Err(_) => String::new(),
                        };
                        let decision = match answer_lower.as_str() {
                            "y" | "yes" => kod_core::engine::ApprovalDecision::Approve,
                            "a" | "always" | "never" => {
                                kod_core::engine::ApprovalDecision::DenyAlways
                            }
                            _ => kod_core::engine::ApprovalDecision::Deny,
                        };
                        let _ = approval_tx_pump.send((item_id, decision)).await;
                    }
                    continue;
                }

                if let Some((id, json)) = kod_core::engine::parse_tool_approval(&chunk) {
                    let request: kod_core::engine::ApprovalRequest = serde_json::from_str(json)
                        .unwrap_or_else(|_| kod_core::engine::ApprovalRequest {
                            tool_name: "?".to_string(),
                            arguments: serde_json::Value::Null,
                            diff: None,
                            summary: "(unparseable approval request)".to_string(),
                            id: None,
                        });
                    println!();
                    println!("── approval required ──");
                    println!("Tool:    {}", request.tool_name);
                    println!("Summary: {}", request.summary);
                    if let Some(diff) = &request.diff {
                        println!();
                        let mut lines = diff.lines();
                        for l in lines.by_ref().take(60) {
                            println!("{l}");
                        }
                        let extra = lines.count();
                        if extra > 0 {
                            println!("… and {extra} more lines of diff");
                        }
                    }
                    print!("Approve? [y/N/a=never] ");
                    let _ = io::stdout().flush();
                    let mut answer = String::new();
                    let answer_lower = match io::stdin().read_line(&mut answer) {
                        Ok(_) => answer.trim().to_lowercase(),
                        Err(_) => String::new(),
                    };
                    let decision = match answer_lower.as_str() {
                        "y" | "yes" => kod_core::engine::ApprovalDecision::Approve,
                        "a" | "always" | "never" => kod_core::engine::ApprovalDecision::DenyAlways,
                        _ => kod_core::engine::ApprovalDecision::Deny,
                    };
                    let _ = approval_tx_pump.send((id, decision)).await;
                    continue;
                }

                if kod_core::engine::parse_tool_start(&chunk).is_some()
                    || kod_core::engine::parse_tool_done(&chunk).is_some()
                    || kod_core::engine::is_thinking_marker(&chunk)
                {
                    continue;
                }
                print!("{chunk}");
                let _ = io::stdout().flush();
                streamed_any = true;
            }
            streamed_any
        });

        let input_with_system = match &system_prompt {
            Some(sys) => format!("[system override] {sys}\n\n{input_line}"),
            None => input_line.to_string(),
        };
        // H-C9: capture the result instead of `?`. The shutdown
        // call below must run even when the prompt errored, or MCP
        // children, watchers, and the redb handle get torn down by
        // process exit rather than clean shutdown.
        let result = engine.process_streaming(&input_with_system, &tx).await;
        drop(tx);
        drop(approval_tx);
        drop(question_tx);
        let _ = approval_forwarder.await;
        let _ = question_forwarder.await;
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
}

pub async fn run_agent_remote(
    name: String,
    goal: String,
    socket: Option<std::path::PathBuf>,
) -> Result<()> {
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

    if goal.trim().is_empty() {
        return Err(KodError::Config("empty goal".to_string()));
    }

    let socket_path = socket.unwrap_or_else(kod_core::serve::default_socket_path);
    if !socket_path.exists() {
        return Err(KodError::InvalidState(format!(
            "no daemon listening at {}. Start one with `kod serve`, \
             or drop --remote to run an embedded agent.",
            socket_path.display(),
        )));
    }

    let stream = tokio::net::UnixStream::connect(&socket_path)
        .await
        .map_err(|e| {
            KodError::InvalidState(format!(
                "could not connect to daemon at {}: {e}",
                socket_path.display()
            ))
        })?;
    let (read_half, mut write_half) = stream.into_split();

    let req = serde_json::json!({
        "v": 1,
        "id": "agent-1",
        "method": "process",
        "params": {
            "input": goal,
            "transcript_key": format!("agent:{name}"),
        },
    });
    let mut frame =
        serde_json::to_string(&req).map_err(|e| KodError::Serialization(e.to_string()))?;
    frame.push('\n');
    write_half
        .write_all(frame.as_bytes())
        .await
        .map_err(KodError::Io)?;
    write_half.flush().await.map_err(KodError::Io)?;

    let mut reader = BufReader::new(read_half).lines();
    while let Some(line) = reader.next_line().await.map_err(KodError::Io)? {
        let v: serde_json::Value = match serde_json::from_str(&line) {
            Ok(v) => v,
            Err(_) => continue,
        };
        if v.get("id").and_then(|x| x.as_str()) != Some("agent-1") {
            continue;
        }
        match v.get("type").and_then(|t| t.as_str()) {
            Some("done") => {
                // The reply is in `data` as a serialized
                // TaskResponse; the `text` field inside is the
                // model's answer. A `null` text (tool-only reply)
                // leaves the agent's output empty, matching the
                // embedded path's behaviour of not printing an
                // empty string as a result.
                let text = v
                    .get("data")
                    .and_then(|d| d.get("text"))
                    .and_then(|t| t.as_str())
                    .unwrap_or("");
                if !text.trim().is_empty() {
                    println!("Agent {}: {}", name, text);
                }
                return Ok(());
            }
            Some("error") => {
                let msg = v
                    .get("data")
                    .and_then(|d| d.get("message"))
                    .and_then(|m| m.as_str())
                    .unwrap_or("(no message)");
                return Err(KodError::Provider(format!("daemon error: {msg}")));
            }
            _ => {}
        }
    }

    Err(KodError::InvalidState(
        "daemon closed the connection before answering".to_string(),
    ))
}

pub async fn run_agent(
    name: String,
    goal: String,
    model: Option<String>,
    cli_preset: Option<String>,
) -> Result<()> {
    let config = KodConfig::load_default()?;

    // S10: one bootstrap for every engine-building command. The
    // pre-extraction shape had each command install a different
    // subset of config-derived settings — `run_agent` never set
    // `network_access` or `generation_defaults`, `run_swarm` never
    // set hooks or limits. That is exactly the drift the helper
    // exists to prevent.
    let engine = engine_from_config(
        &config,
        EngineBootstrapOptions {
            model_override: model.as_deref(),
            cli_preset: cli_preset.as_deref(),
            // `kod agent` has no interactive consumer; a Require
            // sandbox would fail every command with no UI to relax it.
            require_sandbox: false,
            install_mcp: true,
            db_path: None,
            install_embedder: true,
            install_policy: true,
        },
    )
    .await?;

    engine.start().await?;

    println!("Starting agent '{}' with goal: {}", name, goal);

    // H-C9: capture the result and shut down before propagating. The
    // pre-fix `?` skipped `engine.shutdown()`, leaving MCP children
    // and the redb handle to be torn down by process exit.
    let response_result = engine.process(&goal).await;
    let _ = engine.shutdown().await;
    let response = response_result?;

    if let Some(text) = response.text {
        println!("Agent {}: {}", name, text);
    }

    Ok(())
}

pub async fn run_acp(cli_preset: Option<String>) -> Result<()> {
    let config = KodConfig::load_default()?;

    // Same isolation hook `TuiLoop::init_engine` uses. Without it
    // the ACP subprocess opens the shared `~/.kod/data/kod.redb`,
    // which a concurrent test process may hold a lock on — the
    // process then dies before writing the initialize response.
    // S10: shared bootstrap. ACP keeps its `KOD_TEST_DB` env override
    // and its historical no-embedder default.
    let db_path_override = std::env::var("KOD_TEST_DB")
        .ok()
        .map(std::path::PathBuf::from);
    let engine = engine_from_config(
        &config,
        EngineBootstrapOptions {
            model_override: None,
            cli_preset: cli_preset.as_deref(),
            require_sandbox: false,
            install_mcp: true,
            db_path: db_path_override,
            install_embedder: false,
            install_policy: true,
        },
    )
    .await?;
    engine.set_auto_check(config.tools.auto_check);
    engine.set_auto_lsp(config.tools.auto_lsp);
    install_policy_async(&engine, &config, cli_preset.as_deref()).await?;
    // Tier 1.3 — install read-protection from the effective policy.
    if let Some(policy) = engine.policy().await {
        engine.set_read_protection(policy.read_protection().clone());
    }
    kod_core::mcp_adapters::install_from_config(&engine, &config).await;

    engine.start().await?;

    let skills_dirs = config.skills_dirs()?;
    // Diagnostics go to stderr, not stdout: an ACP client (the editor)
    // reads stdout and expects only Content-Length-framed JSON-RPC.
    // A log line on stdout would corrupt the protocol stream, so
    // `eprintln!` is not a workaround — it is the correct channel.
    match engine.load_skills_from_dirs(&skills_dirs).await {
        Ok(0) => {}
        Ok(n) => eprintln!("acp: loaded {n} skill file(s)"),
        Err(e) => eprintln!("acp: could not load skills: {e}"),
    }

    kod_core::acp::serve(engine.clone()).await?;

    engine.shutdown().await?;
    Ok(())
}
