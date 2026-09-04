//! Main KOD engine - orchestrates all subsystems.
//!
//! Coordinates the task router, LLM providers, skills, memory, and swarm
//! to process user requests end-to-end.

use crate::router::{RouterConfig, TaskRouter, TaskResponse};
use kod_error::{KodError, Result};
use kod_provider::{GenerationOptions, LlmProvider};
use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::RwLock;

/// Main engine for KOD
pub struct KodEngine {
    router: Arc<TaskRouter>,
    provider: RwLock<Option<Arc<dyn LlmProvider>>>,
    is_running: RwLock<bool>,
}

impl KodEngine {
    /// Create a new engine
    pub fn new(config: RouterConfig, db_path: PathBuf) -> Result<Self> {
        let router = TaskRouter::new(config, db_path)?;

        Ok(Self {
            router: Arc::new(router),
            provider: RwLock::new(None),
            is_running: RwLock::new(false),
        })
    }

    /// Set the LLM provider
    pub async fn set_provider(&self, provider: Arc<dyn LlmProvider>) {
        *self.provider.write().await = Some(provider);
    }

    /// Start the engine
    pub async fn start(&self) -> Result<()> {
        let mut running = self.is_running.write().await;

        if *running {
            return Err(KodError::InvalidState("Engine already running".to_string()));
        }

        *running = true;

        tracing::info!("KOD engine started");
        Ok(())
    }

    /// Process user input
    pub async fn process(&self, input: &str) -> Result<TaskResponse> {
        // Check if engine is running
        {
            let running = self.is_running.read().await;
            if !*running {
                return Err(KodError::InvalidState("Engine not running".to_string()));
            }
        }

        // Process through router
        let response = self.router.process_input(input).await?;

        // If we have a provider and no text, generate using LLM
        if response.text.is_none() {
            let provider = self.provider.read().await;

            if let Some(provider) = provider.as_ref() {
                let options = GenerationOptions::default();
                let text = provider.generate(input, &options).await?;

                // Create new response with text
                return Ok(TaskResponse {
                    text: Some(text),
                    ..response
                });
            }
        }

        Ok(response)
    }

    /// Run maintenance tasks
    pub async fn run_maintenance(&self) -> Result<()> {
        // Perform periodic maintenance
        // - Compact memory
        // - Clean up expired locks
        // - Update skill cache

        tracing::debug!("Running engine maintenance");
        Ok(())
    }

    /// Shutdown the engine
    pub async fn shutdown(&self) -> Result<()> {
        let mut running = self.is_running.write().await;

        if !*running {
            return Ok(()); // Already stopped
        }

        *running = false;

        // Cleanup
        // - Stop all agents
        // - Release all locks
        // - Flush memory

        tracing::info!("KOD engine shutdown");
        Ok(())
    }

    /// Check if engine is running
    pub async fn is_running(&self) -> bool {
        *self.is_running.read().await
    }

    /// Get router reference
    pub fn router(&self) -> &TaskRouter {
        &self.router
    }

    /// Load skills
    pub async fn load_skills(&self, _skills_dir: &std::path::Path) -> Result<()> {
        // In the current design, the router owns the skill matcher.
        // The router's load_skills method handles loading and matching.
        // Since the router is behind an Arc, we cannot mutate it directly.
        // This is a placeholder for the full implementation that would
        // use interior mutability patterns.
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[tokio::test]
    async fn test_engine_lifecycle() {
        let temp_dir = TempDir::new().unwrap();
        let db_path = temp_dir.path().join("test.redb");

        let engine = KodEngine::new(RouterConfig::default(), db_path).unwrap();

        // Engine starts not running
        assert!(!engine.is_running().await);

        // Start engine
        engine.start().await.unwrap();
        assert!(engine.is_running().await);

        // Shutdown
        engine.shutdown().await.unwrap();
        assert!(!engine.is_running().await);
    }
}
