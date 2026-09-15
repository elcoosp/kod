//! Command definitions and handlers for the KOD CLI.

use clap::Parser;
use clap::Subcommand;
use kod_config::KodConfig;
use kod_core::KodEngine;
use kod_core::{SwarmEvent, SwarmRunner};
use kod_core::RouterConfig;
use kod_error::{KodError, Result};
use kod_provider::LlmProvider;
use kod_provider_openai::OpenAICompatProvider;
use std::io::{self, BufRead, Write};
use std::sync::Arc;

/// KOD - Terminal-native AI coding agent
#[derive(Parser, Debug)]
#[command(name = "kod", version, about)]
pub struct Cli {
    /// Verbose output
    #[arg(short, long, default_value_t = false)]
    pub verbose: bool,

    #[command(subcommand)]
    pub command: Option<Command>,
}

impl Cli {
    pub fn run(&self) -> Result<()> {
        match &self.command {
            Some(Command::Chat {
                model,
                temperature,
                interactive,
            }) => {
                let rt = tokio::runtime::Runtime::new()
                    .map_err(|e| KodError::Internal(format!("Failed to create runtime: {}", e)))?;
                rt.block_on(async { run_chat(model.clone(), *temperature, *interactive).await })
            }
            Some(Command::Agent { name, goal, model }) => {
                let rt = tokio::runtime::Runtime::new()
                    .map_err(|e| KodError::Internal(format!("Failed to create runtime: {}", e)))?;
                rt.block_on(async { run_agent(name.clone(), goal.clone(), model.clone()).await })
            }
            Some(Command::Swarm {
                goal,
                agents,
                model,
                merge,
            }) => {
                let rt = tokio::runtime::Runtime::new()
                    .map_err(|e| KodError::Internal(format!("Failed to create runtime: {}", e)))?;
                rt.block_on(async {
                    run_swarm(goal.clone(), *agents, model.clone(), *merge).await
                })
            }
            Some(Command::Skills) => {
                let rt = tokio::runtime::Runtime::new()
                    .map_err(|e| KodError::Internal(format!("Failed to create runtime: {}", e)))?;
                rt.block_on(async { run_skills_list().await })
            }
            Some(Command::Config) => {
                let rt = tokio::runtime::Runtime::new()
                    .map_err(|e| KodError::Internal(format!("Failed to create runtime: {}", e)))?;
                rt.block_on(async { run_config_display().await })
            }
            Some(Command::Test) => {
                let rt = tokio::runtime::Runtime::new()
                    .map_err(|e| KodError::Internal(format!("Failed to create runtime: {}", e)))?;
                rt.block_on(async { run_tests().await })
            }
            Some(Command::Tui { model }) => {
                let rt = tokio::runtime::Runtime::new()
                    .map_err(|e| KodError::Internal(format!("Failed to create runtime: {}", e)))?;
                rt.block_on(async { run_tui(model.clone()).await })
            }
            None => {
                // First-run UX. A bare `kod` invocation is the most common
                // first experience, and the previous output was one line
                // ("Use --help for usage information") that gave a new
                // user nothing to act on. Show the four entry points,
                // where the config lives, and where skills are read from
                // — everything a fresh install needs to get moving.
                println!("KOD — terminal AI coding agent");
                println!();
                println!("Getting started:");
                println!("  kod tui                  interactive session (recommended)");
                println!("  kod chat                 plain chat REPL");
                println!("  kod agent -g \"<goal>\"    one-shot agent run");
                println!("  kod skills               list loaded skills");
                println!("  kod config               show effective configuration");
                println!("  kod test                 run self-tests");
                println!();
                // Point at the actual paths KodConfig uses, so the
                // output is accurate on macOS (~/Library/Application
                // Support/kod/) as well as Linux (~/.config/kod/).
                match KodConfig::config_dir() {
                    Ok(dir) => println!("Config:  {}", dir.join("config.toml").display()),
                    Err(_) => println!("Config:  (could not determine config directory)"),
                }
                match KodConfig::load_default() {
                    Ok(cfg) => match cfg.skills_dirs() {
                        Ok(dirs) => {
                            let existing: Vec<String> = dirs
                                .iter()
                                .filter(|d| d.is_dir())
                                .map(|d| d.display().to_string())
                                .collect();
                            if existing.is_empty() {
                                println!(
                                    "Skills:  none found — put .md skills in {} or {}",
                                    dirs.first()
                                        .map(|d| d.display().to_string())
                                        .unwrap_or_else(|| "~/.kod/skills".to_string()),
                                    dirs.get(1)
                                        .map(|d| d.display().to_string())
                                        .unwrap_or_else(|| "~/.agents/skills".to_string()),
                                );
                            } else {
                                println!("Skills:  {}", existing.join(", "));
                            }
                        }
                        Err(_) => println!("Skills:  (could not determine skills directories)"),
                    },
                    Err(_) => println!("Skills:  (config could not be loaded)"),
                }
                println!();
                println!("Run `kod --help` for the full command list.");
                Ok(())
            }
        }
    }
}

