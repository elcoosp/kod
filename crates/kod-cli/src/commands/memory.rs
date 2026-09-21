//! `kod memory`, `kod sessions`, `kod replay`, `kod checkpoint`.
//!
//! Grouped because they all read or write the persistent stores
//! (redb, JSONL session logs, checkpoint snapshots). Shared helpers
//! (`preview_line`, `render_session_markdown`, `count_roles`) live
//! here too — they have no other callers.

use super::*;

pub async fn run_memory(action: MemoryAction) -> Result<()> {
    use kod_memory::MemoryManager;
    let config = KodConfig::load_default()?;
    let path = config.memory_db_path()?;
    let manager = MemoryManager::new(path, config.memory.short_term_capacity)?;

    match action {
        MemoryAction::Add { content, tags } => {
            let tag_list: Vec<String> = tags
                .as_deref()
                .map(|s| {
                    s.split(',')
                        .map(|t| t.trim().to_string())
                        .filter(|t| !t.is_empty())
                        .collect()
                })
                .unwrap_or_default();
            let project_key = std::env::current_dir()
                .ok()
                .map(|cwd| kod_core::TaskRouter::project_key_for(&cwd));
            match manager
                .store_with_metadata(
                    kod_types::MemoryType::LongTerm,
                    content.trim(),
                    kod_types::MemoryMetadata {
                        tags: tag_list,
                        project_key,
                        ..Default::default()
                    },
                )
                .await
            {
                Ok(id) => println!(
                    "Added memory entry {} ({} chars).",
                    &id.as_uuid().to_string()[..8],
                    content.len(),
                ),
                Err(e) => {
                    eprintln!("Could not store memory entry: {e}");
                    std::process::exit(1);
                }
            }
            Ok(())
        }
        MemoryAction::Forget { key } => {
            let all = manager.get_all_long_term().await?;
            let trimmed = key.trim();
            let matching: Vec<_> = all
                .iter()
                .filter(|e| {
                    e.id.as_uuid().to_string().starts_with(trimmed)
                        || e.metadata.tags.iter().any(|t| t == trimmed)
                })
                .collect();
            if matching.is_empty() {
                println!("No entries match {:?} (id prefix or tag).", key);
            } else {
                let n = matching.len();
                for e in &matching {
                    let _ = manager.remove(kod_types::MemoryType::LongTerm, &e.id).await;
                }
                println!("Forgot {} entr{}.", n, if n == 1 { "y" } else { "ies" },);
            }
            Ok(())
        }
        MemoryAction::Export { path: dest } => {
            let all = manager.get_all_long_term().await?;
            let arr: Vec<serde_json::Value> = all
                .iter()
                .map(|e| {
                    serde_json::json!({
                        "content": e.content,
                        "relevance": e.relevance,
                        "timestamp": e.timestamp.to_string(),
                    })
                })
                .collect();
            let s = serde_json::to_string_pretty(&arr)
                .map_err(|e| KodError::Serialization(e.to_string()))?;
            if dest.as_os_str() == "-" {
                println!("{}", s);
            } else {
                if let Some(parent) = dest.parent()
                    && !parent.as_os_str().is_empty()
                {
                    std::fs::create_dir_all(parent).map_err(KodError::Io)?;
                }
                std::fs::write(&dest, s.as_bytes()).map_err(KodError::Io)?;
                println!(
                    "Exported {} long-term entr{} to {}",
                    all.len(),
                    if all.len() == 1 { "y" } else { "ies" },
                    dest.display(),
                );
            }
            Ok(())
        }
        MemoryAction::Import { path: src } => {
            let raw = if src.as_os_str() == "-" {
                use std::io::Read;
                let mut buf = String::new();
                std::io::stdin()
                    .read_to_string(&mut buf)
                    .map_err(KodError::Io)?;
                buf
            } else {
                std::fs::read_to_string(&src).map_err(KodError::Io)?
            };
            let arr: Vec<serde_json::Value> =
                serde_json::from_str(&raw).map_err(|e| KodError::Deserialization(e.to_string()))?;
            let mut added = 0usize;
            for v in &arr {
                if let Some(content) = v.get("content").and_then(|c| c.as_str()) {
                    let _ = manager
                        .store(kod_types::MemoryType::LongTerm, content)
                        .await;
                    added += 1;
                }
            }
            println!(
                "Imported {} long-term entr{}.",
                added,
                if added == 1 { "y" } else { "ies" }
            );
            Ok(())
        }
        MemoryAction::List => {
            let all = manager.get_all_long_term().await?;
            if all.is_empty() {
                println!("No long-term memory entries.");
                return Ok(());
            }
            println!("Long-term memory ({} entries):", all.len());
            for e in &all {
                let short = &e.id.as_uuid().to_string()[..8];
                println!(
                    "  {}  {:.2}  {}",
                    short,
                    e.relevance,
                    preview_line(&e.content, 100),
                );
            }
            Ok(())
        }
        MemoryAction::Search { query } => {
            let hits = manager.search(&query).await?;
            if hits.is_empty() {
                println!("No entries match {:?}.", query);
                return Ok(());
            }
            println!("{} match(es) for {:?}:", hits.len(), query);
            for e in &hits {
                let short = &e.id.as_uuid().to_string()[..8];
                println!("  {}  {}", short, preview_line(&e.content, 120));
            }
            Ok(())
        }
        MemoryAction::Delete { id } => {
            let all = manager.get_all_long_term().await?;
            let full = all
                .iter()
                .find(|e| e.id.as_uuid().to_string().starts_with(&id));
            match full {
                Some(entry) => {
                    manager
                        .remove(kod_types::MemoryType::LongTerm, &entry.id)
                        .await?;
                    println!("Deleted {}", &entry.id.as_uuid().to_string()[..8]);
                }
                None => {
                    eprintln!("No entry with id prefix {:?}.", id);
                    std::process::exit(1);
                }
            }
            Ok(())
        }
        MemoryAction::Clear { yes } => {
            if !yes {
                eprint!("Delete all long-term memory entries? This cannot be undone. [y/N] ");
                use std::io::Write;
                let _ = std::io::stderr().flush();
                let mut line = String::new();
                if std::io::stdin().read_line(&mut line).is_err() {
                    eprintln!("(input error — aborting)");
                    std::process::exit(1);
                }
                if !matches!(line.trim().to_lowercase().as_str(), "y" | "yes") {
                    println!("Aborted.");
                    return Ok(());
                }
            }
            let all = manager.get_all_long_term().await?;
            for e in &all {
                let _ = manager.remove(kod_types::MemoryType::LongTerm, &e.id).await;
            }
            println!(
                "Deleted {} entr{}.",
                all.len(),
                if all.len() == 1 { "y" } else { "ies" }
            );
            Ok(())
        }
    }
}

