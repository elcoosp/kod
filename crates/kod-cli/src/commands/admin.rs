//! Administrative subcommands: tui / doctor / init / models / update /
//! profile / policy / tests / map / serve / sandbox-exec.
//!
//! Grouped by "not the interactive chat path, not one of the domain
//! families (skills/config/memory/swarm)." These mostly inspect or
//! configure the workspace.

use super::*;

pub async fn run_tui(
    model: Option<String>,
    no_resume: bool,
    sandbox: bool,
    cli_preset: Option<String>,
) -> Result<()> {
    let mut tui = kod_tui::TuiLoop::new();
    if sandbox {
        tui.set_sandbox_mode(true);
    }
    tui.set_cli_preset(cli_preset);
    tui.set_no_resume(no_resume);
    tui.run(model).await
}

pub async fn run_doctor(json: bool) -> Result<()> {
    use kod_core::doctor::{CheckStatus, run_diagnostics};

    let config = KodConfig::load_default()?;
    let report = run_diagnostics(&config);

    if json {
        let value = report.to_json();
        let pretty = serde_json::to_string_pretty(&value)
            .map_err(|e| KodError::Serialization(e.to_string()))?;
        println!("{}", pretty);
        if report.has_failures() {
            std::process::exit(1);
        }
        return Ok(());
    }

    println!("KOD doctor");
    println!();
    for check in &report.checks {
        let mark = match check.status {
            CheckStatus::Ok => "✓",
            CheckStatus::Warn => "⚠",
            CheckStatus::Fail => "✗",
        };
        println!("  {} {:<14} {}", mark, check.name, check.message);
    }
    println!();

    if report.has_failures() {
        println!("One or more checks failed — review the items marked ✗ above.");
        std::process::exit(1);
    }

    println!("All checks passed.");
    Ok(())
}

pub async fn run_init(force: bool) -> Result<()> {
    let config = KodConfig::load_default()?;
    let config_dir = KodConfig::config_dir()?;
    let path = config_dir.join("config.toml");

    // Write the default config if absent, or rewrite it when `--force`
    // is passed. On --force the previous file is backed up first so a
    // user who ran it by mistake can restore their settings.
    if !path.exists() || force {
        if path.exists() {
            let ts = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs())
                .unwrap_or(0);
            let backup = config_dir.join(format!("config.toml.bak-{ts}"));
            if let Err(e) = std::fs::copy(&path, &backup) {
                eprintln!(
                    "Warning: could not back up {} to {}: {}",
                    path.display(),
                    backup.display(),
                    e
                );
            } else {
                println!("Backed up {} -> {}", path.display(), backup.display());
            }
        }
        let fresh = if force {
            KodConfig::default()
        } else {
            config.clone()
        };
        if let Err(e) = fresh.save_to(&path) {
            eprintln!("Warning: could not write {}: {}", path.display(), e);
        }
    }

    println!("KOD initialized.");
    println!();
    if path.exists() {
        println!("Config:   {}", path.display());
    } else {
        println!(
            "Config:   (in memory only — could not write {})",
            path.display()
        );
    }
    println!("Model:    {}", config.llm.default_endpoint().model);
    println!("Endpoint: {}", config.llm.default_endpoint().base_url);
    println!(
        "Network:  {}",
        if config.llm.network_access {
            "enabled (web_fetch can reach the network)"
        } else {
            "disabled (set llm.network_access = true to enable)"
        }
    );
    println!("Writes:   policy-gated (see [tools] and .kod/policy.toml)");
    println!();
    println!("Built-in model profiles:");
    for p in kod_config::profiles::PRESETS {
        println!("  {:<18} {}", p.name, p.description);
        println!("    model:    {}", p.model);
        if let Some(cmd) = p.install_command {
            println!("    install:  {}", cmd);
        }
    }
    println!();
    println!("Switch profiles with:  kod profile use <name>");
    println!();
    println!("Next steps:");
    println!("  1. Start the model server (e.g. `ollama serve`)");
    println!(
        "  2. Pull the model (e.g. `ollama pull {}`)",
        config.llm.default_endpoint().model
    );
    println!("  3. Verify the setup:  kod doctor");
    println!("  4. Start a session:   kod tui    (interactive)");
    println!("                        kod chat   (plain REPL)");
    println!();
    println!("Optional:");
    println!("  kod serve            long-lived daemon on a unix socket");
    println!("                       (`kod chat --remote` / `kod prompt --remote` attach)");
    println!("  kod policy show      the tool policy that gates every call");
    println!("  kod tools            every tool the model can call");
    println!("  kod mcp.servers.*    add MCP servers in config.toml under [mcp.servers.<name>]");
    println!("  kod config migrate   if this is an older config, move it to v2");
    Ok(())
}