#[derive(Subcommand, Debug)]
pub enum Command {
    /// Start a chat session
    Chat {
        /// Specify the model to use
        #[arg(short, long)]
        model: Option<String>,

        /// Temperature for generation (0.0-1.0)
        #[arg(short, long, default_value_t = 0.7)]
        temperature: f32,

        /// Start interactive REPL
        #[arg(short, long, default_value_t = true)]
        interactive: bool,
    },

    /// Run a multi-agent swarm on a goal: decompose, spawn N agents,
    /// run them concurrently, merge the results.
    Swarm {
        /// Goal for the swarm
        #[arg(short, long)]
        goal: String,

        /// Number of agents. Defaults to `swarm.max_agents` in the
        /// config; clamped to 2-8 by the runner.
        #[arg(short = 'n', long)]
        agents: Option<usize>,

        /// Model to use
        #[arg(short, long)]
        model: Option<String>,

        /// Ask the model to synthesize the per-agent results. When
        /// false, the results are concatenated under their labels.
        #[arg(long, default_value_t = true)]
        merge: bool,
    },

    /// Run an agent with a specific goal
    Agent {
        /// Agent name
        #[arg(short, long, default_value = "kod-agent")]
        name: String,

        /// Agent goal/task
        #[arg(short, long)]
        goal: String,

        /// Specify the model to use
        #[arg(short, long)]
        model: Option<String>,
    },

    /// List available skills
    Skills,

    /// Show configuration
    Config,

    /// Run self-tests
    Test,

    /// Launch the interactive terminal UI
    Tui {
        /// Specify the model to use
        #[arg(short, long)]
        model: Option<String>,
    },
}

/// Run the chat command
pub async fn run_chat(model: Option<String>, _temperature: f32, _interactive: bool) -> Result<()> {
    // Load configuration
    let config = KodConfig::load_default()?;

    // Override model if specified
    let model_name = model.unwrap_or_else(|| config.llm.model.clone());

    // Create the database path
    let home = dirs::home_dir()
        .ok_or_else(|| KodError::Config("Could not determine home directory".to_string()))?;
    let db_path = home.join(".kod").join("data").join("kod.redb");

    // Create engine. Derive the history budget from the model's window
    // (≈3 chars/token) so a small-model user is safe and a large-model
    // user gets useful recall; the engine clamps below its floor.
    // RouterConfig carries the token window itself so the memory manager
    // sizes its own budget from the same source.
    let router_config = RouterConfig {
        context_window: config.llm.context_window,
        ..RouterConfig::default()
    };
    let engine = KodEngine::new(router_config, db_path)?;
    engine.set_history_budget(config.llm.context_window.saturating_mul(3));

    // Set up OpenAI-compatible provider (Ollama /v1, LM Studio, MLX, ...)
    let provider = OpenAICompatProvider::from_config(&config.llm, Some(&model_name))?;
    engine.set_provider(Arc::new(provider)).await;

    // Start the engine
    engine.start().await?;

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
                eprintln!("Could not enable skill hot reload for {}: {}", dir.display(), e);
            }
        }
    }

    println!(
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
        // carries `\0kod-*` markers (tool start / args / done / thinking)
        // that the TUI uses to render its running indicator — the CLI has
        // no such indicator, so it drops them. If nothing streamed (a
        // tool-only reply whose summary is empty, or a provider whose
        // default stream_with_tools emits no Text), fall back to the
        // response's full text.
        let (tx, mut rx) = tokio::sync::mpsc::channel::<String>(64);
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
}

