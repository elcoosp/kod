//! Engine configuration integration with main KOD config.
//!
//! Bridges the main [KodConfig] with the engine-specific configuration
//! needed by the task router and engine.

use kod_config::KodConfig;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// Configuration for the engine
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EngineConfig {
    pub working_dir: PathBuf,
    pub enable_swarm: bool,
    pub enable_memory: bool,
    pub enable_skills: bool,
    pub max_skills_per_query: usize,
    pub db_path: Option<PathBuf>,
}

impl Default for EngineConfig {
    fn default() -> Self {
        Self {
            working_dir: std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")),
            enable_swarm: true,
            enable_memory: true,
            enable_skills: true,
            max_skills_per_query: 3,
            db_path: None,
        }
    }
}

impl EngineConfig {
    /// Create from main KOD config
    pub fn from_kod_config(config: &KodConfig, working_dir: &Path) -> Self {
        Self {
            working_dir: working_dir.to_path_buf(),
            enable_swarm: true, // From swarm config
            enable_memory: config.memory.enable_semantic_search,
            enable_skills: true,
            max_skills_per_query: config.skills.max_skills_per_query,
            db_path: None,
        }
    }

    /// Set database path
    pub fn with_db_path(mut self, path: impl Into<PathBuf>) -> Self {
        self.db_path = Some(path.into());
        self
    }

    /// Get database path (or default)
    pub fn get_db_path(&self) -> PathBuf {
        self.db_path
            .clone()
            .unwrap_or_else(|| self.working_dir.join("kod_memory.redb"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_config_creation() {
        let config = EngineConfig::default();
        assert!(config.enable_swarm);
        assert!(config.enable_memory);
    }
}
