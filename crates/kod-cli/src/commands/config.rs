//! `kod config` subcommands: show / path / edit / migrate / validate.

use super::*;

pub async fn run_config_display() -> Result<()> {
    let config = KodConfig::load_default()?;

    println!("KOD Configuration:");
    println!();
    println!("LLM:");
    {
        let e = config.llm.default_endpoint();
        println!("  Provider: {:?}", e.provider);
    }
    println!("  Model: {}", config.llm.default_endpoint().model);
    println!("  Base URL: {}", config.llm.default_endpoint().base_url);
    println!(
        "  Context Window: {}",
        config.llm.default_endpoint().context_window
    );
    println!(
        "  Max Tokens: {}",
        config.llm.default_endpoint().max_tokens.unwrap_or(2048)
    );
    println!(
        "  Temperature: {}",
        config.llm.default_endpoint().temperature.unwrap_or(0.7)
    );
    println!(
        "  Network access: {}",
        if config.llm.network_access {
            "enabled (web_fetch can reach the network)"
        } else {
            "disabled"
        }
    );
    println!("  Write approval: policy-gated (see [tools] and .kod/policy.toml)");
    println!(
        "  Auto-check: {}",
        if config.tools.auto_check {
            "enabled (a write triggers a project check; diagnostics go to the model)"
        } else {
            "disabled"
        }
    );
    println!();
    println!("Memory:");
    println!(
        "  Short-Term Capacity: {}",
        config.memory.short_term_capacity
    );
    println!(
        "  Scope: {} (project-scoped memory lives at <cwd>/.kod/memory.redb)",
        match config.memory.scope {
            kod_config::MemoryScope::Global => "global",
            kod_config::MemoryScope::Project => "project",
        }
    );
    println!(
        "  Max Skills Per Query: {}",
        config.skills.max_skills_per_query
    );

    // Every path KOD reads or writes, in one place.
    //
    // The three subsystems do not agree on a root directory: config
    // uses the platform-native location (dirs::config_dir, e.g.
    // ~/Library/Application Support/kod on macOS, ~/.config/kod on
    // Linux); the memory database and TUI session state use ~/.kod/;
    // skill discovery checks four conventions including the
    // Claude-style ~/.agents/skills. Rather than re-home any of them
    // — each choice is defensible, and moving a directory breaks
    // existing installs — name them here, once, so a user asking
    // "where does KOD put things?" does not have to know which
    // subsystem follows which convention.
    println!();
    println!("Paths:");

    // Config file.
    match KodConfig::config_dir() {
        Ok(dir) => {
            let p = dir.join("config.toml");
            let mark = if p.exists() { "✓" } else { "·" };
            println!("  {} config:  {}", mark, p.display());
        }
        Err(_) => println!("  · config:  (could not determine)"),
    }

    // Memory database.
    match config.memory_db_path() {
        Ok(p) => {
            let mark = if p.exists() { "✓" } else { "·" };
            println!("  {} memory:  {}", mark, p.display());
        }
        Err(_) => println!("  · memory:  (could not determine)"),
    }

    // TUI session + history (opt-in — they exist only after a TUI run).
    if let Some(p) = kod_tui::app::KodApp::session_path() {
        let mark = if p.exists() { "✓" } else { "·" };
        println!("  {} session: {}", mark, p.display());
    }
    if let Some(p) = kod_tui::app::KodApp::history_path() {
        let mark = if p.exists() { "✓" } else { "·" };
        println!("  {} history: {}", mark, p.display());
    }

    // Skills directories — one line each, marked the same way. The
    // label is first-match-wins order used by discovery, so a user
    // reading the list sees the shadowing rules.
    match config.skills_dirs() {
        Ok(dirs) => {
            for (i, d) in dirs.iter().enumerate() {
                let mark = if d.is_dir() { "✓" } else { "·" };
                let label = if i == 0 { "skills: " } else { "        " };
                println!("  {} {}{}", mark, label, d.display());
            }
        }
        Err(_) => println!("  · skills:  (could not determine)"),
    }
    println!("  (✓ = exists, · = not present; skills are scanned top to bottom)");

    Ok(())
}

pub async fn run_config_path() -> Result<()> {
    let dir = KodConfig::config_dir()?;
    println!("{}", dir.join("config.toml").display());
    Ok(())
}