pub async fn run_sessions(action: SessionsAction) -> Result<()> {
    use kod_tui::app::Message;

    let path = kod_tui::app::KodApp::session_path().ok_or_else(|| {
        KodError::Config(
            "Could not determine the session file path (no home directory, no $KOD_TUI_STATE_DIR)."
                .to_string(),
        )
    })?;

    match action {
        SessionsAction::Show => {
            if !path.exists() {
                println!("No saved session at {}.", path.display());
                println!();
                println!("A session is written after your first reply in `kod tui`.");
                return Ok(());
            }
            let raw = std::fs::read_to_string(&path).map_err(KodError::Io)?;
            let messages: Vec<Message> = serde_json::from_str(&raw)
                .map_err(|e| KodError::Deserialization(format!("{}: {}", path.display(), e)))?;
            let (users, assistants, others) = count_roles(&messages);
            println!("Session: {}", path.display());
            println!("Size:    {} bytes", raw.len());
            println!(
                "Messages: {} total ({} user, {} assistant, {} other)",
                messages.len(),
                users,
                assistants,
                others,
            );
            if let (Some(first), Some(last)) = (messages.first(), messages.last()) {
                println!();
                println!(
                    "First:   [{}] {}",
                    first.timestamp.format("%Y-%m-%d %H:%M:%S"),
                    preview(&first.content, 60)
                );
                println!(
                    "Last:    [{}] {}",
                    last.timestamp.format("%Y-%m-%d %H:%M:%S"),
                    preview(&last.content, 60)
                );
            }
            Ok(())
        }
        SessionsAction::Clear => {
            if !path.exists() {
                println!("No saved session at {} — nothing to clear.", path.display());
                return Ok(());
            }
            std::fs::remove_file(&path).map_err(KodError::Io)?;
            println!("Deleted {}", path.display());
            Ok(())
        }
        SessionsAction::Count => {
            let home = dirs::home_dir()
                .ok_or_else(|| KodError::Config("no home directory".to_string()))?;
            let dir = home.join(".kod").join("sessions");
            if !dir.is_dir() {
                println!("0 session logs (no directory at {}).", dir.display());
                return Ok(());
            }
            let mut count = 0usize;
            let mut total_bytes = 0u64;
            let mut earliest: Option<std::path::PathBuf> = None;
            let mut newest: Option<std::path::PathBuf> = None;
            for e in std::fs::read_dir(&dir).map_err(KodError::Io)? {
                let e = e.map_err(KodError::Io)?;
                let p = e.path();
                if p.extension().and_then(|s| s.to_str()) != Some("jsonl") {
                    continue;
                }
                count += 1;
                total_bytes += e.metadata().map(|m| m.len()).unwrap_or(0);
                earliest = Some(match earliest {
                    Some(prev) if prev < p => prev,
                    _ => p.clone(),
                });
                newest = Some(match newest {
                    Some(prev) if prev > p => prev,
                    _ => p,
                });
            }
            println!("Session logs: {}", count);
            println!("Total size:   {} bytes", total_bytes);
            if let (Some(e), Some(n)) = (&earliest, &newest) {
                println!("Oldest:       {}", e.display());
                println!("Newest:       {}", n.display());
            }
            Ok(())
        }
        SessionsAction::Latest => {
            let home = dirs::home_dir()
                .ok_or_else(|| KodError::Config("no home directory".to_string()))?;
            let dir = home.join(".kod").join("sessions");
            if !dir.is_dir() {
                eprintln!("No session logs at {}.", dir.display());
                std::process::exit(1);
            }
            let mut newest: Option<std::path::PathBuf> = None;
            for e in std::fs::read_dir(&dir).map_err(KodError::Io)? {
                let e = e.map_err(KodError::Io)?;
                let p = e.path();
                if p.extension().and_then(|s| s.to_str()) != Some("jsonl") {
                    continue;
                }
                newest = Some(match newest {
                    Some(prev) if prev > p => prev,
                    _ => p,
                });
            }
            match newest {
                Some(p) => {
                    println!("{}", p.display());
                    Ok(())
                }
                None => {
                    eprintln!("No .jsonl files in {}.", dir.display());
                    std::process::exit(1);
                }
            }
        }
        SessionsAction::Import { path: src } => {
            let raw = std::fs::read_to_string(&src).map_err(KodError::Io)?;
            // Validate parse before touching the destination.
            let messages: Vec<Message> = serde_json::from_str(&raw)
                .map_err(|e| KodError::Deserialization(format!("{}: {}", src.display(), e)))?;
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent).map_err(KodError::Io)?;
            }
            // Write via a temp + rename so a crash mid-write cannot
            // leave a partial session file.
            let tmp = path.with_extension("json.import.tmp");
            std::fs::write(&tmp, raw.as_bytes()).map_err(KodError::Io)?;
            std::fs::rename(&tmp, &path).map_err(KodError::Io)?;
            println!(
                "Imported {} message(s) from {} into {}",
                messages.len(),
                src.display(),
                path.display(),
            );
            Ok(())
        }
        SessionsAction::Export { path: dest, format } => {
            let raw = std::fs::read_to_string(&path).map_err(KodError::Io)?;
            let messages: Vec<Message> = serde_json::from_str(&raw)
                .map_err(|e| KodError::Deserialization(format!("{}: {}", path.display(), e)))?;

            let rendered = match format.to_lowercase().as_str() {
                "json" => serde_json::to_string_pretty(&messages)
                    .map_err(|e| KodError::Serialization(e.to_string()))?,
                "markdown" | "md" | "" => render_session_markdown(&messages),
                other => {
                    return Err(KodError::Config(format!(
                        "Unknown format {:?}. Use `markdown` or `json`.",
                        other
                    )));
                }
            };

            if dest.as_os_str() == "-" {
                print!("{}", rendered);
            } else {
                if let Some(parent) = dest.parent()
                    && !parent.as_os_str().is_empty()
                {
                    std::fs::create_dir_all(parent).map_err(KodError::Io)?;
                }
                std::fs::write(&dest, rendered.as_bytes()).map_err(KodError::Io)?;
                println!(
                    "Wrote {} message(s) as {} to {}",
                    messages.len(),
                    format.to_lowercase(),
                    dest.display()
                );
            }
            Ok(())
        }
    }
}

