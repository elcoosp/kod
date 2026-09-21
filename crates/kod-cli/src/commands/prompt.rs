//! Non-interactive entry points: `kod prompt`, `kod run`, and the
//! `--remote` variant. These produce output on stdout and exit; they
//! are the scripted interface, not the interactive REPL.

use super::*;

pub async fn run_prompt_remote(prompt: String, socket: Option<std::path::PathBuf>) -> Result<()> {
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

    let input = if prompt.trim() == "-" {
        use std::io::Read;
        let mut buf = String::new();
        std::io::stdin()
            .read_to_string(&mut buf)
            .map_err(KodError::Io)?;
        buf
    } else {
        prompt
    };
    if input.trim().is_empty() {
        return Err(KodError::Config("empty prompt".to_string()));
    }

    let socket_path = socket.unwrap_or_else(kod_core::serve::default_socket_path);
    let stream = tokio::net::UnixStream::connect(&socket_path)
        .await
        .map_err(|e| {
            KodError::InvalidState(format!(
                "could not connect to daemon at {}: {e}. \
                 Start one with `kod serve`.",
                socket_path.display()
            ))
        })?;
    let (read_half, mut write_half) = stream.into_split();

    let req = serde_json::json!({
        "v": 1,
        "id": "prompt-1",
        "method": "process_streaming",
        "params": { "input": input, "transcript_key": "" },
    });
    let mut line =
        serde_json::to_string(&req).map_err(|e| KodError::Serialization(e.to_string()))?;
    line.push('\n');
    write_half
        .write_all(line.as_bytes())
        .await
        .map_err(KodError::Io)?;
    write_half.flush().await.map_err(KodError::Io)?;

    let mut reader = BufReader::new(read_half).lines();
    let mut errored = false;
    while let Some(line) = reader.next_line().await.map_err(KodError::Io)? {
        let v: serde_json::Value = match serde_json::from_str(&line) {
            Ok(v) => v,
            Err(_) => continue,
        };
        // Ignore responses for other ids (there are none today, but
        // the protocol allows them).
        if v.get("id").and_then(|x| x.as_str()) != Some("prompt-1") {
            continue;
        }
        match v.get("type").and_then(|t| t.as_str()) {
            Some("chunk") => {
                if let Some(data) = v.get("data").and_then(|d| d.as_str()) {
                    // The daemon sends every engine chunk verbatim,
                    // including `\0kod-*` markers. The CLI drops the
                    // markers (as it does for the embedded path)
                    // and prints only the text.
                    if is_control_marker(data) {
                        continue;
                    }
                    print!("{data}");
                    let _ = std::io::stdout().flush();
                }
            }
            Some("done") => {
                println!();
                return Ok(());
            }
            Some("error") => {
                let msg = v
                    .get("data")
                    .and_then(|d| d.get("message"))
                    .and_then(|m| m.as_str())
                    .unwrap_or("(no message)");
                eprintln!("daemon error: {msg}");
                errored = true;
                break;
            }
            _ => {}
        }
    }
    if errored {
        std::process::exit(1);
    }
    Ok(())
}

pub async fn run_prompt(
    prompt: String,
    model: Option<String>,
    no_log: bool,
    sandbox: bool,
) -> Result<()> {
    let input = if prompt.trim() == "-" {
        use std::io::Read;
        let mut buf = String::new();
        std::io::stdin()
            .read_to_string(&mut buf)
            .map_err(KodError::Io)?;
        buf
    } else {
        prompt
    };

    if input.trim().is_empty() {
        return Err(KodError::Config("empty prompt".to_string()));
    }

    let config = KodConfig::load_default()?;

    // S10: one bootstrap for every engine-building command. The P0-1
    // bug (policy installed on three of seven paths) is exactly what
    // the pre-extraction duplication produced.
    let engine = engine_from_config(
        &config,
        EngineBootstrapOptions {
            model_override: model.as_deref(),
            cli_preset: None,
            require_sandbox: sandbox,
            install_mcp: true,
            db_path: None,
            install_embedder: true,
            install_policy: true,
        },
    )
    .await?;

    engine.start().await?;

    install_session_recorder(&engine, no_log);
    install_jev(&engine, &config);

    let resp = engine.process(&input).await?;
    let text = resp.text.unwrap_or_default();

    // Print only the reply to stdout — a script gets exactly what it
    // asked for. Anything else goes to stderr.
    println!("{}", text.trim_end());

    engine.shutdown().await?;
    Ok(())
}

pub async fn run_streaming_prompt(prompt: String, model: Option<String>) -> Result<()> {
    let input = if prompt.trim() == "-" {
        use std::io::Read;
        let mut buf = String::new();
        std::io::stdin()
            .read_to_string(&mut buf)
            .map_err(KodError::Io)?;
        buf
    } else {
        prompt
    };
    if input.trim().is_empty() {
        return Err(KodError::Config("empty prompt".to_string()));
    }

    let config = KodConfig::load_default()?;

    // S10: shared bootstrap.
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

    let (tx, mut rx) = tokio::sync::mpsc::channel::<String>(64);
    let pump = tokio::spawn(async move {
        use std::io::Write;
        while let Some(chunk) = rx.recv().await {
            // Skip control markers.
            if kod_core::engine::parse_tool_start(&chunk).is_some()
                || kod_core::engine::parse_tool_args(&chunk).is_some()
                || kod_core::engine::parse_tool_done(&chunk).is_some()
                || kod_core::engine::parse_tool_approval(&chunk).is_some()
                || kod_core::engine::parse_question(&chunk).is_some()
                || kod_core::engine::is_thinking_marker(&chunk)
            {
                continue;
            }
            print!("{}", chunk);
            let _ = std::io::stdout().flush();
        }
    });

    // H-C2: capture the result; the pre-fix `let _ =` swallowed any
    // failure and the process exited 0, breaking the documented
    // `kod run ... | tee` scripting contract.
    let run_result = engine.process_streaming(&input, &tx).await;
    drop(tx);
    let _ = pump.await;
    println!();
    // H-C2: shut down cleanly, then propagate the run outcome. A
    // failure now exits non-zero after the stream has flushed.
    engine.shutdown().await?;
    run_result.map(|_| ())
}

pub(super) fn is_control_marker(s: &str) -> bool {
    s.starts_with('\0')
}

pub(super) fn newest_session_log() -> Result<Option<std::path::PathBuf>> {
    let Some(home) = dirs::home_dir() else {
        return Ok(None);
    };
    let dir = home.join(".kod").join("sessions");
    if !dir.is_dir() {
        return Ok(None);
    }
    let mut entries: Vec<(std::time::SystemTime, std::path::PathBuf)> = Vec::new();
    for e in std::fs::read_dir(&dir).map_err(KodError::Io)? {
        let e = e.map_err(KodError::Io)?;
        let path = e.path();
        if path.extension().and_then(|s| s.to_str()) != Some("jsonl") {
            continue;
        }
        let meta = match e.metadata() {
            Ok(m) => m,
            Err(_) => continue,
        };
        let mtime = meta.modified().unwrap_or(std::time::UNIX_EPOCH);
        entries.push((mtime, path));
    }
    entries.sort_by_key(|e| std::cmp::Reverse(e.0));
    Ok(entries.into_iter().next().map(|(_, p)| p))
}