/// Run a multi-agent swarm on a goal.
///
/// Decomposes the goal into N subtasks, spawns one agent per subtask via
/// `kod-swarm`, runs them concurrently against the engine's agentic loop,
/// and merges the results. The command prints labeled progress as each
/// agent works and the merged answer at the end.
pub async fn run_swarm(
    goal: String,
    agents: Option<usize>,
    model: Option<String>,
    merge: bool,
) -> Result<()> {
    let config = KodConfig::load_default()?;
    let model_name = model.unwrap_or_else(|| config.llm.model.clone());
    let n = agents.unwrap_or(config.swarm.max_agents);

    let home = dirs::home_dir()
        .ok_or_else(|| KodError::Config("Could not determine home directory".to_string()))?;
    let db_path = home.join(".kod").join("data").join("kod.redb");
    let _ = std::fs::create_dir_all(db_path.parent().unwrap());

    let router_config = RouterConfig {
        context_window: config.llm.context_window,
        ..RouterConfig::default()
    };
    let engine = KodEngine::new(router_config, db_path)?;
    engine.set_history_budget(config.llm.context_window.saturating_mul(3));

    let provider = OpenAICompatProvider::from_config(&config.llm, Some(&model_name))?;
    engine.set_provider(Arc::new(provider)).await;

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
                eprintln!("Could not enable skill hot reload for {}: {}", dir.display(), e);
            }
        }
    }

    let engine = Arc::new(engine);
    let runner = SwarmRunner::new(engine.clone(), n, merge).await?;
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
                    println!("── {} starts on: {}", name, subtask.lines().next().unwrap_or(""));
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
                SwarmEvent::Merging => {
                    println!("\n── merging results ──\n");
                }
            }
        }
    });

    let result = runner.run(&goal, &tx).await;
    drop(tx);
    let _ = print_task.await;

    let resp = result?;

    println!("\n================ merged ================\n");
    println!("{}", resp.merged);
    if !resp.merged_by_model {
        println!(
            "\n(merged by concatenation — LLM synthesis was disabled or failed)"
        );
    }

    engine.shutdown().await?;
    Ok(())
}

/// Run the agent command
pub async fn run_agent(name: String, goal: String, model: Option<String>) -> Result<()> {
    let config = KodConfig::load_default()?;
    let model_name = model.unwrap_or_else(|| config.llm.model.clone());

    let home = dirs::home_dir()
        .ok_or_else(|| KodError::Config("Could not determine home directory".to_string()))?;
    let db_path = home.join(".kod").join("data").join("kod.redb");

    let router_config = RouterConfig {
        context_window: config.llm.context_window,
        ..RouterConfig::default()
    };
    let engine = KodEngine::new(router_config, db_path)?;
    engine.set_history_budget(config.llm.context_window.saturating_mul(3));

    let provider = OpenAICompatProvider::from_config(&config.llm, Some(&model_name))?;
    engine.set_provider(Arc::new(provider)).await;

    engine.start().await?;

    println!("Starting agent '{}' with goal: {}", name, goal);

    let response = engine.process(&goal).await?;

    if let Some(text) = response.text {
        println!("Agent {}: {}", name, text);
    }

    engine.shutdown().await?;

    Ok(())
}

/// List available skills
pub async fn run_skills_list() -> Result<()> {
    let config = KodConfig::load_default()?;
    let skills_dirs = config.skills_dirs()?;

    let existing: Vec<_> = skills_dirs.iter().filter(|d| d.is_dir()).collect();
    if existing.is_empty() {
        println!("No skills directories found. Checked:");
        for d in &skills_dirs {
            println!("  {}", d.display());
        }
        println!("No skills available.");
        return Ok(());
    }

    let skills = kod_skills::load_from_dirs(&skills_dirs).await?;
    if skills.is_empty() {
        println!("Skills directories exist but contain no parseable .md skills:");
        for d in &existing {
            println!("  {}", d.display());
        }
        return Ok(());
    }

    println!("Available skills ({}):", skills.len());
    for skill in &skills {
        println!(
            "  - {}: {}",
            skill.metadata.name, skill.metadata.description
        );
    }

    Ok(())
}

/// Display the current configuration
pub async fn run_config_display() -> Result<()> {
    let config = KodConfig::load_default()?;

    println!("KOD Configuration:");
    println!();
    println!("LLM:");
    println!("  Provider: {:?}", config.llm.provider);
    println!("  Model: {}", config.llm.model);
    println!("  Base URL: {}", config.llm.base_url);
    println!("  Context Window: {}", config.llm.context_window);
    println!("  Max Tokens: {}", config.llm.max_tokens);
    println!("  Temperature: {}", config.llm.temperature);
    println!();
    println!("Memory:");
    println!(
        "  Short-Term Capacity: {}",
        config.memory.short_term_capacity
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

/// Run tests
pub async fn run_tests() -> Result<()> {
    println!("Running KOD test suite...");

    // Test 1: Configuration loading
    let config = KodConfig::load_default()?;
    println!("  Config: OK (model={})", config.llm.model);

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
    let provider = OpenAICompatProvider::from_config(&config.llm, None)?;
    let provider_name = provider.name();
    println!("  Provider setup: OK (name={})", provider_name);

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

/// Launch the interactive terminal UI
pub async fn run_tui(model: Option<String>) -> Result<()> {
    let mut tui = kod_tui::TuiLoop::new();
    tui.run(model).await
}
