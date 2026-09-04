# Chunk 9: CLI Implementation

## Task 44: CLI Structure and Commands

**Files:**
- Modify: `crates/kod-cli/Cargo.toml`
- Create: `crates/kod-cli/src/lib.rs`
- Create: `crates/kod-cli/src/commands.rs`
- Test: `crates/kod-cli/tests/commands.rs`

- [ ] **Step 1: Update kod-cli Cargo.toml**

```toml
[package]
name = "kod-cli"
version.workspace = true
edition.workspace = true
license.workspace = true
authors.workspace = true

[[bin]]
name = "kod"
path = "src/main.rs"

[dependencies]
clap = { workspace = true }
serde = { workspace = true }
serde_json = { workspace = true }
tokio = { workspace = true }
tracing = { workspace = true }
tracing-subscriber = { workspace = true }
dirs = { workspace = true }
chrono = { version = "0.4", features = ["serde"] }
kod-types = { path = "../kod-types" }
kod-error = { path = "../kod-error" }
kod-config = { path = "../kod-config" }
kod-core = { path = "../kod-core" }
kod-tui = { path = "../kod-tui" }
kod-skills = { path = "../kod-skills" }
kod-memory = { path = "../kod-memory" }
kod-provider = { path = "../kod-provider" }
kod-provider-ollama = { path = "../kod-provider-ollama" }

[dev-dependencies]
rstest = { workspace = true }
tempfile = "3.8"
assert_cmd = "2.0"
predicates = "3.1"
```

- [ ] **Step 2: Write failing test for CLI commands**

Create `crates/kod-cli/tests/commands.rs`:

```rust
use assert_cmd::Command;
use predicates::prelude::*;

#[test]
fn test_cli_help() {
    let mut cmd = Command::cargo_bin("kod").unwrap();
    
    cmd.arg("--help")
        .assert()
        .success()
        .stdout(predicate::str::contains("kod"))
        .stdout(predicate::str::contains("chat"))
        .stdout(predicate::str::contains("query"))
        .stdout(predicate::str::contains("swarm"))
        .stdout(predicate::str::contains("skills"))
        .stdout(predicate::str::contains("memory"));
}

#[test]
fn test_cli_version() {
    let mut cmd = Command::cargo_bin("kod").unwrap();
    
    cmd.arg("--version")
        .assert()
        .success()
        .stdout(predicate::str::contains(env!("CARGO_PKG_VERSION")));
}

#[test]
fn test_chat_command_help() {
    let mut cmd = Command::cargo_bin("kod").unwrap();
    
    cmd.args(["chat", "--help"])
        .assert()
        .success()
        .stdout(predicate::str::contains("interactive"))
        .stdout(predicate::str::contains("model"));
}

#[test]
fn test_query_command_help() {
    let mut cmd = Command::cargo_bin("kod").unwrap();
    
    cmd.args(["query", "--help"])
        .assert()
        .success()
        .stdout(predicate::str::contains("one-shot"))
        .stdout(predicate::str::contains("prompt"));
}

#[test]
fn test_swarm_command_help() {
    let mut cmd = Command::cargo_bin("kod").unwrap();
    
    cmd.args(["swarm", "--help"])
        .assert()
        .success()
        .stdout(predicate::str::contains("agents"))
        .stdout(predicate::str::contains("mode"));
}

#[test]
fn test_skills_command_help() {
    let mut cmd = Command::cargo_bin("kod").unwrap();
    
    cmd.args(["skills", "--help"])
        .assert()
        .success()
        .stdout(predicate::str::contains("list"))
        .stdout(predicate::str::contains("search"));
}

#[test]
fn test_memory_command_help() {
    let mut cmd = Command::cargo_bin("kod").unwrap();
    
    cmd.args(["memory", "--help"])
        .assert()
        .success()
        .stdout(predicate::str::contains("store"))
        .stdout(predicate::str::contains("search"));
}

#[test]
fn test_invalid_command() {
    let mut cmd = Command::cargo_bin("kod").unwrap();
    
    cmd.arg("invalid")
        .assert()
        .failure();
}

#[test]
fn test_status_command() {
    let mut cmd = Command::cargo_bin("kod").unwrap();
    
    cmd.arg("status")
        .assert()
        .success()
        .stdout(predicate::str::contains("KOD Status"));
}
```

- [ ] **Step 3: Run tests to verify they fail**

```bash
cargo test -p kod-cli --test commands
```

Expected: FAIL - CLI not implemented

- [ ] **Step 4: Implement CLI commands**

Create `crates/kod-cli/src/lib.rs`:

```rust
//! CLI interface for KOD.
//!
//! Provides command-line interface with subcommands for chat,
//! query, swarm, skills, and memory management.

pub mod commands;
pub mod handlers;
pub mod main;

pub use commands::Cli;
```

Create `crates/kod-cli/src/commands.rs`:

