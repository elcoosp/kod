//! `kod swarm` (embedded and `--remote`).

use super::*;

pub async fn run_swarm_remote(
    goal: String,
    agents: Option<usize>,
    merge: bool,
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
             or drop --remote to run an embedded swarm.",
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

    let mut params = serde_json::json!({
        "goal": goal,
        "merge": merge,
    });
    if let Some(n) = agents {
        params["max_agents"] = serde_json::json!(n);
    }
    let req = serde_json::json!({
        "v": 1,
        "id": "swarm-1",
        "method": "swarm",
        "params": params,
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
        if v.get("id").and_then(|x| x.as_str()) != Some("swarm-1") {
            continue;
        }
        match v.get("type").and_then(|t| t.as_str()) {
            Some("swarm_event") => {
                print_swarm_event(&v);
            }
            Some("done") => {
                let data = v.get("data");
                let merged = data
                    .and_then(|d| d.get("merged"))
                    .and_then(|m| m.as_str())
                    .unwrap_or("");
                let merged_by_model = data
                    .and_then(|d| d.get("merged_by_model"))
                    .and_then(|b| b.as_bool())
                    .unwrap_or(false);

                if let Some(conflicts) = data
                    .and_then(|d| d.get("conflicts"))
                    .and_then(|c| c.as_array())
                    && !conflicts.is_empty()
                {
                    println!("\n{} file conflict(s):", conflicts.len());
                    for c in conflicts {
                        let file = c.get("file").and_then(|f| f.as_str()).unwrap_or("?");
                        let agents = c
                            .get("agents")
                            .and_then(|a| a.as_array())
                            .map(|arr| {
                                arr.iter()
                                    .filter_map(|x| x.as_str())
                                    .collect::<Vec<_>>()
                                    .join(", ")
                            })
                            .unwrap_or_default();
                        println!("  \u{26a0} {} \u{2014} written by {}", file, agents);
                    }
                }

                println!("\n================ merged ================\n");
                println!("{}", merged);
                if !merged_by_model {
                    println!(
                        "\n(merged by concatenation \u{2014} LLM synthesis was disabled or failed)"
                    );
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
        "daemon closed the connection before completing the swarm run".to_string(),
    ))
}

pub async fn run_swarm(
    goal: String,
    agents: Option<usize>,
    model: Option<String>,
    merge: bool,
) -> Result<()> {
    let config = KodConfig::load_default()?;
    // H-C3: the requested agent count was parsed but discarded; the
    // swarm runner read the config-only value, so `kod swarm -n 8` with
    // `max_agents = 3` produced 3 agents. Honour the CLI override.
    let requested_agents = agents.unwrap_or(config.swarm.max_agents);

    // S10: shared bootstrap. `model_override` is the CLI's `--model`
    // flag; the swarm's per-capability routing still wins for
    // individual agents (see `SwarmRunner::from_config`).
    let engine = engine_from_config(
        &config,
        EngineBootstrapOptions {
            model_override: model.as_deref(),
            cli_preset: None,
            require_sandbox: false,
            install_mcp: true,
            db_path: None,
            install_embedder: true,
            install_policy: true,
        },
    )
    .await?;

    engine.start().await?;

    // Skills, same as the single-agent path so a swarm agent sees the
    // same instructions a single agent would.
    let skills_dirs = config.skills_dirs()?;
    match engine.load_skills_from_dirs(&skills_dirs).await {
        Ok(0) => {}
        Ok(n) => println!("Loaded {} skill file(s)", n),
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

    // (engine is already an Arc from `engine_from_config`.)
    // Design §D4.3: the runner reads per-run budget and retry knobs
    // from `[swarm]`. `from_config` centralises the mapping so this
    // site and the TUI's `/swarm` cannot drift.
    let runner = match SwarmRunner::from_config(engine.clone(), &config.swarm).await {
        Ok(r) => r.with_max_agents(requested_agents),
        Err(e) => {
            // H-C9: shut down before propagating.
            let _ = engine.shutdown().await;
            return Err(e);
        }
    };
    println!(
        "Swarm: up to {} agents, merge {}",
        runner.max_agents(),
        if merge { "on" } else { "off" },
    );

    let (tx, mut rx) = tokio::sync::mpsc::channel::<SwarmEvent>(256);
    let print_task = tokio::spawn(async move {
        while let Some(ev) = rx.recv().await {
            match ev {
                SwarmEvent::Decomposed(subs) => {
                    println!("\nDecomposed into {} subtasks:", subs.len());
                    for (i, s) in subs.iter().enumerate() {
                        println!("  {}. {} — {}", i + 1, s.name, s.description);
                    }
                    println!();
                }
                SwarmEvent::AgentStarted { name, subtask, .. } => {
                    println!(
                        "── {} starts on: {}",
                        name,
                        subtask.lines().next().unwrap_or("")
                    );
                }
                SwarmEvent::AgentChunk { text, .. } => {
                    print!("{}", text);
                    let _ = io::stdout().flush();
                }
                SwarmEvent::AgentCompleted { name, .. } => {
                    println!("\n── {} done\n", name);
                }
                SwarmEvent::AgentFailed { name, error, .. } => {
                    eprintln!("\n── {} failed: {}\n", name, error);
                }
                SwarmEvent::ConflictDetected { file, agents } => {
                    // Surface the conflict live so a user watching the
                    // run sees overlapping work while the merge step
                    // is still ahead of them, not only in the final
                    // answer.
                    eprintln!("\n⚠ conflict: {} written by {}\n", file, agents.join(", "));
                }
                SwarmEvent::AgentRetrying {
                    id: _,
                    name,
                    attempt,
                    max_attempts,
                    previous_error,
                } => {
                    println!(
                        "\n── {} retrying ({}/{}): {} ──\n",
                        name, attempt, max_attempts, previous_error,
                    );
                }
                SwarmEvent::Merging => {
                    println!("\n── merging results ──\n");
                }
                SwarmEvent::WorktreeCreated {
                    agent_name,
                    path,
                    branch,
                } => {
                    println!(
                        "── {}: worktree {} (branch {})",
                        agent_name,
                        path.display(),
                        branch,
                    );
                }
                SwarmEvent::WorktreesMerged {
                    merged,
                    conflicted,
                    failed,
                } => {
                    if conflicted.is_empty() && failed.is_empty() {
                        println!("\n── worktrees merged: {} ok ──\n", merged.len());
                    } else {
                        println!(
                            "\n── worktrees merged: {} ok, {} conflict(s), {} failed ──",
                            merged.len(),
                            conflicted.len(),
                            failed.len(),
                        );
                        for f in &conflicted {
                            println!("   ⚠ conflict: {}", f.display());
                        }
                        for (branch, err) in &failed {
                            println!("   ✗ {}: {}", branch, err);
                        }
                        println!();
                    }
                }
            }
        }
    });

    let result = runner.run(&goal, &tx).await;
    drop(tx);
    let _ = print_task.await;

    // H-C9: shut down before propagating. The two error sources
    // (runner.run and from_config) both skipped shutdown before.
    let resp = match result {
        Ok(r) => r,
        Err(e) => {
            let _ = engine.shutdown().await;
            return Err(e);
        }
    };

    if !resp.conflicts.is_empty() {
        println!("\n{} file conflict(s):", resp.conflicts.len());
        for c in &resp.conflicts {
            println!("  ⚠ {} — written by {}", c.file, c.agents.join(", "));
        }
    }

    println!("\n================ merged ================\n");
    println!("{}", resp.merged);
    if !resp.merged_by_model {
        println!("\n(merged by concatenation — LLM synthesis was disabled or failed)");
    }

    engine.shutdown().await?;
    Ok(())
}

pub(super) fn print_swarm_event(v: &serde_json::Value) {
    let data = v.get("data").cloned().unwrap_or(serde_json::Value::Null);
    let kind = data.get("kind").and_then(|k| k.as_str()).unwrap_or("");
    match kind {
        "decomposed" => {
            let subs = data
                .get("subtasks")
                .and_then(|s| s.as_array())
                .cloned()
                .unwrap_or_default();
            println!("\nDecomposed into {} subtasks:", subs.len());
            for (i, s) in subs.iter().enumerate() {
                let name = s.get("name").and_then(|x| x.as_str()).unwrap_or("?");
                let desc = s.get("description").and_then(|x| x.as_str()).unwrap_or("");
                println!("  {}. {} \u{2014} {}", i + 1, name, desc);
            }
            println!();
        }
        "agent_started" => {
            let name = data.get("name").and_then(|x| x.as_str()).unwrap_or("?");
            let subtask = data.get("subtask").and_then(|x| x.as_str()).unwrap_or("");
            println!(
                "\u{2500}\u{2500} {} starts on: {}",
                name,
                subtask.lines().next().unwrap_or("")
            );
        }
        "agent_chunk" => {
            if let Some(text) = data.get("text").and_then(|x| x.as_str()) {
                print!("{}", text);
                let _ = std::io::stdout().flush();
            }
        }
        "agent_completed" => {
            let name = data.get("name").and_then(|x| x.as_str()).unwrap_or("?");
            println!("\n\u{2500}\u{2500} {} done\n", name);
        }
        "agent_failed" => {
            let name = data.get("name").and_then(|x| x.as_str()).unwrap_or("?");
            let error = data.get("error").and_then(|x| x.as_str()).unwrap_or("");
            eprintln!("\n\u{2500}\u{2500} {} failed: {}\n", name, error);
        }
        "agent_retrying" => {
            let name = data.get("name").and_then(|x| x.as_str()).unwrap_or("?");
            let attempt = data.get("attempt").and_then(|x| x.as_u64()).unwrap_or(0);
            let max = data
                .get("max_attempts")
                .and_then(|x| x.as_u64())
                .unwrap_or(0);
            let prev = data
                .get("previous_error")
                .and_then(|x| x.as_str())
                .unwrap_or("");
            println!(
                "\n\u{2500}\u{2500} {} retrying ({}/{}): {} \u{2500}\u{2500}\n",
                name, attempt, max, prev
            );
        }
        "conflict_detected" => {
            let file = data.get("file").and_then(|x| x.as_str()).unwrap_or("?");
            let agents = data
                .get("agents")
                .and_then(|a| a.as_array())
                .map(|arr| {
                    arr.iter()
                        .filter_map(|x| x.as_str())
                        .collect::<Vec<_>>()
                        .join(", ")
                })
                .unwrap_or_default();
            eprintln!("\n\u{26a0} conflict: {} written by {}\n", file, agents);
        }
        "merging" => {
            println!("\n\u{2500}\u{2500} merging results \u{2500}\u{2500}\n");
        }
        "worktree_created" => {
            let agent = data
                .get("agent_name")
                .and_then(|x| x.as_str())
                .unwrap_or("?");
            let path = data.get("path").and_then(|x| x.as_str()).unwrap_or("");
            let branch = data.get("branch").and_then(|x| x.as_str()).unwrap_or("");
            println!(
                "\u{2500}\u{2500} {}: worktree {} (branch {})",
                agent, path, branch
            );
        }
        "worktrees_merged" => {
            let merged = data
                .get("merged")
                .and_then(|a| a.as_array())
                .map(|a| a.len())
                .unwrap_or(0);
            let conflicted = data
                .get("conflicted")
                .and_then(|a| a.as_array())
                .map(|a| a.len())
                .unwrap_or(0);
            let failed = data
                .get("failed")
                .and_then(|a| a.as_array())
                .map(|a| a.len())
                .unwrap_or(0);
            if conflicted == 0 && failed == 0 {
                println!(
                    "\n\u{2500}\u{2500} worktrees merged: {} ok \u{2500}\u{2500}\n",
                    merged
                );
            } else {
                println!(
                    "\n\u{2500}\u{2500} worktrees merged: {} ok, {} conflict(s), {} failed \u{2500}\u{2500}\n",
                    merged, conflicted, failed
                );
            }
        }
        _ => {}
    }
}
