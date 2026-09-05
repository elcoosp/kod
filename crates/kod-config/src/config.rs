//! Main configuration for KOD.

use crate::{LlmConfig, MemoryConfig, SkillsConfig, SwarmConfig};
use kod_error::{KodError, Result};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct KodConfig {
    pub llm: LlmConfig,
    pub swarm: SwarmConfig,
    pub memory: MemoryConfig,
    pub skills: SkillsConfig,
    pub performance: PerformanceConfig,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PerformanceConfig {
    pub max_memory_mb: usize,
    pub target_response_time_ms: u64,
    pub enable_object_pooling: bool,
}

impl Default for PerformanceConfig {
    fn default() -> Self {
        Self {
            max_memory_mb: 150,
            target_response_time_ms: 200,
            enable_object_pooling: true,
        }
    }
}

impl KodConfig {
    /// Load configuration from the default location
    /// (`dirs::config_dir()/kod/config.toml`,
    /// i.e. `~/Library/Application Support/kod/config.toml` on macOS).
    pub fn load_default() -> Result<Self> {
        let config_dir = Self::config_dir()?;
        let config_path = config_dir.join("config.toml");

        if config_path.exists() {
            Self::load_from(&config_path)
        } else {
            // Create default config
            let config = Self::default();
            config.save_to(&config_path)?;
            Ok(config)
        }
    }

    /// Load configuration from a specific path
    pub fn load_from(path: &Path) -> Result<Self> {
        let content = std::fs::read_to_string(path)
            .map_err(|e| KodError::Config(format!("Failed to read config file: {}", e)))?;

        let config: KodConfig = toml::from_str(&content)
            .map_err(|e| KodError::Config(format!("Failed to parse config: {}", e)))?;

        Ok(config)
    }

    /// Save configuration to a specific path
    pub fn save_to(&self, path: &Path) -> Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|e| KodError::Config(format!("Failed to create config dir: {}", e)))?;
        }

        let content = toml::to_string_pretty(self)
            .map_err(|e| KodError::Config(format!("Failed to serialize config: {}", e)))?;

        std::fs::write(path, content)
            .map_err(|e| KodError::Config(format!("Failed to write config: {}", e)))?;

        Ok(())
    }

    /// Get the configuration directory
    pub fn config_dir() -> Result<PathBuf> {
        dirs::config_dir()
            .map(|d| d.join("kod"))
            .ok_or_else(|| KodError::Config("Could not determine config directory".to_string()))
    }

    /// Get the skills directory
    pub fn skills_dir(&self) -> Result<PathBuf> {
        if let Some(dir) = &self.skills.skills_dir {
            Ok(PathBuf::from(dir))
        } else {
            dirs::home_dir()
                .map(|h| h.join(".kod").join("skills"))
                .ok_or_else(|| KodError::Config("Could not determine home directory".to_string()))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn test_default_config() {
        let config = KodConfig::default();
        assert_eq!(config.llm.model, "codellama:13b");
        assert_eq!(config.swarm.max_agents, 5);
    }

    #[test]
    fn test_config_serialization_roundtrip() {
        let config = KodConfig::default();
        let toml_str = toml::to_string(&config).unwrap();
        let deserialized: KodConfig = toml::from_str(&toml_str).unwrap();
        assert_eq!(config.llm.model, deserialized.llm.model);
    }

    #[test]
    fn test_config_load_from_file() {
        let temp_dir = tempfile::tempdir().unwrap();
        let config_path = temp_dir.path().join("config.toml");

        let config = KodConfig::default();
        config.save_to(&config_path).unwrap();

        let loaded = KodConfig::load_from(&config_path).unwrap();
        assert_eq!(config.llm.model, loaded.llm.model);
    }

    #[test]
    fn test_invalid_config_file() {
        let temp_dir = tempfile::tempdir().unwrap();
        let config_path = temp_dir.path().join("config.toml");

        let mut file = std::fs::File::create(&config_path).unwrap();
        writeln!(file, "invalid toml [").unwrap();

        let result = KodConfig::load_from(&config_path);
        assert!(result.is_err());
    }
}