pub async fn run_replay(path: std::path::PathBuf, execute: bool, yes: bool) -> Result<()> {
    let entries = kod_core::session_log::read_session(&path)?;
    let tool_calls: Vec<_> = entries
        .iter()
        .filter_map(|e| match e {
            kod_core::session_log::SessionEntry::ToolCall {
                tool_name,
                arguments,
                result,
                duration_ms,
                holder,
                ..
            } => Some((
                tool_name.clone(),
                arguments.clone(),
                result.clone(),
                *duration_ms,
                holder.clone(),
            )),
            // The remaining variants — ModelFallback, Cost,
            // MemoryWrite, Approval, Diagnostics — are not tool
            // calls and have nothing to replay. The list grew with
            // AD-15; the wildcard is the point, not a workaround
            // for a compiler warning.
            _ => None,
        })
        .collect();

    if tool_calls.is_empty() {
        println!(
            "No tool calls in {}. (Only tool calls are recorded today; LLM calls are not.)",
            path.display()
        );
        return Ok(());
    }

    if !execute {
        println!(
            "{} tool call(s) in {} — dry run. Pass --execute to actually re-run them.\n",
            tool_calls.len(),
            path.display()
        );
        for (i, (name, args, _result, ms, holder)) in tool_calls.iter().enumerate() {
            println!("{:>3}. [{}] {} ({})", i + 1, holder, name, args);
            println!("     took {}ms", ms);
        }
        return Ok(());
    }

    // H-S1: re-running a log is destructive *iff* it contains a call
    // that can change the filesystem or spawn a process. A log made
    // only of read-only tools (`read_file`, `grep`, `list_files`,
    // `file_info`) has no side effects to gate — the earlier shape of
    // this check refused every replay without `--yes`, which was
    // both stricter than the threat model needs and inconsistent
    // with its own printed message. Destructive logs still require
    // an explicit acknowledgement.
    let destructive_names = ["execute_command", "write_file", "patch_file"];
    let destructive: Vec<_> = tool_calls
        .iter()
        .enumerate()
        .filter(|(_, (name, _, _, _, _))| destructive_names.contains(&name.as_str()))
        .collect();
    if !yes && !destructive.is_empty() {
        println!(
            "Refusing to execute: {} destructive call(s) in {}.\n",
            destructive.len(),
            path.display(),
        );
        for (i, (name, args, _, _, _)) in &destructive {
            println!("  {}. {} ({})", i + 1, name, args);
        }
        println!("\nRe-run with --execute --yes to acknowledge and proceed.",);
        return Err(KodError::PermissionDenied {
            action: "replay --execute".to_string(),
            reason: "the --yes flag is required to re-run destructive tool calls".to_string(),
        });
    }

    // --yes is set; still summarise the destructive calls so the log
    // ends up in the terminal, not just in the JSONL.
    if !destructive.is_empty() {
        println!(
            "Re-running {} destructive call(s) from {}:",
            destructive.len(),
            path.display(),
        );
        for (i, (name, args, _, _, _)) in &destructive {
            println!("  {}. {} ({})", i + 1, name, args);
        }
        println!();
    }

    let config = KodConfig::load_default()?;
    let db_path = config.memory_db_path()?;
    // Design D2.1: build the embedder the memory subsystem will use
    // for semantic retrieval. `None` (the config default) leaves the
    // keyword+recency fallback in place; no retrieval path is broken
    // by an absent embedder.
    let embedder = kod_memory::embedding::from_config(
        &config.memory,
        Some(&config.llm.default_endpoint().base_url),
    );
    let router_config = RouterConfig {
        enable_memory: false,
        skill_threshold: config.skills.match_threshold,
        context_window: config.llm.default_endpoint().context_window,
        short_term_capacity: config.memory.short_term_capacity,
        embedder,
        ..RouterConfig::default()
    };
    let engine = KodEngine::new(router_config, db_path)?;
    // P0-1: install the PolicyEngine so the standard preset's
    // write-approval and deny rules actually gate tool calls.
    install_policy_async(&engine, &config, None).await?;

    engine.start().await?;

    let mut matched = 0usize;
    let mut mismatched = 0usize;
    for (i, (name, args, recorded, _ms, holder)) in tool_calls.iter().enumerate() {
        println!("\n[{}/{}] [{}] {}", i + 1, tool_calls.len(), holder, name);
        println!("     args: {}", args);
        match engine.run_tool(name, args.clone()).await {
            Ok(result) => {
                let fresh = match &result {
                    kod_types::ToolResult::Success(v) => serde_json::json!({ "success": v }),
                    kod_types::ToolResult::Error(e) => serde_json::json!({ "error": e }),
                    kod_types::ToolResult::RequiresConfirmation { description, .. } => {
                        serde_json::json!({ "requires_confirmation": description })
                    }
                };
                if &fresh == recorded {
                    matched += 1;
                    println!("     result: matches recorded");
                } else {
                    mismatched += 1;
                    println!("     result: DIFFERS from recorded");
                    println!("       recorded: {}", recorded);
                    println!("       fresh:    {}", fresh);
                }
            }
            Err(e) => {
                mismatched += 1;
                println!("     error: {}", e);
            }
        }
    }

    engine.shutdown().await?;
    println!();
    println!("{} matched, {} differed", matched, mismatched);
    Ok(())
}

