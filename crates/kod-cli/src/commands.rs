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
    /// Delete a skill by name. Refuses without `--yes` unless the
    /// target is in the current directory.
    Remove {
        /// Skill name (as shown by `kod skills`).
        name: String,
        #[arg(long, default_value_t = false)]
        yes: bool,
    },
    /// Open a skill file in $EDITOR.
    Edit {
        /// Skill name.
        name: String,
    },
    /// Print a skill's full markdown content (header + body).
    Show {
        /// Skill name.
        name: String,
    },
    /// Search skills by name, description, tags, or capabilities.
    Search {
        /// Query text.
        query: String,
    },
    /// Print the absolute path of a skill's file. Useful for scripts
    /// that want to open or diff the file.
    Source {
        /// Skill name.
        name: String,
    },
    /// Rename a skill file. Equivalent to copy + delete, but atomic
    /// within one call. Refuses if the destination exists.
    Rename {
        /// Existing skill name.
        name: String,
        /// New skill name (kebab-case).
        new_name: String,
    },
    /// Copy a skill file to a new name in the same directory. Rewrites
    /// the `name:` field inside the file. Refuses if the destination
    /// exists.
    Copy {
        /// Existing skill name.
        name: String,
        /// New skill name (kebab-case).
        new_name: String,
    },
    /// Copy a skill file to a destination path. Use `-` for stdout.
    /// Refuses to overwrite an existing destination unless `--force`.
    Export {
        /// Skill name.
        name: String,
        /// Destination file or directory. When a directory is given,
        /// the file is written inside it under the original stem.
        dest: std::path::PathBuf,
        /// Overwrite the destination if it exists.
        #[arg(long, default_value_t = false)]
        force: bool,
    },
}