pub async fn run_config_edit() -> Result<()> {
    let dir = KodConfig::config_dir()?;
    let path = dir.join("config.toml");
    // Ensure the file exists so `$EDITOR` opens something.
    if !path.exists() {
        let config = KodConfig::load_default()?;
        config.save_to(&path)?;
    }

    let candidates: Vec<String> = ["EDITOR", "VISUAL"]
        .iter()
        .filter_map(|k| std::env::var(k).ok())
        .filter(|s| !s.trim().is_empty())
        .chain(std::iter::once("vi".to_string()))
        .chain(std::iter::once("nano".to_string()))
        .collect();

    for editor in candidates {
        // `sh -c` so a value like `code --wait` works.
        let status = std::process::Command::new("sh")
            .arg("-c")
            .arg(format!(
                "{} {}",
                editor,
                shell_quote(&path.to_string_lossy())
            ))
            .status();
        match status {
            Ok(s) if s.success() => return Ok(()),
            Ok(s) => {
                return Err(KodError::Internal(format!(
                    "editor {:?} exited with status {:?}",
                    editor,
                    s.code()
                )));
            }
            Err(_) => continue,
        }
    }
    Err(KodError::Internal(
        "no editor found — set $EDITOR or install vi/nano".to_string(),
    ))
}

pub async fn run_config_migrate(dry_run: bool) -> Result<()> {
    let dir = KodConfig::config_dir()?;
    std::fs::create_dir_all(&dir).map_err(KodError::Io)?;
    let path = dir.join("config.toml");

    if !path.exists() {
        let fresh = KodConfig {
            config_version: 2,
            ..KodConfig::default()
        };
        if dry_run {
            let s = toml::to_string_pretty(&fresh)
                .map_err(|e| KodError::Serialization(e.to_string()))?;
            print!("{}", s);
            return Ok(());
        }
        fresh.save_to(&path)?;
        println!(
            "No config found; wrote a fresh v2 default to {}.",
            path.display()
        );
        return Ok(());
    }

    let mut cfg = KodConfig::load_from(&path)?;
    let original = std::fs::read_to_string(&path).map_err(KodError::Io)?;

    if !cfg.needs_migration() {
        println!(
            "{} is already at config_version = {}; nothing to migrate.",
            path.display(),
            cfg.effective_version()
        );
        return Ok(());
    }

    cfg.config_version = 2;

    let migrated =
        toml::to_string_pretty(&cfg).map_err(|e| KodError::Serialization(e.to_string()))?;

    if dry_run {
        eprintln!(
            "# dry run: would back up {} and write the config below",
            path.display()
        );
        print!("{}", migrated);
        return Ok(());
    }

    let ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let backup = dir.join(format!("config.toml.bak-{ts}"));
    std::fs::write(&backup, original.as_bytes()).map_err(KodError::Io)?;

    let tmp = dir.join(format!("config.toml.migrate-{ts}.tmp"));
    std::fs::write(&tmp, migrated.as_bytes()).map_err(KodError::Io)?;
    std::fs::rename(&tmp, &path).map_err(KodError::Io)?;

    println!("Migrated {} to config_version = 2.", path.display());
    println!("Backup: {}", backup.display());
    println!();
    println!("The v2 shape is:");
    println!("  - [[llm.endpoints]] blocks with an explicit `name`,");
    println!("    `provider`, `base_url`, and `model` — a v1 config's flat");
    println!("    fields are now one endpoint named `default`.");
    println!("  - [llm.routing] for per-task-type endpoints (absent here;");
    println!("    every task routes to `default` until you add one).");
    println!("  - [mcp.servers.*] for MCP plugins (absent unless you added them).");
    println!();
    println!("Use `kod config show-merged` to see the effective v2 shape.");
    Ok(())
}

pub async fn run_config_validate() -> Result<()> {
    let dir = KodConfig::config_dir()?;
    let path = dir.join("config.toml");

    if !path.exists() {
        println!(
            "No config file at {} — a default will be created on the next run.",
            path.display()
        );
        return Ok(());
    }

    match KodConfig::load_from(&path) {
        Ok(cfg) => {
            println!("{}: valid.", path.display());
            println!(
                "  model = {:?}, base_url = {:?}, context_window = {}",
                cfg.llm.default_endpoint().model,
                cfg.llm.default_endpoint().base_url,
                cfg.llm.default_endpoint().context_window,
            );
            if !cfg.commands.is_empty() {
                println!(
                    "  custom commands: {}",
                    cfg.commands.keys().cloned().collect::<Vec<_>>().join(", ")
                );
            }
            Ok(())
        }
        Err(e) => {
            eprintln!("{}: INVALID.", path.display());
            eprintln!("  {}", e);
            eprintln!();
            eprintln!("Fix the file, or delete it to fall back to defaults:");
            eprintln!("  rm {}", path.display());
            std::process::exit(1);
        }
    }
}

pub(super) fn shell_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}