pub async fn run_checkpoint(action: CheckpointAction) -> Result<()> {
    use kod_core::checkpoint::CheckpointManager;

    let cwd = std::env::current_dir()
        .map_err(|e| KodError::Config(format!("Could not determine working directory: {e}")))?;
    let manager = CheckpointManager::for_working_dir(&cwd).ok_or_else(|| {
        KodError::Config(
            "Could not determine a checkpoint directory — no home directory is available."
                .to_string(),
        )
    })?;

    match action {
        CheckpointAction::Diff { id } => {
            let snap = manager
                .find(&id)?
                .ok_or_else(|| KodError::InvalidParameters {
                    reason: format!("no checkpoint with id {id:?}"),
                })?;
            let now = std::fs::read_to_string(&snap.path).unwrap_or_default();
            let diff = kod_tools::patch::render_unified_diff(
                &snap.content,
                &now,
                &snap.path.display().to_string(),
            );
            if diff.trim().is_empty() {
                println!(
                    "{}: no difference between snapshot and current content.",
                    snap.path.display()
                );
            } else {
                print!("{diff}");
            }
            Ok(())
        }
        CheckpointAction::List { limit } => {
            let all = manager.list()?;
            if all.is_empty() {
                println!(
                    "No checkpoints for {}. A checkpoint is written before each write_file or patch_file.",
                    cwd.display()
                );
                return Ok(());
            }
            let shown: Vec<_> = all.iter().take(limit).collect();
            println!(
                "Checkpoints for {} ({} total, showing {}):",
                cwd.display(),
                all.len(),
                shown.len()
            );
            println!();
            for s in &shown {
                let when = format_timestamp_ms(s.taken_at_ms);
                let kind = if s.existed { "modify" } else { "create" };
                println!(
                    "  {}  {:<7} {:<12} {}",
                    s.id,
                    kind,
                    s.tool,
                    s.path.display()
                );
                println!("             taken {}", when);
            }
            if all.len() > shown.len() {
                println!();
                println!(
                    "… and {} older — use --limit to show more.",
                    all.len() - shown.len()
                );
            }
            Ok(())
        }
        CheckpointAction::Restore { id } => {
            let path = manager.restore(&id)?;
            println!("Restored {} from checkpoint {}.", path.display(), id);
            Ok(())
        }
        CheckpointAction::Clear => {
            let n = manager.clear()?;
            if n == 0 {
                println!("No checkpoints to clear.");
            } else {
                println!("Cleared {} checkpoint(s).", n);
            }
            Ok(())
        }
    }
}