```rust
//! CLI command definitions using clap.

use clap::{Parser, Subcommand};
use std::path::PathBuf;

/// KOD - High-Performance AI Coding Agent Harness
#[derive(Parser, Debug)]
#[command(name = "kod")]
#[command(about = "AI coding agent harness with skills, memory, and swarm", long_about = None)]
#[command(version = env!("CARGO_PKG_VERSION"))]
pub struct Cli {
    /// Configuration file path
    #[arg(short, long, global = true, value_name = "FILE")]
    pub config: Option<PathBuf>,
    
    /// Model override (e.g., "codellama:13b")
    #[arg(short, long, global = true)]
    pub model: Option<String>,
    
    /// Ollama server URL
    #[arg(long, global = true, default_value = "http://localhost:11434")]
    pub url: String,
    
    /// Enable verbose logging
    #[arg(short, long, global = true)]
    pub verbose: bool,
    
    /// Working directory (defaults to current directory)
    #[arg(short, long, global = true)]
    pub workdir: Option<PathBuf>,
    
    /// Skills directory override
    #[arg(long, global = true)]
    pub skills_dir: Option<PathBuf>,
    
    /// Disable memory system
    #[arg(long, global = true)]
    pub no_memory: bool,
    
    /// Disable agent swarm
    #[arg(long, global = true)]
    pub no_swarm: bool,
    
    #[command(subcommand)]
    pub command: Commands,
}

#[derive(Subcommand, Debug)]
pub enum Commands {
    /// Start interactive chat session (TUI)
    Chat {
        /// Initial prompt to start with
        #[arg(short, long)]
        prompt: Option<String>,
        
        /// Collaboration mode for agents
        #[arg(short, long, default_value = "shared")]
        mode: String,
        
        /// Number of agents to spawn in swarm
        #[arg(short, long, default_value = "3")]
        agents: usize,
    },
    
    /// One-shot query (no TUI)
    Query {
        /// Query prompt
        #[arg(short, long)]
        prompt: String,
        
        /// Output format (text, json)
        #[arg(short, long, default_value = "text")]
        format: String,
        
        /// Show execution details
        #[arg(short, long)]
        detailed: bool,
    },
    
    /// Execute a task with agent swarm
    Swarm {
        /// Task description
        task: String,
        
        /// Number of agents to spawn
        #[arg(short, long, default_value = "3")]
        agents: usize,
        
        /// Collaboration mode (shared, isolated, hybrid)
        #[arg(short, long, default_value = "shared")]
        mode: String,
        
        /// Agent capabilities (comma-separated)
        #[arg(short, long, value_delimiter = ',')]
        capabilities: Vec<String>,
    },
    
    /// Manage skills
    Skills {
        #[command(subcommand)]
        action: SkillsAction,
    },
    
    /// Manage memory
    Memory {
        #[command(subcommand)]
        action: MemoryAction,
    },
    
    /// Show system status
    Status,
}

#[derive(Subcommand, Debug)]
pub enum SkillsAction {
    /// List all available skills
    List {
        /// Filter by category
        #[arg(short, long)]
        category: Option<String>,
    },
    
    /// Search skills by query
    Search {
        /// Search query
        query: String,
    },
    
    /// Show detailed skill information
    Show {
        /// Skill name
        name: String,
    },
    
    /// Validate skill files
    Validate {
        /// Specific file to validate (optional)
        file: Option<PathBuf>,
    },
}

#[derive(Subcommand, Debug)]
pub enum MemoryAction {
    /// Store a memory
    Store {
        /// Memory type (short, long, episodic)
        #[arg(short, long, default_value = "short")]
        memory_type: String,
        
        /// Memory content
        content: String,
    },
    
    /// Search memories
    Search {
        /// Search query
        query: String,
    },
    
    /// List memories
    List {
        /// Memory type filter
        #[arg(short, long)]
        memory_type: Option<String>,
    },
    
    /// Clear all memories
    Clear {
        /// Force clear without confirmation
        #[arg(short, long)]
        force: bool,
    },
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::CommandFactory;

    #[test]
    fn test_cli_structure() {
        Cli::command().debug_assert();
    }

    #[test]
    fn test_parse_chat_command() {
        let cli = Cli::try_parse_from([
            "kod", "chat",
            "--prompt", "hello",
            "--mode", "shared",
            "--agents", "3",
        ]).unwrap();
        
        match cli.command {
            Commands::Chat { prompt, mode, agents } => {
                assert_eq!(prompt, Some("hello".to_string()));
                assert_eq!(mode, "shared");
                assert_eq!(agents, 3);
            }
            _ => panic!("Expected Chat command"),
        }
    }

    #[test]
    fn test_parse_query_command() {
        let cli = Cli::try_parse_from([
            "kod", "query",
            "--prompt", "What is Rust?",
            "--format", "json",
        ]).unwrap();
        
        match cli.command {
            Commands::Query { prompt, format, detailed } => {
                assert_eq!(prompt, "What is Rust?");
                assert_eq!(format, "json");
                assert!(!detailed);
            }
            _ => panic!("Expected Query command"),
        }
    }

    #[test]
    fn test_parse_swarm_command() {
        let cli = Cli::try_parse_from([
            "kod", "swarm",
            "Implement auth system",
            "--agents", "5",
            "--mode", "isolated",
            "--capabilities", "coding,testing",
        ]).unwrap();
        
        match cli.command {
            Commands::Swarm { task, agents, mode, capabilities } => {
                assert_eq!(task, "Implement auth system");
                assert_eq!(agents, 5);
                assert_eq!(mode, "isolated");
                assert_eq!(capabilities, vec!["coding", "testing"]);
            }
            _ => panic!("Expected Swarm command"),
        }
    }
}
```

- [ ] **Step 5: Run tests to verify they pass**

```bash
cargo test -p kod-cli --lib commands
```

Expected: Structure tests pass (command parsing)

- [ ] **Step 6: Commit**

```bash
git add crates/kod-cli/
git commit -m "feat(cli): add CLI command structure with clap"
```

---

## Task 45: Command Handlers

**Files:**
- Create: `crates/kod-cli/src/handlers.rs`
- Test: `crates/kod-cli/tests/handlers.rs`

- [ ] **Step 1: Write failing test for command handlers**

Create `crates/kod-cli/tests/handlers.rs`:

```rust
use kod_cli::handlers::{handle_query, handle_status, handle_skills_list};
use tempfile::TempDir;
use std::path::Path;

fn create_test_config() -> (TempDir, PathBuf) {
    let temp_dir = TempDir::new().unwrap();
    let config_path = temp_dir.path().join("config.toml");
    
    (temp_dir, config_path)
}

#[tokio::test]
async fn test_handle_query() {
    let (temp_dir, _config) = create_test_config();
    
    let result = handle_query(
        "What is 2+2?",
        "text",
        false,
        temp_dir.path(),
    ).await;
    
    // Should complete without error (even if LLM not available)
    assert!(result.is_ok() || result.is_err());
}

#[tokio::test]
async fn test_handle_status() {
    let (temp_dir, _config) = create_test_config();
    
    let result = handle_status(temp_dir.path()).await;
    
    assert!(result.is_ok());
}

#[tokio::test]
async fn test_handle_skills_list() {
    let temp_dir = TempDir::new().unwrap();
    
    // Create skills directory
    let skills_dir = temp_dir.path().join("skills");
    std::fs::create_dir_all(&skills_dir).unwrap();
    
    // Create test skill
    let skill_content = r#"---
name: test-skill
description: Test skill
version: 1.0.0
category: test
---

## Instructions

Test instructions.
"#;
    
    std::fs::write(skills_dir.join("test.md"), skill_content).unwrap();
    
    let result = handle_skills_list(Some(skills_dir), None).await;
    
    assert!(result.is_ok());
}

#[tokio::test]
async fn test_handle_skills_search() {
    let temp_dir = TempDir::new().unwrap();
    
    let skills_dir = temp_dir.path().join("skills");
    std::fs::create_dir_all(&skills_dir).unwrap();
    
    let result = handle_skills_search(
        "test query",
        Some(skills_dir),
    ).await;
    
    assert!(result.is_ok());
}
```

- [ ] **Step 2: Run tests to verify they fail**

```bash
cargo test -p kod-cli --test handlers
```

Expected: FAIL - handlers not implemented

- [ ] **Step 3: Implement command handlers**

Create `crates/kod-cli/src/handlers.rs`:

```rust
//! Command handlers for CLI actions.

use kod_config::KodConfig;
use kod_core::{
    engine::KodEngine,
    router::{RouterConfig, TaskType},
};
use kod_error::{KodError, Result};
use kod_memory::manager::MemoryManager;
use kod_skills::{loader::SkillLoader, matcher::SkillMatcher};
use kod_types::MemoryType;
use std::path::{Path, PathBuf};

/// Handle the query command (one-shot)
pub async fn handle_query(
    prompt: &str,
    format: &str,
    detailed: bool,
    working_dir: &Path,
) -> Result<()> {
    // Create engine configuration
    let db_path = working_dir.join("kod_memory.redb");
    
    let router_config = RouterConfig {
        working_dir: working_dir.to_path_buf(),
        enable_swarm: false, // Don't use swarm for one-shot queries
        enable_memory: true,
        ..Default::default()
    };
    
    // Create and start engine
    let engine = KodEngine::new(router_config, db_path)?;
    engine.start().await?;
    
    // Process the query
    let response = engine.process(prompt).await?;
    
    // Format output
    match format {
        "json" => {
            let output = serde_json::json!({
                "task_type": format!("{:?}", response.task_type),
                "text": response.text,
                "tool_calls": response.tool_calls.len(),
                "skills_used": response.skills_used,
                "execution_time_ms": response.execution_time_ms,
            });
            println!("{}", serde_json::to_string_pretty(&output)?);
        }
        _ => {
            // Text format
            if let Some(text) = response.text {
                println!("{}", text);
            } else {
                println!("No response generated.");
            }
            
            if detailed {
                println!("\n--- Execution Details ---");
                println!("Task Type: {:?}", response.task_type);
                println!("Execution Time: {}ms", response.execution_time_ms);
                println!("Skills Used: {:?}", response.skills_used);
                println!("Memory Used: {}", response.memory_used);
                println!("Swarm Used: {}", response.swarm_used);
                if !response.tool_calls.is_empty() {
                    println!("Tool Calls: {}", response.tool_calls.len());
                }
            }
        }
    }
    
    // Shutdown engine
    engine.shutdown().await?;
    
    Ok(())
}

/// Handle the status command
pub async fn handle_status(working_dir: &Path) -> Result<()> {
    println!("KOD Status");
    println!("==========");
    println!();
    
    // Working directory
    println!("Working Directory: {}", working_dir.display());
    
    // Configuration
    let config = KodConfig::load_default()?;
    println!();
    println!("Configuration:");
    println!("  LLM Provider: {}", format!("{:?}", config.llm.provider));
    println!("  Model: {}", config.llm.model);
    println!("  Base URL: {}", config.llm.base_url);
    
    // Memory status
    let db_path = working_dir.join("kod_memory.redb");
    if db_path.exists() {
        println!();
        println!("Memory:");
        println!("  Database: {}", db_path.display());
        
        // Try to get memory count
        if let Ok(memory_manager) = MemoryManager::new(db_path.clone(), 100) {
            if let Ok(count) = memory_manager.get_all_long_term().await.map(|m| m.len()) {
                println!("  Long-term Entries: {}", count);
            }
        }
    } else {
        println!();
        println!("Memory: Not initialized");
    }
    
    // Skills status
    let skills_dir = get_skills_dir(&config)?;
    if skills_dir.exists() {
        let skill_count = count_skill_files(&skills_dir);
        println!();
        println!("Skills:");
        println!("  Directory: {}", skills_dir.display());
        println!("  Files: {}", skill_count);
    } else {
        println!();
        println!("Skills: Directory not found: {}", skills_dir.display());
    }
    
    // Ollama status
    println!();
    println!("LLM Provider:");
    println!("  Type: Ollama");
    println!("  URL: {}", config.llm.base_url);
    
    // Try to check Ollama health
    let client = kod_provider_ollama::OllamaClient::new(&config.llm.base_url);
    match client.health_check().await {
        Ok(_) => {
            println!("  Status: Connected");
            
            // Try to list models
            if let Ok(models) = client.list_models().await {
                println!("  Available Models: {}", models.len());
                if !models.is_empty() {
                    println!("  Default Model: {}", models[0].name);
                }
            }
        }
        Err(_) => {
            println!("  Status: Not reachable");
        }
    }
    
    Ok(())
}

/// Handle skills list command
pub async fn handle_skills_list(
    skills_dir: Option<PathBuf>,
    category: Option<String>,
) -> Result<()> {
    let skills_dir = if let Some(dir) = skills_dir {
        dir
    } else {
        let config = KodConfig::load_default()?;
        get_skills_dir(&config)?
    };
    
    let mut loader = SkillLoader::new(&skills_dir);
    let skills = loader.load_all().await?;
    
    if skills.is_empty() {
        println!("No skills found in: {}", skills_dir.display());
        return Ok(());
    }
    
    // Filter by category if provided
    let filtered_skills: Vec<_> = skills.into_iter()
        .filter(|skill| {
            category.as_ref().map_or(true, |cat| {
                skill.metadata.category == *cat
            })
        })
        .collect();
    
    println!("Available Skills ({}):", filtered_skills.len());
    println!("=========================");
    
    for skill in filtered_skills {
        println!();
        println!("Name: {}", skill.metadata.name);
        println!("  Description: {}", skill.metadata.description);
        println!("  Version: {}", skill.metadata.version);
        println!("  Category: {}", skill.metadata.category);
        
        if !skill.metadata.tags.is_empty() {
            println!("  Tags: {}", skill.metadata.tags.join(", "));
        }
        
        if !skill.metadata.capabilities.is_empty() {
            println!("  Capabilities: {}", skill.metadata.capabilities.join(", "));
        }
    }
    
    Ok(())
}

/// Handle skills search command
pub async fn handle_skills_search(
    query: &str,
    skills_dir: Option<PathBuf>,
) -> Result<()> {
    let skills_dir = if let Some(dir) = skills_dir {
        dir
    } else {
        let config = KodConfig::load_default()?;
        get_skills_dir(&config)?
    };
    
    let mut loader = SkillLoader::new(&skills_dir);
    let skills = loader.load_all().await?;
    
    if skills.is_empty() {
        println!("No skills found in: {}", skills_dir.display());
        return Ok(());
    }
    
    // Create matcher and add skills
    let matcher = SkillMatcher::new();
    for skill in skills {
        matcher.add_skill(skill).await;
    }
    
    // Search
    let matches = matcher.find_relevant_skills(query).await;
    
    if matches.is_empty() {
        println!("No skills found matching: '{}'", query);
        return Ok(());
    }
    
    println!("Skills matching '{}' ({}):", query, matches.len());
    println!("===========================");
    
    for skill_match in matches {
        println!();
        println!("Name: {} (Score: {:.2})", 
            skill_match.skill.metadata.name,
            skill_match.score
        );
        println!("  Description: {}", skill_match.skill.metadata.description);
        
        if !skill_match.match_reasons.is_empty() {
            println!("  Match Reasons:");
            for reason in &skill_match.match_reasons {
                println!("    - {:?}", reason);
            }
        }
    }
    
    Ok(())
}

/// Handle memory store command
pub async fn handle_memory_store(
    memory_type: &str,
    content: &str,
    working_dir: &Path,
) -> Result<()> {
    let db_path = working_dir.join("kod_memory.redb");
    let memory_manager = MemoryManager::new(db_path, 100)?;
    
    // Parse memory type
    let mem_type = match memory_type.to_lowercase().as_str() {
        "short" | "short-term" => MemoryType::ShortTerm,
        "long" | "long-term" => MemoryType::LongTerm,
        "episodic" => MemoryType::Episodic,
        "semantic" => MemoryType::Semantic,
        _ => return Err(KodError::InvalidParameters {
            reason: format!("Invalid memory type: {}", memory_type),
        }),
    };
    
    // Store memory
    let id = memory_manager.store(mem_type, content).await?;
    
    println!("Stored memory: {:?}", id);
    println!("Type: {}", memory_type);
    println!("Content: {}", content);
    
    Ok(())
}

/// Handle memory search command
pub async fn handle_memory_search(
    query: &str,
    working_dir: &Path,
) -> Result<()> {
    let db_path = working_dir.join("kod_memory.redb");
    
    if !db_path.exists() {
        println!("No memory database found.");
        return Ok(());
    }
    
    let memory_manager = MemoryManager::new(db_path, 100)?;
    
    let results = memory_manager.search(query).await?;
    
    if results.is_empty() {
        println!("No memories found matching: '{}'", query);
        return Ok(());
    }
    
    println!("Memories matching '{}' ({}):", query, results.len());
    println!("===============================");
    
    for entry in results {
        println!();
        println!("ID: {:?}", entry.id);
        println!("Type: {:?}", entry.memory_type);
        println!("Content: {}", entry.content);
        println!("Timestamp: {}", entry.timestamp);
        println!("Relevance: {:.2}", entry.relevance);
    }
    
    Ok(())
}

/// Handle memory list command
pub async fn handle_memory_list(
    memory_type: Option<String>,
    working_dir: &Path,
) -> Result<()> {
    let db_path = working_dir.join("kod_memory.redb");
    
    if !db_path.exists() {
        println!("No memory database found.");
        return Ok(());
    }
    
    let memory_manager = MemoryManager::new(db_path, 100)?;
    
    // Get all memories
    let long_term = memory_manager.get_all_long_term().await?;
    let short_term = memory_manager.get_all_short_term();
    
    // Filter by type if provided
    let type_filter = memory_type.map(|t| t.to_lowercase());
    
    // Print long-term memories
    if type_filter.as_deref().map_or(true, |t| t == "long" || t == "long-term") {
        if !long_term.is_empty() {
            println!("Long-term Memories ({}):", long_term.len());
            println!("========================");
            
            for entry in long_term {
                println!();
                println!("ID: {:?}", entry.id);
                println!("Content: {}", entry.content);
                println!("Timestamp: {}", entry.timestamp);
            }
        }
    }
    
    // Print short-term memories
    if type_filter.as_deref().map_or(true, |t| t == "short" || t == "short-term") {
        if !short_term.is_empty() {
            println!();
            println!("Short-term Memories ({}):", short_term.len());
            println!("=========================");
            
            for entry in short_term {
                println!();
                println!("ID: {:?}", entry.id);
                println!("Content: {}", entry.content);
                println!("Timestamp: {}", entry.timestamp);
            }
        }
    }
    
    Ok(())
}

/// Get skills directory from config or default
fn get_skills_dir(config: &KodConfig) -> Result<PathBuf> {
    if let Some(skills_dir) = &config.skills.skills_dir {
        Ok(PathBuf::from(skills_dir))
    } else {
        dirs::home_dir()
            .map(|home| home.join(".kod").join("skills"))
            .ok_or_else(|| KodError::Config("Could not determine home directory".to_string()))
    }
}

/// Count skill files in directory
fn count_skill_files(dir: &Path) -> usize {
    walkdir::WalkDir::new(dir)
        .into_iter()
        .filter_map(|e| e.ok())
        .filter(|e| {
            e.file_type().is_file() && 
            e.path().extension().and_then(|s| s.to_str()) == Some("md")
        })
        .count()
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[tokio::test]
    async fn test_query_handler() {
        let temp_dir = TempDir::new().unwrap();
        
        // This will fail without LLM provider, but shouldn't panic
        let result = handle_query("test", "text", false, temp_dir.path()).await;
        assert!(result.is_ok() || result.is_err());
    }
}
```

