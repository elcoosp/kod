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

/// Where embeddings come from (D2-B1).
///
/// `None` is the default: no embeddings are computed, retrieval falls
/// back to keyword + recency only. `Ollama` and `OpenAI` select the
/// two HTTP embedder shapes we support. Both speak a JSON API; neither
/// links a model into the binary.
///
/// A future `Local` variant would use the opt-in `fastembed` feature;
/// it is deliberately not part of this enum yet — the "no ML weight in
/// the default binary" rule (README, ADR-04) is easier to hold when
/// the enum simply does not mention it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum EmbeddingEndpoint {
    /// No embeddings. Retrieval is keyword + recency only.
    #[default]
    None,
    /// Ollama `/api/embed`. The URL is derived from the LLM base_url
    /// when `embedding_url` is not set: `http://host:port/v1` becomes
    /// `http://host:port`.
    Ollama,
    /// OpenAI `/v1/embeddings`. `embedding_api_key_env` names the
    /// environment variable holding the key; the value is never
    /// written to the config file.
    OpenAI,
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

    // ---- D2-B1 embedding fields ----

    /// Where embeddings come from. Default `None`.
    pub embedding_endpoint: EmbeddingEndpoint,
    /// Explicit embedder URL. When `None`, a caller (the CLI/TUI)
    /// derives one from `llm.base_url` for the Ollama case.
    pub embedding_url: Option<String>,
    /// Environment variable holding the embedder's API key, for the
    /// OpenAI case. The value is read at startup; never stored.
    pub embedding_api_key_env: Option<String>,

    // ---- D2-B3b extraction fields ----

    /// When true, `KodEngine::shutdown` runs a one-shot extraction
    /// pass over the session transcript: a cheap LLM call pulls
    /// durable facts out of the transcript and stores them as
    /// long-term entries. Default false — extraction costs one LLM
    /// call per session.
    pub extract_on_shutdown: bool,
    /// Cap on the number of facts extracted per session.
    pub extract_max_entries: usize,
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
            embedding_endpoint: EmbeddingEndpoint::None,
            embedding_url: None,
            embedding_api_key_env: None,
            extract_on_shutdown: false,
            extract_max_entries: 12,
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