pub async fn run_models(filter: Option<String>) -> Result<()> {
    let config = KodConfig::load_default()?;
    let (registry, default_model, _routing) = kod_core::build_registry(&config.llm, None)?;

    let provider = match registry.resolve(&default_model) {
        Ok(p) => p,
        Err(e) => {
            eprintln!(
                "Could not resolve provider for {}: {}",
                default_model.display(),
                e
            );
            std::process::exit(1);
        }
    };

    let models = match provider.list_models().await {
        Ok(m) => m,
        Err(e) => {
            eprintln!(
                "Could not list models from {}: {}",
                config.llm.default_endpoint().base_url,
                e
            );
            eprintln!();
            eprintln!("Check that the server is running and `base_url` in the config is correct.");
            eprintln!("For Ollama: `ollama serve`, then retry.");
            std::process::exit(1);
        }
    };

    let needle = filter.as_ref().map(|s| s.to_lowercase());
    let shown: Vec<&String> = match &needle {
        Some(n) => models
            .iter()
            .filter(|m| m.to_lowercase().contains(n))
            .collect(),
        None => models.iter().collect(),
    };

    if models.is_empty() {
        println!(
            "The provider at {} is reachable but reports no models.",
            config.llm.default_endpoint().base_url
        );
        println!();
        println!("Pull one first, e.g.:");
        println!("  ollama pull {}", config.llm.default_endpoint().model);
        return Ok(());
    }

    if shown.is_empty() {
        println!(
            "No model matches {:?} ({} model{} on the server).",
            filter.as_deref().unwrap_or(""),
            models.len(),
            if models.len() == 1 { "" } else { "s" },
        );
        return Ok(());
    }

    if let Some(n) = &needle {
        println!(
            "{} of {} model{} match {:?}:",
            shown.len(),
            models.len(),
            if models.len() == 1 { "" } else { "s" },
            n,
        );
    } else {
        println!(
            "{} model(s) on {}:",
            shown.len(),
            config.llm.default_endpoint().base_url
        );
    }
    for m in &shown {
        if m.as_str() == config.llm.default_endpoint().model {
            println!("  - {}  (current)", m);
        } else {
            println!("  - {}", m);
        }
    }
    Ok(())
}

