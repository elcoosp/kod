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
    pub enable_memory: bool,
    pub enable_skills: bool,
    pub max_skills_per_query: usize,
    pub db_path: Option<PathBuf>,
}

impl Default for EngineConfig {
    fn default() -> Self {
        Self {
            working_dir: std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")),
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
        assert!(config.enable_memory);
    }
}

#[cfg(test)]
mod coverage_engine_config {
    //! `EngineConfig` is the engine's own configuration; the CLI
    //! and TUI build one from `KodConfig` at startup. The
    //! accessors here decide which database path the engine opens
    //! and how many skills a query retrieves — a regression
    //! silently shifts both.
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn default_enables_memory_and_skills() {
        let c = EngineConfig::default();
        assert!(c.enable_memory);
        assert!(c.enable_skills);
        assert_eq!(c.max_skills_per_query, 3);
        assert!(c.db_path.is_none());
    }

    #[test]
    fn default_working_dir_is_the_process_cwd() {
        // Falls back to "." if the cwd is unavailable; the
        // contract is "non-empty in any plausible environment".
        let c = EngineConfig::default();
        assert!(!c.working_dir.as_os_str().is_empty());
    }

    #[test]
    fn with_db_path_overrides_the_default() {
        let c = EngineConfig::default().with_db_path("/tmp/custom.redb");
        assert_eq!(c.db_path, Some(PathBuf::from("/tmp/custom.redb")));
    }

    #[test]
    fn get_db_path_falls_back_to_working_dir() {
        // No `db_path` set: the accessor derives one under the
        // working directory. A regression that returned an empty
        // path would make the engine open a database file literally
        // called "".
        let c = EngineConfig {
            working_dir: PathBuf::from("/tmp/project"),
            ..Default::default()
        };
        let p = c.get_db_path();
        assert_eq!(p, PathBuf::from("/tmp/project/kod_memory.redb"));
    }

    #[test]
    fn get_db_path_prefers_the_explicit_path() {
        let c = EngineConfig {
            working_dir: PathBuf::from("/tmp/project"),
            db_path: Some(PathBuf::from("/tmp/custom.redb")),
            ..Default::default()
        };
        assert_eq!(c.get_db_path(), PathBuf::from("/tmp/custom.redb"));
    }

    #[test]
    fn from_kod_config_takes_working_dir_and_capacity_from_the_source() {
        use kod_config::KodConfig;
        let mut cfg = KodConfig::default();
        cfg.memory.enable_semantic_search = true;
        cfg.skills.max_skills_per_query = 7;
        let ec = EngineConfig::from_kod_config(&cfg, std::path::Path::new("/tmp/from"));
        assert_eq!(ec.working_dir, PathBuf::from("/tmp/from"));
        assert_eq!(ec.max_skills_per_query, 7);
        assert!(ec.enable_skills, "skills are always on in this mapping");
        // `enable_memory` mirrors `memory.enable_semantic_search`.
        assert_eq!(ec.enable_memory, cfg.memory.enable_semantic_search);
    }

    #[test]
    fn engine_config_round_trips_through_json() {
        let c = EngineConfig {
            working_dir: PathBuf::from("/tmp/rt"),
            enable_memory: false,
            enable_skills: true,
            max_skills_per_query: 5,
            db_path: Some(PathBuf::from("/tmp/rt.redb")),
        };
        let json = serde_json::to_string(&c).unwrap();
        let parsed: EngineConfig = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.working_dir, c.working_dir);
        assert!(!parsed.enable_memory);
        assert_eq!(parsed.max_skills_per_query, 5);
        assert_eq!(parsed.db_path, c.db_path);
    }
}