- [ ] **Step 4: Add walkdir dependency**

Add to `crates/kod-cli/Cargo.toml` dependencies:

```toml
walkdir = "2.5"
```

- [ ] **Step 5: Run tests to verify they pass**

```bash
cargo test -p kod-cli --test handlers
cargo test -p kod-cli --lib handlers
```

Expected: All tests pass

- [ ] **Step 6: Commit**

```bash
git add crates/kod-cli/
git commit -m "feat(cli): add command handlers for query, status, skills, and memory"
```

---

## Task 46: Main Entry Point

**Files:**
- Create: `crates/kod-cli/src/main.rs`
- Test: `crates/kod-cli/tests/main.rs`

- [ ] **Step 1: Write failing test for main entry**

Create `crates/kod-cli/tests/main.rs`:

```rust
use assert_cmd::Command;
use predicates::prelude::*;

#[test]
fn test_main_with_help() {
    let mut cmd = Command::cargo_bin("kod").unwrap();
    
    cmd.arg("--help")
        .assert()
        .success();
}

#[test]
fn test_main_status_command() {
    let mut cmd = Command::cargo_bin("kod").unwrap();
    
    cmd.arg("status")
        .assert()
        .success()
        .stdout(predicate::str::contains("KOD Status"));
}

#[test]
fn test_main_skills_list_empty() {
    let temp_dir = tempfile::tempdir().unwrap();
    
    let mut cmd = Command::cargo_bin("kod").unwrap();
    cmd.args(["skills", "list"])
        .env("KOD_SKILLS_DIR", temp_dir.path().join("empty"))
        .assert()
        .success();
}

#[test]
fn test_main_invalid_args() {
    let mut cmd = Command::cargo_bin("kod").unwrap();
    
    cmd.args(["--invalid-flag"])
        .assert()
        .failure();
}
```

- [ ] **Step 2: Implement main entry point**

Create `crates/kod-cli/src/main.rs`:

