//! Memory system configuration.

use serde::{Deserialize, Serialize};

/// Where the long-term memory database lives when
/// `memory.long_term_db_path` is not set.
///
/// The previous behavior was always `Global`: one database at
/// `~/.kod/data/kod.redb`, shared across every project the user ever
/// opened. A fact learned while working on project A ("this project
/// uses `sqlx`") was retrievable while working on project B, where it
/// may be wrong. `Project` scopes the default to
/// `<cwd>/.kod/memory.redb`, following `.git`'s example.
///
/// An explicit `long_term_db_path` still wins over either scope: a
/// user who pointed the database at a specific file meant it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum MemoryScope {
    /// One shared database at `~/.kod/data/kod.redb`. The default, so
    /// existing installs keep their data.
    #[default]
    Global,
    /// One database per project at `<cwd>/.kod/memory.redb`.
    Project,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct MemoryConfig {
    pub short_term_capacity: usize,
    pub long_term_db_path: Option<String>,
    pub enable_semantic_search: bool,
    pub embedding_model: String,
    pub context_window: usize,
    pub compaction_interval_secs: u64,
    /// Which default the engine uses when `long_term_db_path` is not
    /// set. See [`MemoryScope`].
    pub scope: MemoryScope,
}

impl Default for MemoryConfig {
    fn default() -> Self {
        Self {
            short_term_capacity: 100,
            long_term_db_path: None,
            enable_semantic_search: true,
            embedding_model: "all-MiniLM-L6-v2".to_string(),
            context_window: 4096,
            compaction_interval_secs: 3600,
            scope: MemoryScope::Global,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_default_memory_config() {
        let config = MemoryConfig::default();
        assert_eq!(config.short_term_capacity, 100);
        assert!(config.enable_semantic_search);
    }

    #[test]
    fn test_memory_scope_default_is_global() {
        let config = MemoryConfig::default();
        assert_eq!(config.scope, MemoryScope::Global);
    }

    /// `scope = "project"` (lowercase) and `scope = "global"` are the
    /// shapes the strategy doc uses. A config written with those must
    /// deserialize; the serde rename_all = "lowercase" is the contract.
    #[test]
    fn test_memory_scope_deserializes_from_lowercase() {
        let config: MemoryConfig = toml::from_str("scope = \"project\"").unwrap();
        assert_eq!(config.scope, MemoryScope::Project);

        let config: MemoryConfig = toml::from_str("scope = \"global\"").unwrap();
        assert_eq!(config.scope, MemoryScope::Global);
    }

    /// An existing config file without a `scope` key must still parse —
    /// the field is `#[serde(default)]` at the struct level, so a config
    /// written before the field existed gets Global.
    #[test]
    fn test_memory_scope_absent_defaults_to_global() {
        let config: MemoryConfig = toml::from_str("short_term_capacity = 42").unwrap();
        assert_eq!(config.scope, MemoryScope::Global);
        assert_eq!(config.short_term_capacity, 42);
    }
}