pub async fn run_update() -> Result<()> {
    let current = env!("CARGO_PKG_VERSION");
    let repo = std::env::var("KOD_UPDATE_REPO").unwrap_or_else(|_| "elcoosp/kod".to_string());
    let url = format!("https://api.github.com/repos/{repo}/releases/latest");

    println!("Current: v{}", current);
    println!("Checking {} …", url);

    let client = reqwest::Client::builder()
        .user_agent(concat!("kod/", env!("CARGO_PKG_VERSION")))
        .timeout(std::time::Duration::from_secs(10))
        .build()
        .map_err(|e| KodError::Internal(format!("could not build http client: {e}")))?;

    let resp = match client.get(&url).send().await {
        Ok(r) => r,
        Err(e) => {
            eprintln!("Could not reach GitHub: {e}");
            eprintln!();
            eprintln!("The check requires network access. If you are offline or behind");
            eprintln!("a proxy, this command cannot help — check");
            eprintln!("  https://github.com/{repo}/releases");
            eprintln!("manually.");
            std::process::exit(1);
        }
    };

    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        let short = if body.len() > 300 {
            format!("{}…", kod_types::strutil::truncate_chars(&body, 300))
        } else {
            body
        };
        eprintln!("GitHub returned {status}: {short}");
        std::process::exit(1);
    }

    let body: serde_json::Value = resp
        .json()
        .await
        .map_err(|e| KodError::Provider(format!("invalid JSON: {e}")))?;

    let tag = body.get("tag_name").and_then(|t| t.as_str()).unwrap_or("");
    let html_url = body.get("html_url").and_then(|u| u.as_str()).unwrap_or("");
    let tag_clean = tag.trim_start_matches('v');

    if tag_clean.is_empty() {
        eprintln!("Release metadata is missing tag_name; cannot compare versions.");
        std::process::exit(1);
    }

    if versions_equal(current, tag_clean) || version_is_older(tag_clean, current) {
        println!();
        println!("You are on the latest release (v{}).", current);
        return Ok(());
    }

    println!();
    println!("A newer release is available: {} → {}", current, tag_clean);
    if !html_url.is_empty() {
        println!("Release notes: {}", html_url);
    }
    println!();
    println!("To update, reinstall from source:");
    println!("  cargo install --path crates/kod-cli --force");
    println!("Or download the release asset for your platform from the URL above.");
    Ok(())
}

pub async fn run_profile(action: ProfileAction) -> Result<()> {
    match action {
        ProfileAction::List { json } => {
            if json {
                let arr: Vec<serde_json::Value> = kod_config::profiles::PRESETS
                    .iter()
                    .map(|p| {
                        serde_json::json!({
                            "name": p.name,
                            "description": p.description,
                            "model": p.model,
                            "base_url": p.base_url,
                            "context_window": p.context_window,
                            "max_tokens": p.max_tokens,
                            "install_command": p.install_command,
                        })
                    })
                    .collect();
                let s = serde_json::to_string_pretty(&arr)
                    .map_err(|e| KodError::Serialization(e.to_string()))?;
                println!("{}", s);
            } else {
                println!("Built-in profiles:\n");
                for p in kod_config::profiles::PRESETS {
                    println!("  {:<18} {}", p.name, p.description);
                    println!("    model:          {}", p.model);
                    println!("    base_url:       {}", p.base_url);
                    println!("    context_window: {}", p.context_window);
                    if let Some(cmd) = p.install_command {
                        println!("    install:        {}", cmd);
                    }
                    println!();
                }
                println!("Switch with: kod profile use <name>");
            }
            Ok(())
        }
        ProfileAction::Show => {
            let config = KodConfig::load_default()?;
            println!("Effective [llm] config:");
            {
                let e = config.llm.default_endpoint();
                println!("  provider       = {:?}", e.provider);
            }
            println!(
                "  model          = \"{}\"",
                config.llm.default_endpoint().model
            );
            println!(
                "  base_url       = \"{}\"",
                config.llm.default_endpoint().base_url
            );
            println!(
                "  context_window = {}",
                config.llm.default_endpoint().context_window
            );
            println!(
                "  max_tokens     = {}",
                config.llm.default_endpoint().max_tokens.unwrap_or(2048)
            );
            println!(
                "  temperature    = {}",
                config.llm.default_endpoint().temperature.unwrap_or(0.7)
            );
            println!(
                "  timeout_secs   = {}",
                config.llm.default_endpoint().timeout_secs
            );
            Ok(())
        }
        ProfileAction::Use { name, dry_run } => {
            let profile = kod_config::profiles::by_name(&name).ok_or_else(|| {
                KodError::Config(format!(
                    "Unknown profile {:?}. Known profiles: {}",
                    name,
                    kod_config::profiles::names_csv()
                ))
            })?;
            let mut config = KodConfig::load_default()?;
            config.llm.default_endpoint_mut().model = profile.model.to_string();
            config.llm.default_endpoint_mut().base_url = profile.base_url.to_string();
            config.llm.default_endpoint_mut().context_window = profile.context_window;
            config.llm.default_endpoint_mut().max_tokens = Some(profile.max_tokens);
            let dir = KodConfig::config_dir()?;
            let path = dir.join("config.toml");
            if dry_run {
                println!(
                    "Dry run — would write profile {:?} to {}.",
                    name,
                    path.display()
                );
                println!("  model:          {}", profile.model);
                println!("  base_url:       {}", profile.base_url);
                println!("  context_window: {}", profile.context_window);
                println!("  max_tokens:     {}", profile.max_tokens);
                return Ok(());
            }
            config.save_to(&path)?;
            println!("Wrote profile {:?} to {}", name, path.display());
            println!("  model:          {}", profile.model);
            println!("  base_url:       {}", profile.base_url);
            println!("  context_window: {}", profile.context_window);
            if let Some(cmd) = profile.install_command {
                println!();
                println!("Next step (if not already installed):");
                println!("  {}", cmd);
            }
            Ok(())
        }
    }
}