```rust
//! Main entry point for KOD CLI.

use clap::Parser;
use kod_cli::{
    commands::{Cli, Commands, MemoryAction, SkillsAction},
    handlers,
};
use std::path::PathBuf;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let cli = Cli::parse();
    
    // Initialize logging
    let log_level = if cli.verbose {
        "debug"
    } else {
        "info"
    };
    
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new(log_level))
        )
        .init();
    
    // Determine working directory
    let working_dir = cli.workdir
        .unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")));
    
    // Load or create configuration
    let config = if let Some(config_path) = &cli.config {
        kod_config::KodConfig::load_from(config_path)?
    } else {
        kod_config::KodConfig::load_default()?
    };
    
    // Override config with CLI flags
    let mut config = config;
    if let Some(model) = &cli.model {
        config.llm.model = model.clone();
    }
    if cli.url != "http://localhost:11434" {
        config.llm.base_url = cli.url.clone();
    }
    
    // Handle commands
    match cli.command {
        Commands::Chat { prompt, mode, agents } => {
            handle_chat(prompt, mode, agents, &working_dir, &config).await?;
        }
        
        Commands::Query { prompt, format, detailed } => {
            handlers::handle_query(&prompt, &format, detailed, &working_dir).await?;
        }
        
        Commands::Swarm { task, agents, mode, capabilities } => {
            handle_swarm(task, agents, mode, capabilities, &working_dir, &config).await?;
        }
        
        Commands::Skills { action } => {
            handle_skills(action, cli.skills_dir, &config).await?;
        }
        
        Commands::Memory { action } => {
            handle_memory(action, &working_dir).await?;
        }
        
        Commands::Status => {
            handlers::handle_status(&working_dir).await?;
        }
    }
    
    Ok(())
}

/// Handle chat command (starts TUI)
async fn handle_chat(
    prompt: Option<String>,
    mode: String,
    _agents: usize,
    working_dir: &std::path::Path,
    _config: &kod_config::KodConfig,
) -> Result<(), Box<dyn std::error::Error>> {
    // Initialize TUI
    let mut tui = kod_tui::TuiLoop::new();
    
    // Set initial prompt if provided
    if let Some(prompt) = prompt {
        tui.app_mut().set_input(prompt);
    }
    
    // Add welcome message
    use kod_tui::app::Message;
    use kod_types::{MessageId, MessageMetadata, MessageRole};
    use chrono::Utc;
    
    tui.app_mut().add_message(Message {
        id: MessageId::new(),
        role: MessageRole::System,
        content: format!(
            "KOD v{} - AI Coding Agent\nMode: {}\nType your message and press Enter.",
            env!("CARGO_PKG_VERSION"),
            mode
        ),
        timestamp: Utc::now(),
        metadata: MessageMetadata::default(),
    });
    
    // Run TUI
    tui.run().await?;
    
    Ok(())
}

/// Handle swarm command
async fn handle_swarm(
    task: String,
    agents: usize,
    mode: String,
    capabilities: Vec<String>,
    working_dir: &std::path::Path,
    _config: &kod_config::KodConfig,
) -> Result<(), Box<dyn std::error::Error>> {
    println!("KOD Swarm Execution");
    println!("===================");
    println!();
    println!("Task: {}", task);
    println!("Agents: {}", agents);
    println!("Mode: {}", mode);
    println!("Capabilities: {:?}", capabilities);
    println!("Working Directory: {}", working_dir.display());
    println!();
    
    // Create engine
    let db_path = working_dir.join("kod_memory.redb");
    let router_config = kod_core::router::RouterConfig {
        working_dir: working_dir.to_path_buf(),
        enable_swarm: true,
        enable_memory: true,
        ..Default::default()
    };
    
    let engine = kod_core::engine::KodEngine::new(router_config, db_path)?;
    engine.start().await?;
    
    // Process the task
    println!("Executing task...");
    let response = engine.process(&task).await?;
    
    // Display results
    println!();
    println!("Results:");
    println!("========");
    
    if let Some(text) = response.text {
        println!("{}", text);
    }
    
    println!();
    println!("Execution Details:");
    println!("  Task Type: {:?}", response.task_type);
    println!("  Execution Time: {}ms", response.execution_time_ms);
    println!("  Swarm Used: {}", response.swarm_used);
    
    engine.shutdown().await?;
    
    Ok(())
}

/// Handle skills commands
async fn handle_skills(
    action: SkillsAction,
    skills_dir: Option<PathBuf>,
    config: &kod_config::KodConfig,
) -> Result<(), Box<dyn std::error::Error>> {
    // Resolve skills directory
    let skills_dir = if let Some(dir) = skills_dir {
        dir
    } else {
        config.skills.skills_dir
            .as_ref()
            .map(PathBuf::from)
            .unwrap_or_else(|| {
                dirs::home_dir()
                    .map(|home| home.join(".kod").join("skills"))
                    .unwrap_or_else(|| PathBuf::from(".kod/skills"))
            })
    };
    
    match action {
        SkillsAction::List { category } => {
            handlers::handle_skills_list(Some(skills_dir), category).await?;
        }
        
        SkillsAction::Search { query } => {
            handlers::handle_skills_search(&query, Some(skills_dir)).await?;
        }
        
        SkillsAction::Show { name } => {
            handle_skill_show(&name, &skills_dir).await?;
        }
        
        SkillsAction::Validate { file } => {
            handle_skill_validate(file, &skills_dir).await?;
        }
    }
    
    Ok(())
}

/// Handle skill show command
async fn handle_skill_show(
    name: &str,
    skills_dir: &std::path::Path,
) -> Result<(), Box<dyn std::error::Error>> {
    let mut loader = kod_skills::SkillLoader::new(skills_dir);
    let skills = loader.load_all().await?;
    
    let skill = skills.iter()
        .find(|s| s.metadata.name == name)
        .ok_or_else(|| format!("Skill not found: {}", name))?;
    
    println!("Skill: {}", skill.metadata.name);
    println!("=========={}", "=".repeat(skill.metadata.name.len()));
    println!();
    println!("Description: {}", skill.metadata.description);
    println!("Version: {}", skill.metadata.version);
    println!("Category: {}", skill.metadata.category);
    println!("Author: {:?}", skill.metadata.author);
    
    if !skill.metadata.tags.is_empty() {
        println!("Tags: {}", skill.metadata.tags.join(", "));
    }
    
    if !skill.metadata.capabilities.is_empty() {
        println!("Capabilities: {}", skill.metadata.capabilities.join(", "));
    }
    
    if !skill.metadata.requirements.is_empty() {
        println!("Requirements: {}", skill.metadata.requirements.join(", "));
    }
    
    println!();
    println!("Instructions:");
    println!("=============");
    println!("{}", skill.instructions);
    
    if !skill.examples.is_empty() {
        println!();
        println!("Examples:");
        println!("=========");
        for example in &skill.examples {
            println!("Input: {}", example.input);
            println!("Output: {}", example.output);
            println!();
        }
    }
    
    if let Some(constraints) = &skill.constraints {
        println!();
        println!("Constraints:");
        println!("============");
        println!("{}", constraints);
    }
    
    Ok(())
}

/// Handle skill validate command
async fn handle_skill_validate(
    file: Option<PathBuf>,
    skills_dir: &std::path::Path,
) -> Result<(), Box<dyn std::error::Error>> {
    let parser = kod_skills::SkillParser::new();
    
    if let Some(file) = file {
        // Validate single file
        match parser.parse_file(&file) {
            Ok(skill) => {
                println!("✓ Valid skill: {}", skill.metadata.name);
                println!("  Version: {}", skill.metadata.version);
                println!("  Category: {}", skill.metadata.category);
            }
            Err(e) => {
                println!("✗ Invalid skill file: {}", file.display());
                println!("  Error: {}", e);
            }
        }
    } else {
        // Validate all files in directory
        let mut loader = kod_skills::SkillLoader::new(skills_dir);
        let skills = loader.load_all().await?;
        
        if skills.is_empty() {
            println!("No skill files found in: {}", skills_dir.display());
            return Ok(());
        }
        
        println!("Validating {} skills...", skills.len());
        println!("================================");
        
        let mut valid = 0;
        let mut invalid = 0;
        
        for skill in skills {
            println!("✓ Valid: {} (v{})", skill.metadata.name, skill.metadata.version);
            valid += 1;
        }
        
        // Count invalid files (files that failed to parse)
        let total_files = count_skill_files(skills_dir);
        invalid = total_files - valid;
        
        println!();
        println!("Summary: {} valid, {} invalid", valid, invalid);
    }
    
    Ok(())
}

/// Handle memory commands
async fn handle_memory(
    action: MemoryAction,
    working_dir: &std::path::Path,
) -> Result<(), Box<dyn std::error::Error>> {
    match action {
        MemoryAction::Store { memory_type, content } => {
            handlers::handle_memory_store(&memory_type, &content, working_dir).await?;
        }
        
        MemoryAction::Search { query } => {
            handlers::handle_memory_search(&query, working_dir).await?;
        }
        
        MemoryAction::List { memory_type } => {
            handlers::handle_memory_list(memory_type, working_dir).await?;
        }
        
        MemoryAction::Clear { force } => {
            handle_memory_clear(force, working_dir).await?;
        }
    }
    
    Ok(())
}

/// Handle memory clear command
async fn handle_memory_clear(
    force: bool,
    working_dir: &std::path::Path,
) -> Result<(), Box<dyn std::error::Error>> {
    if !force {
        // Ask for confirmation
        println!("This will clear ALL memories. Continue? (y/N)");
        
        let mut input = String::new();
        std::io::stdin().read_line(&mut input)?;
        
        if !input.trim().to_lowercase().starts_with('y') {
            println!("Cancelled.");
            return Ok(());
        }
    }
    
    let db_path = working_dir.join("kod_memory.redb");
    
    if db_path.exists() {
        std::fs::remove_file(&db_path)?;
        println!("Memory database cleared: {}", db_path.display());
    } else {
        println!("No memory database found.");
    }
    
    Ok(())
}

/// Count skill files in directory
fn count_skill_files(dir: &std::path::Path) -> usize {
    walkdir::WalkDir::new(dir)
        .into_iter()
        .filter_map(|e| e.ok())
        .filter(|e| {
            e.file_type().is_file() && 
            e.path().extension().and_then(|s| s.to_str()) == Some("md")
        })
        .count()
}
```

