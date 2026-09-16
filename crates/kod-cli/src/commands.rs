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

/// `kod skills` subcommands.
#[derive(Subcommand, Debug, Clone)]
pub enum SkillsAction {
    /// List skills (same as `kod skills` without a subcommand).
    List,
    /// Scaffold a new skill file in the first writable skills
    /// directory. Refuses to overwrite an existing file.
    New {
        /// Skill name (kebab-case; used as the file stem and the
        /// `name:` field).
        name: String,
    },
}

/// `kod config` subcommands.
#[derive(Subcommand, Debug, Clone)]
pub enum ConfigAction {
    /// Print the path to the config file.
    Path,
    /// Open the config file in $EDITOR (or $VISUAL, or `vi`).
    Edit,
}

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
                sandbox,
            }) => {
                let rt = tokio::runtime::Runtime::new()
                    .map_err(|e| KodError::Internal(format!("Failed to create runtime: {}", e)))?;
                rt.block_on(async {
                    run_chat(model.clone(), *temperature, *interactive, *sandbox).await
                })
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
            Some(Command::Skills { action, json }) => {
                let rt = tokio::runtime::Runtime::new()
                    .map_err(|e| KodError::Internal(format!("Failed to create runtime: {}", e)))?;
                rt.block_on(async {
                    match action {
                        None | Some(SkillsAction::List) => run_skills_list(*json).await,
                        Some(SkillsAction::New { name }) => run_skills_new(name).await,
                    }
                })
            }
            Some(Command::ValidateSkills) => {
                let rt = tokio::runtime::Runtime::new()
                    .map_err(|e| KodError::Internal(format!("Failed to create runtime: {}", e)))?;
                rt.block_on(async { run_skills_validate().await })
            }
            Some(Command::Config { action }) => {
                let rt = tokio::runtime::Runtime::new()
                    .map_err(|e| KodError::Internal(format!("Failed to create runtime: {}", e)))?;
                rt.block_on(async {
                    match action {
                        None => run_config_display().await,
                        Some(ConfigAction::Path) => run_config_path().await,
                        Some(ConfigAction::Edit) => run_config_edit().await,
                    }
                })
            }
            Some(Command::Test) => {
                let rt = tokio::runtime::Runtime::new()
                    .map_err(|e| KodError::Internal(format!("Failed to create runtime: {}", e)))?;
                rt.block_on(async { run_tests().await })
            }
            Some(Command::Map { max_chars }) => {
                let rt = tokio::runtime::Runtime::new()
                    .map_err(|e| KodError::Internal(format!("Failed to create runtime: {}", e)))?;
                rt.block_on(async { run_map(*max_chars).await })
            }
            Some(Command::Replay { path, execute }) => {
                let rt = tokio::runtime::Runtime::new()
                    .map_err(|e| KodError::Internal(format!("Failed to create runtime: {}", e)))?;
                rt.block_on(async { run_replay(path.clone(), *execute).await })
            }
            Some(Command::Profile { action }) => {
                let rt = tokio::runtime::Runtime::new()
                    .map_err(|e| KodError::Internal(format!("Failed to create runtime: {}", e)))?;
                rt.block_on(async { run_profile(action.clone()).await })
            }
            Some(Command::Tui { model }) => {
                let rt = tokio::runtime::Runtime::new()
                    .map_err(|e| KodError::Internal(format!("Failed to create runtime: {}", e)))?;
                rt.block_on(async { run_tui(model.clone()).await })
            }
            Some(Command::Doctor) => {
                let rt = tokio::runtime::Runtime::new()
                    .map_err(|e| KodError::Internal(format!("Failed to create runtime: {}", e)))?;
                rt.block_on(async { run_doctor().await })
            }
            Some(Command::Init) => {
                let rt = tokio::runtime::Runtime::new()
                    .map_err(|e| KodError::Internal(format!("Failed to create runtime: {}", e)))?;
                rt.block_on(async { run_init().await })
            }
            Some(Command::Models { filter }) => {
                let rt = tokio::runtime::Runtime::new()
                    .map_err(|e| KodError::Internal(format!("Failed to create runtime: {}", e)))?;
                rt.block_on(async { run_models(filter.clone()).await })
            }
            Some(Command::Sessions { action }) => {
                let rt = tokio::runtime::Runtime::new()
                    .map_err(|e| KodError::Internal(format!("Failed to create runtime: {}", e)))?;
                rt.block_on(async { run_sessions(action.clone()).await })
            }
            Some(Command::Completions { shell }) => {
                let mut cmd = <Cli as clap::CommandFactory>::command();
                let bin_name = cmd.get_name().to_string();
                clap_complete::generate(*shell, &mut cmd, bin_name, &mut std::io::stdout());
                Ok(())
            }
            Some(Command::Checkpoint { action }) => {
                let rt = tokio::runtime::Runtime::new()
                    .map_err(|e| KodError::Internal(format!("Failed to create runtime: {}", e)))?;
                rt.block_on(async { run_checkpoint(action.clone()).await })
            }
            Some(Command::Update) => {
                let rt = tokio::runtime::Runtime::new()
                    .map_err(|e| KodError::Internal(format!("Failed to create runtime: {}", e)))?;
                rt.block_on(async { run_update().await })
            }
            Some(Command::Which) => {
                let rt = tokio::runtime::Runtime::new()
                    .map_err(|e| KodError::Internal(format!("Failed to create runtime: {}", e)))?;
                rt.block_on(async { run_which().await })
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

#[derive(Subcommand, Debug, Clone)]
pub enum ProfileAction {
    /// List the built-in profiles.
    List,
    /// Print the effective `[llm]` config from the loaded config file.
    Show,
    /// Write a named profile's values into `[llm]` in the config file.
    Use {
        /// Profile name (see `kod profile list`).
        name: String,
    },
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

        /// Run shell commands through the platform sandbox (`bwrap` on
        /// Linux, `sandbox-exec` on macOS). Fails loudly if the
        /// primitive is not available. Recommended when letting the
        /// agent run unsupervised.
        #[arg(long, default_value_t = false)]
        sandbox: bool,
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

    /// Work with skills. `kod skills` (no subcommand) lists them;
    /// `kod skills list` is the same; `kod skills new <name>`
    /// scaffolds a new skill file.
    Skills {
        #[command(subcommand)]
        action: Option<SkillsAction>,
        /// Emit machine-readable JSON instead of the human text
        /// listing. Applies to the list action.
        #[arg(long, default_value_t = false)]
        json: bool,
    },

    /// Validate every skill file: parse each .md, report any that fail,
    /// and exit non-zero if at least one did. Useful in CI and after
    /// editing a skill by hand.
    ValidateSkills,

    /// Show configuration. `kod config` prints the effective config;
    /// `kod config path` prints the file path; `kod config edit`
    /// opens the file in $EDITOR.
    Config {
        #[command(subcommand)]
        action: Option<ConfigAction>,
    },

    /// Run self-tests
    Test,

    /// Print the repository map: top-level symbols per recognized source file.
    Map {
        /// Cap the map at this many characters. Defaults to 16000 (~4k tokens).
        #[arg(long, default_value_t = 16000)]
        max_chars: usize,
    },

    /// Re-run every tool call recorded in a session log, without the model.
    Replay {
        /// Path to the JSONL session log.
        path: std::path::PathBuf,
        /// When false (the default), just print what would run. When true,
        /// actually execute every recorded tool call.
        #[arg(long, default_value_t = false)]
        execute: bool,
    },

    /// Work with preset model profiles: list, show the effective
    /// `[llm]` config, or write a named preset into `config.toml`.
    Profile {
        #[command(subcommand)]
        action: ProfileAction,
    },

    /// Launch the interactive terminal UI
    Tui {
        /// Specify the model to use
        #[arg(short, long)]
        model: Option<String>,
    },

    /// Print a diagnostics report: config file presence, LLM endpoint
    /// shape, skill directories, and the memory database path. Exits
    /// non-zero if any check fails, so it can gate CI or a first-run
    /// script.
    Doctor,

    /// First-run helper: ensure a config file exists, print where it
    /// lives, and print the next three commands a new user should run.
    /// Idempotent — running it twice is a no-op.
    Init,

    /// List, or filter, the models the configured provider offers.
    /// Read-only: never pulls, downloads, or deletes — those are the
    /// server's job, and running them from here would need a progress
    /// UI that the CLI does not have.
    Models {
        /// When set, only print models whose id contains this substring
        /// (case-insensitive). A plain substring match is what a user
        /// wanting `qwen2.5-coder` needs; a real regex is a footgun for
        /// a filter this small.
        #[arg(short, long)]
        filter: Option<String>,
    },

    /// Inspect, export, or clear the chat session saved by the TUI
    /// (`~/.kod/tui_session.json`, or `$KOD_TUI_STATE_DIR`). Read-only
    /// by default; `clear` deletes the file.
    Sessions {
        #[command(subcommand)]
        action: SessionsAction,
    },

    /// Print a shell completion script for the given shell on stdout.
    /// Typical install:
    ///   bash:  kod completions bash > ~/.local/share/bash-completion/completions/kod
    ///   zsh:   kod completions zsh  > ~/.zfunc/_kod
    ///   fish:  kod completions fish > ~/.config/fish/completions/kod.fish
    Completions {
        /// One of: bash, zsh, fish, elvish, powershell.
        shell: clap_complete::Shell,
    },

    /// Inspect, restore, or clear file checkpoints. A checkpoint is
    /// captured automatically before `write_file` and `patch_file`
    /// run, so a session that does not like an edit can undo it.
    Checkpoint {
        #[command(subcommand)]
        action: CheckpointAction,
    },

    /// Check GitHub for a newer release and, if one exists, print its
    /// URL and installation instructions. Read-only — never replaces
    /// the running binary.
    Update,

    /// Print every path KOD reads or writes: config, memory db, skills
    /// dirs, session, history, checkpoints. Useful for scripting and
    /// for "where does this thing live?" questions.
    Which,
}

/// `kod checkpoint` subcommands.
#[derive(Subcommand, Debug, Clone)]
pub enum CheckpointAction {
    /// List checkpoints for the current directory, newest first.
    List {
        /// Maximum entries to print. Defaults to 20.
        #[arg(short, long, default_value_t = 20)]
        limit: usize,
    },
    /// Restore the file contents recorded in one checkpoint. Does not
    /// delete the checkpoint — a restore can be restored again.
    Restore {
        /// Snapshot id, as printed by `kod checkpoint list`.
        id: String,
    },
    /// Delete every checkpoint for the current directory.
    Clear,
}

/// `kod sessions` subcommands.
#[derive(Subcommand, Debug, Clone)]
pub enum SessionsAction {
    /// Print where the session file lives and how many messages it holds.
    /// When the file does not exist, say so — a user running this on a
    /// fresh machine should not get a silent empty output.
    Show,
    /// Delete the saved session file. An explicit `kod sessions clear`
    /// is consent; no confirmation prompt.
    Clear,
    /// Write the saved session to `path` (or stdout when `-`).
    /// Default format is Markdown — the shape a user pastes into a
    /// gist. `json` round-trips through `kod sessions export --format
    /// json | ...`.
    Export {
        /// Output path. `-` writes to stdout.
        #[arg(default_value = "-")]
        path: std::path::PathBuf,
        /// `markdown` (default) or `json`.
        #[arg(short, long, default_value = "markdown")]
        format: String,
    },
}

/// Run the chat command
pub async fn run_chat(
    model: Option<String>,
    _temperature: f32,
    _interactive: bool,
    sandbox: bool,
) -> Result<()> {
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
    // Arc because the approval forwarder task (spawned below) needs to
    // call `respond_to_approval` while `process_streaming` runs on the
    // same engine.
    let engine = Arc::new(KodEngine::new(router_config, db_path)?);
    engine.set_history_budget(config.llm.context_window.saturating_mul(3));

    // Set up OpenAI-compatible provider (Ollama /v1, LM Studio, MLX, ...)
    let provider = OpenAICompatProvider::from_config(&config.llm, Some(&model_name))?;
    engine.set_provider(Arc::new(provider)).await;
    engine.set_hooks(config.hooks.clone());
    engine.set_network_access(config.llm.network_access);
    engine.set_confirm_writes(config.tools.confirm_writes);

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
        // Approval channel: the pump sends (id, decision); the
        // forwarder calls engine.respond_to_approval. The split
        // exists because the pump owns its scope and cannot also
        // borrow the engine across the response loop.
        let (approval_tx, mut approval_rx) =
            tokio::sync::mpsc::channel::<(u64, kod_core::engine::ApprovalDecision)>(16);
        let engine_for_approvals = engine.clone();
        let approval_forwarder = tokio::spawn(async move {
            while let Some((id, decision)) = approval_rx.recv().await {
                let _ = engine_for_approvals
                    .respond_to_approval(id, decision)
                    .await;
            }
        });
        // Clone so the outer scope retains its own sender: dropping it
        // after `process_streaming` closes the channel, and the pump's
        // clone is dropped with the task. Without the clone, the outer
        // `drop(approval_tx)` is a use-after-move.
        let (question_tx, mut question_rx) =
            tokio::sync::mpsc::channel::<(u64, String)>(16);
        let engine_for_questions = engine.clone();
        let question_forwarder = tokio::spawn(async move {
            while let Some((id, answer)) = question_rx.recv().await {
                let _ = engine_for_questions
                    .respond_to_question(id, answer)
                    .await;
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
                    let request: kod_tools::ask::QuestionRequest =
                        serde_json::from_str(json).unwrap_or_else(|_| {
                            kod_tools::ask::QuestionRequest {
                                question: "(unparseable question)".to_string(),
                                placeholder: None,
                            }
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

                if let Some((id, json)) = kod_core::engine::parse_tool_approval(&chunk) {
                    let request: kod_core::engine::ApprovalRequest =
                        serde_json::from_str(json).unwrap_or_else(|_| {
                            kod_core::engine::ApprovalRequest {
                                tool_name: "?".to_string(),
                                arguments: serde_json::Value::Null,
                                diff: None,
                                summary: "(unparseable approval request)".to_string(),
                            }
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
                    print!("Approve? [y/N] ");
                    let _ = io::stdout().flush();
                    let mut answer = String::new();
                    let approved = match io::stdin().read_line(&mut answer) {
                        Ok(_) => {
                            let a = answer.trim().to_lowercase();
                            a == "y" || a == "yes"
                        }
                        Err(_) => false,
                    };
                    let decision = if approved {
                        kod_core::engine::ApprovalDecision::Approve
                    } else {
                        kod_core::engine::ApprovalDecision::Deny
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

        let result = engine.process_streaming(input_line, &tx).await;
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
                SwarmEvent::ConflictDetected { file, agents } => {
                    // Surface the conflict live so a user watching the
                    // run sees overlapping work while the merge step
                    // is still ahead of them, not only in the final
                    // answer.
                    eprintln!(
                        "\n⚠ conflict: {} written by {}\n",
                        file,
                        agents.join(", ")
                    );
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

    if !resp.conflicts.is_empty() {
        println!("\n{} file conflict(s):", resp.conflicts.len());
        for c in &resp.conflicts {
            println!("  ⚠ {} — written by {}", c.file, c.agents.join(", "));
        }
    }

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
    // `kod agent` has no interactive consumer. When confirm_writes is
    // on, the engine refuses every write with a message the model and
    // the user can act on. Approving silently would defeat the flag.
    engine.set_confirm_writes(config.tools.confirm_writes);

    engine.start().await?;

    println!("Starting agent '{}' with goal: {}", name, goal);

    let response = engine.process(&goal).await?;

    if let Some(text) = response.text {
        println!("Agent {}: {}", name, text);
    }

    engine.shutdown().await?;

    Ok(())
}

/// List available skills. With `json = true`, prints a JSON array of
/// the same data instead of the human-readable text — every consumer
/// of the text output today (a script, a viewer) can instead consume
/// the array and stop parsing prose.
pub async fn run_skills_list(json: bool) -> Result<()> {
    let config = KodConfig::load_default()?;
    let skills_dirs = config.skills_dirs()?;

    if json {
        let skills = kod_skills::load_from_dirs(&skills_dirs).await?;
        let arr: Vec<serde_json::Value> = skills
            .iter()
            .map(|s| {
                serde_json::json!({
                    "name": s.metadata.name,
                    "description": s.metadata.description,
                    "version": s.metadata.version,
                    "category": s.metadata.category,
                    "tags": s.metadata.tags,
                    "capabilities": s.metadata.capabilities,
                    "triggers": s.metadata.triggers,
                    "path": s.path.display().to_string(),
                })
            })
            .collect();
        let out = serde_json::to_string_pretty(&arr)
            .map_err(|e| KodError::Serialization(e.to_string()))?;
        println!("{}", out);
        return Ok(());
    }

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
    println!(
        "  Network access: {}",
        if config.llm.network_access {
            "enabled (web_fetch can reach the network)"
        } else {
            "disabled"
        }
    );
    println!(
        "  Confirm writes: {}",
        if config.tools.confirm_writes {
            "enabled (write_file / patch_file require approval)"
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

/// Print the repository map to stdout: one line per source file, followed
/// by its top-level symbols. Summary counts go to stderr so stdout can be
/// piped into a file cleanly.
pub async fn run_map(max_chars: usize) -> Result<()> {
    let cwd = std::env::current_dir().map_err(|e| {
        KodError::Config(format!("Could not determine working directory: {}", e))
    })?;
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

/// Re-run every tool call recorded in a session log.
///
/// With `execute = false` (the default) this is a preview: it prints what
/// would run and touches nothing. With `execute = true`, it builds a bare
/// engine and calls `run_tool` for each recorded call, printing whether
/// the fresh result matches the recorded one.
pub async fn run_replay(path: std::path::PathBuf, execute: bool) -> Result<()> {
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
            #[allow(unreachable_patterns)]
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

    let config = KodConfig::load_default()?;
    let db_path = config.memory_db_path()?;
    let router_config = RouterConfig {
        context_window: config.llm.context_window,
        ..RouterConfig::default()
    };
    let engine = KodEngine::new(router_config, db_path)?;
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

/// Handle `kod profile list`, `kod profile show`, and
/// `kod profile use <name>`.
///
/// `use` writes the preset's values into `[llm]` in the config file and
/// saves. It does not touch any other section: `[hooks]`, `[skills]`,
/// `[memory.scope]` survive the switch.
pub async fn run_profile(action: ProfileAction) -> Result<()> {
    match action {
        ProfileAction::List => {
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
            Ok(())
        }
        ProfileAction::Show => {
            let config = KodConfig::load_default()?;
            println!("Effective [llm] config:");
            println!("  provider       = {:?}", config.llm.provider);
            println!("  model          = \"{}\"", config.llm.model);
            println!("  base_url       = \"{}\"", config.llm.base_url);
            println!("  context_window = {}", config.llm.context_window);
            println!("  max_tokens     = {}", config.llm.max_tokens);
            println!("  temperature    = {}", config.llm.temperature);
            println!("  timeout_secs   = {}", config.llm.timeout_secs);
            Ok(())
        }
        ProfileAction::Use { name } => {
            let profile = kod_config::profiles::by_name(&name).ok_or_else(|| {
                KodError::Config(format!(
                    "Unknown profile {:?}. Known profiles: {}",
                    name,
                    kod_config::profiles::names_csv()
                ))
            })?;
            let mut config = KodConfig::load_default()?;
            config.llm.model = profile.model.to_string();
            config.llm.base_url = profile.base_url.to_string();
            config.llm.context_window = profile.context_window;
            config.llm.max_tokens = profile.max_tokens;
            let dir = KodConfig::config_dir()?;
            let path = dir.join("config.toml");
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

/// Launch the interactive terminal UI
pub async fn run_tui(model: Option<String>) -> Result<()> {
    let mut tui = kod_tui::TuiLoop::new();
    tui.run(model).await
}

/// Print a diagnostics report. Read-only: never writes to the config,
/// the database, or the skills directories. Exit code carries the
/// verdict so a first-run script or CI job can gate on it.
pub async fn run_doctor() -> Result<()> {
    use kod_core::doctor::{CheckStatus, run_diagnostics};

    let config = KodConfig::load_default()?;
    let report = run_diagnostics(&config);

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

/// First-run helper. `KodConfig::load_default()` already writes the
/// default config on the first call; this command makes that side
/// effect explicit and prints the next steps a new user needs. Running
/// it twice is a no-op — the second call finds the config present and
/// prints the same summary.
///
/// Deliberately does not prompt or modify the config: an init that
/// silently rewrites a user's config is worse than no init at all.
/// Users who want to change a setting are pointed at the file itself.
pub async fn run_init() -> Result<()> {
    let config = KodConfig::load_default()?;
    let config_dir = KodConfig::config_dir()?;
    let path = config_dir.join("config.toml");

    println!("KOD initialized.");
    println!();
    if path.exists() {
        println!("Config:   {}", path.display());
    } else {
        println!("Config:   (in memory only — could not write {})", path.display());
    }
    println!("Model:    {}", config.llm.model);
    println!("Endpoint: {}", config.llm.base_url);
    println!();
    println!("Next steps:");
    println!("  1. Start the model server (e.g. `ollama serve`)");
    println!("  2. Pull the model (e.g. `ollama pull {}`)", config.llm.model);
    println!("  3. Verify the setup:  kod doctor");
    println!("  4. Start a session:   kod tui    (interactive)");
    println!("                        kod chat   (plain REPL)");
    Ok(())
}

/// Print the models the configured provider offers, optionally
/// filtered by a case-insensitive substring.
///
/// Read-only. The provider builds its model list from
/// `GET /v1/models` (Ollama, LM Studio, MLX, vLLM, OpenAI all expose
/// this). Distinguishes three outcomes a user must be able to tell
/// apart:
///
///   - the server answered with a list: print it (filtered, if asked);
///   - the server answered but the list was empty: say so, and print
///     the pull command for the configured model;
///   - the server could not be reached: name the failure, do not
///     pretend the list was empty.
///
/// The previous `Engine::list_models` on a fresh session could not
/// distinguish (2) from (3) once the messages were printed; this
/// command keeps the distinction visible.
pub async fn run_models(filter: Option<String>) -> Result<()> {
    let config = KodConfig::load_default()?;
    let provider = OpenAICompatProvider::from_config(&config.llm, None)?;

    let models = match provider.list_models().await {
        Ok(m) => m,
        Err(e) => {
            eprintln!(
                "Could not list models from {}: {}",
                config.llm.base_url, e
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
            config.llm.base_url
        );
        println!();
        println!("Pull one first, e.g.:");
        println!("  ollama pull {}", config.llm.model);
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
        println!("{} model(s) on {}:", shown.len(), config.llm.base_url);
    }
    for m in &shown {
        if m.as_str() == config.llm.model {
            println!("  - {}  (current)", m);
        } else {
            println!("  - {}", m);
        }
    }
    Ok(())
}

/// `kod sessions <action>`.
///
/// Reads the session file the TUI writes at `~/.kod/tui_session.json`
/// (overridable via `$KOD_TUI_STATE_DIR`, which the TUI itself honors
/// and which this command therefore respects too — the two must agree
/// on which file they are talking about). `Show` and `Export` are
/// read-only; `Clear` deletes. The file is JSON-serialized
/// `kod_tui::Message` records, so the CLI decodes through the same
/// type the TUI wrote.
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
                println!("First:   [{}] {}", first.timestamp.format("%Y-%m-%d %H:%M:%S"), preview(&first.content, 60));
                println!("Last:    [{}] {}", last.timestamp.format("%Y-%m-%d %H:%M:%S"), preview(&last.content, 60));
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
        SessionsAction::Export {
            path: dest,
            format,
        } => {
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

/// Count (user, assistant, other) roles in a session.
fn count_roles(messages: &[kod_tui::app::Message]) -> (usize, usize, usize) {
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

/// One-line preview of `s`, clipped to `max` chars (char-aware).
fn preview(s: &str, max: usize) -> String {
    let first = s.lines().next().unwrap_or("");
    if first.chars().count() <= max {
        return first.to_string();
    }
    let cut: String = first.chars().take(max).collect();
    format!("{cut}…")
}

/// Render a session as Markdown: a heading per role, fenced code blocks
/// for tool output so a code-heavy transcript stays readable.
fn render_session_markdown(messages: &[kod_tui::app::Message]) -> String {
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

/// `kod checkpoint <action>`.
///
/// Operates on the checkpoint directory for the current working
/// directory (see `kod_core::checkpoint`). Read-only for `list`;
/// `restore` writes the snapshot's content back; `clear` deletes
/// every snapshot for this project.
///
/// Prints an explicit "no checkpoints directory" message when the
/// manager is None (no home directory) rather than a silent empty
/// list, because a user running this in a stripped container should
/// know *why* there is nothing to see.
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

/// `YYYY-MM-DD HH:MM:SS` from Unix milliseconds, in local time when the
/// platform provides it, UTC otherwise. Falls back to the raw ms when
/// the timestamp is nonsensical.
fn format_timestamp_ms(ms: u64) -> String {
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

/// Validate every skill file: parse each `.md` in every configured
/// skills directory, print one line per file (✓ or ✗), and exit
/// non-zero if at least one file failed to parse.
///
/// Distinct from `kod skills` — that command loads through the matcher
/// (which logs a warning and *skips* a malformed file), so a typo in
/// one skill is invisible until the file is actually needed. This one
/// looks at every file, and the exit code carries the verdict.
pub async fn run_skills_validate() -> Result<()> {
    let config = KodConfig::load_default()?;
    let skills_dirs = config.skills_dirs()?;
    let parser = kod_skills::SkillParser::new();

    let mut total = 0usize;
    let mut ok = 0usize;
    let mut failed: Vec<(std::path::PathBuf, String)> = Vec::new();

    for dir in &skills_dirs {
        if !dir.is_dir() {
            continue;
        }
        for entry in walkdir::WalkDir::new(dir)
            .follow_links(false)
            .into_iter()
            .filter_map(|e| e.ok())
        {
            if !entry.file_type().is_file() {
                continue;
            }
            if entry.path().extension().and_then(|s| s.to_str()) != Some("md") {
                continue;
            }
            total += 1;
            match parser.parse_file(entry.path()) {
                Ok(skill) => {
                    ok += 1;
                    println!("✓ {} ({})", skill.metadata.name, entry.path().display());
                }
                Err(e) => {
                    let msg = e.to_string();
                    failed.push((entry.path().to_path_buf(), msg.clone()));
                    println!("✗ {} — {}", entry.path().display(), msg);
                }
            }
        }
    }

    if total == 0 {
        println!("No skill files found. Checked:");
        for d in &skills_dirs {
            println!("  {}", d.display());
        }
        return Ok(());
    }

    println!();
    println!(
        "{} skill file(s) checked: {} parsed, {} failed.",
        total,
        ok,
        failed.len()
    );

    if !failed.is_empty() {
        std::process::exit(1);
    }
    Ok(())
}


/// Check GitHub for a newer release of KOD.
///
/// Hits the public `releases/latest` endpoint for the project repo
/// (configurable via `KOD_UPDATE_REPO` so a fork can point elsewhere)
/// and compares the tag to `CARGO_PKG_VERSION`. Three outcomes:
///
/// - the running version matches or exceeds the latest: "up to date";
/// - a newer release exists: prints the tag, the release URL, and the
///   install command for the current platform;
/// - the network request fails: reports the failure and exits non-zero
///   so a script can branch on it. A check that silently no-ops on
///   network failure is worse than a check that says so.
///
/// Deliberately does not download or replace the binary. Auto-update of
/// a self-installed Rust binary is a footgun: the running process has
/// the file open on Windows, and on Unix a partial replacement can
/// leave a broken executable. A user who wants a new version runs the
/// install command the tool prints.
pub async fn run_update() -> Result<()> {
    let current = env!("CARGO_PKG_VERSION");
    let repo = std::env::var("KOD_UPDATE_REPO")
        .unwrap_or_else(|_| "kod-team/kod".to_string());
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
            format!("{}…", &body[..300])
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

    let tag = body
        .get("tag_name")
        .and_then(|t| t.as_str())
        .unwrap_or("");
    let html_url = body
        .get("html_url")
        .and_then(|u| u.as_str())
        .unwrap_or("");
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

/// `true` when `a` and `b` name the same version, ignoring a leading `v`.
fn versions_equal(a: &str, b: &str) -> bool {
    a.trim_start_matches('v') == b.trim_start_matches('v')
}

/// `true` when `candidate` is strictly older than `running`.
///
/// Parses each version as `major.minor.patch` (any missing component
/// counts as 0). Suffixes like `-rc1` are compared as "less than" the
/// same numeric version without the suffix — a release candidate is
/// older than its final release, which is the intuitive order.
fn version_is_older(candidate: &str, running: &str) -> bool {
    fn parse(v: &str) -> (u32, u32, u32, bool) {
        let (numeric, pre) = match v.split_once('-') {
            Some((n, _)) => (n, true),
            None => (v, false),
        };
        let mut parts = numeric.split('.');
        let major = parts.next().and_then(|s| s.parse().ok()).unwrap_or(0);
        let minor = parts.next().and_then(|s| s.parse().ok()).unwrap_or(0);
        let patch = parts.next().and_then(|s| s.parse().ok()).unwrap_or(0);
        (major, minor, patch, pre)
    }
    let (cm, cn, cp, cpre) = parse(candidate);
    let (rm, rn, rp, rpre) = parse(running);
    (cm, cn, cp) < (rm, rn, rp) || ((cm, cn, cp) == (rm, rn, rp) && cpre && !rpre)
}


#[cfg(test)]
mod update_tests {
    use super::{version_is_older, versions_equal};

    #[test]
    fn versions_equal_ignores_leading_v() {
        assert!(versions_equal("0.1.0", "0.1.0"));
        assert!(versions_equal("v0.1.0", "0.1.0"));
        assert!(versions_equal("0.1.0", "v0.1.0"));
        assert!(!versions_equal("0.1.0", "0.1.1"));
    }

    #[test]
    fn version_is_older_compares_semver_triples() {
        assert!(version_is_older("0.1.0", "0.1.1"));
        assert!(version_is_older("0.1.9", "0.2.0"));
        assert!(version_is_older("0.9.9", "1.0.0"));
        assert!(!version_is_older("0.1.0", "0.1.0"));
        assert!(!version_is_older("0.2.0", "0.1.9"));
        assert!(!version_is_older("1.0.0", "0.9.9"));
    }

    #[test]
    fn version_is_older_handles_missing_components() {
        assert!(version_is_older("1", "1.0.1"));
        assert!(!version_is_older("1.0.1", "1"));
        assert!(version_is_older("1.0", "1.0.1"));
        assert!(version_is_older("0", "0.0.1"));
    }

    #[test]
    fn version_is_older_treats_prerelease_as_older() {
        assert!(version_is_older("0.1.0-rc1", "0.1.0"));
        assert!(!version_is_older("0.1.0", "0.1.0-rc1"));
        assert!(version_is_older("0.1.0-rc1", "0.1.1-rc1"));
    }

    #[test]
    fn version_is_older_with_leading_v() {
        assert!(version_is_older("v0.1.0", "v0.1.1"));
        assert!(version_is_older("v0.1.0", "0.1.1"));
        assert!(version_is_older("0.1.0", "v0.1.1"));
    }
}


/// Print just the config file path. Useful for `$(kod config path)`.
pub async fn run_config_path() -> Result<()> {
    let dir = KodConfig::config_dir()?;
    println!("{}", dir.join("config.toml").display());
    Ok(())
}

/// Open the config file in the user's editor. Falls back through
/// `$EDITOR`, `$VISUAL`, `vi`, and `nano` — the first one that exists on
/// PATH is used. Exits non-zero when no editor is available so a script
/// that wants to gate on this can.
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
            .arg(format!("{} {}", editor, shell_quote(&path.to_string_lossy())))
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

/// Quote a string for safe interpolation into a `sh -c` command. Only
/// wraps in single quotes; the common editor invocation is a path, and
/// a path with a single quote in it is pathological.
fn shell_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}


/// Print every filesystem path KOD touches, one per line with a stable
/// key on the left. Deliberately unstyled and stable-keyed so a shell
/// script can `kod which | grep config | cut -f2`.
pub async fn run_which() -> Result<()> {
    let config = KodConfig::load_default()?;

    if let Ok(dir) = KodConfig::config_dir() {
        println!("config\t{}", dir.join("config.toml").display());
    }
    if let Ok(p) = config.memory_db_path() {
        println!("memory\t{}", p.display());
    }
    if let Some(p) = kod_tui::app::KodApp::session_path() {
        println!("session\t{}", p.display());
    }
    if let Some(p) = kod_tui::app::KodApp::history_path() {
        println!("history\t{}", p.display());
    }
    if let Ok(cwd) = std::env::current_dir()
        && let Some(cp) = kod_core::checkpoint::CheckpointManager::for_working_dir(&cwd)
    {
        println!("checkpoints\t{}", cp.dir().display());
    }
    if let Ok(dirs) = config.skills_dirs() {
        for (i, d) in dirs.iter().enumerate() {
            println!("skills.{i}\t{}", d.display());
        }
    }
    Ok(())
}


/// Scaffold a new skill file. Writes into the first writable directory
/// among the standard locations (project-local .agents/skills first,
/// then ~/.agents/skills). Refuses to overwrite an existing file with
/// the same name — a scaffold that silently replaces a real skill is
/// worse than no scaffold.
pub async fn run_skills_new(name: &str) -> Result<()> {
    // Validate the name: kebab-case, [a-z0-9-].
    let trimmed = name.trim();
    if trimmed.is_empty() {
        return Err(KodError::Config("skill name is required".to_string()));
    }
    if !trimmed
        .chars()
        .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
    {
        return Err(KodError::Config(format!(
            "invalid skill name {:?}: use lowercase letters, digits, and hyphens only",
            trimmed
        )));
    }

    let config = KodConfig::load_default()?;
    let dirs = config.skills_dirs()?;

    // Prefer a project-local path if the cwd is inside one; else the
    // first home-level path.
    let cwd = std::env::current_dir().ok();
    let target_dir = dirs
        .iter()
        .find(|d| {
            cwd.as_ref()
                .map(|c| d.starts_with(c) || d.parent().map(|p| p.starts_with(c)).unwrap_or(false))
                .unwrap_or(false)
        })
        .or_else(|| dirs.first())
        .cloned()
        .ok_or_else(|| {
            KodError::Config(
                "could not determine a skills directory to write to".to_string(),
            )
        })?;

    std::fs::create_dir_all(&target_dir).map_err(KodError::Io)?;
    let path = target_dir.join(format!("{trimmed}.md"));
    if path.exists() {
        return Err(KodError::Config(format!(
            "{} already exists — refusing to overwrite",
            path.display()
        )));
    }

    let title = to_title_case(trimmed);
    let body = format!(
        "---\n         name: {name}\n         description: TODO: one-sentence description of what this skill does\n         version: 0.1.0\n         category: general\n         tags: []\n         capabilities: []\n         triggers:\n  - \"TODO trigger phrase\"\n         ---\n\n         # {title}\n\n         ## Instructions\n\n         Describe the skill's guidance here. The model reads this section\n         when the skill's triggers match the user's request.\n\n         ## Examples\n\n         <example input=\"A sample user request\">\n         A sample response that demonstrates the skill.\n         </example>\n\n         ## Constraints\n\n         Optional. Rules the model must respect when applying the skill.\n",
        name = trimmed,
        title = title,
    );

    std::fs::write(&path, body.as_bytes()).map_err(KodError::Io)?;

    println!("Created {}", path.display());
    println!();
    println!("Edit it to fill in the description, triggers, and instructions.");
    println!("Validate with: kod validate-skills");
    Ok(())
}

/// Convert a kebab-case identifier to Title Case for the markdown
/// heading: `rust-refactoring` -> `Rust Refactoring`.
fn to_title_case(s: &str) -> String {
    s.split('-')
        .map(|part| {
            let mut chars = part.chars();
            match chars.next() {
                Some(c) => c.to_uppercase().collect::<String>() + chars.as_str(),
                None => String::new(),
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}
