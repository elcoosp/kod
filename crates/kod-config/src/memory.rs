//! Memory system configuration.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct MemoryConfig {
    pub short_term_capacity: usize,
    pub long_term_db_path: Option<String>,
    pub enable_semantic_search: bool,
    pub embedding_model: String,
    pub context_window: usize,
    pub compaction_interval_secs: u64,
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
}