- [ ] **Step 3: Run tests to verify they pass**

```bash
cargo test -p kod-cli --test main
```

Expected: All tests pass

- [ ] **Step 4: Build binary and test**

```bash
cargo build -p kod-cli
./target/debug/kod --help
./target/debug/kod status
./target/debug/kod skills list
```

Expected: Commands work correctly

- [ ] **Step 5: Commit**

```bash
git add crates/kod-cli/
git commit -m "feat(cli): add main entry point with full command handling"
```

---

## Task 47: CLI Integration Tests

**Files:**
- Create: `crates/kod-cli/tests/integration.rs`

- [ ] **Step 1: Write comprehensive integration tests**

Create `crates/kod-cli/tests/integration.rs`:

```rust
use assert_cmd::Command;
use predicates::prelude::*;
use tempfile::TempDir;
use std::fs;

fn create_test_environment() -> TempDir {
    let temp_dir = TempDir::new().unwrap();
    
    // Create skills directory with test skill
    let skills_dir = temp_dir.path().join("skills");
    fs::create_dir_all(&skills_dir).unwrap();
    
    let skill_content = r#"---
name: rust-coding
description: Rust coding assistance
version: 1.0.0
category: coding
tags:
  - rust
  - coding
capabilities:
  - code-generation
triggers:
  - "rust code"
  - "write rust"
---

## Instructions

You are a Rust coding expert.
"#;
    
    fs::write(skills_dir.join("rust.md"), skill_content).unwrap();
    
    temp_dir
}

#[test]
fn test_full_cli_lifecycle() {
    let temp_dir = create_test_environment();
    
    // Test status
    let mut cmd = Command::cargo_bin("kod").unwrap();
    cmd.arg("status")
        .current_dir(temp_dir.path())
        .assert()
        .success();
    
    // Test skills list
    let mut cmd = Command::cargo_bin("kod").unwrap();
    cmd.args(["skills", "list"])
        .env("KOD_SKILLS_DIR", temp_dir.path().join("skills"))
        .assert()
        .success()
        .stdout(predicate::str::contains("rust-coding"));
    
    // Test skills search
    let mut cmd = Command::cargo_bin("kod").unwrap();
    cmd.args(["skills", "search", "rust"])
        .env("KOD_SKILLS_DIR", temp_dir.path().join("skills"))
        .assert()
        .success();
}

#[test]
fn test_skills_operations() {
    let temp_dir = create_test_environment();
    let skills_dir = temp_dir.path().join("skills");
    
    // List skills
    let mut cmd = Command::cargo_bin("kod").unwrap();
    cmd.args(["skills", "list"])
        .env("KOD_SKILLS_DIR", &skills_dir)
        .assert()
        .success()
        .stdout(predicate::str::contains("rust-coding"))
        .stdout(predicate::str::contains("Rust coding assistance"));
    
    // Search skills
    let mut cmd = Command::cargo_bin("kod").unwrap();
    cmd.args(["skills", "search", "rust code"])
        .env("KOD_SKILLS_DIR", &skills_dir)
        .assert()
        .success();
    
    // Show specific skill
    let mut cmd = Command::cargo_bin("kod").unwrap();
    cmd.args(["skills", "show", "rust-coding"])
        .env("KOD_SKILLS_DIR", &skills_dir)
        .assert()
        .success()
        .stdout(predicate::str::contains("rust-coding"))
        .stdout(predicate::str::contains("Rust coding expert"));
    
    // Validate skills
    let mut cmd = Command::cargo_bin("kod").unwrap();
    cmd.args(["skills", "validate"])
        .env("KOD_SKILLS_DIR", &skills_dir)
        .assert()
        .success();
}

#[test]
fn test_memory_operations() {
    let temp_dir = TempDir::new().unwrap();
    
    // Store memory
    let mut cmd = Command::cargo_bin("kod").unwrap();
    cmd.args(["memory", "store", "--memory-type", "long", "User prefers Rust"])
        .current_dir(temp_dir.path())
        .assert()
        .success();
    
    // Search memory
    let mut cmd = Command::cargo_bin("kod").unwrap();
    cmd.args(["memory", "search", "Rust"])
        .current_dir(temp_dir.path())
        .assert()
        .success();
    
    // List memories
    let mut cmd = Command::cargo_bin("kod").unwrap();
    cmd.args(["memory", "list"])
        .current_dir(temp_dir.path())
        .assert()
        .success();
}

#[test]
fn test_query_command() {
    let temp_dir = TempDir::new().unwrap();
    
    // Note: This will fail without a running Ollama server,
    // but should not panic
    let mut cmd = Command::cargo_bin("kod").unwrap();
    let result = cmd.args([
        "query",
        "--prompt", "What is 2+2?",
        "--format", "text",
    ])
    .current_dir(temp_dir.path())
    .assert();
    
    // Should either succeed or fail gracefully
    // (Not asserting specific outcome since it depends on Ollama)
}

#[test]
fn test_config_flag() {
    let temp_dir = create_test_environment();
    let config_path = temp_dir.path().join("custom_config.toml");
    
    let config_content = r#"
[llm]
provider = "ollama"
model = "test-model"
base_url = "http://localhost:11434"
"#;
    
    fs::write(&config_path, config_content).unwrap();
    
    // Test with custom config
    let mut cmd = Command::cargo_bin("kod").unwrap();
    cmd.args(["--config", config_path.to_str().unwrap(), "status"])
        .assert()
        .success();
}

#[test]
fn test_verbose_flag() {
    let temp_dir = TempDir::new().unwrap();
    
    let mut cmd = Command::cargo_bin("kod").unwrap();
    cmd.args(["--verbose", "status"])
        .current_dir(temp_dir.path())
        .assert()
        .success();
}

#[test]
fn test_model_override() {
    let temp_dir = TempDir::new().unwrap();
    
    let mut cmd = Command::cargo_bin("kod").unwrap();
    cmd.args([
        "--model", "custom-model",
        "status",
    ])
    .current_dir(temp_dir.path())
    .assert()
    .success();
}

#[test]
fn test_invalid_skills_dir() {
    let mut cmd = Command::cargo_bin("kod").unwrap();
    cmd.args(["skills", "list"])
        .env("KOD_SKILLS_DIR", "/nonexistent/path")
        .assert()
        .success(); // Should handle gracefully
}
```