pub async fn run_policy(action: PolicyAction) -> Result<()> {
    let config = KodConfig::load_default()?;
    let cwd = std::env::current_dir()
        .map_err(|e| KodError::Config(format!("could not determine cwd: {e}")))?;
    let policy = kod_config::PolicyEngine::load(&config, Some(&cwd), None)?;

    match action {
        PolicyAction::Show => {
            println!("Effective policy for {}", cwd.display());
            println!();
            println!("{}", policy.describe());
            println!();
            println!("Layer precedence (later overrides earlier):");
            println!("  1. preset                 (built-in default: standard)");
            println!("  2. global [tools]         (~/.config/kod/config.toml)");
            println!("  3. .kod/policy.toml       (this project, if present)");
            println!("  4. --preset CLI flag      (a session override)");
            println!();
            let project = cwd.join(".kod").join("policy.toml");
            if project.is_file() {
                println!("Project policy: {} (read)", project.display());
            } else {
                println!("Project policy: {} (not present)", project.display());
            }
        }
        PolicyAction::Forget { n } => {
            // The session deny rules live on the engine, but this CLI
            // invocation is a one-shot read of the policy layers — it
            // never constructs one. `kod serve`'s daemon is the
            // long-lived engine a rule would accumulate in; the CLI's
            // own `deny_rules` set is always empty. We therefore print
            // the rules the daemon would show when reachable, and say
            // so when it is not.
            //
            // A future `kod policy forget` that reaches the daemon (a
            // `set_policy_rule` NDJSON method) is a follow-up; today
            // the honest answer is the one below.
            let socket = kod_core::serve::default_socket_path();
            if !socket.exists() {
                println!("No daemon listening at {}.", socket.display());
                println!();
                println!("Session deny rules accumulate in a running `kod serve`");
                println!("daemon or an interactive `kod tui` session — this CLI");
                println!("invocation has no live engine to read them from.");
                println!();
                println!("To list and drop rules in the current session, use the");
                println!("TUI's `/policy` command (see /help) or restart the");
                println!("session, which clears the set.");
                return Ok(());
            }

            // A daemon is running. Today the daemon does not expose a
            // listing or a forget method over the socket; the honest
            // answer names the limitation and points at the TUI.
            match n {
                None => println!(
                    "A daemon is listening at {}, but this CLI does not yet",
                    socket.display(),
                ),
                Some(i) => println!(
                    "A daemon is listening at {}, but this CLI cannot drop rule {} over",
                    socket.display(),
                    i,
                ),
            }
            println!("the NDJSON protocol. Use the TUI's `/policy` command in the");
            println!("attached session, or restart the daemon (which clears the");
            println!("session's deny rules).");
        }
        PolicyAction::Explain { tool, args } => {
            let parsed = parse_kv_args(&args)?;
            let empty_denies = std::collections::HashSet::new();
            let decision = policy.decide(&tool, &parsed, &cwd, &empty_denies);
            println!("tool:    {}", tool);
            println!("args:    {}", parsed);
            println!("outcome: {:?}", decision.outcome);
            println!("rule:    {}", decision.rule);
            println!("source:  {:?}", decision.source);
            let word = match decision.outcome {
                kod_config::Decision::Allow => "allow",
                kod_config::Decision::Deny => "deny",
                kod_config::Decision::Ask => "ask",
            };
            println!("summary: {word}");
        }
    }
    Ok(())
}