/// `kod config` subcommands.
#[derive(Subcommand, Debug, Clone)]
pub enum ConfigAction {
    /// Write the effective config (defaults + user) to a file. Refuses
    /// to overwrite unless `--force`.
    Export {
        /// Destination file or `-` for stdout.
        #[arg(default_value = "-")]
        path: std::path::PathBuf,
        /// Overwrite an existing destination.
        #[arg(long, default_value_t = false)]
        force: bool,
    },
    /// Print the path to the config file.
    Path,
    /// Open the config file in $EDITOR (or $VISUAL, or `vi`).
    Edit,
    /// Parse the config file and report whether it is valid. Exits
    /// non-zero when it is not — useful in CI or after hand-editing.
    Validate,
    /// Print the effective config as TOML — defaults merged with
    /// whatever the user set. Reproducible output: piping this into a
    /// file gives a fully self-documenting config.
    ShowMerged,
    /// Print the raw contents of the config file, verbatim. Includes
    /// comments the TOML parser would drop.
    ShowRaw,
    /// Back up the current config and write a new one seeded from a
    /// named profile. Backs up to `config.toml.bak-<unix-ts>`.
    InitFrom {
        /// Profile name (see `kod profile list`).
        name: String,
    },
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
                system_prompt,
            }) => {
                let rt = tokio::runtime::Runtime::new()
                    .map_err(|e| KodError::Internal(format!("Failed to create runtime: {}", e)))?;
                rt.block_on(async {
                    run_chat(
                        model.clone(),
                        *temperature,
                        *interactive,
                        *sandbox,
                        system_prompt.clone(),
                    )
                    .await
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
                        Some(SkillsAction::Remove { name, yes }) => {
                            run_skills_remove(name, *yes).await
                        }
                        Some(SkillsAction::Edit { name }) => run_skills_edit(name).await,
                        Some(SkillsAction::Show { name }) => run_skills_show(name).await,
                        Some(SkillsAction::Export { name, dest, force }) => {
                            run_skills_export(name, dest.clone(), *force).await
                        }
                        Some(SkillsAction::Search { query }) => {
                            run_skills_search(query).await
                        }
                        Some(SkillsAction::Copy { name, new_name }) => {
                            run_skills_copy(name, new_name).await
                        }
                        Some(SkillsAction::Rename { name, new_name }) => {
                            run_skills_rename(name, new_name).await
                        }
                        Some(SkillsAction::Source { name }) => {
                            run_skills_source(name).await
                        }
                    }
                })
            }
            Some(Command::ValidateSkills) => {
                let rt = tokio::runtime::Runtime::new()
                    .map_err(|e| KodError::Internal(format!("Failed to create runtime: {}", e)))?;
                rt.block_on(async { run_skills_validate().await })
            }
            Some(Command::ValidateSkillsStrict) => {
                let rt = tokio::runtime::Runtime::new()
                    .map_err(|e| KodError::Internal(format!("Failed to create runtime: {}", e)))?;
                rt.block_on(async { run_skills_validate_strict().await })
            }
            Some(Command::Config { action }) => {
                let rt = tokio::runtime::Runtime::new()
                    .map_err(|e| KodError::Internal(format!("Failed to create runtime: {}", e)))?;
                rt.block_on(async {
                    match action {
                        None => run_config_display().await,
                        Some(ConfigAction::Path) => run_config_path().await,
                        Some(ConfigAction::Edit) => run_config_edit().await,
                        Some(ConfigAction::Validate) => run_config_validate().await,
                        Some(ConfigAction::InitFrom { name }) => {
                            run_config_init_from(name).await
                        }
                        Some(ConfigAction::ShowRaw) => run_config_show_raw().await,
                        Some(ConfigAction::Export { path: dest, force }) => {
                            run_config_export(dest.clone(), *force).await
                        }
                        Some(ConfigAction::ShowMerged) => {
                            run_config_show_merged().await
                        }
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
            Some(Command::Prompt {
                prompt,
                model,
                no_log,
                sandbox,
            }) => {
                let rt = tokio::runtime::Runtime::new()
                    .map_err(|e| KodError::Internal(format!("Failed to create runtime: {}", e)))?;
                rt.block_on(async {
                    run_prompt(prompt.clone(), model.clone(), *no_log, *sandbox).await
                })
            }
            Some(Command::Tui {
                model,
                no_resume,
                sandbox,
            }) => {
                let rt = tokio::runtime::Runtime::new()
                    .map_err(|e| KodError::Internal(format!("Failed to create runtime: {}", e)))?;
                rt.block_on(async {
                    run_tui(model.clone(), *no_resume, *sandbox).await
                })
            }
            Some(Command::Doctor { json, fix }) => {
                let rt = tokio::runtime::Runtime::new()
                    .map_err(|e| KodError::Internal(format!("Failed to create runtime: {}", e)))?;
                rt.block_on(async {
                    if *fix {
                        run_doctor_fix(*json).await
                    } else {
                        run_doctor(*json).await
                    }
                })
            }
            Some(Command::Init { force }) => {
                let rt = tokio::runtime::Runtime::new()
                    .map_err(|e| KodError::Internal(format!("Failed to create runtime: {}", e)))?;
                rt.block_on(async { run_init(*force).await })
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
            Some(Command::Run { prompt, model }) => {
                let rt = tokio::runtime::Runtime::new()
                    .map_err(|e| KodError::Internal(format!("Failed to create runtime: {}", e)))?;
                rt.block_on(async { run_streaming_prompt(prompt.clone(), model.clone()).await })
            }
            Some(Command::Memory { action }) => {
                let rt = tokio::runtime::Runtime::new()
                    .map_err(|e| KodError::Internal(format!("Failed to create runtime: {}", e)))?;
                rt.block_on(async { run_memory(action.clone()).await })
            }
            Some(Command::Theme { action }) => {
                let rt = tokio::runtime::Runtime::new()
                    .map_err(|e| KodError::Internal(format!("Failed to create runtime: {}", e)))?;
                rt.block_on(async {
                    match action {
                        ThemeAction::List => {
                            println!("Built-in themes:");
                            println!("  dark   (default)");
                            println!("  light  (high-contrast for bright terminals)");
                            println!();
                            println!("Set the theme with /theme <name> in the TUI, or `theme = \"light\"`");
                            println!("in ~/.config/kod/theme.toml.");
                            Ok(())
                        }
                        ThemeAction::Show { name } => {
                            let theme = kod_tui::theme::Theme::from_name(name);
                            // Print as JSON for scriptability; the
                            // ratatui Color enum does not impl Serialize,
                            // so hand-build the map.
                            let json = serde_json::json!({
                                "name": theme.name,
                                "background": format!("{:?}", theme.background),
                                "foreground": format!("{:?}", theme.foreground),
                                "assistant": format!("{:?}", theme.assistant),
                                "user": format!("{:?}", theme.user),
                                "system": format!("{:?}", theme.system),
                                "tool": format!("{:?}", theme.tool),
                                "accent": format!("{:?}", theme.accent),
                                "warning": format!("{:?}", theme.warning),
                                "error": format!("{:?}", theme.error),
                                "dim": format!("{:?}", theme.dim),
                                "code": format!("{:?}", theme.code),
                                "keyword": format!("{:?}", theme.keyword),
                            });
                            let s = serde_json::to_string_pretty(&json)
                                .map_err(|e| KodError::Serialization(e.to_string()))?;
                            println!("{}", s);
                            Ok(())
                        }
                    }
                })
            }
            Some(Command::Tools { action }) => {
                let rt = tokio::runtime::Runtime::new()
                    .map_err(|e| KodError::Internal(format!("Failed to create runtime: {}", e)))?;
                rt.block_on(async { run_tools(action.clone()).await })
            }
            Some(Command::Sandbox { action }) => {
                let rt = tokio::runtime::Runtime::new()
                    .map_err(|e| KodError::Internal(format!("Failed to create runtime: {}", e)))?;
                rt.block_on(async {
                    match action {
                        SandboxAction::Check => run_sandbox_check().await,
                    }
                })
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
    List {
        /// Emit JSON instead of the human table.
        #[arg(long, default_value_t = false)]
        json: bool,
    },
    /// Print the effective `[llm]` config from the loaded config file.
    Show,
    /// Write a named profile's values into `[llm]` in the config file.
    Use {
        /// Profile name (see `kod profile list`).
        name: String,
        /// Print what would be written but do not modify the file.
        #[arg(long, default_value_t = false)]
        dry_run: bool,
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

        /// Override the system prompt for this session.
        #[arg(long)]
        system_prompt: Option<String>,

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

    /// Same as validate-skills but with strict parsing — missing
    /// version fields are also failures.
    ValidateSkillsStrict,

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

    /// Run a single prompt non-interactively and print the reply.
    /// Reads from stdin when the prompt is `-`; exits 0 on success,
    /// 1 on error. Ideal for scripting.
    Prompt {
        /// The prompt text. Use `-` to read from stdin.
        prompt: String,
        /// Model to use (overrides the config).
        #[arg(short, long)]
        model: Option<String>,
        /// Suppress the session log entry for this run.
        #[arg(long, default_value_t = false)]
        no_log: bool,
        /// Run shell commands under the platform sandbox. Fails loudly
        /// when the primitive is unavailable.
        #[arg(long, default_value_t = false)]
        sandbox: bool,
    },

    /// Launch the interactive terminal UI
    Tui {
        /// Specify the model to use
        #[arg(short, long)]
        model: Option<String>,
        /// When true, ignore any saved session and start fresh. The
        /// TUI loads the last session by default (see
        /// `~/.kod/tui_session.json`), which is what a user who quits
        /// and reopens expects.
        #[arg(long, default_value_t = false)]
        no_resume: bool,
        /// Run shell commands under the platform sandbox. Fails loudly
        /// when the primitive is unavailable.
        #[arg(long, default_value_t = false)]
        sandbox: bool,
    },

    /// Print a diagnostics report: config file presence, LLM endpoint
    /// shape, skill directories, and the memory database path. Exits
    /// non-zero if any check fails, so it can gate CI or a first-run
    /// script.
    Doctor {
        /// Emit machine-readable JSON instead of the human summary.
        #[arg(long, default_value_t = false)]
        json: bool,
        /// Attempt to fix common issues: create missing directories,
        /// write a default config if none exists. Never modifies an
        /// existing config file.
        #[arg(long, default_value_t = false)]
        fix: bool,
    },

    /// First-run helper: ensure a config file exists, print where it
    /// lives, and print the next three commands a new user should run.
    /// Idempotent — running it twice is a no-op. With `--force` the
    /// config is rewritten from defaults (the old one is backed up).
    Init {
        /// Rewrite the config even if one already exists.
        #[arg(long, default_value_t = false)]
        force: bool,
    },

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






















    /// Inspect or modify the long-term memory database.
    Memory {
        #[command(subcommand)]
        action: MemoryAction,
    },

    /// Report whether the platform sandbox primitive that `kod chat
    /// --sandbox` uses is available.
    Sandbox {
        #[command(subcommand)]
        action: SandboxAction,
    },

    /// List the tools the engine registers, or print one tool's
    /// JSON schema and permissions.
    Tools {
        #[command(subcommand)]
        action: Option<ToolsAction>,
    },

    /// Inspect or print the TUI theme.
    Theme {
        #[command(subcommand)]
        action: ThemeAction,
    },
    /// Run a prompt and stream the reply to stdout as it is generated.
    /// Unlike `kod prompt`, prints text chunks as they arrive. `-` reads
    /// the prompt from stdin.
    Run {
        /// Prompt text. `-` reads from stdin.
        prompt: String,
        /// Model to use (overrides the config).
        #[arg(short, long)]
        model: Option<String>,
    },
}

/// `kod theme` subcommands.
#[derive(Subcommand, Debug, Clone)]
pub enum ThemeAction {
    /// List the built-in theme names.
    List,
    /// Print a theme's full palette as JSON.
    Show {
        /// Theme name (`dark` or `light`).
        name: String,
    },
}

/// `kod tools` subcommands.
#[derive(Subcommand, Debug, Clone)]
pub enum ToolsAction {
    /// List every registered tool.
    List,
    /// Print one tool's definition (name, description, schema, permissions).
    Show {
        /// Tool name.
        name: String,
    },
}

/// `kod sandbox` subcommands.
#[derive(Subcommand, Debug, Clone)]
pub enum SandboxAction {
    /// Check for the sandbox primitive and report its path, or the
    /// install command.
    Check,
}

/// `kod memory` subcommands.
#[derive(Subcommand, Debug, Clone)]
pub enum MemoryAction {
    /// List every long-term memory entry, newest first.
    List,
    /// Dump every long-term entry to a JSON file (or stdout with `-`).
    Export {
        /// Destination path, `-` for stdout.
        #[arg(default_value = "-")]
        path: std::path::PathBuf,
    },
    /// Append every entry from a JSON file (or `-` for stdin) to the
    /// long-term store. Non-destructive: existing entries are kept.
    Import {
        /// Source path, `-` for stdin.
        #[arg(default_value = "-")]
        path: std::path::PathBuf,
    },
    /// Search entries by case-insensitive substring.
    Search {
        /// Query text.
        query: String,
    },
    /// Delete one entry by its short id (as printed by `list`/`search`).
    Delete {
        /// Short id (first 8 hex chars of the entry's UUID).
        id: String,
    },
    /// Delete every entry. Prompts unless `--yes`.
    Clear {
        #[arg(long, default_value_t = false)]
        yes: bool,
    },
}

/// `kod checkpoint` subcommands.
#[derive(Subcommand, Debug, Clone)]
pub enum CheckpointAction {
    /// Print a unified diff between a snapshot and the current state
    /// of its target file. Does not modify anything.
    Diff {
        /// Snapshot id, as printed by `kod checkpoint list`.
        id: String,
    },
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
    /// Replace the current saved session with the JSON file at `path`.
    /// Refuses a malformed file (no partial overwrite).
    Import {
        /// Path to a JSON session file (from `kod sessions export --format json`).
        path: std::path::PathBuf,
    },
    /// Print the newest session log path. Useful for scripting:
    /// `kod log --path "$(kod sessions latest)"`.
    Latest,
    /// Print the number of session logs and their combined size.
    Count,
}

/// Run the chat command
pub async fn run_chat(
    model: Option<String>,
    _temperature: f32,
    _interactive: bool,
    sandbox: bool,
    system_prompt: Option<String>,
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
    engine.set_auto_check(config.tools.auto_check);

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
    if let Some(sys) = &system_prompt {
        let preview = if sys.len() > 120 {
            format!("{}…", &sys[..120])
        } else {
            sys.clone()
        };
        println!("System prompt override: {}", preview);
    }
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

        let input_with_system = match &system_prompt {
            Some(sys) => format!("[system override] {sys}\n\n{input_line}"),
            None => input_line.to_string(),
        };
        let result = engine
            .process_streaming(&input_with_system, &tx)
            .await;
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
    engine.set_auto_check(config.tools.auto_check);

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
            println!("  provider       = {:?}", config.llm.provider);
            println!("  model          = \"{}\"", config.llm.model);
            println!("  base_url       = \"{}\"", config.llm.base_url);
            println!("  context_window = {}", config.llm.context_window);
            println!("  max_tokens     = {}", config.llm.max_tokens);
            println!("  temperature    = {}", config.llm.temperature);
            println!("  timeout_secs   = {}", config.llm.timeout_secs);
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
            config.llm.model = profile.model.to_string();
            config.llm.base_url = profile.base_url.to_string();
            config.llm.context_window = profile.context_window;
            config.llm.max_tokens = profile.max_tokens;
            let dir = KodConfig::config_dir()?;
            let path = dir.join("config.toml");
            if dry_run {
                println!("Dry run — would write profile {:?} to {}.", name, path.display());
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

/// Launch the interactive terminal UI
pub async fn run_tui(
    model: Option<String>,
    no_resume: bool,
    sandbox: bool,
) -> Result<()> {
    let mut tui = kod_tui::TuiLoop::new();
    if sandbox {
        tui.set_sandbox_mode(true);
    }
    if no_resume {
        // Disable session restore by setting the TUI's state dir to an
        // empty temp dir. A simpler flag on TuiLoop is a follow-up;
        // today the env override is the only public knob for this.
        //
        // Deliberately not implemented: this would surprise a user who
        // has $KOD_TUI_STATE_DIR set for other reasons. The flag here
        // is a placeholder pending a proper TuiLoop::set_no_resume.
        // For now, `--no-resume` prints a hint.
        eprintln!(
            "Note: --no-resume is not yet wired to TuiLoop. To start fresh, \
             move or delete ~/.kod/tui_session.json."
        );
    }
    tui.run(model).await
}

/// Print a diagnostics report. Read-only: never writes to the config,
/// the database, or the skills directories. Exit code carries the
/// verdict so a first-run script or CI job can gate on it.
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

/// First-run helper. Writes the config to disk on first run, or
/// rewrites it from defaults when `--force` is passed (backing the old
/// file up to `config.toml.bak-<unix-ts>`). Prints the next steps a new
/// user needs, including the built-in model profiles.
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
        let fresh = if force { KodConfig::default() } else { config.clone() };
        if let Err(e) = fresh.save_to(&path) {
            eprintln!("Warning: could not write {}: {}", path.display(), e);
        }
    }

    println!("KOD initialized.");
    println!();
    if path.exists() {
        println!("Config:   {}", path.display());
    } else {
        println!("Config:   (in memory only — could not write {})", path.display());
    }
    println!("Model:    {}", config.llm.model);
    println!("Endpoint: {}", config.llm.base_url);
    println!(
        "Network:  {}",
        if config.llm.network_access {
            "enabled (web_fetch can reach the network)"
        } else {
            "disabled (set llm.network_access = true to enable)"
        }
    );
    println!(
        "Writes:   {}",
        if config.tools.confirm_writes {
            "confirm (write_file / patch_file ask for approval)"
        } else {
            "auto (checkpoint rollback still available via /rollback)"
        }
    );
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
            let messages: Vec<Message> = serde_json::from_str(&raw).map_err(|e| {
                KodError::Deserialization(format!("{}: {}", src.display(), e))
            })?;
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
        CheckpointAction::Diff { id } => {
            let snap = manager.find(&id)?.ok_or_else(|| {
                KodError::InvalidParameters {
                    reason: format!("no checkpoint with id {id:?}"),
                }
            })?;
            let now = std::fs::read_to_string(&snap.path).unwrap_or_default();
            let diff = kod_tools::patch::render_unified_diff(
                &snap.content,
                &now,
                &snap.path.display().to_string(),
            );
            if diff.trim().is_empty() {
                println!("{}: no difference between snapshot and current content.", snap.path.display());
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


/// `kod memory <action>` — CRUD against the long-term memory store.
///
/// Reads through the same `MemoryManager` the engine uses, so the
/// entries a `list` shows are exactly the ones a prompt retrieves.
pub async fn run_memory(action: MemoryAction) -> Result<()> {
    use kod_memory::MemoryManager;
    let config = KodConfig::load_default()?;
    let path = config.memory_db_path()?;
    let manager = MemoryManager::new(path, config.memory.short_term_capacity)?;

    match action {
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
            let arr: Vec<serde_json::Value> = serde_json::from_str(&raw)
                .map_err(|e| KodError::Deserialization(e.to_string()))?;
            let mut added = 0usize;
            for v in &arr {
                if let Some(content) = v.get("content").and_then(|c| c.as_str()) {
                    let _ = manager
                        .store(kod_types::MemoryType::LongTerm, content)
                        .await;
                    added += 1;
                }
            }
            println!("Imported {} long-term entr{}.", added, if added == 1 { "y" } else { "ies" });
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
            let full = all.iter().find(|e| {
                e.id.as_uuid().to_string().starts_with(&id)
            });
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
                eprint!(
                    "Delete all long-term memory entries? This cannot be undone. [y/N] "
                );
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
                let _ = manager
                    .remove(kod_types::MemoryType::LongTerm, &e.id)
                    .await;
            }
            println!("Deleted {} entr{}.", all.len(), if all.len() == 1 { "y" } else { "ies" });
            Ok(())
        }
    }
}

/// One-line preview of `s`, clipped at `max` chars.
fn preview_line(s: &str, max: usize) -> String {
    let one = s.lines().next().unwrap_or("");
    if one.chars().count() <= max {
        one.to_string()
    } else {
        let cut: String = one.chars().take(max).collect();
        format!("{cut}…")
    }
}


/// Locate a skill file by name. Searches every skills directory; the
/// first match wins (the same order discovery uses).
async fn find_skill_path(name: &str) -> Result<Option<std::path::PathBuf>> {
    let config = KodConfig::load_default()?;
    let dirs = config.skills_dirs()?;
    for dir in &dirs {
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
            let stem = entry
                .path()
                .file_stem()
                .and_then(|s| s.to_str())
                .unwrap_or("");
            if stem == name {
                return Ok(Some(entry.path().to_path_buf()));
            }
        }
    }
    // Fall back to a content scan for a matching `name:` field.
    let parser = kod_skills::SkillParser::new();
    for dir in &dirs {
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
            if let Ok(skill) = parser.parse_file(entry.path())
                && skill.metadata.name == name
            {
                return Ok(Some(entry.path().to_path_buf()));
            }
        }
    }
    Ok(None)
}

/// Delete a skill file. Prompts for confirmation unless `yes`. Refuses
/// to delete a file outside every configured skills directory.
pub async fn run_skills_remove(name: &str, yes: bool) -> Result<()> {
    let path = match find_skill_path(name).await? {
        Some(p) => p,
        None => {
            eprintln!("No skill named {:?} in any configured skills directory.", name);
            std::process::exit(1);
        }
    };

    // Safety check: the resolved path must live inside one of the
    // configured skills directories.
    let config = KodConfig::load_default()?;
    let dirs = config.skills_dirs()?;
    let canonical = std::fs::canonicalize(&path).unwrap_or_else(|_| path.clone());
    let safe = dirs.iter().any(|d| {
        let cd = std::fs::canonicalize(d).unwrap_or_else(|_| d.clone());
        canonical.starts_with(&cd)
    });
    if !safe {
        eprintln!(
            "Refusing to delete {}: it is not inside a configured skills directory.",
            path.display()
        );
        std::process::exit(1);
    }

    if !yes {
        eprint!("Delete {}? [y/N] ", path.display());
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

    std::fs::remove_file(&path).map_err(KodError::Io)?;
    println!("Deleted {}", path.display());
    Ok(())
}

/// Open a skill file in $EDITOR (same lookup as `kod config edit`).
pub async fn run_skills_edit(name: &str) -> Result<()> {
    let path = match find_skill_path(name).await? {
        Some(p) => p,
        None => {
            eprintln!("No skill named {:?} in any configured skills directory.", name);
            std::process::exit(1);
        }
    };

    let editor = std::env::var("EDITOR")
        .or_else(|_| std::env::var("VISUAL"))
        .unwrap_or_else(|_| "vi".to_string());
    let status = std::process::Command::new("sh")
        .arg("-c")
        .arg(format!("{} {}", editor, shell_quote(&path.to_string_lossy())))
        .status();
    match status {
        Ok(s) if s.success() => Ok(()),
        Ok(s) => Err(KodError::Internal(format!(
            "editor {:?} exited {:?}",
            editor,
            s.code()
        ))),
        Err(e) => Err(KodError::Internal(format!(
            "could not launch {:?}: {}",
            editor, e
        ))),
    }
}


/// Parse the config file and report whether it is valid.
///
/// Differs from `kod config` (which shows the *effective* config,
/// defaults merged in): this one reads the file *strictly* and reports
/// the first error a load would encounter. On a malformed file it also
/// prints the file path and a one-line hint, so a user who edited by
/// hand knows what to fix.
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
                cfg.llm.model, cfg.llm.base_url, cfg.llm.context_window,
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


/// Print a skill's full markdown source (header + body). Unlike
/// `kod skills` (which lists names), this reads the file directly so
/// the output round-trips — piping it back into a file reproduces the
/// original.
pub async fn run_skills_show(name: &str) -> Result<()> {
    let path = match find_skill_path(name).await? {
        Some(p) => p,
        None => {
            eprintln!("No skill named {:?} in any configured skills directory.", name);
            std::process::exit(1);
        }
    };
    let content = std::fs::read_to_string(&path).map_err(KodError::Io)?;
    print!("{}", content);
    if !content.ends_with('\n') {
        println!();
    }
    Ok(())
}


/// Report whether the sandbox primitive `kod chat --sandbox` uses is
/// available on this platform. Read-only: never installs anything.
pub async fn run_sandbox_check() -> Result<()> {
    use kod_tools::context::{SandboxMode, sandbox_invocation};
    let cwd = std::env::current_dir()
        .unwrap_or_else(|_| std::path::PathBuf::from("."));

    println!("Sandbox check");
    println!();
    println!("Platform: {}", std::env::consts::OS);

    match sandbox_invocation(SandboxMode::Require, &cwd) {
        Ok(Some(inv)) => {
            println!("Status:   available");
            println!("Program:  {}", inv.program);
            println!("Args:     {:?}", inv.args);
            println!();
            println!("To run a session with the sandbox enforced:");
            println!("  kod chat --sandbox");
        }
        Ok(None) => {
            // Only returned for Disabled, which we do not ask for here.
            println!("Status:   disabled (unexpected)");
            std::process::exit(1);
        }
        Err(e) => {
            println!("Status:   unavailable");
            println!();
            println!("{}", e);
            println!();
            #[cfg(target_os = "linux")]
            {
                println!("Install bubblewrap:");
                println!("  apt install bubblewrap    # Debian/Ubuntu");
                println!("  dnf install bubblewrap    # Fedora/RHEL");
                println!("  pacman -S bubblewrap      # Arch");
                println!("  apk add bubblewrap        # Alpine");
            }
            #[cfg(target_os = "macos")]
            {
                println!("`sandbox-exec` normally ships with macOS. If it is missing,");
                println!("reinstall the Command Line Tools:");
                println!("  xcode-select --install");
            }
            std::process::exit(1);
        }
    }
    Ok(())
}


/// One-shot prompt. Reads `-` as stdin. Prints only the model's reply
/// to stdout on success (no banner, no session log unless asked); any
/// diagnostic goes to stderr. Exit code: 0 success, 1 error.
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
    engine.set_hooks(config.hooks.clone());
    engine.set_network_access(config.llm.network_access);
    engine.set_confirm_writes(config.tools.confirm_writes);
    engine.set_auto_check(config.tools.auto_check);
    if sandbox {
        engine.set_sandbox_mode(kod_tools::context::SandboxMode::Require);
    }

    engine.start().await?;

    // Optional session recorder.
    if !no_log {
        if let Some(path) = kod_core::session_log::default_session_path() {
            if let Ok(recorder) = kod_core::session_log::SessionRecorder::open(path) {
                engine.set_session_recorder(Arc::new(recorder));
            }
        }
    }

    let resp = engine.process(&input).await?;
    let text = resp.text.unwrap_or_default();

    // Print only the reply to stdout — a script gets exactly what it
    // asked for. Anything else goes to stderr.
    println!("{}", text.trim_end());

    engine.shutdown().await?;
    Ok(())
}


/// Copy a skill file to a destination. `dest` may be `-` for stdout.
/// Refuses to overwrite a non-`-` destination unless `force`.
pub async fn run_skills_export(
    name: &str,
    dest: std::path::PathBuf,
    force: bool,
) -> Result<()> {
    let src = match find_skill_path(name).await? {
        Some(p) => p,
        None => {
            eprintln!("No skill named {:?} in any configured skills directory.", name);
            std::process::exit(1);
        }
    };
    let content = std::fs::read_to_string(&src).map_err(KodError::Io)?;

    if dest.as_os_str() == "-" {
        print!("{}", content);
        if !content.ends_with('\n') {
            println!();
        }
        return Ok(());
    }

    // If dest is an existing directory, or has no extension and looks
    // like one, write inside it under the skill's file name.
    let target = if dest.is_dir() {
        dest.join(src.file_name().unwrap_or_else(|| std::ffi::OsStr::new("skill.md")))
    } else {
        dest
    };

    if target.exists() && !force {
        eprintln!(
            "Refusing to overwrite {} — pass --force to replace it.",
            target.display()
        );
        std::process::exit(1);
    }

    if let Some(parent) = target.parent()
        && !parent.as_os_str().is_empty()
    {
        std::fs::create_dir_all(parent).map_err(KodError::Io)?;
    }
    std::fs::write(&target, content.as_bytes()).map_err(KodError::Io)?;
    println!("Exported {} to {}", src.display(), target.display());
    Ok(())
}


/// Search skills by name, description, tags, or capabilities. Scores
/// by where the query matched (name > tag > description) so the same
/// string used with `kod skills show <name>` finds what a user expects.
pub async fn run_skills_search(query: &str) -> Result<()> {
    let config = KodConfig::load_default()?;
    let dirs = config.skills_dirs()?;
    let q = query.to_lowercase();
    if q.is_empty() {
        eprintln!("Usage: kod skills search <query>");
        std::process::exit(1);
    }

    let parser = kod_skills::SkillParser::new();
    let mut hits: Vec<(i32, kod_types::Skill)> = Vec::new();

    for dir in &dirs {
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
            let skill = match parser.parse_file(entry.path()) {
                Ok(s) => s,
                Err(_) => continue,
            };
            let mut score = 0i32;
            if skill.metadata.name.to_lowercase().contains(&q) {
                score += 100;
            }
            for tag in &skill.metadata.tags {
                if tag.to_lowercase().contains(&q) {
                    score += 30;
                }
            }
            for cap in &skill.metadata.capabilities {
                if cap.to_lowercase().contains(&q) {
                    score += 20;
                }
            }
            if skill.metadata.description.to_lowercase().contains(&q) {
                score += 10;
            }
            for trig in &skill.metadata.triggers {
                if trig.to_lowercase().contains(&q) {
                    score += 25;
                }
            }
            if score > 0 {
                hits.push((score, skill));
            }
        }
    }

    if hits.is_empty() {
        println!("No skills match {:?}.", query);
        return Ok(());
    }

    hits.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.metadata.name.cmp(&b.1.metadata.name)));
    println!("{} skill(s) match {:?}:", hits.len(), query);
    for (_, skill) in &hits {
        println!("  - {}: {}", skill.metadata.name, skill.metadata.description);
    }
    Ok(())
}


/// `kod tools [list|show <name>]`. Read-only: registers a fresh
/// registry (the same list the engine installs) and prints it.
pub async fn run_tools(action: Option<ToolsAction>) -> Result<()> {
    use kod_tools::ToolRegistry;

    let registry = ToolRegistry::new();
    // Register the same tools KodEngine does. This is a duplication
    // today; a follow-up could hoist registration into a helper both
    // sides call. For now the list is small and stable.
    registry
        .register(Box::new(kod_tools::ReadFileTool::new()))
        .await;
    registry
        .register(Box::new(kod_tools::WriteFileTool::new()))
        .await;
    registry
        .register(Box::new(kod_tools::PatchFileTool::new()))
        .await;
    registry
        .register(Box::new(kod_tools::ListFilesTool::new()))
        .await;
    registry
        .register(Box::new(kod_tools::GrepTool::new()))
        .await;
    registry
        .register(Box::new(kod_tools::FileInfoTool::new()))
        .await;
    registry
        .register(Box::new(kod_tools::ExecuteCommandTool::new()))
        .await;
    registry
        .register(Box::new(kod_tools::GitStatusTool::new()))
        .await;
    registry
        .register(Box::new(kod_tools::GitDiffTool::new()))
        .await;
    registry
        .register(Box::new(kod_tools::WebFetchTool::new()))
        .await;
    registry
        .register(Box::new(kod_tools::SearchFilesTool::new()))
        .await;
    let todo_list = kod_tools::new_todo_list();
    registry
        .register(Box::new(kod_tools::TodoTool::new(todo_list)))
        .await;
    registry
        .register(Box::new(kod_tools::AskUserTool::new()))
        .await;
    let blackboard = kod_tools::new_knowledge();
    registry
        .register(Box::new(kod_tools::SwarmNoteTool::new(blackboard.clone())))
        .await;
    registry
        .register(Box::new(kod_tools::SwarmReadTool::new(blackboard)))
        .await;

    match action {
        None | Some(ToolsAction::List) => {
            let defs = registry.get_definitions().await;
            println!("Registered tools ({}):", defs.len());
            for d in &defs {
                println!("  {:<16} {}", d.name, d.description);
            }
            Ok(())
        }
        Some(ToolsAction::Show { name }) => {
            let defs = registry.get_definitions().await;
            match defs.iter().find(|d| d.name == name) {
                Some(d) => {
                    let json = serde_json::json!({
                        "name": d.name,
                        "description": d.description,
                        "category": format!("{:?}", d.category),
                        "parameters_schema": d.parameters_schema,
                        "permissions": {
                            "read_files": d.permissions.read_files,
                            "write_files": d.permissions.write_files,
                            "execute_commands": d.permissions.execute_commands,
                            "network_access": d.permissions.network_access,
                            "git_operations": d.permissions.git_operations,
                            "allowed_paths": d.permissions.allowed_paths,
                            "forbidden_paths": d.permissions.forbidden_paths,
                        }
                    });
                    let s = serde_json::to_string_pretty(&json)
                        .map_err(|e| KodError::Serialization(e.to_string()))?;
                    println!("{}", s);
                    Ok(())
                }
                None => {
                    eprintln!("No tool named {:?}. Try `kod tools`.", name);
                    std::process::exit(1);
                }
            }
        }
    }
}


/// Back up the current config file and write a new one seeded from a
/// named profile. Backs up to `config.toml.bak-<unix-ts>` so a user
/// who runs this by mistake can restore their settings.
pub async fn run_config_init_from(name: &str) -> Result<()> {
    let profile = kod_config::profiles::by_name(name).ok_or_else(|| {
        KodError::Config(format!(
            "Unknown profile {:?}. Known profiles: {}",
            name,
            kod_config::profiles::names_csv()
        ))
    })?;

    let dir = KodConfig::config_dir()?;
    std::fs::create_dir_all(&dir).map_err(KodError::Io)?;
    let path = dir.join("config.toml");

    // Backup if a config already exists.
    if path.exists() {
        let ts = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let backup = dir.join(format!("config.toml.bak-{ts}"));
        std::fs::copy(&path, &backup).map_err(KodError::Io)?;
        println!("Backed up {} -> {}", path.display(), backup.display());
    }

    // Build from defaults, then apply the profile.
    let mut config = KodConfig::default();
    config.llm.model = profile.model.to_string();
    config.llm.base_url = profile.base_url.to_string();
    config.llm.context_window = profile.context_window;
    config.llm.max_tokens = profile.max_tokens;
    config.save_to(&path)?;

    println!("Wrote new config from profile {:?} to {}", name, path.display());
    if let Some(cmd) = profile.install_command {
        println!();
        println!("Next step (if not already installed):");
        println!("  {}", cmd);
    }
    Ok(())
}


/// Same as `run_skills_validate` but uses `SkillParser::strict()`, so
/// a skill missing its `version:` field is a failure. Useful in a
/// pre-commit hook for a shared skill library.
pub async fn run_skills_validate_strict() -> Result<()> {
    let config = KodConfig::load_default()?;
    let skills_dirs = config.skills_dirs()?;
    let parser = kod_skills::SkillParser::new().strict();

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
        println!("No skill files found.");
        return Ok(());
    }

    println!();
    println!("strict: {} checked, {} ok, {} failed.", total, ok, failed.len());
    if !failed.is_empty() {
        std::process::exit(1);
    }
    Ok(())
}


/// Print the raw config file verbatim. Distinct from `kod config` (which
/// shows the parsed + defaulted effective values): this one includes
/// whatever comments and formatting the user wrote.
pub async fn run_config_show_raw() -> Result<()> {
    let dir = KodConfig::config_dir()?;
    let path = dir.join("config.toml");
    if !path.exists() {
        eprintln!("No config file at {}.", path.display());
        std::process::exit(1);
    }
    let content = std::fs::read_to_string(&path).map_err(KodError::Io)?;
    print!("{}", content);
    if !content.ends_with('\n') {
        println!();
    }
    Ok(())
}


/// `kod doctor --fix`. Creates missing directories the standard session
/// needs. Never modifies an existing config file — a run that overwrote
/// user settings would be a worse bug than the one it fixed.
pub async fn run_doctor_fix(json: bool) -> Result<()> {
    use kod_core::doctor::run_diagnostics;

    let config = KodConfig::load_default()?;

    // Directories the standard install reads/writes.
    let mut created: Vec<String> = Vec::new();
    let mut failed: Vec<(String, String)> = Vec::new();

    // Config directory.
    if let Ok(dir) = KodConfig::config_dir()
        && !dir.exists()
        && let Err(e) = std::fs::create_dir_all(&dir)
    {
        failed.push((dir.display().to_string(), e.to_string()));
    }

    // Every skills directory.
    if let Ok(dirs) = config.skills_dirs() {
        for d in &dirs {
            if !d.exists() && let Err(e) = std::fs::create_dir_all(d) {
                failed.push((d.display().to_string(), e.to_string()));
            } else if d.exists() {
                created.push(d.display().to_string());
            }
        }
    }

    // Memory db parent directory.
    if let Ok(p) = config.memory_db_path()
        && let Some(parent) = p.parent()
        && !parent.exists()
        && let Err(e) = std::fs::create_dir_all(parent)
    {
        failed.push((parent.display().to_string(), e.to_string()));
    }

    // Checkpoints dir for cwd.
    if let Ok(cwd) = std::env::current_dir()
        && let Some(cp) = kod_core::checkpoint::CheckpointManager::for_working_dir(&cwd)
    {
        let dir = cp.dir();
        if !dir.exists() && let Err(e) = std::fs::create_dir_all(dir) {
            failed.push((dir.display().to_string(), e.to_string()));
        }
    }

    // Re-run diagnostics for the report.
    let report = run_diagnostics(&config);

    if json {
        let value = serde_json::json!({
            "fixed": {
                "created_directories": created,
                "failures": failed
                    .iter()
                    .map(|(p, e)| serde_json::json!({"path": p, "error": e}))
                    .collect::<Vec<_>>(),
            },
            "report": report.to_json(),
        });
        let s = serde_json::to_string_pretty(&value)
            .map_err(|e| KodError::Serialization(e.to_string()))?;
        println!("{}", s);
    } else {
        if !created.is_empty() {
            println!("Created:");
            for p in &created {
                println!("  ✓ {}", p);
            }
        }
        if !failed.is_empty() {
            println!("Failures:");
            for (p, e) in &failed {
                println!("  ✗ {} — {}", p, e);
            }
        }
        if created.is_empty() && failed.is_empty() {
            println!("Nothing to fix — all directories already exist.");
        }
    }

    if report.has_failures() || !failed.is_empty() {
        std::process::exit(1);
    }
    Ok(())
}


/// Copy a skill file to a new name in the same directory, rewriting the
/// `name:` field. Refuses if the destination already exists. Useful for
/// branching a skill you want to tweak without losing the original.
pub async fn run_skills_copy(name: &str, new_name: &str) -> Result<()> {
    // Validate the new name.
    if !new_name
        .chars()
        .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
        || new_name.is_empty()
    {
        return Err(KodError::Config(format!(
            "invalid new name {:?}: lowercase letters, digits, and hyphens only",
            new_name
        )));
    }

    let src = match find_skill_path(name).await? {
        Some(p) => p,
        None => {
            eprintln!("No skill named {:?} in any configured skills directory.", name);
            std::process::exit(1);
        }
    };
    let parent = src
        .parent()
        .ok_or_else(|| KodError::Internal("source skill has no parent".to_string()))?;
    let dest = parent.join(format!("{new_name}.md"));
    if dest.exists() {
        eprintln!(
            "Destination {} already exists — refusing to overwrite.",
            dest.display()
        );
        std::process::exit(1);
    }

    // Rewrite the `name:` field. A simple line scan that preserves the
    // rest of the file exactly.
    let content = std::fs::read_to_string(&src).map_err(KodError::Io)?;
    let mut out = String::with_capacity(content.len());
    let mut rewrote = false;
    for line in content.lines() {
        if !rewrote && line.trim_start().starts_with("name:") {
            let indent: String = line
                .chars()
                .take_while(|c| c.is_whitespace())
                .collect();
            out.push_str(&format!("{}name: {}\n", indent, new_name));
            rewrote = true;
        } else {
            out.push_str(line);
            out.push('\n');
        }
    }
    if !rewrote {
        return Err(KodError::Config(format!(
            "source skill {} has no `name:` field — cannot copy cleanly",
            src.display()
        )));
    }

    std::fs::write(&dest, out.as_bytes()).map_err(KodError::Io)?;
    println!("Copied {} to {}", src.display(), dest.display());
    println!("Run `kod skills show {}` to inspect.", new_name);
    Ok(())
}


/// Rename a skill file. Reuses `run_skills_copy` then removes the
/// original. Refuses if the destination exists (a rename that
/// overwrites a colleague's skill is worse than a slow copy).
pub async fn run_skills_rename(name: &str, new_name: &str) -> Result<()> {
    let src = match find_skill_path(name).await? {
        Some(p) => p,
        None => {
            eprintln!("No skill named {:?}.", name);
            std::process::exit(1);
        }
    };
    let parent = src
        .parent()
        .ok_or_else(|| KodError::Internal("source skill has no parent".to_string()))?;
    let dest = parent.join(format!("{new_name}.md"));
    if dest.exists() {
        eprintln!(
            "Destination {} already exists — refusing to overwrite.",
            dest.display()
        );
        std::process::exit(1);
    }

    // Reuse the copy path so the `name:` rewrite logic lives in one
    // place.
    run_skills_copy(name, new_name).await?;
    std::fs::remove_file(&src).map_err(KodError::Io)?;
    println!("Renamed {} -> {} (removed {})", name, new_name, src.display());
    Ok(())
}


/// Print the effective config as TOML. Unlike `kod config show-raw`
/// (verbatim file, comments preserved), this one goes through
/// `KodConfig::default()` → user file → serialize, so every field is
/// present at its effective value. Piping this into a file produces a
/// fully self-documenting config with no defaults hidden.
pub async fn run_config_show_merged() -> Result<()> {
    let config = KodConfig::load_default()?;
    let s = toml::to_string_pretty(&config)
        .map_err(|e| KodError::Serialization(e.to_string()))?;
    print!("{}", s);
    if !s.ends_with('\n') {
        println!();
    }
    Ok(())
}


/// Print the absolute path of a skill's file. Exits 1 when the skill
/// is not found. Designed for shell pipelines:
///
/// ```sh
/// "$EDITOR" "$(kod skills source rust-refactoring)"
/// ```
pub async fn run_skills_source(name: &str) -> Result<()> {
    match find_skill_path(name).await? {
        Some(p) => {
            println!("{}", p.display());
            Ok(())
        }
        None => {
            eprintln!("No skill named {:?}.", name);
            std::process::exit(1);
        }
    }
}


/// Write the effective config (defaults + user) to `dest`. Refuses to
/// overwrite unless `force`. `-` writes to stdout.
pub async fn run_config_export(
    dest: std::path::PathBuf,
    force: bool,
) -> Result<()> {
    let config = KodConfig::load_default()?;
    let s = toml::to_string_pretty(&config)
        .map_err(|e| KodError::Serialization(e.to_string()))?;
    if dest.as_os_str() == "-" {
        print!("{}", s);
        if !s.ends_with('\n') {
            println!();
        }
        return Ok(());
    }
    if dest.exists() && !force {
        eprintln!("Refusing to overwrite {} — pass --force.", dest.display());
        std::process::exit(1);
    }
    if let Some(parent) = dest.parent()
        && !parent.as_os_str().is_empty()
    {
        std::fs::create_dir_all(parent).map_err(KodError::Io)?;
    }
    std::fs::write(&dest, s.as_bytes()).map_err(KodError::Io)?;
    println!(
        "Wrote effective config ({} bytes) to {}",
        s.len(),
        dest.display()
    );
    Ok(())
}


/// Like `run_prompt` but streams text chunks live to stdout as they
/// arrive. Turns are not separated by markers; the reply comes out as
/// the model produces it, which is what makes this useful for
/// interactive scripting (`kod run ... | tee /tmp/reply`).
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
    engine.set_hooks(config.hooks.clone());
    engine.set_network_access(config.llm.network_access);
    engine.set_confirm_writes(config.tools.confirm_writes);
    engine.set_auto_check(config.tools.auto_check);
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

    let _ = engine.process_streaming(&input, &tx).await;
    drop(tx);
    let _ = pump.await;
    println!();
    engine.shutdown().await?;
    Ok(())
}