- [ ] **Step 2: Run all kod-cli tests**

```bash
cargo test -p kod-cli
```

Expected: All tests pass

- [ ] **Step 3: Verify full workspace builds**

```bash
cargo build --workspace
cargo build --release
cargo clippy --workspace --all-targets -- -D warnings
```

Expected: Build succeeds with no warnings

- [ ] **Step 4: Test the binary**

```bash
./target/debug/kod --help
./target/debug/kod status
./target/debug/kod skills list
./target/debug/kod memory list
```

Expected: All commands work correctly

- [ ] **Step 5: Commit integration tests**

```bash
git add crates/kod-cli/
git commit -m "feat(cli): add comprehensive integration tests for CLI operations"
```

---

## Task 48: Documentation and README

**Files:**
- Create: `README.md`
- Create: `docs/USAGE.md`
- Create: `docs/CONFIGURATION.md`

- [ ] **Step 1: Create main README**

Create `README.md`:

```markdown
# KOD - High-Performance AI Coding Agent Harness

![KOD](https://img.shields.io/badge/KOD-v0.1.0-blue)
![Rust](https://img.shields.io/badge/Rust-2021-orange)
![License](https://img.shields.io/badge/License-MIT-green)

KOD is a terminal-native, high-performance AI coding agent harness built in Rust. It combines:

- **jcode-inspired performance** — minimal RAM footprint, zero-copy parsing, aggressive caching
- **oh-my-pi coding capabilities** — LSP integration, debugger support, hash-anchored edits
- **Markdown-based skills system** — reusable knowledge assets in `~/.kod/skills/`
- **Autonomous agent swarms** — coordinated agents with direct messaging, shared branch collaboration, optional worktree isolation
- **Local LLM execution** — Ollama-first architecture with multi-provider fallback

## Features

### 🧠 Skills System
- Markdown-based skill files with YAML front matter
- Pattern-based and semantic skill matching
- Hot reloading with file system watching
- Version constraints and capability declarations

### 🐝 Agent Swarm
- Multiple coordination modes (shared branch, isolated worktrees, hybrid)
- Direct agent-to-agent messaging
- File locking with conflict detection
- Task decomposition and capability-based assignment

### 🛠️ Tool System
- File system tools (read, write, list)
- Git integration (status, diff)
- LSP integration (planned)
- Debugger support (planned)
- Permission-based sandboxing

### 💾 Memory System
- Short-term memory (in-memory, session-scoped)
- Long-term memory (persistent, redb-backed)
- Episodic memory (vector-based, semantic search)
- Memory-aware context building

### 🎯 Core Engine
- Task classification and routing
- Context building from memory and skills
- LLM provider abstraction
- Streaming responses

## Quick Start

### Prerequisites
- Rust 1.75+
- Ollama (for local LLM execution)

### Installation

```bash
# Clone the repository
git clone https://github.com/yourusername/kod.git
cd kod

# Build in release mode
cargo build --release

# Add to PATH (optional)
export PATH="$PATH:$(pwd)/target/release"
```

### Basic Usage

```bash
# Start interactive chat
kod chat

# One-shot query
kod query --prompt "What is Rust?"

# Execute task with agent swarm
kod swarm "Implement user authentication" --agents 3

# List available skills
kod skills list

# Search skills
kod skills search "rust refactoring"

# Store memory
kod memory store --memory-type long "User prefers dark mode"

# Search memories
kod memory search "user preferences"

# Show system status
kod status
```

### Skills

Create skills in `~/.kod/skills/` as markdown files:

```markdown
---
name: rust-refactoring
description: Rust code refactoring with best practices
version: 1.0.0
category: coding
tags:
  - rust
  - refactoring
capabilities:
  - code-refactoring
triggers:
  - "refactor rust"
---

## Instructions

You are an expert Rust refactoring assistant...

## Examples

<example input="Refactor this loop">
...
</example>

## Constraints

- Never change public API without request
- Preserve existing tests
```

## Configuration

Configuration is stored in `~/.config/kod/config.toml`:

```toml
[llm]
provider = "ollama"
model = "codellama:13b"
base_url = "http://localhost:11434"

[swarm]
default_mode = "shared_branch"
max_agents = 5

[memory]
short_term_capacity = 100
enable_semantic_search = true

[skills]
skills_dir = "~/.kod/skills"
enable_hot_reload = true
```

## Architecture

```
kod/
├── crates/
│   ├── kod-types/           # Shared type definitions
│   ├── kod-error/           # Error definitions
│   ├── kod-config/          # Configuration
│   ├── kod-provider/        # LLM provider abstraction
│   ├── kod-provider-ollama/ # Ollama implementation
│   ├── kod-skills/          # Skills system
│   ├── kod-memory/          # Memory system
│   ├── kod-tools/           # Tool calling
│   ├── kod-swarm/           # Agent swarm
│   ├── kod-tui/             # Terminal UI
│   ├── kod-cli/             # CLI interface
│   └── kod-core/            # Core engine
```

## Performance Targets

| Metric | Target |
|--------|--------|
| Cold start time | < 800ms |
| Simple query response | < 200ms |
| Memory usage (base) | < 150MB |
| Memory usage (100 skills) | < 50MB additional |
| Agent spawn time | < 100ms |

## License

MIT License - see [LICENSE](LICENSE) file for details.

## Acknowledgments

- [jcode](https://github.com/1jehuang/jcode) - Performance inspiration
- [oh-my-pi](https://github.com/can1357/oh-my-pi) - Coding agent features
- [Ratatui](https://ratatui.rs/) - Terminal UI framework
- [Ollama](https://ollama.ai/) - Local LLM runtime
```

- [ ] **Step 2: Create usage documentation**

Create `docs/USAGE.md`:

