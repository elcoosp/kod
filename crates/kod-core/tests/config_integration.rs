use kod_core::config::EngineConfig;
use kod_config::{KodConfig, LlmConfig, MemoryConfig, SkillsConfig, SwarmConfig};
use tempfile::TempDir;

#[test]
fn test_engine_config_from_kod_config() {
    let temp_dir = TempDir::new().unwrap();

    let kod_config = KodConfig {
        llm: LlmConfig::default(),
        swarm: SwarmConfig::default(),
        memory: MemoryConfig::default(),
        skills: SkillsConfig::default(),
        ..Default::default()
    };

    let engine_config = EngineConfig::from_kod_config(&kod_config, temp_dir.path());

    assert_eq!(engine_config.working_dir, temp_dir.path().to_path_buf());
    assert!(engine_config.enable_swarm);
    assert!(engine_config.enable_memory);
}

#[test]
fn test_engine_config_defaults() {
    let config = EngineConfig::default();

    assert!(config.enable_swarm);
    assert!(config.enable_memory);
    assert_eq!(config.max_skills_per_query, 3);
}

#[test]
fn test_config_serialization() {
    let config = EngineConfig::default();

    let json = serde_json::to_string(&config).unwrap();
    let deserialized: EngineConfig = serde_json::from_str(&json).unwrap();

    assert_eq!(config.enable_swarm, deserialized.enable_swarm);
}