pub async fn run_tests() -> Result<()> {
    println!("Running KOD test suite...");

    // Test 1: Configuration loading
    let config = KodConfig::load_default()?;
    println!(
        "  Config: OK (model={})",
        config.llm.default_endpoint().model
    );

    // Test 2: Engine lifecycle.
    //
    // Uses a per-process scratch directory under the OS temp dir
    // rather than `~/.kod/data/test.redb`. The previous path put a
    // self-test artifact in the user's production data directory:
    // two `kod test` invocations concurrently collided on the same
    // file (redb's database lock would fail the second run), and a
    // plain diagnostic left `test.redb` behind to accumulate across
    // runs and confuse anyone inspecting ~/.kod/data.
    //
    // A per-pid subdirectory keeps concurrent runs from colliding
    // and lets us clean up at the end. `std::env::temp_dir` on every
    // supported platform honors the OS's own temp-location policy;
    // no new dependency on `tempfile` (a dev-dependency) is needed.
    let scratch = std::env::temp_dir().join(format!("kod-selftest-{}", std::process::id()));
    std::fs::create_dir_all(&scratch).map_err(|e| {
        KodError::Internal(format!(
            "Could not create self-test scratch dir {}: {}",
            scratch.display(),
            e
        ))
    })?;
    let db_path = scratch.join("test.redb");

    let router_config = RouterConfig::default();
    let engine = KodEngine::new(router_config, db_path)?;
    engine.start().await?;
    assert!(engine.is_running().await);
    engine.shutdown().await?;
    assert!(!engine.is_running().await);
    // Engine owns the file; drop the guard before removing the dir.
    drop(engine);
    // Best-effort cleanup: a failed removal leaves a temp artifact,
    // not a corrupted production directory.
    let _ = std::fs::remove_dir_all(&scratch);
    println!("  Engine lifecycle: OK");

    // Test 3: Provider setup
    {
        let (registry, _default_model, _routing) = kod_core::build_registry(&config.llm, None)?;
        let endpoint_names = registry.names();
        println!("  Provider setup: OK (endpoints={:?})", endpoint_names);
    }

    // Test 4: Skill loading across all standard directories
    let skills_dirs = config.skills_dirs()?;
    let any_dir_exists = skills_dirs.iter().any(|d| d.is_dir());
    if any_dir_exists {
        let skills = kod_skills::load_from_dirs(&skills_dirs).await?;
        println!("  Skill loading: OK ({} skills)", skills.len());
    } else {
        println!("  Skill loading: SKIPPED (no skills directories exist)");
    }

    println!();
    println!("All tests passed!");

    Ok(())
}

pub async fn run_map(max_chars: usize) -> Result<()> {
    let cwd = std::env::current_dir()
        .map_err(|e| KodError::Config(format!("Could not determine working directory: {}", e)))?;
    let map = kod_core::repomap::build_repo_map(&cwd);
    let rendered = map.render(max_chars);
    print!("{}", rendered);
    eprintln!(
        "{} files, {} symbols, {} chars (budget {})",
        map.file_count(),
        map.symbol_count(),
        rendered.len(),
        max_chars,
    );
    Ok(())
}

