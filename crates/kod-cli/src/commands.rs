//! Command definitions and handlers for the KOD CLI.

use clap::Parser;
use clap::Subcommand;
use kod_config::KodConfig;
use kod_core::KodEngine;
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
                println!("KOD - Terminal-native AI coding agent");
                println!("Use --help for usage information.");
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

    // Create engine
    let router_config = RouterConfig::default();
    let engine = KodEngine::new(router_config, db_path)?;

    // Set up OpenAI-compatible provider (Ollama /v1, LM Studio, MLX, ...)
    let provider = OpenAICompatProvider::from_config(&config.llm, Some(&model_name))?;
    engine.set_provider(Arc::new(provider)).await;

    // Start the engine
    engine.start().await?;

    // Load skills into the engine so the router has skill inventory and instructions
    let skills_dir = config.skills_dir()?;
    if skills_dir.exists() {
        match engine.load_skills(&skills_dir).await {
            Ok(n) if n > 0 => println!("Loaded {} skills", n),
            Ok(_) => {}
            Err(e) => eprintln!("Could not load skills: {}", e),
        }
    }

    println!(
        "KOD Chat (model: {}) - Type 'quit' or Ctrl+C to exit",
        model_name
    );
    println!();

    let stdin = io::stdin();
    let mut input = String::new();
    print!("> ");
    let _ = io::stdout().flush();

    while let Ok(bytes) = stdin.lock().read_line(&mut input) {
        if bytes == 0 {
            break;
        }
        let input_line = input.trim();
        if input_line.is_empty() {
            print!("> ");
            let _ = io::stdout().flush();
            continue;
        }
        if input_line == "quit" || input_line == "exit" {
            break;
        }

        let response = engine.process(input_line).await?;

        if let Some(text) = response.text {
            println!();
            println!("{}", text);
            println!();
        }

        input.clear();
        print!("> ");
        let _ = io::stdout().flush();
    }

    // Shutdown
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

    let router_config = RouterConfig::default();
    let engine = KodEngine::new(router_config, db_path)?;

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
    let skills_dir = config.skills_dir()?;

    if !skills_dir.exists() {
        println!("Skills directory not found: {}", skills_dir.display());
        println!("No skills available.");
        return Ok(());
    }

    let mut loader = kod_skills::SkillLoader::new(&skills_dir);
    let skills = loader.load_all().await?;

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
    println!("Swarm:");
    println!("  Max Agents: {}", config.swarm.max_agents);
    println!("  Default Mode: {:?}", config.swarm.default_mode);
    println!();
    println!("Memory:");
    println!(
        "  Short-Term Capacity: {}",
        config.memory.short_term_capacity
    );
    println!("  Long-Term DB Path: {:?}", config.memory.long_term_db_path);
    println!();
    println!("Skills:");
    println!(
        "  Skills Directory: {}",
        config
            .skills
            .skills_dir
            .clone()
            .unwrap_or_else(|| "default".to_string())
    );
    println!(
        "  Max Skills Per Query: {}",
        config.skills.max_skills_per_query
    );

    Ok(())
}

/// Run tests
pub async fn run_tests() -> Result<()> {
    println!("Running KOD test suite...");

    // Test 1: Configuration loading
    let config = KodConfig::load_default()?;
    println!("  Config: OK (model={})", config.llm.model);

    // Test 2: Engine lifecycle
    let home = dirs::home_dir()
        .ok_or_else(|| KodError::Config("Could not determine home directory".to_string()))?;
    let db_path = home.join(".kod").join("data").join("test.redb");

    let router_config = RouterConfig::default();
    let engine = KodEngine::new(router_config, db_path)?;
    engine.start().await?;
    assert!(engine.is_running().await);
    engine.shutdown().await?;
    assert!(!engine.is_running().await);
    println!("  Engine lifecycle: OK");

    // Test 3: Provider setup
    let provider = OpenAICompatProvider::from_config(&config.llm, None)?;
    let provider_name = provider.name();
    println!("  Provider setup: OK (name={})", provider_name);

    // Test 4: Skill loading
    let skills_dir = config.skills_dir()?;
    if skills_dir.exists() {
        let loader = kod_skills::SkillLoader::new(&skills_dir);
        let count = loader.count().await;
        println!("  Skill loading: OK ({} skills)", count);
    } else {
        println!("  Skill loading: SKIPPED (no skills directory)");
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