pub(super) fn format_timestamp_ms(ms: u64) -> String {
    use std::time::{Duration, UNIX_EPOCH};
    let secs = ms / 1000;
    match UNIX_EPOCH.checked_add(Duration::from_secs(secs)) {
        Some(t) => {
            let dt: chrono::DateTime<chrono::Local> = t.into();
            dt.format("%Y-%m-%d %H:%M:%S").to_string()
        }
        None => format!("{ms} ms"),
    }
}

pub(super) fn count_roles(messages: &[kod_tui::app::Message]) -> (usize, usize, usize) {
    use kod_types::MessageRole;
    let mut users = 0;
    let mut assistants = 0;
    let mut others = 0;
    for m in messages {
        match m.role {
            MessageRole::User => users += 1,
            MessageRole::Assistant => assistants += 1,
            _ => others += 1,
        }
    }
    (users, assistants, others)
}

pub(super) fn render_session_markdown(messages: &[kod_tui::app::Message]) -> String {
    use kod_types::MessageRole;
    let mut out = String::from("# KOD session\n\n");
    for m in messages {
        let (label, fence) = match &m.role {
            MessageRole::User => ("## you", false),
            MessageRole::Assistant => ("## ai", true),
            MessageRole::System => ("## sys", false),
            MessageRole::Tool => ("## tool", true),
            MessageRole::Agent(id) => {
                out.push_str(&format!("## agent {}\n\n", id));
                out.push_str(&m.content);
                out.push_str("\n\n");
                continue;
            }
        };
        out.push_str(&format!(
            "{} · {}\n\n",
            label,
            m.timestamp.format("%Y-%m-%d %H:%M:%S"),
        ));
        if fence {
            out.push_str("```\n");
            out.push_str(&m.content);
            if !m.content.ends_with('\n') {
                out.push('\n');
            }
            out.push_str("```\n\n");
        } else {
            out.push_str(&m.content);
            out.push_str("\n\n");
        }
    }
    out
}

pub(super) fn preview_line(s: &str, max: usize) -> String {
    let one = s.lines().next().unwrap_or("");
    if one.chars().count() <= max {
        one.to_string()
    } else {
        let cut: String = one.chars().take(max).collect();
        format!("{cut}…")
    }
}
