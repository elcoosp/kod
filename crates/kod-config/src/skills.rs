//! Skills system configuration.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct SkillsConfig {
    pub skills_dir: Option<String>,
    pub enable_hot_reload: bool,
    pub max_cache_size_mb: usize,
    pub max_skills_per_query: usize,
    pub match_threshold: f32,
}

impl Default for SkillsConfig {
    fn default() -> Self {
        Self {
            skills_dir: None,
            enable_hot_reload: true,
            max_cache_size_mb: 50,
            max_skills_per_query: 3,
            match_threshold: 0.7,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_default_skills_config() {
        let config = SkillsConfig::default();
        assert!(config.enable_hot_reload);
        assert_eq!(config.max_skills_per_query, 3);
    }
}
