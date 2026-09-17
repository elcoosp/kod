//! Common utilities for integration tests.
//!
//! Provides test environment setup and helper functions for
//! running end-to-end integration tests.

use std::fs;
use std::path::{Path, PathBuf};
use tempfile::TempDir;

/// Test environment with all necessary directories set up
#[allow(dead_code)]
pub struct TestEnvironment {
    pub temp_dir: TempDir,
    pub working_dir: PathBuf,
    pub skills_dir: PathBuf,
    pub db_path: PathBuf,
}

#[allow(dead_code)]
impl TestEnvironment {
    /// Create a new test environment
    pub fn new() -> Self {
        let temp_dir = TempDir::new().unwrap();
        let working_dir = temp_dir.path().to_path_buf();
        let skills_dir = working_dir.join("skills");
        let db_path = working_dir.join("kod_memory.redb");

        // Create skills directory
        fs::create_dir_all(&skills_dir).unwrap();

        Self {
            temp_dir,
            working_dir,
            skills_dir,
            db_path,
        }
    }

    /// Add a test skill
    pub fn add_skill(&self, name: &str, category: &str, triggers: &[&str]) {
        let skill_content = format!(
            r#"---
name: {}
description: Test skill for {}
version: 1.0.0
category: {}
tags:
  - test
  - {}
capabilities:
  - testing
triggers:
{}
---

## Instructions

This is a test skill for {}.

## Examples

<example input="Test {}">
Output for {} test.
</example>
"#,
            name,
            category,
            category,
            category,
            triggers
                .iter()
                .map(|t| format!("  - \"{}\"", t))
                .collect::<Vec<_>>()
                .join("\n"),
            name,
            name,
            name,
        );

        let file_name = format!("{}.md", name);
        fs::write(self.skills_dir.join(file_name), skill_content).unwrap();
    }

    /// Create a test config file
    pub fn create_config(&self, model: &str) -> PathBuf {
        let config_path = self.working_dir.join("config.toml");

        // Use KodConfig::default() and modify the model field
        let mut config = kod_config::KodConfig::default();
        config.llm.default_endpoint_mut().model = model.to_string();
        let config_content = toml::to_string(&config).unwrap();

        fs::write(&config_path, config_content).unwrap();
        config_path
    }

    /// Verify database exists
    pub fn verify_db_exists(&self) -> bool {
        self.db_path.exists()
    }
}

impl Default for TestEnvironment {
    fn default() -> Self {
        Self::new()
    }
}

/// Run a kod command in the test environment
#[allow(dead_code)]
pub fn run_kod_command(args: &[&str], working_dir: &Path) -> Result<String, String> {
    use std::process::Command;

    let binary_path = env!("CARGO_BIN_EXE_kod");

    let output = Command::new(binary_path)
        .args(args)
        .current_dir(working_dir)
        .env("KOD_NO_SWARM", "1")
        .env("RUST_LOG", "error") // Reduce log noise
        .output()
        .expect("Failed to execute command");

    let stdout = String::from_utf8_lossy(&output.stdout).to_string();
    let stderr = String::from_utf8_lossy(&output.stderr).to_string();

    if output.status.success() {
        Ok(stdout)
    } else {
        Err(format!(
            "Command failed: {}\nStderr: {}",
            args.join(" "),
            stderr
        ))
    }
}

/// Create a mock LLM response for testing
#[allow(dead_code)]
pub fn create_mock_response(prompt: &str) -> String {
    format!(
        "Mock response for: {}\n\nThis is a test response that simulates an LLM output.",
        prompt
    )
}