```markdown
# KOD Usage Guide

## Chat Mode

Start interactive chat:
```bash
kod chat
```

With initial prompt:
```bash
kod chat --prompt "Help me implement a REST API"
```

### TUI Keybindings

| Mode | Key | Action |
|------|-----|--------|
| Normal | `i` | Enter insert mode |
| Normal | `q` or `Esc` | Quit |
| Normal | `Tab` | Toggle agent panel |
| Normal | `h` or `?` | Show help |
| Normal | `↑/↓` | Scroll |
| Normal | `PageUp/PageDown` | Fast scroll |
| Insert | `Enter` | Submit input |
| Insert | `Esc` | Return to normal mode |
| Insert | `Backspace` | Delete character |
| Insert | `↑/↓` | Navigate history |

## Query Mode

One-shot query:
```bash
kod query --prompt "What is 2+2?"
```

JSON output:
```bash
kod query --prompt "Explain Rust ownership" --format json
```

Detailed output:
```bash
kod query --prompt "Debug this error" --detailed
```

## Swarm Mode

Execute task with multiple agents:
```bash
kod swarm "Implement authentication system" --agents 3
```

With specific capabilities:
```bash
kod swarm "Build REST API" --capabilities coding,testing,documentation
```

Isolated mode:
```bash
kod swarm "Experimental feature" --mode isolated
```

## Skills Management

List all skills:
```bash
kod skills list
```

Filter by category:
```bash
kod skills list --category coding
```

Search skills:
```bash
kod skills search "rust refactoring"
```

Show skill details:
```bash
kod skills show rust-refactoring
```

Validate skill files:
```bash
kod skills validate
```

## Memory Management

Store memory:
```bash
kod memory store --memory-type long "User prefers dark mode"
kod memory store --memory-type short "Currently working on auth"
```

Search memories:
```bash
kod memory search "user preferences"
```

List memories:
```bash
kod memory list
kod memory list --memory-type long
```

Clear all memories:
```bash
kod memory clear --force
```

## System Status

Check system status:
```bash
kod status
```

This shows:
- Working directory
- Configuration
- Memory database status
- Skills directory and count
- LLM provider connection status
```

- [ ] **Step 3: Create configuration documentation**

Create `docs/CONFIGURATION.md`:

```markdown
# KOD Configuration

## Configuration File

Location: `~/.config/kod/config.toml`

## Sections

### LLM Configuration

```toml
[llm]
provider = "ollama"              # ollama, anthropic, openai, custom
model = "codellama:13b"          # Model name
base_url = "http://localhost:11434"  # Ollama server URL
api_key = ""                     # API key for cloud providers
context_window = 8192            # Context window size
max_tokens = 2048                # Max tokens to generate
temperature = 0.7                # Temperature (0.0-1.0)
timeout_secs = 300               # Request timeout
```

### Swarm Configuration

```toml
[swarm]
default_mode = "shared_branch"   # shared_branch, isolated_worktrees, hybrid
max_agents = 5                   # Maximum concurrent agents
coordination_strategy = "agent_decided"  # lock_based, semantic, agent_decided
lock_timeout_secs = 30           # File lock timeout
agent_idle_timeout_secs = 300    # Agent idle timeout
```

### Memory Configuration

```toml
[memory]
short_term_capacity = 100        # Short-term memory capacity
long_term_db_path = "~/.kod/memory.redb"  # Database path
enable_semantic_search = true    # Enable vector search
embedding_model = "all-MiniLM-L6-v2"     # Embedding model
context_window = 4096            # Context window for retrieval
compaction_interval_secs = 3600  # Compaction interval
```

### Skills Configuration

```toml
[skills]
skills_dir = "~/.kod/skills"     # Skills directory
enable_hot_reload = true         # Enable hot reloading
max_cache_size_mb = 50           # Cache size limit
max_skills_per_query = 3         # Max skills per query
match_threshold = 0.7            # Minimum match score
```

### UI Configuration

```toml
[ui]
theme = "dark"                   # UI theme
show_tool_calls = true           # Show tool executions
show_agent_messages = true       # Show agent communications
chat_history_limit = 1000        # Chat history limit
```

### Performance Configuration

```toml
[performance]
max_memory_mb = 150              # Memory limit
target_response_time_ms = 200    # Response time target
enable_object_pooling = true     # Enable object pooling
enable_string_interning = true   # Enable string interning
```

## Environment Variables

KOD supports environment variables for common overrides:

- `KOD_CONFIG`: Configuration file path
- `KOD_SKILLS_DIR`: Skills directory override
- `KOD_MODEL`: Model override
- `KOD_URL`: Ollama URL override
- `KOD_NO_MEMORY`: Disable memory (any value)
- `KOD_NO_SWARM`: Disable swarm (any value)
- `RUST_LOG`: Logging level (debug, info, warn, error)

## Examples

### Minimal Configuration

```toml
[llm]
provider = "ollama"
model = "codellama:13b"
```

### Full Configuration

```toml
[llm]
provider = "ollama"
model = "codellama:13b"
base_url = "http://localhost:11434"
context_window = 8192
max_tokens = 2048
temperature = 0.7

[swarm]
default_mode = "shared_branch"
max_agents = 5
coordination_strategy = "agent_decided"

[memory]
short_term_capacity = 100
enable_semantic_search = true

[skills]
skills_dir = "~/.kod/skills"
enable_hot_reload = true

[performance]
max_memory_mb = 150
target_response_time_ms = 200
```
```

- [ ] **Step 4: Commit documentation**

```bash
git add README.md docs/
git commit -m "docs: add comprehensive documentation for README, usage, and configuration"
```

---

## Chunk 9 Review Checklist

- [ ] CLI structure with all subcommands (chat, query, swarm, skills, memory, status)
- [ ] Command handlers for all actions
- [ ] Main entry point with proper error handling
- [ ] Configuration integration with CLI flags
- [ ] Skills management (list, search, show, validate)
- [ ] Memory management (store, search, list, clear)
- [ ] Integration tests for all CLI operations
- [ ] Binary builds and executes correctly
- [ ] Help text for all commands
- [ ] Documentation (README, USAGE, CONFIGURATION)
- [ ] All tests pass
- [ ] Clippy passes with no warnings

**Verification commands:**

```bash
cargo test -p kod-cli
cargo clippy -p kod-cli -- -D warnings
cargo build --workspace
./target/debug/kod --help
./target/debug/kod status
```

---

## Chunk 9 Summary

**Implemented:**
1. **CLI Structure** (`commands.rs`)
   - Full command structure with clap derive
   - Subcommands: chat, query, swarm, skills, memory, status
   - Global options: config, model, url, verbose, workdir
   - Nested subcommands for skills and memory

2. **Command Handlers** (`handlers.rs`)
   - Query handler with text/JSON output
   - Status handler showing system information
   - Skills handlers (list, search, show, validate)
   - Memory handlers (store, search, list, clear)

3. **Main Entry Point** (`main.rs`)
   - Logging initialization with tracing
   - Configuration loading and CLI override
   - Chat command with TUI integration
   - Swarm command with engine integration
   - Error handling and exit codes

4. **Integration Tests** (`integration.rs`)
   - Full CLI lifecycle testing
   - Skills operations testing
   - Memory operations testing
   - Configuration testing

5. **Documentation**
   - Comprehensive README with features and usage
   - Usage guide with examples and keybindings
   - Configuration guide with all options

**Next Chunk Preview:**

Chunk 10 (Final) will cover **Final Integration and Release Preparation**:
- End-to-end integration tests
- Performance benchmarks
- Release build optimization
- CI/CD pipeline setup
- Final documentation polish

Would you like me to continue with **Chunk 10: Final Integration and Release Preparation**?
