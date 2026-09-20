//! Command definitions and handlers for the KOD CLI.

use clap::Parser;
use clap::Subcommand;
use kod_config::KodConfig;
use kod_core::KodEngine;
use kod_core::RouterConfig;
use kod_core::{SwarmEvent, SwarmRunner};
use kod_error::{KodError, Result};
use std::io::{self, BufRead, Write};
use std::sync::Arc;

/// Parse a `--preset` argument into a `kod_config::Preset`. Returns
/// `Ok(None)` for an absent flag and a descriptive `Err` for an
/// unknown value — a typo must not silently fall back to the default.
fn parse_preset(s: Option<&str>) -> Result<Option<kod_config::Preset>> {
    match s {
        None => Ok(None),
        Some("read-only") | Some("readonly") => Ok(Some(kod_config::Preset::ReadOnly)),
        Some("standard") | Some("default") => Ok(Some(kod_config::Preset::Standard)),
        Some("yolo") | Some("unrestricted") => Ok(Some(kod_config::Preset::Yolo)),
        Some(other) => Err(kod_error::KodError::Config(format!(
            "unknown --preset {other:?}. Known presets: read-only, standard, yolo"
        ))),
    }
}

/// Async version of `install_policy`: builds the engine's PolicyEngine
/// from config + cwd + CLI preset, and installs it.
async fn install_policy_async(
    engine: &kod_core::KodEngine,
    config: &kod_config::KodConfig,
    cli_preset: Option<&str>,
) -> Result<()> {
    let preset = parse_preset(cli_preset)?;
    let cwd = std::env::current_dir()
        .map_err(|e| kod_error::KodError::Config(format!("could not determine cwd: {e}")))?;
    let policy = kod_config::PolicyEngine::load(config, Some(&cwd), preset)?;
    engine.set_policy(std::sync::Arc::new(policy)).await;
    Ok(())
}

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
    /// Read the config file, resolve every v1 field into its v2
    /// equivalent, back up the original to
    /// `config.toml.bak-<unix-ts>`, and write a `config_version = 2`
    /// file. Idempotent: a v2 file is reported as already current
    /// and is not rewritten.
    ///
    /// The v1→v2 transformation is entirely syntactic — a v1 file's
    /// `[llm]` block already has a `provider` / `model` /
    /// `base_url`; the effective v2 config synthesises an endpoint
    /// named `"default"` from those. `kod config export` already
    /// prints the effective v2 shape, so this command is simply
    /// that plus a backup and a version stamp.
    ///
    /// `--dry-run` prints the v2 TOML to stdout without writing
    /// anything, so a user can review the change before applying it.
    Migrate {
        /// Print the migrated config to stdout without writing.
        #[arg(long, default_value_t = false)]
        dry_run: bool,
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
                sandbox,
                system_prompt,
                preset,
                remote,
                socket,
            }) => {
                let rt = tokio::runtime::Runtime::new()
                    .map_err(|e| KodError::Internal(format!("Failed to create runtime: {}", e)))?;
                rt.block_on(async {
                    if *remote {
                        run_chat_remote(socket.clone()).await
                    } else {
                        run_chat(
                            model.clone(),
                            *sandbox,
                            system_prompt.clone(),
                            preset.clone(),
                        )
                        .await
                    }
                })
            }
            Some(Command::Agent {
                name,
                goal,
                model,
                preset,
                remote,
                socket,
            }) => {
                let rt = tokio::runtime::Runtime::new()
                    .map_err(|e| KodError::Internal(format!("Failed to create runtime: {}", e)))?;
                rt.block_on(async {
                    if *remote {
                        run_agent_remote(name.clone(), goal.clone(), socket.clone()).await
                    } else {
                        run_agent(name.clone(), goal.clone(), model.clone(), preset.clone()).await
                    }
                })
            }
            Some(Command::Swarm {
                goal,
                agents,
                model,
                merge,
                remote,
                socket,
            }) => {
                let rt = tokio::runtime::Runtime::new()
                    .map_err(|e| KodError::Internal(format!("Failed to create runtime: {}", e)))?;
                rt.block_on(async {
                    if *remote {
                        run_swarm_remote(goal.clone(), *agents, *merge, socket.clone()).await
                    } else {
                        run_swarm(goal.clone(), *agents, model.clone(), *merge).await
                    }
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
                        Some(SkillsAction::Search { query }) => run_skills_search(query).await,
                        Some(SkillsAction::Copy { name, new_name }) => {
                            run_skills_copy(name, new_name).await
                        }
                        Some(SkillsAction::Rename { name, new_name }) => {
                            run_skills_rename(name, new_name).await
                        }
                        Some(SkillsAction::Source { name }) => run_skills_source(name).await,
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
                        Some(ConfigAction::InitFrom { name }) => run_config_init_from(name).await,
                        Some(ConfigAction::Migrate { dry_run }) => {
                            run_config_migrate(*dry_run).await
                        }
                        Some(ConfigAction::ShowRaw) => run_config_show_raw().await,
                        Some(ConfigAction::Export { path: dest, force }) => {
                            run_config_export(dest.clone(), *force).await
                        }
                        Some(ConfigAction::ShowMerged) => run_config_show_merged().await,
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
                remote,
                socket,
            }) => {
                let rt = tokio::runtime::Runtime::new()
                    .map_err(|e| KodError::Internal(format!("Failed to create runtime: {}", e)))?;
                rt.block_on(async {
                    if *remote {
                        run_prompt_remote(prompt.clone(), socket.clone()).await
                    } else {
                        run_prompt(prompt.clone(), model.clone(), *no_log, *sandbox).await
                    }
                })
            }
            Some(Command::Tui {
                model,
                no_resume,
                sandbox,
                preset,
            }) => {
                let rt = tokio::runtime::Runtime::new()
                    .map_err(|e| KodError::Internal(format!("Failed to create runtime: {}", e)))?;
                rt.block_on(async {
                    run_tui(model.clone(), *no_resume, *sandbox, preset.clone()).await
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
            Some(Command::Serve { stop, socket }) => {
                let rt = tokio::runtime::Runtime::new()
                    .map_err(|e| KodError::Internal(format!("Failed to create runtime: {}", e)))?;
                rt.block_on(async { run_serve(*stop, socket.clone()).await })
            }
            Some(Command::Acp { preset }) => {
                let rt = tokio::runtime::Runtime::new()
                    .map_err(|e| KodError::Internal(format!("Failed to create runtime: {}", e)))?;
                rt.block_on(async { run_acp(preset.clone()).await })
            }
            Some(Command::SandboxExec { profile, cmd }) => {
                // No tokio runtime: exec replaces the process, so
                // any runtime state would be lost anyway. Running
                // this before the runtime is created makes the
                // launcher's startup path as small as possible.
                run_sandbox_exec(profile.clone(), cmd.clone())
            }
            Some(Command::Trace { action }) => {
                let rt = tokio::runtime::Runtime::new()
                    .map_err(|e| KodError::Internal(format!("Failed to create runtime: {}", e)))?;
                rt.block_on(async {
                    match action {
                        TraceAction::List { limit, path } => {
                            run_trace_list(*limit, path.clone()).await
                        }
                        TraceAction::Show { id, path } => {
                            run_trace_show(*id, path.clone()).await
                        }
                        TraceAction::Json { path } => run_trace_json(path.clone()).await,
                    }
                })
            }
            Some(Command::Fixture { action }) => {
                let rt = tokio::runtime::Runtime::new()
                    .map_err(|e| KodError::Internal(format!("Failed to create runtime: {}", e)))?;
                rt.block_on(async {
                    match action {
                        FixtureAction::Save { name, turns, force } => {
                            run_fixture_save_v2(name, turns.clone(), *force).await
                        }
                        FixtureAction::Replay { name, strict, first_round_only } => {
                            let _ = run_fixture_replay(name, *strict, *first_round_only)
                                .await?;
                            Ok(())
                        }
                        FixtureAction::List => run_fixture_list().await,
                    }
                })
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
            Some(Command::Policy { action }) => {
                let rt = tokio::runtime::Runtime::new()
                    .map_err(|e| KodError::Internal(format!("Failed to create runtime: {}", e)))?;
                rt.block_on(async { run_policy(action.clone()).await })
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

        /// Override the system prompt for this session.
        #[arg(long)]
        system_prompt: Option<String>,

        /// Run shell commands through the platform sandbox (`bwrap` on
        /// Linux, `sandbox-exec` on macOS). Fails loudly if the
        /// primitive is not available. Recommended when letting the
        /// agent run unsupervised.
        #[arg(long, default_value_t = false)]
        sandbox: bool,

        /// Policy preset (read-only | standard | yolo). Overrides the
        /// global config and any `.kod/policy.toml` in the project.
        /// Defaults to Standard (which matches the pre-policy default
        /// when `tools.confirm_writes = true`).
        #[arg(long)]
        preset: Option<String>,

        /// Attach to a running `kod serve` daemon instead of building
        /// an in-process engine. The daemon keeps one session alive
        /// across many `kod chat` invocations, so the transcript,
        /// model, and memory survive a terminal that is closed and
        /// reopened. Requires `kod serve` to be running.
        #[arg(long, default_value_t = false)]
        remote: bool,

        /// Socket path to attach to when `--remote` is set. Defaults
        /// to `kod serve`'s default path.
        #[arg(long)]
        socket: Option<std::path::PathBuf>,
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

        /// Send the goal to a running `kod serve` daemon instead of
        /// building an in-process engine. `--model` is ignored under
        /// `--remote`: the daemon's `[llm.routing.swarm]` decides
        /// per-agent routing.
        #[arg(long, default_value_t = false)]
        remote: bool,

        /// Socket path to attach to when `--remote` is set.
        #[arg(long)]
        socket: Option<std::path::PathBuf>,
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

        /// Policy preset (read-only | standard | yolo).
        #[arg(long)]
        preset: Option<String>,

        /// Send the goal to a running `kod serve` daemon instead of
        /// building an in-process engine. Uses the daemon's `process`
        /// (non-streaming) method, so the answer arrives in one piece
        /// — a `kod agent` run does not print live tokens even in
        /// the embedded path.
        #[arg(long, default_value_t = false)]
        remote: bool,

        /// Socket path to attach to when `--remote` is set.
        #[arg(long)]
        socket: Option<std::path::PathBuf>,
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
        /// Send the prompt to a running `kod serve` daemon instead of
        /// building an in-process engine. The daemon must already be
        /// listening on `--socket` (default: the standard path).
        #[arg(long, default_value_t = false)]
        remote: bool,
        /// Socket path to attach to when `--remote` is set.
        #[arg(long)]
        socket: Option<std::path::PathBuf>,
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
        /// Policy preset (read-only | standard | yolo).
        #[arg(long)]
        preset: Option<String>,
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

    /// Inspect the effective tool policy: what the engine would
    /// allow, deny, or ask for. Read-only — never writes to a
    /// `.kod/policy.toml`.
    Policy {
        #[command(subcommand)]
        action: PolicyAction,
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
    /// Start (or stop) the long-lived daemon that `kod prompt
    /// --remote` and `kod chat --remote` attach to. Unix socket
    /// only, never TCP.
    ///
    /// With `--stop`: connect to the running daemon and ask it to
    /// exit, then wait for the socket file to disappear.
    Serve {
        /// Ask a running daemon to stop instead of starting one.
        #[arg(long, default_value_t = false)]
        stop: bool,
        /// Override the socket path. Defaults to
        /// `$XDG_RUNTIME_DIR/kod.sock` (Linux) or
        /// `~/.kod/run/kod.sock` (macOS).
        #[arg(long)]
        socket: Option<std::path::PathBuf>,
    },

    /// Run the Agent Client Protocol (ACP) bridge on stdin/stdout
    /// (design §11.2). Spawned by an editor that speaks ACP — Zed,
    /// for example. The engine is built from the current config
    /// exactly as `kod chat` does; the transport is JSON-RPC with
    /// `Content-Length` framing on stdio, per the ACP v1 spec.
    ///
    /// No `--remote`: ACP is a stdio protocol (the editor spawns this
    /// process and talks to it over pipes), not a socket client.
    Acp {
        /// Policy preset (read-only | standard | yolo). Overrides the
        /// global config and any `.kod/policy.toml`.
        #[arg(long)]
        preset: Option<String>,
    },

    /// Hidden subcommand: apply a Landlock sandbox to the current
    /// process and exec the given command. Reachable only as
    /// `kod __sandbox-exec` from the parent process that built the
    /// profile. Not documented in `--help` on purpose.
    #[command(hide = true, name = "__sandbox-exec")]
    SandboxExec {
        /// Path to the profile JSON.
        profile: std::path::PathBuf,
        /// The command to exec, after `--`.
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        cmd: Vec<String>,
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

    /// Inspect the structured turn traces written by `KodEngine`
    /// (Tier 1.4). Reads `turns.jsonl` from the session directory;
    /// `--path` overrides.
    Trace {
        #[command(subcommand)]
        action: TraceAction,
    },

    /// Save and replay deterministic fixtures (Tier 1.5). Fixtures
    /// capture one complete streaming session as an ordered list of
    /// rounds; replay drives the engine against the fixture to
    /// detect a request-shape drift.
    Fixture {
        #[command(subcommand)]
        action: FixtureAction,
    },
}

/// `kod fixture` subcommands.
#[derive(Subcommand, Debug, Clone)]
pub enum FixtureAction {
    /// Save the current session's turn traces as a named fixture.
    /// Reads `~/.kod/sessions/turns.jsonl` by default; `--turns`
    /// overrides the path.
    Save {
        /// Fixture name (also the file stem under
        /// `~/.kod/fixtures/`).
        name: String,
        /// Path to the turn-traces JSONL. Defaults to the standard
        /// session directory.
        #[arg(long)]
        turns: Option<std::path::PathBuf>,
        /// Overwrite an existing fixture with the same name.
        #[arg(long, default_value_t = false)]
        force: bool,
    },
    /// Replay a saved fixture against the current engine and report
    /// any request-shape divergence.
    Replay {
        /// Fixture name.
        name: String,
        /// Exit non-zero on any divergence, including a benign one
        /// (round count mismatch).
        #[arg(long, default_value_t = false)]
        strict: bool,
        /// Replay only the first round. Safe mode: a first-round
        /// divergence is where almost all prompt drift surfaces, and
        /// this avoids re-executing any tool side-effects the later
        /// rounds would otherwise trigger.
        #[arg(long, default_value_t = false)]
        first_round_only: bool,
    },
    /// List available fixtures under `~/.kod/fixtures/`.
    List,
}

/// `kod trace` subcommands.
#[derive(Subcommand, Debug, Clone)]
pub enum TraceAction {
    /// Print a table of the most recent turns.
    List {
        /// Cap on rows printed. Default 20, max 200.
        #[arg(long, default_value_t = 20)]
        limit: usize,
        /// Path to the `turns.jsonl` file. Defaults to
        /// `~/.kod/sessions/turns.jsonl`.
        #[arg(long)]
        path: Option<std::path::PathBuf>,
    },
    /// Print one turn's full round-by-round tree.
    Show {
        /// Turn id, as printed by `kod trace list`.
        id: u64,
        /// Path to the `turns.jsonl` file.
        #[arg(long)]
        path: Option<std::path::PathBuf>,
    },
    /// Print a JSON summary of every turn. Useful in scripts.
    Json {
        #[arg(long)]
        path: Option<std::path::PathBuf>,
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

/// `kod policy` subcommands.
#[derive(Subcommand, Debug, Clone)]
pub enum PolicyAction {
    /// Print the effective policy: the preset, every per-tool
    /// override, and the provenance of each rule (which layer set
    /// it — the preset, the global config, `.kod/policy.toml`, or
    /// the CLI override).
    Show,
    /// Print the session's accumulated "never" rules — the ones
    /// added by choosing `a` on the approval dialog — with their
    /// 1-based indices. Call with an index to drop one.
    ///
    /// This layer is otherwise only removable by restarting `kod`:
    /// the CLI's other policy verbs (`show`, `explain`) are read-only
    /// and `/clear` resets the chat, not the deny rules.
    Forget {
        /// 1-based index of the rule to drop, as printed by
        /// `kod policy forget` (no argument lists the rules).
        /// When `None`, the list is printed and nothing is removed.
        n: Option<usize>,
    },
    /// Answer "what would the engine decide for this call?" without
    /// running anything. `tool` is a tool name (`write_file`,
    /// `execute_command`, `mcp:filesystem.read_file`, …). Arguments
    /// are `key=value` pairs; a value is parsed as JSON when it
    /// parses, and treated as a string otherwise.
    ///
    /// Example:
    ///   kod policy explain write_file path=src/main.rs
    ///   kod policy explain execute_command command="cargo test"
    ///   kod policy explain web_fetch url=https://docs.rs/
    ///
    /// The decision is the same one the engine makes: session deny
    /// rules are not consulted (there are none in a fresh CLI
    /// process), the project policy layer is loaded from the
    /// current directory, and any `--preset` override is not
    /// applied (a CLI inspection is meant to show the config's
    /// answer, not a hypothetical).
    Explain {
        /// Tool name.
        tool: String,
        /// `key=value` argument pairs. A value that parses as JSON
        /// is used as-is; anything else is used as a string.
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<String>,
    },
}

/// `kod memory` subcommands.
#[derive(Subcommand, Debug, Clone)]
pub enum MemoryAction {
    /// Add a long-term memory entry. The text is stored verbatim; use
    /// `--tags` for a comma-separated list for later filtering.
    Add {
        /// The content to remember, one sentence or a short paragraph.
        content: String,
        /// Optional comma-separated tags, e.g. `preference,rust`.
        #[arg(long)]
        tags: Option<String>,
    },
    /// Forget entries by id prefix (as printed by `list`) or by tag.
    /// A missing entry is reported, not an error.
    Forget {
        /// Id prefix or tag name.
        key: String,
    },
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

/// `kod chat --remote` — attach an interactive REPL to a running
/// `kod serve` daemon.
///
/// # Why a separate function from `run_chat`
///
/// The embedded path builds an engine, loads skills, wires the
/// provider registry, and runs the agentic loop in-process. The
/// remote path does none of that: it opens a Unix socket, sends
/// NDJSON requests, and prints chunks. Folding them together would
/// mean either the daemon checks `--remote` in fifty places or the
/// embedded path pays for a transport it does not use.
///
/// # Session continuity
///
/// The daemon keeps one transcript keyed by `transcript_key`. This
/// function uses `"chat"` — a fixed key distinct from the empty
/// key `kod prompt --remote` uses, so an interactive REPL attached
/// to a daemon and a one-shot prompt attached to the same daemon do
/// not interleave their transcripts.
///
/// # Approval and question markers
///
/// A `kod serve` daemon currently cannot route approvals or
/// `ask_user` questions to a remote client — the daemon's engine
/// would have to pause on a request and wait for a client that may
/// not be there. The current behaviour when a policy requires an
/// approval is: the daemon times out at `AWAIT_APPROVAL_SECS` and
/// denies. A `chat --remote` session therefore works with
/// `[tools.*] mode = "allow"` policies (or `--preset yolo` on the
/// daemon side) and produces a clean denial in strict modes.
/// Routing approvals over the socket is a follow-up; the NDJSON
/// protocol already carries the markers unchanged, so the daemon
/// change is a `select!` on the approval channel and this client's
/// change is a prompt printout.
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

/// Run the chat command
pub async fn run_chat(
    model: Option<String>,
    sandbox: bool,
    system_prompt: Option<String>,
    cli_preset: Option<String>,
) -> Result<()> {
    // Load configuration
    let config = KodConfig::load_default()?;

    // Override model if specified
    let model_name = model.unwrap_or_else(|| config.llm.default_endpoint().model.clone());

    // Create the database path
    let home = dirs::home_dir()
        .ok_or_else(|| KodError::Config("Could not determine home directory".to_string()))?;
    let db_path = home.join(".kod").join("data").join("kod.redb");

    // Create engine. Derive the history budget from the model's window
    // (≈3 chars/token) so a small-model user is safe and a large-model
    // user gets useful recall; the engine clamps below its floor.
    // RouterConfig carries the token window itself so the memory manager
    // sizes its own budget from the same source.
    // Design D2.1: build the embedder the memory subsystem will use
    // for semantic retrieval. `None` (the config default) leaves the
    // keyword+recency fallback in place; no retrieval path is broken
    // by an absent embedder.
    let embedder = kod_memory::embedding::from_config(
        &config.memory,
        Some(&config.llm.default_endpoint().base_url),
    );
    let router_config = RouterConfig {
        skill_threshold: config.skills.match_threshold,
        context_window: config.llm.default_endpoint().context_window,
        short_term_capacity: config.memory.short_term_capacity,
        embedder,
        ..RouterConfig::default()
    };
    // Arc because the approval forwarder task (spawned below) needs to
    // call `respond_to_approval` while `process_streaming` runs on the
    // same engine.
    let engine = Arc::new(KodEngine::new(router_config, db_path)?);
    engine.set_history_budget(
        config
            .llm
            .default_endpoint()
            .context_window
            .saturating_mul(3),
    );

    // Set up OpenAI-compatible provider (Ollama /v1, LM Studio, MLX, ...)
    let (registry, default_model, routing) =
        kod_core::build_registry(&config.llm, Some(&model_name))?;
    engine.set_registry(registry, default_model, routing).await;
    engine.set_hooks(config.hooks.clone());
    engine.set_network_access(config.llm.network_access);
    engine.set_auto_check(config.tools.auto_check);
    engine.set_auto_lsp(config.tools.auto_lsp);
    engine.set_generation_defaults(
        Some(config.llm.default_endpoint().temperature.unwrap_or(0.7)),
        Some(config.llm.default_endpoint().max_tokens.unwrap_or(2048)),
    );
    install_policy_async(&engine, &config, cli_preset.as_deref()).await?;
    // Tier 1.3 — install read-protection from the effective policy.
    if let Some(policy) = engine.policy().await {
        engine.set_read_protection(policy.read_protection().clone());
    }
    // Tier 1.2 — install the session cost caps.
    engine.install_limits(&config.limits);
    kod_core::mcp_adapters::install_from_config(&engine, &config).await;

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
        && let Some(trace_path) =
            kod_core::TraceWriter::default_for_session(&log_path)
        && let Ok(w) = kod_core::TraceWriter::open(trace_path)
    {
        engine.set_turn_trace_writer(std::sync::Arc::new(w));
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

/// `kod swarm --remote` — run a swarm on a `kod serve` daemon and
/// print the per-agent progress live.
///
/// The daemon streams `swarm_event` NDJSON lines and terminates the
/// run with a `done` line carrying the merged answer. The client
/// prints each event the same way `run_swarm`'s local print loop
/// does, so the two paths speak the same vocabulary.
///
/// `--model` is deliberately not sent over the wire. A swarm's
/// per-agent models come from the daemon's own config
/// (`[llm.routing.swarm]`); letting a client override them
/// per-run would produce a swarm whose agents obeyed a different
/// routing table from the one the daemon was started with.
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
    let model_name = model.unwrap_or_else(|| config.llm.default_endpoint().model.clone());
    let _n = agents.unwrap_or(config.swarm.max_agents);

    let home = dirs::home_dir()
        .ok_or_else(|| KodError::Config("Could not determine home directory".to_string()))?;
    let db_path = home.join(".kod").join("data").join("kod.redb");
    let _ = std::fs::create_dir_all(db_path.parent().unwrap());

    // Design D2.1: build the embedder the memory subsystem will use
    // for semantic retrieval. `None` (the config default) leaves the
    // keyword+recency fallback in place; no retrieval path is broken
    // by an absent embedder.
    let embedder = kod_memory::embedding::from_config(
        &config.memory,
        Some(&config.llm.default_endpoint().base_url),
    );
    let router_config = RouterConfig {
        skill_threshold: config.skills.match_threshold,
        context_window: config.llm.default_endpoint().context_window,
        short_term_capacity: config.memory.short_term_capacity,
        embedder,
        ..RouterConfig::default()
    };
    let engine = KodEngine::new(router_config, db_path)?;
    engine.set_history_budget(
        config
            .llm
            .default_endpoint()
            .context_window
            .saturating_mul(3),
    );

    let (registry, default_model, routing) =
        kod_core::build_registry(&config.llm, Some(&model_name))?;
    engine.set_registry(registry, default_model, routing).await;
    kod_core::mcp_adapters::install_from_config(&engine, &config).await;

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

    let engine = Arc::new(engine);
    // Design §D4.3: the runner reads per-run budget and retry knobs
    // from `[swarm]`. `from_config` centralises the mapping so this
    // site and the TUI's `/swarm` cannot drift.
    let runner = SwarmRunner::from_config(engine.clone(), &config.swarm).await?;
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
        println!("\n(merged by concatenation — LLM synthesis was disabled or failed)");
    }

    engine.shutdown().await?;
    Ok(())
}

/// `kod agent --remote` — send a goal to a running `kod serve`
/// daemon and print the reply in one piece.
///
/// Uses the daemon's non-streaming `process` method. Streaming a
/// single agent's reply to a client that runs unattended and prints
/// the final answer is not useful — the interesting artifact is the
/// complete reply, not the token cadence. `kod chat --remote` is the
/// streaming surface for interactive use.
///
/// The transcript key is `"agent:<name>"`, distinct from the chat
/// and one-shot keys, so a named agent run and an interactive chat
/// attached to the same daemon keep separate transcripts.
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

/// Run the agent command
pub async fn run_agent(
    name: String,
    goal: String,
    model: Option<String>,
    cli_preset: Option<String>,
) -> Result<()> {
    let config = KodConfig::load_default()?;
    let model_name = model.unwrap_or_else(|| config.llm.default_endpoint().model.clone());

    let home = dirs::home_dir()
        .ok_or_else(|| KodError::Config("Could not determine home directory".to_string()))?;
    let db_path = home.join(".kod").join("data").join("kod.redb");

    // Design D2.1: build the embedder the memory subsystem will use
    // for semantic retrieval. `None` (the config default) leaves the
    // keyword+recency fallback in place; no retrieval path is broken
    // by an absent embedder.
    let embedder = kod_memory::embedding::from_config(
        &config.memory,
        Some(&config.llm.default_endpoint().base_url),
    );
    let router_config = RouterConfig {
        skill_threshold: config.skills.match_threshold,
        context_window: config.llm.default_endpoint().context_window,
        short_term_capacity: config.memory.short_term_capacity,
        embedder,
        ..RouterConfig::default()
    };
    let engine = KodEngine::new(router_config, db_path)?;
    engine.set_history_budget(
        config
            .llm
            .default_endpoint()
            .context_window
            .saturating_mul(3),
    );

    let (registry, default_model, routing) =
        kod_core::build_registry(&config.llm, Some(&model_name))?;
    engine.set_registry(registry, default_model, routing).await;
    // `kod agent` has no interactive consumer. When confirm_writes is
    // on, the engine refuses every write with a message the model and
    // the user can act on. Approving silently would defeat the flag.
    engine.set_auto_check(config.tools.auto_check);
    engine.set_auto_lsp(config.tools.auto_lsp);
    install_policy_async(&engine, &config, cli_preset.as_deref()).await?;
    // Tier 1.3 — install read-protection from the effective policy.
    if let Some(policy) = engine.policy().await {
        engine.set_read_protection(policy.read_protection().clone());
    }
    kod_core::mcp_adapters::install_from_config(&engine, &config).await;

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

/// Run tests
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

/// Print the repository map to stdout: one line per source file, followed
/// by its top-level symbols. Summary counts go to stderr so stdout can be
/// piped into a file cleanly.
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

/// Launch the interactive terminal UI
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

/// Print one `swarm_event` line the way `run_swarm`'s local print
/// loop does. The two surfaces must agree.
fn print_swarm_event(v: &serde_json::Value) {
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

/// Run the Agent Client Protocol (ACP) bridge on stdin/stdout.
///
/// Builds an engine from the current config exactly as `kod chat`
/// does, then hands it to `kod_core::acp::serve`, which speaks the ACP
/// v1 protocol on stdio. The process is meant to be spawned by an
/// editor (Zed, for instance), not run by a human; stderr is where any
/// diagnostic goes.
///
/// The engine holds the same standard subsystems the CLI sessions
/// hold — policy, memory, checkpoints, session log if enabled — so a
/// session attached by an editor is not a reduced capability.
pub async fn run_acp(cli_preset: Option<String>) -> Result<()> {
    let config = KodConfig::load_default()?;

    // Same isolation hook `TuiLoop::init_engine` uses. Without it
    // the ACP subprocess opens the shared `~/.kod/data/kod.redb`,
    // which a concurrent test process may hold a lock on — the
    // process then dies before writing the initialize response.
    let db_path = match std::env::var("KOD_TEST_DB") {
        Ok(p) => std::path::PathBuf::from(p),
        Err(_) => {
            let home = dirs::home_dir()
                .ok_or_else(|| KodError::Config("Could not determine home directory".to_string()))?;
            home.join(".kod").join("data").join("kod.redb")
        }
    };
    let _ = std::fs::create_dir_all(db_path.parent().unwrap_or(std::path::Path::new(".")));

    let router_config = RouterConfig {
        skill_threshold: config.skills.match_threshold,
        context_window: config.llm.default_endpoint().context_window,
        short_term_capacity: config.memory.short_term_capacity,
        ..RouterConfig::default()
    };
    let engine = Arc::new(KodEngine::new(router_config, db_path)?);
    engine.set_history_budget(
        config
            .llm
            .default_endpoint()
            .context_window
            .saturating_mul(3),
    );

    let (registry, default_model, routing) = kod_core::build_registry(&config.llm, None)?;
    engine.set_registry(registry, default_model, routing).await;
    engine.set_hooks(config.hooks.clone());
    engine.set_network_access(config.llm.network_access);
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

/// `kod serve` — start the daemon, or stop a running one with
/// `--stop`.
///
/// Starting: builds a `KodEngine` from the current config exactly
/// as `kod chat` does, then hands it to `kod_core::serve::serve`.
/// The daemon blocks until it receives a `shutdown` request or a
/// SIGINT.
///
/// Stopping: connects to the socket, sends `shutdown`, then polls
/// for the socket file to disappear (a 5 s budget). A daemon that
/// does not exit cleanly in that window gets a warning, not a hard
/// failure — the file may have been unlinked by an earlier run and
/// the actual process is what matters.
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
    let db_path = config.memory_db_path()?;
    if let Some(parent) = db_path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    // Design D2.1: build the embedder the memory subsystem will use
    // for semantic retrieval. `None` (the config default) leaves the
    // keyword+recency fallback in place; no retrieval path is broken
    // by an absent embedder.
    let embedder = kod_memory::embedding::from_config(
        &config.memory,
        Some(&config.llm.default_endpoint().base_url),
    );
    let router_config = RouterConfig {
        context_window: config.llm.default_endpoint().context_window,
        short_term_capacity: config.memory.short_term_capacity,
        skill_threshold: config.skills.match_threshold,
        embedder,
        ..RouterConfig::default()
    };
    let engine = KodEngine::new(router_config, db_path)?;
    engine.set_history_budget(
        config
            .llm
            .default_endpoint()
            .context_window
            .saturating_mul(3),
    );

    let (registry, default_model, routing) = kod_core::build_registry(&config.llm, None)?;
    engine.set_registry(registry, default_model, routing).await;
    engine.set_hooks(config.hooks.clone());
    engine.set_network_access(config.llm.network_access);
    engine.set_auto_check(config.tools.auto_check);
    engine.set_auto_lsp(config.tools.auto_lsp);

    // Policy: a daemon has no CLI preset.
    let cwd = std::env::current_dir()
        .map_err(|e| KodError::Config(format!("could not determine cwd: {e}")))?;
    let policy = kod_config::PolicyEngine::load(&config, Some(&cwd), None)?;
    engine.set_policy(std::sync::Arc::new(policy)).await;
    kod_core::mcp_adapters::install_from_config(&engine, &config).await;

    engine.start().await?;

    let engine = std::sync::Arc::new(engine);
    println!(
        "Starting daemon at {} (Ctrl+C to stop).",
        socket_path.display()
    );
    let result = kod_core::serve::serve(engine.clone(), socket_path.clone()).await;

    // Graceful engine shutdown after the accept loop exits.
    let _ = engine.shutdown().await;
    result
}

/// Hidden subcommand handler: apply a Landlock sandbox and exec.
///
/// The launcher is spawned by `SandboxResolver::invocation` (see
/// `kod_tools::context`), never called directly by a user. It:
///
/// 1. Reads the profile JSON.
/// 2. Unlinks the profile file (so it does not linger in /tmp).
/// 3. Applies the Landlock ruleset to itself.
/// 4. `execvp`s the inner command. Because `exec` replaces the
///    process image, the sandbox applies to the new image too — the
///    restriction is inherited by every descendant.
///
/// On a failure at steps 1–3, the launcher exits non-zero and the
/// parent's `ExecuteCommandTool` reports the failure. There is no
/// fallback to running the command unsandboxed: a caller that
/// reached `SandboxMode::Require` and got a launcher failure did
/// so on purpose.
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
            KodError::Config("could not determine a skills directory to write to".to_string())
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
            eprintln!(
                "No skill named {:?} in any configured skills directory.",
                name
            );
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
            eprintln!(
                "No skill named {:?} in any configured skills directory.",
                name
            );
            std::process::exit(1);
        }
    };

    let editor = std::env::var("EDITOR")
        .or_else(|_| std::env::var("VISUAL"))
        .unwrap_or_else(|_| "vi".to_string());
    let status = std::process::Command::new("sh")
        .arg("-c")
        .arg(format!(
            "{} {}",
            editor,
            shell_quote(&path.to_string_lossy())
        ))
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

/// `kod config migrate [--dry-run]` — bring a v1 config up to v2.
///
/// Read the file at the standard location, resolve it through the
/// same effective-endpoint synthesis a v1 config uses at load time,
/// stamp `config_version = 2`, and write it back after a backup.
/// The original is never destroyed: the backup file carries the
/// original bytes, and a failed write leaves the original in place.
///
/// A v2 file is reported as current and not rewritten — running the
/// command twice does not create two backups, and does not reorder
/// a hand-formatted file.
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

/// Print a skill's full markdown source (header + body). Unlike
/// `kod skills` (which lists names), this reads the file directly so
/// the output round-trips — piping it back into a file reproduces the
/// original.
pub async fn run_skills_show(name: &str) -> Result<()> {
    let path = match find_skill_path(name).await? {
        Some(p) => p,
        None => {
            eprintln!(
                "No skill named {:?} in any configured skills directory.",
                name
            );
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

/// `kod policy show` / `kod policy explain`.
///
/// Both load the effective `PolicyEngine` exactly as the CLI does
/// at session start (`PolicyEngine::load(config, cwd, None)`) so
/// what they report is what the engine would decide. No engine is
/// spun up — this is a pure read of the policy layers.
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

/// Parse `key=value` argument tokens into a JSON object. A value
/// that parses as JSON (a number, `true`/`false`, a quoted string,
/// an array) is used as-is; anything else is treated as a plain
/// string.
///
/// The grammar is deliberately minimal — a single level of
/// `key=value`, no nesting, no equals-signs inside unquoted values.
/// Complex arguments go through a JSON blob instead, with the shell
/// handling the quoting.
///
/// Every argument must be `key=value`; a bare token is a usage
/// error, not a silently ignored one.
fn parse_kv_args(args: &[String]) -> Result<serde_json::Value> {
    let mut obj = serde_json::Map::new();
    for token in args {
        let (k, v) = match token.split_once('=') {
            Some(pair) => pair,
            None => {
                return Err(KodError::InvalidParameters {
                    reason: format!(
                        "argument {token:?} is not key=value. Use `key=value` for every argument, or pass a JSON object as a single token."
                    ),
                });
            }
        };
        if k.is_empty() {
            return Err(KodError::InvalidParameters {
                reason: format!("argument {token:?} has an empty key"),
            });
        }
        let value: serde_json::Value =
            serde_json::from_str(v).unwrap_or_else(|_| serde_json::Value::String(v.to_string()));
        obj.insert(k.to_string(), value);
    }
    Ok(serde_json::Value::Object(obj))
}

/// Report whether the sandbox primitive `kod chat --sandbox` uses is
/// available on this platform. Read-only: never installs anything.
pub async fn run_sandbox_check() -> Result<()> {
    use kod_tools::context::{SandboxMode, sandbox_invocation};
    let cwd = std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."));

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

/// `kod prompt --remote` — send a prompt to a running `kod serve`
/// daemon, print the streamed reply on stdout, exit non-zero on
/// error.
///
/// The protocol is the one `kod_core::serve` speaks: NDJSON, one
/// request per line, `chunk` lines stream the text, a `done` line
/// ends the response, an `error` line aborts. `--remote` does not
/// fall back to the in-process path — a user who asked for the
/// daemon wants the daemon, and a silent fallback would mask a
/// misconfigured socket.
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

/// True when `s` starts with one of the engine's `\0kod-*` markers.
/// Extracted so the remote path and the embedded path cannot disagree
/// on what counts as a control marker.
fn is_control_marker(s: &str) -> bool {
    s.starts_with('\0')
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
    let model_name = model.unwrap_or_else(|| config.llm.default_endpoint().model.clone());

    let home = dirs::home_dir()
        .ok_or_else(|| KodError::Config("Could not determine home directory".to_string()))?;
    let db_path = home.join(".kod").join("data").join("kod.redb");

    // Design D2.1: build the embedder the memory subsystem will use
    // for semantic retrieval. `None` (the config default) leaves the
    // keyword+recency fallback in place; no retrieval path is broken
    // by an absent embedder.
    let embedder = kod_memory::embedding::from_config(
        &config.memory,
        Some(&config.llm.default_endpoint().base_url),
    );
    let router_config = RouterConfig {
        skill_threshold: config.skills.match_threshold,
        context_window: config.llm.default_endpoint().context_window,
        short_term_capacity: config.memory.short_term_capacity,
        embedder,
        ..RouterConfig::default()
    };
    let engine = KodEngine::new(router_config, db_path)?;
    engine.set_history_budget(
        config
            .llm
            .default_endpoint()
            .context_window
            .saturating_mul(3),
    );

    let (registry, default_model, routing) =
        kod_core::build_registry(&config.llm, Some(&model_name))?;
    engine.set_registry(registry, default_model, routing).await;
    engine.set_hooks(config.hooks.clone());
    engine.set_network_access(config.llm.network_access);
    engine.set_auto_check(config.tools.auto_check);
    engine.set_auto_lsp(config.tools.auto_lsp);
    if sandbox {
        engine.set_sandbox_mode(kod_tools::context::SandboxMode::Require);
    }
    kod_core::mcp_adapters::install_from_config(&engine, &config).await;

    engine.start().await?;

    // Optional session recorder.
    if !no_log
        && let Some(path) = kod_core::session_log::default_session_path()
        && let Ok(recorder) = kod_core::session_log::SessionRecorder::open(path)
    {
        engine.set_session_recorder(Arc::new(recorder));
    }

    // Install the Jev client when enabled.
    if let Err(e) = kod_core::install_jev_from_config(&engine, &config.jev) {
        eprintln!("Jev configuration error (continuing without): {e}");
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
pub async fn run_skills_export(name: &str, dest: std::path::PathBuf, force: bool) -> Result<()> {
    let src = match find_skill_path(name).await? {
        Some(p) => p,
        None => {
            eprintln!(
                "No skill named {:?} in any configured skills directory.",
                name
            );
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
        dest.join(
            src.file_name()
                .unwrap_or_else(|| std::ffi::OsStr::new("skill.md")),
        )
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

    hits.sort_by(|a, b| {
        b.0.cmp(&a.0)
            .then_with(|| a.1.metadata.name.cmp(&b.1.metadata.name))
    });
    println!("{} skill(s) match {:?}:", hits.len(), query);
    for (_, skill) in &hits {
        println!(
            "  - {}: {}",
            skill.metadata.name, skill.metadata.description
        );
    }
    Ok(())
}

/// `kod tools [list|show <name>]`. Read-only: registers a fresh
/// registry (the same list the engine installs) and prints it.
pub async fn run_tools(action: Option<ToolsAction>) -> Result<()> {
    use kod_tools::ToolRegistry;

    // `kod tools` is a read-only listing of what the engine registers
    // when it starts. Every tool KodEngine::start constructs without
    // needing engine state — file, git, shell, search, todo, the
    // user-facing ask — is registered here so its definition can be
    // read.
    //
    // The tools that need engine state (an LSP client slot, a memory
    // router, a swarm hub) cannot be constructed in a bare CLI
    // process. They are listed as static entries in `CORE_ONLY` below
    // so the listing names them truthfully without inventing engine
    // state. If a name in that list drifts from what the engine
    // registers, the engine's own tests catch it — not this command.
    let registry = ToolRegistry::new();
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
        .register(Box::new(kod_tools::GitCommitTool::new()))
        .await;
    registry
        .register(Box::new(kod_tools::GitBranchTool::new()))
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
    registry
        .register(Box::new(kod_tools::CheckTool::new()))
        .await;

    // Engine-scoped tools, listed but not constructed. Each needs a
    // handle the CLI does not have without an engine: an LSP client
    // slot, a memory router, or the swarm communication hub.
    const CORE_ONLY: &[(&str, &str)] = &[
        (
            "lsp_diagnostics",
            "LSP diagnostics for one file (engine-scoped, D5.3)",
        ),
        (
            "lsp_definition",
            "LSP go-to-definition (engine-scoped, D5.3)",
        ),
        (
            "lsp_references",
            "LSP find-references (engine-scoped, D5.3)",
        ),
        ("lsp_hover", "LSP hover summary (engine-scoped, D5.3)"),
        (
            "memory_save",
            "store a fact in long-term memory (engine-scoped, D2.4)",
        ),
        (
            "memory_search",
            "search long-term memory (engine-scoped, D2.4)",
        ),
        (
            "swarm_note",
            "broadcast a fact to the other swarm agents (engine-scoped, D4.3)",
        ),
        (
            "swarm_read",
            "read facts broadcast by the other swarm agents (engine-scoped, D4.3)",
        ),
        (
            "mcp:<server>.<tool>",
            "one tool per MCP server tool, added at engine start (D6.1)",
        ),
    ];

    match action {
        None | Some(ToolsAction::List) => {
            let defs = registry.get_definitions().await;
            println!(
                "Registered tools ({} registerable + {} engine-scoped):",
                defs.len(),
                CORE_ONLY.len()
            );
            for d in &defs {
                println!("  {:<16} {}", d.name, d.description);
            }
            for (name, desc) in CORE_ONLY {
                println!("  {:<16} {}", name, desc);
            }
            Ok(())
        }
        Some(ToolsAction::Show { name }) => {
            // A name that appears only in the engine-scoped list has
            // no full definition available here — no schema, no
            // permissions. Report it as engine-scoped rather than
            // claiming it does not exist.
            if let Some((n, desc)) = CORE_ONLY.iter().find(|(n, _)| *n == name) {
                println!(
                    "{}: {}\n\nengine-scoped: registered by KodEngine::start, not by `kod tools`.\nRun `kod tools list` to see the full inventory.",
                    n, desc
                );
                return Ok(());
            }
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
                            "git_operations": d.permissions.git_access,
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
    config.llm.default_endpoint_mut().model = profile.model.to_string();
    config.llm.default_endpoint_mut().base_url = profile.base_url.to_string();
    config.llm.default_endpoint_mut().context_window = profile.context_window;
    config.llm.default_endpoint_mut().max_tokens = Some(profile.max_tokens);
    config.save_to(&path)?;

    println!(
        "Wrote new config from profile {:?} to {}",
        name,
        path.display()
    );
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
    println!(
        "strict: {} checked, {} ok, {} failed.",
        total,
        ok,
        failed.len()
    );
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
            if !d.exists()
                && let Err(e) = std::fs::create_dir_all(d)
            {
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
        if !dir.exists()
            && let Err(e) = std::fs::create_dir_all(dir)
        {
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
            eprintln!(
                "No skill named {:?} in any configured skills directory.",
                name
            );
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
            let indent: String = line.chars().take_while(|c| c.is_whitespace()).collect();
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
    println!(
        "Renamed {} -> {} (removed {})",
        name,
        new_name,
        src.display()
    );
    Ok(())
}

/// Print the effective config as TOML. Unlike `kod config show-raw`
/// (verbatim file, comments preserved), this one goes through
/// `KodConfig::default()` → user file → serialize, so every field is
/// present at its effective value. Piping this into a file produces a
/// fully self-documenting config with no defaults hidden.
pub async fn run_config_show_merged() -> Result<()> {
    let config = KodConfig::load_default()?;
    let s = toml::to_string_pretty(&config).map_err(|e| KodError::Serialization(e.to_string()))?;
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
pub async fn run_config_export(dest: std::path::PathBuf, force: bool) -> Result<()> {
    let config = KodConfig::load_default()?;
    let s = toml::to_string_pretty(&config).map_err(|e| KodError::Serialization(e.to_string()))?;
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
    let model_name = model.unwrap_or_else(|| config.llm.default_endpoint().model.clone());
    let home = dirs::home_dir()
        .ok_or_else(|| KodError::Config("Could not determine home directory".to_string()))?;
    let db_path = home.join(".kod").join("data").join("kod.redb");

    // Design D2.1: build the embedder the memory subsystem will use
    // for semantic retrieval. `None` (the config default) leaves the
    // keyword+recency fallback in place; no retrieval path is broken
    // by an absent embedder.
    let embedder = kod_memory::embedding::from_config(
        &config.memory,
        Some(&config.llm.default_endpoint().base_url),
    );
    let router_config = RouterConfig {
        skill_threshold: config.skills.match_threshold,
        context_window: config.llm.default_endpoint().context_window,
        short_term_capacity: config.memory.short_term_capacity,
        embedder,
        ..RouterConfig::default()
    };
    let engine = KodEngine::new(router_config, db_path)?;
    engine.set_history_budget(
        config
            .llm
            .default_endpoint()
            .context_window
            .saturating_mul(3),
    );
    let (registry, default_model, routing) =
        kod_core::build_registry(&config.llm, Some(&model_name))?;
    engine.set_registry(registry, default_model, routing).await;
    engine.set_hooks(config.hooks.clone());
    engine.set_network_access(config.llm.network_access);
    engine.set_auto_check(config.tools.auto_check);
    engine.set_auto_lsp(config.tools.auto_lsp);
    kod_core::mcp_adapters::install_from_config(&engine, &config).await;
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

#[cfg(test)]
mod coverage_cli_helpers {
    //! The command handlers in this file are thin wrappers over
    //! helper functions that do the actual work. The helpers are
    //! pure and easy to test in isolation; a regression in any of
    //! them shows up as a subtly wrong message, a wrong shell
    //! command, or a wrong skill filename — never as a crash.
    use super::*;

    #[test]
    fn shell_quote_wraps_in_single_quotes() {
        assert_eq!(shell_quote("abc"), "'abc'");
        assert_eq!(shell_quote("a b"), "'a b'");
        assert_eq!(shell_quote(""), "''");
    }

    #[test]
    fn shell_quote_escapes_embedded_single_quotes() {
        // The POSIX idiom `'\''` closes the quote, emits a literal
        // quote, and reopens it. A regression that used double
        // quotes would break on `$` and backtick expansion.
        assert_eq!(shell_quote("a'b"), "'a'\\''b'");
        assert_eq!(shell_quote("''"), "''\\'''\\'''");
    }

    #[test]
    fn preview_takes_the_first_line_only() {
        assert_eq!(preview("line one\nline two", 80), "line one");
    }

    #[test]
    fn preview_returns_short_strings_unchanged() {
        assert_eq!(preview("short", 10), "short");
    }

    #[test]
    fn preview_appends_an_ellipsis_when_cut() {
        let long = "a".repeat(100);
        let out = preview(&long, 10);
        assert!(out.ends_with('…'), "got: {out}");
        // 10 chars + the ellipsis.
        assert_eq!(out.chars().count(), 11);
    }

    #[test]
    fn preview_handles_empty_input() {
        assert_eq!(preview("", 10), "");
    }

    #[test]
    fn to_title_case_capitalises_each_hyphen_segment() {
        assert_eq!(to_title_case("rust-refactoring"), "Rust Refactoring");
        assert_eq!(to_title_case("a-b-c"), "A B C");
        assert_eq!(to_title_case("single"), "Single");
    }

    #[test]
    fn to_title_case_of_empty_is_empty() {
        assert_eq!(to_title_case(""), "");
    }

    #[test]
    fn parse_kv_args_parses_numbers_and_booleans_as_json() {
        let args = vec!["a=1".to_string(), "b=true".to_string()];
        let v = parse_kv_args(&args).unwrap();
        assert_eq!(v["a"], 1);
        assert_eq!(v["b"], true);
    }

    #[test]
    fn parse_kv_args_falls_back_to_strings() {
        let args = vec!["a=hello".to_string(), "b=with spaces".to_string()];
        let v = parse_kv_args(&args).unwrap();
        assert_eq!(v["a"], "hello");
        assert_eq!(v["b"], "with spaces");
    }

    #[test]
    fn parse_kv_args_accepts_a_quoted_json_string() {
        let args = vec!["a=\"quoted\"".to_string()];
        let v = parse_kv_args(&args).unwrap();
        assert_eq!(v["a"], "quoted");
    }

    #[test]
    fn parse_kv_args_rejects_a_bare_token() {
        let args = vec!["noequals".to_string()];
        let err = parse_kv_args(&args).unwrap_err();
        assert!(err.to_string().contains("key=value"), "got: {err}");
    }

    #[test]
    fn parse_kv_args_rejects_an_empty_key() {
        let args = vec!["=value".to_string()];
        let err = parse_kv_args(&args).unwrap_err();
        assert!(err.to_string().contains("empty key"), "got: {err}");
    }

    #[test]
    fn parse_kv_args_of_empty_slice_is_an_empty_object() {
        let v = parse_kv_args(&[]).unwrap();
        assert!(v.as_object().unwrap().is_empty());
    }

    #[test]
    fn format_timestamp_ms_produces_a_readable_date() {
        // The exact string depends on the local timezone; the
        // contract is "a plausible date string", not a specific
        // value. Anchor on the shape.
        let s = format_timestamp_ms(1_700_000_000_000);
        assert!(s.len() >= 10, "too short to be a date: {s}");
        assert!(
            s.contains('-') && s.contains(':'),
            "not a plausible date: {s}",
        );
    }
}

#[cfg(test)]
mod coverage_cli_actions {
    //! `parse_preset` and `is_control_marker` are the two pure
    //! helpers the CLI's policy paths depend on. A regression in
    //! either is user-facing: a typo'd `--preset` silently falls
    //! back to the default (bad — the flag exists to override) or
    //! a `\0kod-*` marker leaks into the printed chat (a stray
    //! control sequence in the user's terminal).
    use super::*;

    #[test]
    fn parse_preset_returns_none_for_absent_flag() {
        assert!(parse_preset(None).unwrap().is_none());
    }

    #[test]
    fn parse_preset_accepts_the_canonical_spellings() {
        assert_eq!(
            parse_preset(Some("read-only")).unwrap(),
            Some(kod_config::Preset::ReadOnly),
        );
        assert_eq!(
            parse_preset(Some("standard")).unwrap(),
            Some(kod_config::Preset::Standard),
        );
        assert_eq!(
            parse_preset(Some("yolo")).unwrap(),
            Some(kod_config::Preset::Yolo),
        );
    }

    #[test]
    fn parse_preset_accepts_the_documented_aliases() {
        assert_eq!(
            parse_preset(Some("readonly")).unwrap(),
            Some(kod_config::Preset::ReadOnly),
        );
        assert_eq!(
            parse_preset(Some("default")).unwrap(),
            Some(kod_config::Preset::Standard),
        );
        assert_eq!(
            parse_preset(Some("unrestricted")).unwrap(),
            Some(kod_config::Preset::Yolo),
        );
    }

    #[test]
    fn parse_preset_rejects_unknown_names_with_a_helpful_message() {
        // A typo must fail loudly, not silently fall back. The
        // error must name the valid presets so the user can fix
        // the invocation without consulting the docs.
        let err = parse_preset(Some("paranoid")).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("paranoid"), "should name the bad value: {msg}");
        assert!(msg.contains("read-only"), "should list valid values: {msg}");
        assert!(msg.contains("standard"), "should list valid values: {msg}");
        assert!(msg.contains("yolo"), "should list valid values: {msg}");
    }

    #[test]
    fn parse_preset_is_case_sensitive() {
        // The canonical spelling is kebab-case. `Read-Only` is a
        // typo; accepting it would silently broaden the accepted
        // set and make a future rename a breaking change.
        assert!(parse_preset(Some("Read-Only")).is_err());
        assert!(parse_preset(Some("YOLO")).is_err());
    }

    #[test]
    fn is_control_marker_recognizes_the_kod_prefix() {
        // The engine's markers all start with a NUL byte. The
        // predicate's contract is "this chunk is not user text".
        assert!(is_control_marker("\0kod-tool:read_file\0"));
        assert!(is_control_marker("\0kod-args:path=a\0"));
        assert!(is_control_marker("\0kod-done:h\0s\07"));
        assert!(is_control_marker("\0kod-thinking\0"));
        assert!(is_control_marker("\0kod-approval:1:{}\0"));
        assert!(is_control_marker("\0kod-question:1:{}\0"));
    }

    #[test]
    fn is_control_marker_rejects_ordinary_text() {
        assert!(!is_control_marker("hello world"));
        assert!(!is_control_marker(""));
        assert!(!is_control_marker("kod-tool:read_file"));
        assert!(!is_control_marker("prefix \0kod"));
    }

    #[test]
    fn is_control_marker_accepts_any_nul_prefixed_chunk() {
        // The predicate is intentionally broad: any NUL-prefixed
        // chunk is a marker. A regression that narrowed it to the
        // specific prefixes would break the moment a new marker is
        // added — the CLI would print the marker's bytes to the
        // user's terminal.
        assert!(is_control_marker("\0anything"));
        assert!(is_control_marker("\0"));
    }
}

/// Parse-surface coverage for the clap derive tree. Each test asserts
/// that one documented invocation parses — this catches a silently
/// renamed subcommand, a missing `#[arg]` on a required field, or a
/// short/long alias that drifted. The negative cases are equally
/// important: an unknown subcommand, missing required positionals, and
/// bogus flags must all be rejected.
#[cfg(test)]
mod coverage_cli_parsing {
    use super::*;

    fn parse_ok(args: &[&str]) -> Cli {
        Cli::try_parse_from(args).unwrap_or_else(|e| {
            panic!("expected `{}` to parse, got error: {e}", args.join(" "))
        })
    }

    fn parse_err(args: &[&str]) {
        assert!(
            Cli::try_parse_from(args).is_err(),
            "expected `{}` to be rejected, but it parsed",
            args.join(" ")
        );
    }

    // ---- top-level shape -----------------------------------------------

    #[test]
    fn no_subcommand_is_accepted_and_leaves_command_none() {
        // `kod` with no args must not error: `Cli.command` is
        // `Option<Command>` and the binary chooses what to do.
        let cli = parse_ok(&["kod"]);
        assert!(cli.command.is_none());
        assert!(!cli.verbose);
    }

    #[test]
    fn verbose_flag_is_accepted_before_the_subcommand() {
        let cli = parse_ok(&["kod", "--verbose", "test"]);
        assert!(cli.verbose);
        assert!(matches!(cli.command, Some(Command::Test)));
        // Short form too.
        let cli = parse_ok(&["kod", "-v", "test"]);
        assert!(cli.verbose);
    }

    // ---- chat ----------------------------------------------------------

    #[test]
    fn chat_parses_with_no_flags() {
        assert!(matches!(
            parse_ok(&["kod", "chat"]).command,
            Some(Command::Chat { .. })
        ));
    }

    #[test]
    fn chat_parses_every_documented_flag() {
        let cli = parse_ok(&[
            "kod", "chat",
            "--model", "claude-sonnet-4-5",
            "--system-prompt", "be terse",
            "--sandbox",
            "--preset", "yolo",
            "--remote",
            "--socket", "/tmp/kod.sock",
        ]);
        match cli.command {
            Some(Command::Chat {
                model,
                system_prompt,
                sandbox,
                preset,
                remote,
                socket,
            }) => {
                assert_eq!(model.as_deref(), Some("claude-sonnet-4-5"));
                assert_eq!(system_prompt.as_deref(), Some("be terse"));
                assert!(sandbox);
                assert_eq!(preset.as_deref(), Some("yolo"));
                assert!(remote);
                assert_eq!(
                    socket.as_deref(),
                    Some(std::path::Path::new("/tmp/kod.sock"))
                );
            }
            other => panic!("expected Chat, got {other:?}"),
        }
    }

    #[test]
    fn chat_model_short_alias_works() {
        match parse_ok(&["kod", "chat", "-m", "gpt-5"]).command {
            Some(Command::Chat { model, .. }) => {
                assert_eq!(model.as_deref(), Some("gpt-5"));
            }
            _ => panic!("expected Chat"),
        }
    }

    // ---- skills + skills actions --------------------------------------

    #[test]
    fn skills_with_no_action_parses() {
        assert!(matches!(
            parse_ok(&["kod", "skills"]).command,
            Some(Command::Skills { action: None, json: false })
        ));
    }

    #[test]
    fn skills_json_flag_sits_on_the_parent() {
        // `--json` is a flag on `Skills`, not on `List`; `kod skills
        // --json` (no action) is the shape that exercises it
        // independently of subcommand ordering.
        assert!(matches!(
            parse_ok(&["kod", "skills", "--json"]).command,
            Some(Command::Skills { action: None, json: true })
        ));
    }

    #[test]
    fn skills_list_subcommand_parses() {
        match parse_ok(&["kod", "skills", "list"]).command {
            Some(Command::Skills { action: Some(SkillsAction::List), json }) => {
                assert!(!json);
            }
            _ => panic!("expected Skills::List"),
        }
    }

    #[test]
    fn skills_new_takes_a_positional_name() {
        match parse_ok(&["kod", "skills", "new", "my-skill"]).command {
            Some(Command::Skills { action: Some(SkillsAction::New { name }), .. }) => {
                assert_eq!(name, "my-skill");
            }
            _ => panic!("expected Skills::New"),
        }
    }

    #[test]
    fn skills_remove_takes_a_name_and_optional_yes_flag() {
        match parse_ok(&["kod", "skills", "remove", "old", "--yes"]).command {
            Some(Command::Skills {
                action: Some(SkillsAction::Remove { name, yes }),
                ..
            }) => {
                assert_eq!(name, "old");
                assert!(yes);
            }
            _ => panic!("expected Skills::Remove"),
        }
        // `--yes` is optional; default false.
        match parse_ok(&["kod", "skills", "remove", "old"]).command {
            Some(Command::Skills {
                action: Some(SkillsAction::Remove { yes, .. }),
                ..
            }) => assert!(!yes),
            _ => panic!("expected Skills::Remove"),
        }
    }

    #[test]
    fn skills_export_takes_a_name_and_a_destination_path() {
        match parse_ok(&["kod", "skills", "export", "my-skill", "/tmp/out.md"]).command {
            Some(Command::Skills {
                action: Some(SkillsAction::Export { name, dest, force }),
                ..
            }) => {
                assert_eq!(name, "my-skill");
                assert_eq!(dest, std::path::PathBuf::from("/tmp/out.md"));
                assert!(!force);
            }
            _ => panic!("expected Skills::Export"),
        }
    }

    #[test]
    fn skills_rename_and_copy_take_two_positionals() {
        match parse_ok(&["kod", "skills", "rename", "old", "new"]).command {
            Some(Command::Skills {
                action: Some(SkillsAction::Rename { name, new_name }),
                ..
            }) => {
                assert_eq!(name, "old");
                assert_eq!(new_name, "new");
            }
            _ => panic!("expected Skills::Rename"),
        }
        assert!(matches!(
            parse_ok(&["kod", "skills", "copy", "a", "b"]).command,
            Some(Command::Skills { action: Some(SkillsAction::Copy { .. }), .. })
        ));
    }

    #[test]
    fn skills_search_takes_a_query() {
        match parse_ok(&["kod", "skills", "search", "rust"]).command {
            Some(Command::Skills {
                action: Some(SkillsAction::Search { query }),
                ..
            }) => assert_eq!(query, "rust"),
            _ => panic!("expected Skills::Search"),
        }
    }

    #[test]
    fn skills_source_show_and_edit_take_a_name() {
        for verb in ["source", "show", "edit"] {
            assert!(
                Cli::try_parse_from(["kod", "skills", verb, "x"]).is_ok(),
                "`skills {verb} x` must parse"
            );
        }
    }

    // ---- config actions ------------------------------------------------

    #[test]
    fn config_with_no_action_parses() {
        assert!(matches!(
            parse_ok(&["kod", "config"]).command,
            Some(Command::Config { action: None })
        ));
    }

    #[test]
    fn config_leaf_actions_parse() {
        for verb in ["path", "validate", "show-merged", "show-raw", "edit"] {
            assert!(
                Cli::try_parse_from(["kod", "config", verb]).is_ok(),
                "`config {verb}` must parse"
            );
        }
    }

    #[test]
    fn config_export_defaults_path_to_dash() {
        match parse_ok(&["kod", "config", "export"]).command {
            Some(Command::Config {
                action: Some(ConfigAction::Export { path, force }),
            }) => {
                assert_eq!(path, std::path::PathBuf::from("-"));
                assert!(!force);
            }
            _ => panic!("expected Config::Export"),
        }
    }

    #[test]
    fn config_export_accepts_a_path_and_force() {
        match parse_ok(&["kod", "config", "export", "/tmp/c.toml", "--force"]).command {
            Some(Command::Config {
                action: Some(ConfigAction::Export { path, force }),
            }) => {
                assert_eq!(path, std::path::PathBuf::from("/tmp/c.toml"));
                assert!(force);
            }
            _ => panic!("expected Config::Export"),
        }
    }

    #[test]
    fn config_init_from_takes_a_profile_name() {
        match parse_ok(&["kod", "config", "init-from", "ollama"]).command {
            Some(Command::Config {
                action: Some(ConfigAction::InitFrom { name }),
            }) => assert_eq!(name, "ollama"),
            _ => panic!("expected Config::InitFrom"),
        }
    }

    // ---- top-level leaf commands --------------------------------------

    #[test]
    fn simple_leaf_commands_parse() {
        for verb in [
            "test",
            "update",
            "validate-skills",
            "validate-skills-strict",
        ] {
            assert!(
                Cli::try_parse_from(["kod", verb]).is_ok(),
                "`{verb}` must parse"
            );
        }
    }

    // ---- swarm / agent -------------------------------------------------

    #[test]
    fn swarm_requires_goal() {
        parse_err(&["kod", "swarm"]);
        assert!(Cli::try_parse_from(["kod", "swarm", "-g", "refactor"]).is_ok());
        assert!(Cli::try_parse_from(["kod", "swarm", "--goal", "refactor"]).is_ok());
    }

    #[test]
    fn swarm_accepts_agents_and_model() {
        match parse_ok(&["kod", "swarm", "-g", "x", "-n", "4", "-m", "gpt-5"]).command {
            Some(Command::Swarm { agents, model, .. }) => {
                assert_eq!(agents, Some(4));
                assert_eq!(model.as_deref(), Some("gpt-5"));
            }
            _ => panic!("expected Swarm"),
        }
    }

    #[test]
    fn agent_requires_goal_and_defaults_name() {
        parse_err(&["kod", "agent"]);
        match parse_ok(&["kod", "agent", "-g", "fix bug"]).command {
            Some(Command::Agent { name, goal, .. }) => {
                assert_eq!(name, "kod-agent");
                assert_eq!(goal, "fix bug");
            }
            _ => panic!("expected Agent"),
        }
        match parse_ok(&["kod", "agent", "-g", "fix", "--name", "alice"]).command {
            Some(Command::Agent { name, .. }) => assert_eq!(name, "alice"),
            _ => panic!("expected Agent"),
        }
    }

    // ---- prompt / run --------------------------------------------------

    #[test]
    fn prompt_requires_a_positional() {
        parse_err(&["kod", "prompt"]);
        match parse_ok(&["kod", "prompt", "hello"]).command {
            Some(Command::Prompt { prompt, .. }) => assert_eq!(prompt, "hello"),
            _ => panic!("expected Prompt"),
        }
    }

    #[test]
    fn prompt_dash_is_a_valid_positional() {
        // `-` is a literal positional (means "read from stdin"); it
        // must not be parsed as a flag.
        match parse_ok(&["kod", "prompt", "-"]).command {
            Some(Command::Prompt { prompt, .. }) => assert_eq!(prompt, "-"),
            _ => panic!("expected Prompt"),
        }
    }

    #[test]
    fn prompt_accepts_no_log_remote_and_socket() {
        match parse_ok(&[
            "kod", "prompt", "hi",
            "--no-log", "--remote",
            "--socket", "/tmp/k.sock",
        ])
        .command
        {
            Some(Command::Prompt { no_log, remote, socket, .. }) => {
                assert!(no_log);
                assert!(remote);
                assert_eq!(socket.as_deref(), Some(std::path::Path::new("/tmp/k.sock")));
            }
            _ => panic!("expected Prompt"),
        }
    }

    #[test]
    fn run_takes_a_prompt_and_optional_model() {
        parse_err(&["kod", "run"]);
        assert!(Cli::try_parse_from(["kod", "run", "hi"]).is_ok());
        assert!(Cli::try_parse_from(["kod", "run", "hi", "-m", "x"]).is_ok());
    }

    // ---- tui / doctor / init / models ---------------------------------

    #[test]
    fn tui_parses_no_flags_and_all_flags() {
        assert!(Cli::try_parse_from(["kod", "tui"]).is_ok());
        match parse_ok(&[
            "kod", "tui",
            "--model", "x",
            "--no-resume",
            "--sandbox",
            "--preset", "read-only",
        ])
        .command
        {
            Some(Command::Tui { no_resume, sandbox, .. }) => {
                assert!(no_resume);
                assert!(sandbox);
            }
            _ => panic!("expected Tui"),
        }
    }

    #[test]
    fn doctor_json_and_fix_are_independent_flags() {
        assert!(matches!(
            parse_ok(&["kod", "doctor"]).command,
            Some(Command::Doctor { json: false, fix: false })
        ));
        assert!(matches!(
            parse_ok(&["kod", "doctor", "--json"]).command,
            Some(Command::Doctor { json: true, .. })
        ));
        assert!(matches!(
            parse_ok(&["kod", "doctor", "--fix"]).command,
            Some(Command::Doctor { fix: true, .. })
        ));
    }

    #[test]
    fn init_force_flag_is_optional() {
        assert!(matches!(
            parse_ok(&["kod", "init"]).command,
            Some(Command::Init { force: false })
        ));
        assert!(matches!(
            parse_ok(&["kod", "init", "--force"]).command,
            Some(Command::Init { force: true })
        ));
    }

    #[test]
    fn models_filter_is_optional_and_has_a_short_alias() {
        assert!(matches!(
            parse_ok(&["kod", "models"]).command,
            Some(Command::Models { filter: None })
        ));
        match parse_ok(&["kod", "models", "-f", "qwen"]).command {
            Some(Command::Models { filter }) => assert_eq!(filter.as_deref(), Some("qwen")),
            _ => panic!("expected Models"),
        }
    }

    // ---- map / replay --------------------------------------------------

    #[test]
    fn map_defaults_max_chars_to_16000() {
        match parse_ok(&["kod", "map"]).command {
            Some(Command::Map { max_chars }) => assert_eq!(max_chars, 16000),
            _ => panic!("expected Map"),
        }
        match parse_ok(&["kod", "map", "--max-chars", "8000"]).command {
            Some(Command::Map { max_chars }) => assert_eq!(max_chars, 8000),
            _ => panic!("expected Map"),
        }
    }

    #[test]
    fn replay_requires_a_path_and_has_an_optional_execute_flag() {
        parse_err(&["kod", "replay"]);
        match parse_ok(&["kod", "replay", "/tmp/log.jsonl"]).command {
            Some(Command::Replay { execute, .. }) => assert!(!execute),
            _ => panic!("expected Replay"),
        }
        match parse_ok(&["kod", "replay", "/tmp/log.jsonl", "--execute"]).command {
            Some(Command::Replay { execute, .. }) => assert!(execute),
            _ => panic!("expected Replay"),
        }
    }

    // ---- completions ---------------------------------------------------

    #[test]
    fn completions_accepts_every_supported_shell() {
        for shell in ["bash", "zsh", "fish", "elvish", "powershell"] {
            assert!(
                Cli::try_parse_from(["kod", "completions", shell]).is_ok(),
                "completions {shell} must parse"
            );
        }
        // An unknown shell must be rejected by clap's value parser.
        parse_err(&["kod", "completions", "tcsh"]);
    }

    // ---- serve / acp ---------------------------------------------------

    #[test]
    fn serve_stop_is_a_flag_and_socket_overrides_the_default() {
        assert!(matches!(
            parse_ok(&["kod", "serve"]).command,
            Some(Command::Serve { stop: false, socket: None })
        ));
        match parse_ok(&["kod", "serve", "--stop", "--socket", "/tmp/k.sock"]).command {
            Some(Command::Serve { stop, socket }) => {
                assert!(stop);
                assert_eq!(socket, Some(std::path::PathBuf::from("/tmp/k.sock")));
            }
            _ => panic!("expected Serve"),
        }
    }

    #[test]
    fn acp_preset_is_optional() {
        assert!(matches!(
            parse_ok(&["kod", "acp"]).command,
            Some(Command::Acp { preset: None })
        ));
        match parse_ok(&["kod", "acp", "--preset", "standard"]).command {
            Some(Command::Acp { preset }) => assert_eq!(preset.as_deref(), Some("standard")),
            _ => panic!("expected Acp"),
        }
    }

    // ---- hidden sandbox-exec ------------------------------------------

    #[test]
    fn sandbox_exec_is_hidden_but_still_parses() {
        // Hidden from `--help` but reachable as `kod __sandbox-exec`.
        // The command after `--` is captured verbatim.
        match parse_ok(&[
            "kod", "__sandbox-exec", "/tmp/profile.json",
            "--", "echo", "hi",
        ])
        .command
        {
            Some(Command::SandboxExec { profile, cmd }) => {
                assert_eq!(profile, std::path::PathBuf::from("/tmp/profile.json"));
                assert_eq!(cmd, vec!["echo".to_string(), "hi".to_string()]);
            }
            _ => panic!("expected SandboxExec"),
        }
    }

    // ---- negative cases ------------------------------------------------

    #[test]
    fn unknown_subcommand_is_rejected() {
        parse_err(&["kod", "not-a-command"]);
    }

    #[test]
    fn unknown_flag_is_rejected() {
        parse_err(&["kod", "chat", "--no-such-flag"]);
    }

    #[test]
    fn skills_new_without_a_name_is_rejected() {
        parse_err(&["kod", "skills", "new"]);
    }

    #[test]
    fn skills_export_without_a_destination_is_rejected() {
        parse_err(&["kod", "skills", "export", "name-only"]);
    }

    #[test]
    fn fixture_save_parses() {
        match parse_ok(&["kod", "fixture", "save", "auth"]).command {
            Some(Command::Fixture {
                action: FixtureAction::Save { name, turns, force },
            }) => {
                assert_eq!(name, "auth");
                assert!(turns.is_none());
                assert!(!force);
            }
            _ => panic!("expected Fixture::Save"),
        }
    }

    #[test]
    fn fixture_save_accepts_turns_path_and_force() {
        match parse_ok(&[
            "kod", "fixture", "save", "auth",
            "--turns", "/tmp/turns.jsonl",
            "--force",
        ])
        .command
        {
            Some(Command::Fixture {
                action: FixtureAction::Save { name, turns, force },
            }) => {
                assert_eq!(name, "auth");
                assert_eq!(
                    turns,
                    Some(std::path::PathBuf::from("/tmp/turns.jsonl"))
                );
                assert!(force);
            }
            _ => panic!("expected Fixture::Save with flags"),
        }
    }

    #[test]
    fn fixture_replay_parses() {
        match parse_ok(&["kod", "fixture", "replay", "auth"]).command {
            Some(Command::Fixture {
                action: FixtureAction::Replay {
                    name,
                    strict,
                    first_round_only,
                },
            }) => {
                assert_eq!(name, "auth");
                assert!(!strict);
                assert!(!first_round_only);
            }
            _ => panic!("expected Fixture::Replay"),
        }
    }

    #[test]
    fn fixture_replay_strict_flag() {
        match parse_ok(&["kod", "fixture", "replay", "auth", "--strict"]).command {
            Some(Command::Fixture {
                action: FixtureAction::Replay { strict, .. },
            }) => assert!(strict),
            _ => panic!("expected Fixture::Replay with --strict"),
        }
    }

    #[test]
    fn fixture_replay_first_round_only_flag() {
        match parse_ok(&[
            "kod", "fixture", "replay", "auth", "--first-round-only",
        ])
        .command
        {
            Some(Command::Fixture {
                action: FixtureAction::Replay { first_round_only, .. },
            }) => assert!(first_round_only),
            _ => panic!("expected Fixture::Replay with --first-round-only"),
        }
    }

    #[test]
    fn fixture_list_parses() {
        match parse_ok(&["kod", "fixture", "list"]).command {
            Some(Command::Fixture {
                action: FixtureAction::List,
            }) => {}
            _ => panic!("expected Fixture::List"),
        }
    }

    #[test]
    fn fixture_without_subcommand_is_rejected() {
        parse_err(&["kod", "fixture"]);
    }

    #[test]
    fn fixture_save_without_a_name_is_rejected() {
        parse_err(&["kod", "fixture", "save"]);
    }

    #[test]
    fn fixture_unknown_subcommand_is_rejected() {
        parse_err(&["kod", "fixture", "frobnicate"]);
    }
}

/// Coverage for the pure rendering helpers that turn messages into
/// session-log markdown and one-line previews. No I/O, no engine, no
/// fixtures — the timestamp is frozen so output comparison is exact.
#[cfg(test)]
mod coverage_cli_render {
    use super::*;
    use chrono::{DateTime, Utc};
    use kod_tui::app::Message;
    use kod_types::{AgentId, MessageId, MessageRole};

    fn frozen_ts() -> DateTime<Utc> {
        DateTime::parse_from_rfc3339("2024-01-01T12:00:00Z")
            .expect("frozen timestamp")
            .with_timezone(&Utc)
    }

    fn msg(role: MessageRole, content: &str) -> Message {
        Message {
            id: MessageId::new(),
            role,
            content: content.to_string(),
            timestamp: frozen_ts(),
            metadata: Default::default(),
            sequence: 0,
        }
    }

    // ---- count_roles ---------------------------------------------------

    #[test]
    fn count_roles_of_empty_is_all_zero() {
        assert_eq!(count_roles(&[]), (0, 0, 0));
    }

    #[test]
    fn count_roles_partitions_user_assistant_other() {
        let msgs = vec![
            msg(MessageRole::User, "u1"),
            msg(MessageRole::User, "u2"),
            msg(MessageRole::Assistant, "a1"),
            msg(MessageRole::System, "s1"),
            msg(MessageRole::Tool, "t1"),
            msg(MessageRole::Agent(AgentId::new()), "ag1"),
        ];
        assert_eq!(count_roles(&msgs), (2, 1, 3));
    }

    // ---- render_session_markdown ---------------------------------------

    #[test]
    fn render_session_markdown_empty_has_only_the_header() {
        assert_eq!(render_session_markdown(&[]), "# KOD session\n\n");
    }

    #[test]
    fn render_session_markdown_user_section_has_no_fence() {
        let out = render_session_markdown(&[msg(MessageRole::User, "hello")]);
        assert!(out.starts_with("# KOD session\n\n"));
        assert!(out.contains("## you · 2024-01-01 12:00:00"));
        assert!(out.contains("\n\nhello\n\n"));
        assert!(!out.contains("```"));
    }

    #[test]
    fn render_session_markdown_assistant_section_is_fenced() {
        let out = render_session_markdown(&[msg(MessageRole::Assistant, "reply")]);
        assert!(out.contains("## ai · 2024-01-01 12:00:00"));
        assert!(out.contains("```\nreply\n```"));
    }

    #[test]
    fn render_session_markdown_system_section_is_not_fenced() {
        let out = render_session_markdown(&[msg(MessageRole::System, "sys")]);
        assert!(out.contains("## sys · 2024-01-01 12:00:00"));
        assert!(!out.contains("```"));
    }

    #[test]
    fn render_session_markdown_tool_section_is_fenced() {
        let out = render_session_markdown(&[msg(MessageRole::Tool, "tool out")]);
        assert!(out.contains("## tool · 2024-01-01 12:00:00"));
        assert!(out.contains("```\ntool out\n```"));
    }

    #[test]
    fn render_session_markdown_agent_section_uses_the_id_and_is_unfenced() {
        let out = render_session_markdown(&[msg(
            MessageRole::Agent(AgentId::new()),
            "planning",
        )]);
        // Assert on the prefix rather than the full id form so a
        // future change to AgentId's Display does not silently break
        // the assertion here.
        assert!(out.contains("## agent "));
        assert!(out.contains("planning"));
        assert!(!out.contains("```"));
    }

    #[test]
    fn render_session_markdown_appends_a_newline_before_the_closing_fence() {
        // Fenced content that does not already end in `\n` gets one
        // inserted before the closing fence; otherwise the fence
        // marker sits on the same line as the last content char.
        let out = render_session_markdown(&[msg(MessageRole::Assistant, "no trailing newline")]);
        assert!(
            out.contains("no trailing newline\n```"),
            "expected a newline before the closing fence, got: {out:?}"
        );
    }

    #[test]
    fn render_session_markdown_preserves_content_that_already_ends_in_newline() {
        // Content that already ends in `\n` must NOT get an extra
        // newline; the fence sits directly under it, not two lines
        // below.
        let out = render_session_markdown(&[msg(MessageRole::Assistant, "with newline\n")]);
        assert!(
            out.contains("with newline\n```"),
            "fence must follow the content's own newline, got: {out:?}"
        );
        assert!(
            !out.contains("with newline\n\n```"),
            "must not insert a second newline, got: {out:?}"
        );
    }

    #[test]
    fn render_session_markdown_orders_messages_as_given() {
        let msgs = vec![
            msg(MessageRole::User, "first"),
            msg(MessageRole::Assistant, "second"),
            msg(MessageRole::User, "third"),
        ];
        let out = render_session_markdown(&msgs);
        let i_first = out.find("first").expect("first present");
        let i_second = out.find("second").expect("second present");
        let i_third = out.find("third").expect("third present");
        assert!(i_first < i_second, "first must precede second");
        assert!(i_second < i_third, "second must precede third");
    }

    // ---- preview_line --------------------------------------------------

    #[test]
    fn preview_line_returns_short_input_unchanged() {
        assert_eq!(preview_line("hello", 10), "hello");
    }

    #[test]
    fn preview_line_takes_only_the_first_line() {
        assert_eq!(preview_line("first\nsecond", 20), "first");
    }

    #[test]
    fn preview_line_truncates_with_an_ellipsis() {
        assert_eq!(preview_line("abcdefghij", 4), "abcd…");
    }

    #[test]
    fn preview_line_handles_empty_input() {
        assert_eq!(preview_line("", 10), "");
    }

    #[test]
    fn preview_line_counts_chars_not_bytes() {
        // A multi-byte character is one char. The clip is char-aware:
        // max=3 keeps three chars, not three bytes.
        assert_eq!(preview_line("日本語です", 3), "日本語…");
    }
}

/// Parse-surface coverage for the eight sub-action enums that hang
/// off the top-level commands. Same shape as `coverage_cli_parsing`:
/// one test per documented invocation, plus the flags and defaults
/// that a caller relies on.
#[cfg(test)]
mod coverage_cli_subactions {
    use super::*;

    fn parse_ok(args: &[&str]) -> Cli {
        Cli::try_parse_from(args).unwrap_or_else(|e| {
            panic!("expected `{}` to parse, got error: {e}", args.join(" "))
        })
    }

    fn parse_err(args: &[&str]) {
        assert!(
            Cli::try_parse_from(args).is_err(),
            "expected `{}` to be rejected, but it parsed",
            args.join(" ")
        );
    }

    // ---- ProfileAction -------------------------------------------------

    #[test]
    fn profile_list_defaults_json_to_false_and_accepts_the_flag() {
        match parse_ok(&["kod", "profile", "list"]).command {
            Some(Command::Profile {
                action: ProfileAction::List { json },
            }) => assert!(!json),
            _ => panic!("expected Profile::List"),
        }
        match parse_ok(&["kod", "profile", "list", "--json"]).command {
            Some(Command::Profile {
                action: ProfileAction::List { json: true },
            }) => {}
            _ => panic!("expected Profile::List with json=true"),
        }
    }

    #[test]
    fn profile_show_takes_no_arguments() {
        assert!(matches!(
            parse_ok(&["kod", "profile", "show"]).command,
            Some(Command::Profile {
                action: ProfileAction::Show
            })
        ));
        parse_err(&["kod", "profile", "show", "extra"]);
    }

    #[test]
    fn profile_use_takes_a_name_and_optional_dry_run() {
        match parse_ok(&["kod", "profile", "use", "ollama"]).command {
            Some(Command::Profile {
                action: ProfileAction::Use { name, dry_run },
            }) => {
                assert_eq!(name, "ollama");
                assert!(!dry_run);
            }
            _ => panic!("expected Profile::Use"),
        }
        match parse_ok(&["kod", "profile", "use", "ollama", "--dry-run"]).command {
            Some(Command::Profile {
                action: ProfileAction::Use { dry_run: true, .. },
            }) => {}
            _ => panic!("expected Profile::Use with dry_run"),
        }
    }

    // ---- ThemeAction ---------------------------------------------------

    #[test]
    fn theme_actions_parse() {
        assert!(matches!(
            parse_ok(&["kod", "theme", "list"]).command,
            Some(Command::Theme {
                action: ThemeAction::List
            })
        ));
        match parse_ok(&["kod", "theme", "show", "dark"]).command {
            Some(Command::Theme {
                action: ThemeAction::Show { name },
            }) => assert_eq!(name, "dark"),
            _ => panic!("expected Theme::Show"),
        }
    }

    #[test]
    fn theme_show_without_a_name_is_rejected() {
        parse_err(&["kod", "theme", "show"]);
    }

    // ---- ToolsAction ---------------------------------------------------

    #[test]
    fn tools_with_no_action_parses_as_none() {
        assert!(matches!(
            parse_ok(&["kod", "tools"]).command,
            Some(Command::Tools { action: None })
        ));
    }

    #[test]
    fn tools_list_and_show_parse() {
        assert!(matches!(
            parse_ok(&["kod", "tools", "list"]).command,
            Some(Command::Tools {
                action: Some(ToolsAction::List)
            })
        ));
        match parse_ok(&["kod", "tools", "show", "write_file"]).command {
            Some(Command::Tools {
                action: Some(ToolsAction::Show { name }),
            }) => assert_eq!(name, "write_file"),
            _ => panic!("expected Tools::Show"),
        }
    }

    // ---- SandboxAction -------------------------------------------------

    #[test]
    fn sandbox_check_is_a_required_subcommand() {
        parse_err(&["kod", "sandbox"]);
        assert!(matches!(
            parse_ok(&["kod", "sandbox", "check"]).command,
            Some(Command::Sandbox {
                action: SandboxAction::Check
            })
        ));
    }

    // ---- PolicyAction --------------------------------------------------

    #[test]
    fn policy_show_parses() {
        assert!(matches!(
            parse_ok(&["kod", "policy", "show"]).command,
            Some(Command::Policy {
                action: PolicyAction::Show
            })
        ));
    }

    #[test]
    fn policy_forget_takes_an_optional_index() {
        // Bare `forget` lists the rules; with an index it drops one.
        match parse_ok(&["kod", "policy", "forget"]).command {
            Some(Command::Policy {
                action: PolicyAction::Forget { n },
            }) => assert!(n.is_none()),
            _ => panic!("expected Policy::Forget"),
        }
        match parse_ok(&["kod", "policy", "forget", "2"]).command {
            Some(Command::Policy {
                action: PolicyAction::Forget { n },
            }) => assert_eq!(n, Some(2)),
            _ => panic!("expected Policy::Forget with index"),
        }
    }

    #[test]
    fn policy_forget_rejects_a_non_numeric_index() {
        parse_err(&["kod", "policy", "forget", "abc"]);
    }

    #[test]
    fn policy_explain_takes_a_tool_and_trailing_var_args() {
        match parse_ok(&["kod", "policy", "explain", "write_file"]).command {
            Some(Command::Policy {
                action: PolicyAction::Explain { tool, args },
            }) => {
                assert_eq!(tool, "write_file");
                assert!(args.is_empty());
            }
            _ => panic!("expected Policy::Explain"),
        }
        match parse_ok(&[
            "kod", "policy", "explain", "execute_command",
            "command=cargo test",
            "env=dev",
        ])
        .command
        {
            Some(Command::Policy {
                action: PolicyAction::Explain { tool, args },
            }) => {
                assert_eq!(tool, "execute_command");
                assert_eq!(args, vec!["command=cargo test", "env=dev"]);
            }
            _ => panic!("expected Policy::Explain with args"),
        }
    }

    #[test]
    fn policy_explain_allows_hyphen_values_in_trailing_args() {
        // The `allow_hyphen_values` attribute is what lets a caller
        // write `kod policy explain web_fetch url=https://...` and
        // also pass a bare `-x` without clap treating it as a flag.
        match parse_ok(&[
            "kod", "policy", "explain", "execute_command",
            "command=ls", "-la",
        ])
        .command
        {
            Some(Command::Policy {
                action: PolicyAction::Explain { args, .. },
            }) => {
                assert_eq!(args, vec!["command=ls", "-la"]);
            }
            _ => panic!("expected Policy::Explain"),
        }
    }

    #[test]
    fn policy_explain_without_a_tool_is_rejected() {
        parse_err(&["kod", "policy", "explain"]);
    }

    // ---- MemoryAction --------------------------------------------------

    #[test]
    fn memory_add_takes_content_and_optional_tags() {
        match parse_ok(&["kod", "memory", "add", "remember this"]).command {
            Some(Command::Memory {
                action: MemoryAction::Add { content, tags },
            }) => {
                assert_eq!(content, "remember this");
                assert!(tags.is_none());
            }
            _ => panic!("expected Memory::Add"),
        }
        match parse_ok(&[
            "kod", "memory", "add", "remember", "--tags", "preference,rust",
        ])
        .command
        {
            Some(Command::Memory {
                action: MemoryAction::Add { tags, .. },
            }) => assert_eq!(tags.as_deref(), Some("preference,rust")),
            _ => panic!("expected Memory::Add with tags"),
        }
    }

    #[test]
    fn memory_forget_takes_a_key() {
        match parse_ok(&["kod", "memory", "forget", "preference"]).command {
            Some(Command::Memory {
                action: MemoryAction::Forget { key },
            }) => assert_eq!(key, "preference"),
            _ => panic!("expected Memory::Forget"),
        }
    }

    #[test]
    fn memory_list_parses() {
        assert!(matches!(
            parse_ok(&["kod", "memory", "list"]).command,
            Some(Command::Memory {
                action: MemoryAction::List
            })
        ));
    }

    #[test]
    fn memory_export_and_import_default_path_to_dash() {
        match parse_ok(&["kod", "memory", "export"]).command {
            Some(Command::Memory {
                action: MemoryAction::Export { path },
            }) => assert_eq!(path, std::path::PathBuf::from("-")),
            _ => panic!("expected Memory::Export"),
        }
        match parse_ok(&["kod", "memory", "import"]).command {
            Some(Command::Memory {
                action: MemoryAction::Import { path },
            }) => assert_eq!(path, std::path::PathBuf::from("-")),
            _ => panic!("expected Memory::Import"),
        }
        // An explicit path overrides the default.
        match parse_ok(&["kod", "memory", "export", "/tmp/m.json"]).command {
            Some(Command::Memory {
                action: MemoryAction::Export { path },
            }) => assert_eq!(path, std::path::PathBuf::from("/tmp/m.json")),
            _ => panic!("expected Memory::Export with path"),
        }
    }

    #[test]
    fn memory_search_takes_a_query() {
        match parse_ok(&["kod", "memory", "search", "rust"]).command {
            Some(Command::Memory {
                action: MemoryAction::Search { query },
            }) => assert_eq!(query, "rust"),
            _ => panic!("expected Memory::Search"),
        }
    }

    #[test]
    fn memory_delete_takes_an_id() {
        match parse_ok(&["kod", "memory", "delete", "deadbeef"]).command {
            Some(Command::Memory {
                action: MemoryAction::Delete { id },
            }) => assert_eq!(id, "deadbeef"),
            _ => panic!("expected Memory::Delete"),
        }
    }

    #[test]
    fn memory_clear_defaults_yes_to_false() {
        match parse_ok(&["kod", "memory", "clear"]).command {
            Some(Command::Memory {
                action: MemoryAction::Clear { yes },
            }) => assert!(!yes),
            _ => panic!("expected Memory::Clear"),
        }
        match parse_ok(&["kod", "memory", "clear", "--yes"]).command {
            Some(Command::Memory {
                action: MemoryAction::Clear { yes: true },
            }) => {}
            _ => panic!("expected Memory::Clear with yes"),
        }
    }

    // ---- CheckpointAction ----------------------------------------------

    #[test]
    fn checkpoint_diff_and_restore_take_an_id() {
        match parse_ok(&["kod", "checkpoint", "diff", "abc123"]).command {
            Some(Command::Checkpoint {
                action: CheckpointAction::Diff { id },
            }) => assert_eq!(id, "abc123"),
            _ => panic!("expected Checkpoint::Diff"),
        }
        match parse_ok(&["kod", "checkpoint", "restore", "abc123"]).command {
            Some(Command::Checkpoint {
                action: CheckpointAction::Restore { id },
            }) => assert_eq!(id, "abc123"),
            _ => panic!("expected Checkpoint::Restore"),
        }
    }

    #[test]
    fn checkpoint_list_defaults_limit_to_20_and_has_a_short_alias() {
        match parse_ok(&["kod", "checkpoint", "list"]).command {
            Some(Command::Checkpoint {
                action: CheckpointAction::List { limit },
            }) => assert_eq!(limit, 20),
            _ => panic!("expected Checkpoint::List"),
        }
        match parse_ok(&["kod", "checkpoint", "list", "-l", "5"]).command {
            Some(Command::Checkpoint {
                action: CheckpointAction::List { limit },
            }) => assert_eq!(limit, 5),
            _ => panic!("expected Checkpoint::List with -l"),
        }
        match parse_ok(&["kod", "checkpoint", "list", "--limit", "3"]).command {
            Some(Command::Checkpoint {
                action: CheckpointAction::List { limit },
            }) => assert_eq!(limit, 3),
            _ => panic!("expected Checkpoint::List with --limit"),
        }
    }

    #[test]
    fn checkpoint_clear_parses() {
        assert!(matches!(
            parse_ok(&["kod", "checkpoint", "clear"]).command,
            Some(Command::Checkpoint {
                action: CheckpointAction::Clear
            })
        ));
    }

    #[test]
    fn checkpoint_without_a_subcommand_is_rejected() {
        parse_err(&["kod", "checkpoint"]);
    }

    // ---- SessionsAction ------------------------------------------------

    #[test]
    fn sessions_show_clear_latest_count_parse() {
        assert!(matches!(
            parse_ok(&["kod", "sessions", "show"]).command,
            Some(Command::Sessions {
                action: SessionsAction::Show
            })
        ));
        assert!(matches!(
            parse_ok(&["kod", "sessions", "clear"]).command,
            Some(Command::Sessions {
                action: SessionsAction::Clear
            })
        ));
        assert!(matches!(
            parse_ok(&["kod", "sessions", "latest"]).command,
            Some(Command::Sessions {
                action: SessionsAction::Latest
            })
        ));
        assert!(matches!(
            parse_ok(&["kod", "sessions", "count"]).command,
            Some(Command::Sessions {
                action: SessionsAction::Count
            })
        ));
    }

    #[test]
    fn sessions_export_defaults_to_stdout_markdown() {
        match parse_ok(&["kod", "sessions", "export"]).command {
            Some(Command::Sessions {
                action: SessionsAction::Export { path, format },
            }) => {
                assert_eq!(path, std::path::PathBuf::from("-"));
                assert_eq!(format, "markdown");
            }
            _ => panic!("expected Sessions::Export"),
        }
    }

    #[test]
    fn sessions_export_accepts_path_and_format() {
        match parse_ok(&[
            "kod", "sessions", "export", "/tmp/s.json", "-f", "json",
        ])
        .command
        {
            Some(Command::Sessions {
                action: SessionsAction::Export { path, format },
            }) => {
                assert_eq!(path, std::path::PathBuf::from("/tmp/s.json"));
                assert_eq!(format, "json");
            }
            _ => panic!("expected Sessions::Export with path and format"),
        }
        // Long form of the format flag.
        match parse_ok(&["kod", "sessions", "export", "-", "--format", "json"]).command {
            Some(Command::Sessions {
                action: SessionsAction::Export { format, .. },
            }) => assert_eq!(format, "json"),
            _ => panic!("expected Sessions::Export with --format"),
        }
    }

    #[test]
    fn sessions_import_requires_a_path() {
        parse_err(&["kod", "sessions", "import"]);
        match parse_ok(&["kod", "sessions", "import", "/tmp/s.json"]).command {
            Some(Command::Sessions {
                action: SessionsAction::Import { path },
            }) => assert_eq!(path, std::path::PathBuf::from("/tmp/s.json")),
            _ => panic!("expected Sessions::Import"),
        }
    }

    #[test]
    fn sessions_without_a_subcommand_is_rejected() {
        parse_err(&["kod", "sessions"]);
    }
}


// ---------------------------------------------------------------------------
// Tier 1.4 — `kod trace`
// ---------------------------------------------------------------------------

/// Default `turns.jsonl` path under the user's home directory.
fn default_trace_path() -> Option<std::path::PathBuf> {
    dirs::home_dir().map(|h| h.join(".kod").join("sessions").join("turns.jsonl"))
}

/// Resolve the path argument, falling back to the default, or error.
fn resolve_trace_path(
    arg: Option<std::path::PathBuf>,
) -> Result<std::path::PathBuf> {
    match arg {
        Some(p) => Ok(p),
        None => default_trace_path().ok_or_else(|| {
            KodError::Config("no home directory to resolve the trace path".to_string())
        }),
    }
}

/// `kod trace list` — a compact table of recent turns.
pub async fn run_trace_list(
    limit: usize,
    path: Option<std::path::PathBuf>,
) -> Result<()> {
    let path = resolve_trace_path(path)?;
    if !path.exists() {
        eprintln!("no trace file at {}", path.display());
        eprintln!("  Set KOD_SESSION_LOG and run a session first.");
        return Ok(());
    }
    let traces = kod_core::read_traces(&path)?;
    if traces.is_empty() {
        eprintln!("no turns recorded in {}", path.display());
        return Ok(());
    }
    let cap = limit.min(200).min(traces.len());
    println!("{:<6} {:<14} {:>9} {:>9} {:>10} {:>7}",
        "id", "holder", "duration", "cost", "tokens", "tools");
    // Newest first.
    for t in traces.iter().rev().take(cap) {
        let dur = format!("{:.2}s", t.duration_ms() as f64 / 1000.0);
        let cost = format!("${:.4}", t.cost_usd);
        let toks = format!("{}→{}", t.prompt_tokens, t.completion_tokens);
        println!(
            "{:<6} {:<14} {:>9} {:>9} {:>10} {:>7}",
            t.id,
            truncate_field(&t.holder, 14),
            dur,
            cost,
            toks,
            t.tool_call_count,
        );
    }
    if traces.len() > cap {
        println!();
        println!("  … {} more (use --limit to see them)", traces.len() - cap);
    }
    Ok(())
}

/// `kod trace show <id>` — one turn's full tree.
pub async fn run_trace_show(
    id: u64,
    path: Option<std::path::PathBuf>,
) -> Result<()> {
    let path = resolve_trace_path(path)?;
    let traces = kod_core::read_traces(&path)?;
    let Some(t) = traces.iter().find(|t| t.id == id) else {
        eprintln!("no trace with id {id} in {}", path.display());
        return Ok(());
    };
    println!(
        "Turn #{} ({})   {:.2}s   ${:.4}   {} in → {} out",
        t.id,
        t.holder,
        t.duration_ms() as f64 / 1000.0,
        t.cost_usd,
        t.prompt_tokens,
        t.completion_tokens,
    );
    println!(
        "  prompt: {} chars   reply: {} chars   tools: {}",
        t.prompt_chars, t.reply_chars, t.tool_call_count,
    );
    println!(
        "  jev: {} decisions ({} cached)",
        t.jev_decisions, t.jev_cache_hits,
    );
    println!("  outcome: {:?}", t.outcome);
    if let Some(r) = &t.reason {
        println!("  reason: {r}");
    }
    if t.rounds.is_empty() {
        return Ok(());
    }
    println!();
    println!("Rounds");
    for (i, r) in t.rounds.iter().enumerate() {
        println!(
            "  {:>2}  {:<12} {:<24} {:>6}ms  {} in → {} out",
            i + 1,
            format!("{:?}", r.kind).to_lowercase(),
            format!("{}/{}", r.endpoint, r.model),
            r.duration_ms,
            r.input_tokens,
            r.output_tokens,
        );
        for c in &r.tool_calls {
            println!(
                "       tool  {:<16} {}ms  {}  {} bytes",
                c.name,
                c.duration_ms,
                match c.outcome {
                    kod_core::ToolOutcomeKind::Success => "ok",
                    kod_core::ToolOutcomeKind::Error => "err",
                    kod_core::ToolOutcomeKind::Denied => "denied",
                    kod_core::ToolOutcomeKind::RequiresConfirmation => "ask",
                },
                c.output_bytes,
            );
        }
        for rr in &r.retries {
            println!(
                "       retry  {} → {}  ({})",
                rr.from_endpoint, rr.to_endpoint, rr.reason,
            );
        }
    }
    Ok(())
}

/// `kod trace json` — every turn as a JSON array.
pub async fn run_trace_json(path: Option<std::path::PathBuf>) -> Result<()> {
    let path = resolve_trace_path(path)?;
    let traces = kod_core::read_traces(&path)?;
    let s = serde_json::to_string_pretty(&traces)
        .map_err(|e| KodError::Serialization(e.to_string()))?;
    println!("{s}");
    Ok(())
}

/// Truncate a display field to `max` chars, adding `…` on cut.
fn truncate_field(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let mut out: String = s.chars().take(max.saturating_sub(1)).collect();
    out.push('…');
    out
}

// ---------------------------------------------------------------------------
// Tier 1.5 — fixture replay and diff
// ---------------------------------------------------------------------------

/// Replay a saved fixture against the current engine, printing any
/// divergence in the request shape (Tier 1.5). Returns the number of
/// divergent rounds; zero means a clean replay.
pub async fn run_fixture_replay(
    name: &str,
    strict: bool,
    first_round_only: bool,
) -> Result<i32> {
    use kod_provider::replay::{ReplayProvider, ReplayRound, ReplayToolCall};

    let path = kod_core::Fixture::default_path(name).ok_or_else(|| {
        KodError::Config("could not determine fixtures directory".to_string())
    })?;
    let fixture = kod_core::Fixture::load_from(&path).map_err(|e| {
        KodError::Config(format!("could not load fixture {}: {e}", path.display()))
    })?;
    eprintln!(
        "Replaying fixture {} ({} rounds, created at {})",
        fixture.name,
        fixture.rounds.len(),
        fixture.created_at_ms,
    );

    // Build the replay provider from the fixture.
    let rounds: Vec<ReplayRound> = fixture
        .rounds
        .iter()
        .map(|r| ReplayRound {
            text: r.response.text.clone(),
            tool_calls: r
                .response
                .tool_calls
                .iter()
                .map(|c| ReplayToolCall {
                    id: c.id.clone(),
                    name: c.name.clone(),
                    arguments: c.arguments.clone(),
                })
                .collect(),
            usage: r.response.usage.as_ref().map(|u| kod_provider::TokenUsage {
                prompt_tokens: u.prompt_tokens,
                completion_tokens: u.completion_tokens,
                total_tokens: u.prompt_tokens + u.completion_tokens,
            }),
        })
        .collect();
    // Two handles to the same provider: the concrete one for
    // `.captured()`, and an `Arc<dyn LlmProvider>` to install on the
    // registry.
    let replay_concrete = std::sync::Arc::new(ReplayProvider::new(rounds));
    let replay: std::sync::Arc<dyn kod_provider::LlmProvider> = replay_concrete.clone();

    // Build a fresh engine with the replay provider installed as the
    // only endpoint. We assemble the registry directly with
    // `ProviderRegistry::insert` — the in-crate `install_test_provider`
    // shim is `pub(crate)` and not reachable from kod-cli.
    let tmp_root = std::env::temp_dir().join(format!(
        "kod-fixture-replay-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0),
    ));
    std::fs::create_dir_all(&tmp_root).map_err(KodError::Io)?;
    let cfg = kod_core::RouterConfig {
        working_dir: std::env::current_dir()
            .map_err(|e| KodError::Internal(format!("cwd: {e}")))?,
        enable_memory: false,
        ..kod_core::RouterConfig::default()
    };
    let engine = kod_core::KodEngine::new(cfg, tmp_root.join("replay.redb"))
        .map_err(|e| KodError::Internal(format!("engine: {e}")))?;
    let mut registry = kod_provider::ProviderRegistry::new();
    registry.insert(
        "replay",
        replay.clone(),
        kod_provider::ProviderCapabilities {
            tools: true,
            streaming_tools: true,
            ..kod_provider::ProviderCapabilities::conservative()
        },
        "replay-fixture",
    );
    engine
        .set_registry(
            std::sync::Arc::new(registry),
            kod_provider::ModelRef::new("replay", "replay-fixture"),
            None,
        )
        .await;
    engine
        .start()
        .await
        .map_err(|e| KodError::Internal(format!("start: {e}")))?;

    // Drive each round's user prompt through the engine. The
    // ReplayProvider answers with the recorded response, and captures
    // the request the engine built.
    let mut divergent = 0_i32;
    let rounds_to_run = if first_round_only {
        fixture
            .rounds
            .iter()
            .position(|r| !r.user_prompt.is_empty())
            .map(|i| i + 1)
            .unwrap_or(fixture.rounds.len())
    } else {
        fixture.rounds.len()
    };
    for (idx, round) in fixture.rounds.iter().take(rounds_to_run).enumerate() {
        if round.user_prompt.is_empty() {
            continue;
        }
        if let Err(e) = engine.process_for("session", &round.user_prompt).await {
            eprintln!("  round {idx}: engine error: {e}");
            divergent += 1;
            continue;
        }
    }
    let _ = engine.shutdown().await;
    // Best-effort cleanup of the scratch directory.
    let _ = std::fs::remove_dir_all(&tmp_root);

    // Compare the captured requests to the fixture's summaries.
    let captured = replay_concrete.captured();
    let cmp_len = captured.len().min(fixture.rounds.len());
    for i in 0..cmp_len {
        let want = &fixture.rounds[i].request_summary;
        let got = kod_core::RequestSummary::from_request(&captured[i]);
        if want.hash() != got.hash() {
            divergent += 1;
            eprintln!();
            eprintln!(
                "✗ round {i} request mismatch (expected hash {}, got {})",
                &fixture.rounds[i].request_hash[..8.min(fixture.rounds[i].request_hash.len())],
                &got.hash()[..8],
            );
            let diff = kod_core::diff_rounds(
                &fixture.rounds[i],
                &kod_core::RoundFixture {
                    seq: i as u32,
                    user_prompt: fixture.rounds[i].user_prompt.clone(),
                    request_hash: got.hash(),
                    request_summary: got.clone(),
                    response: kod_core::ResponseFixture {
                        text: String::new(),
                        tool_calls: Vec::new(),
                        usage: None,
                    },
                    at_ms: 0,
                },
            );
            eprint!("{diff}");
        }
    }

    // Round-count mismatch is a soft divergence unless strict. When
    // `--first-round-only` is set, we only ran one round; comparing
    // counts is not meaningful.
    if !first_round_only && captured.len() != fixture.rounds.len() {
        let msg = format!(
            "round count: fixture has {}, replay produced {}",
            fixture.rounds.len(),
            captured.len(),
        );
        if strict {
            eprintln!("✗ {msg} (strict)");
            divergent += 1;
        } else {
            eprintln!("⚠ {msg}");
        }
    }

    if divergent == 0 {
        eprintln!(
            "✓ replay clean: {} round(s) matched",
            cmp_len,
        );
        Ok(0)
    } else {
        eprintln!("✗ replay diverged on {} round(s)", divergent);
        Ok(1)
    }
}

/// Tier 1.5 — save turn traces as a fixture, taking the turns path
/// explicitly. Prefers `<home>/.kod/sessions/turns.jsonl` when
/// `turns` is `None`.
///
/// Refuses to overwrite an existing fixture unless `force` is set.
pub async fn run_fixture_save_v2(
    name: &str,
    turns: Option<std::path::PathBuf>,
    force: bool,
) -> Result<()> {
    let turns_path = match turns {
        Some(p) => p,
        None => {
            let home = dirs::home_dir()
                .ok_or_else(|| KodError::Config("no home directory".to_string()))?;
            home.join(".kod").join("sessions").join("turns.jsonl")
        }
    };
    if !turns_path.exists() {
        return Err(KodError::Config(format!(
            "no turn traces at {} — run a session with \
             KOD_SESSION_LOG unset first, or pass --turns <path>",
            turns_path.display(),
        )));
    }
    let dest = kod_core::Fixture::default_path(name).ok_or_else(|| {
        KodError::Config("could not determine fixtures directory".to_string())
    })?;
    if dest.exists() && !force {
        return Err(KodError::Config(format!(
            "fixture {} already exists; pass --force to overwrite",
            dest.display(),
        )));
    }
    run_fixture_save(name, &turns_path).await
}

/// Tier 1.5 — list the fixtures under `~/.kod/fixtures/`.
pub async fn run_fixture_list() -> Result<()> {
    let Some(dir) = kod_core::Fixture::fixtures_dir() else {
        return Err(KodError::Config("no home directory".to_string()));
    };
    if !dir.exists() {
        eprintln!("no fixtures directory at {}", dir.display());
        return Ok(());
    }
    let mut rows: Vec<(String, u64, usize)> = Vec::new();
    for entry in std::fs::read_dir(&dir).map_err(KodError::Io)? {
        let entry = entry.map_err(KodError::Io)?;
        let path = entry.path();
        if path.extension().and_then(|s| s.to_str()) != Some("json") {
            continue;
        }
        let name = path
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("(unknown)")
            .to_string();
        match kod_core::Fixture::load_from(&path) {
            Ok(f) => rows.push((name, f.created_at_ms, f.rounds.len())),
            Err(e) => {
                eprintln!("WARN: could not load {}: {e}", path.display());
                rows.push((name, 0, 0));
            }
        }
    }
    if rows.is_empty() {
        eprintln!("no fixtures in {}", dir.display());
        return Ok(());
    }
    rows.sort_by(|a, b| a.0.cmp(&b.0));
    println!("Fixtures in {}", dir.display());
    for (name, ms, n) in &rows {
        let when = if *ms > 0 {
            format!("{ms}")
        } else {
            "(unreadable)".to_string()
        };
        println!("  {name:<32}  rounds={n:<5}  created_at_ms={when}");
    }
    Ok(())
}

/// Save the current session's turns as a fixture.
pub async fn run_fixture_save(name: &str, turns_path: &std::path::Path) -> Result<()> {
    let traces = kod_core::read_traces(turns_path).map_err(|e| {
        KodError::Config(format!("could not read turn traces: {e}"))
    })?;
    if traces.is_empty() {
        return Err(KodError::Config(
            "no turn traces recorded; set KOD_SESSION_LOG and run a session first".to_string(),
        ));
    }
    let mut fixture = kod_core::Fixture::new(name);
    for (i, t) in traces.iter().enumerate() {
        let summary = kod_core::RequestSummary {
            system_chars: t.prompt_chars,
            message_count: t.rounds.first().map(|_| 1).unwrap_or(0),
            tool_names: Vec::new(),
            model: "unknown".to_string(),
            endpoint: "unknown".to_string(),
        };
        let hash = summary.hash();
        fixture.rounds.push(kod_core::RoundFixture {
            seq: i as u32,
            // Tier 1.5 — the trace carries the user prompt; copy
            // it so replay can re-drive the turn.
            user_prompt: t.user_prompt.clone(),
            request_hash: hash,
            request_summary: summary,
            response: kod_core::ResponseFixture {
                text: String::new(),
                tool_calls: Vec::new(),
                usage: None,
            },
            at_ms: t.ended_at_ms,
        });
    }
    let path = kod_core::Fixture::default_path(name).ok_or_else(|| {
        KodError::Config("could not determine fixtures directory".to_string())
    })?;
    fixture.save_to(&path).map_err(KodError::Io)?;
    eprintln!("Wrote fixture {} ({} rounds)", path.display(), fixture.rounds.len());
    Ok(())
}