pub async fn run_serve(stop: bool, socket: Option<std::path::PathBuf>) -> Result<()> {
    let socket_path = socket.unwrap_or_else(kod_core::serve::default_socket_path);

    if stop {
        if !socket_path.exists() {
            eprintln!(
                "No daemon listening at {} — nothing to stop.",
                socket_path.display()
            );
            return Ok(());
        }
        kod_core::serve::stop_daemon(&socket_path).await?;
        // Poll for the socket file to disappear.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while std::time::Instant::now() < deadline {
            if !socket_path.exists() {
                println!("Stopped daemon at {}.", socket_path.display());
                return Ok(());
            }
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
        eprintln!(
            "Sent shutdown to {}, but the socket is still present after 5s. \
             The daemon may be busy; check with `ps`.",
            socket_path.display()
        );
        return Ok(());
    }

    let config = KodConfig::load_default()?;

    // S10: shared bootstrap. `kod serve` uses `config.memory_db_path()`
    // (which respects `memory.scope`) rather than the default under
    // home, and it installs the policy itself so the daemon's
    // per-connection cwd can be threaded through.
    let db_path = config.memory_db_path()?;
    let engine = engine_from_config(
        &config,
        EngineBootstrapOptions {
            model_override: None,
            // The daemon sets its own policy below.
            cli_preset: None,
            require_sandbox: false,
            install_mcp: true,
            db_path: Some(db_path),
            install_embedder: true,
            install_policy: true,
        },
    )
    .await?;

    // Policy: a daemon has no CLI preset, and the policy is loaded
    // against the *current* cwd — the shared bootstrap uses `None`
    // for `cli_preset` so this remains the source of truth.
    let cwd = std::env::current_dir()
        .map_err(|e| KodError::Config(format!("could not determine cwd: {e}")))?;
    let policy = kod_config::PolicyEngine::load(&config, Some(&cwd), None)?;
    engine.set_policy(std::sync::Arc::new(policy)).await;

    engine.start().await?;
    println!(
        "Starting daemon at {} (Ctrl+C to stop).",
        socket_path.display()
    );
    let result = kod_core::serve::serve(engine.clone(), socket_path.clone()).await;

    // Graceful engine shutdown after the accept loop exits.
    let _ = engine.shutdown().await;
    result
}

pub fn run_sandbox_exec(profile_path: std::path::PathBuf, cmd: Vec<String>) -> Result<()> {
    #[cfg(not(target_os = "linux"))]
    {
        let _ = (profile_path, cmd);
        Err(KodError::SandboxViolation(
            "__sandbox-exec is Linux-only".to_string(),
        ))
    }
    #[cfg(target_os = "linux")]
    {
        // Defensive: clap strips the leading `--` when
        // `trailing_var_arg` is on, but a direct invocation
        // (`kod __sandbox-exec profile -- echo hi`) from a script
        // could leave it in.
        let mut cmd = cmd;
        if cmd.first().map(|s| s.as_str()) == Some("--") {
            cmd.remove(0);
        }
        if cmd.is_empty() {
            return Err(KodError::SandboxViolation(
                "__sandbox-exec: no command given".to_string(),
            ));
        }

        let raw = std::fs::read_to_string(&profile_path).map_err(|e| {
            KodError::SandboxViolation(format!(
                "__sandbox-exec: could not read profile {}: {e}",
                profile_path.display()
            ))
        })?;
        let profile = kod_tools::sandbox::landlock::LandlockProfile::from_json(&raw)?;

        // Remove the profile before exec. Best-effort: if it fails,
        // the file lingers in /tmp — not a correctness issue.
        let _ = std::fs::remove_file(&profile_path);

        kod_tools::sandbox::landlock::apply(&profile)?;

        // exec replaces this process. The command inherits the
        // sandbox we just installed.
        use std::os::unix::process::CommandExt;
        let mut command = std::process::Command::new(&cmd[0]);
        command.args(&cmd[1..]);
        let err = command.exec();
        // execvp only returns on failure — success never comes back.
        Err(KodError::SandboxViolation(format!(
            "__sandbox-exec: exec of {} failed: {err}",
            cmd[0]
        )))
    }
}
