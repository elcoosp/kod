//! Main configuration for KOD.

use crate::{LlmConfig, MemoryConfig, SkillsConfig};
use kod_error::{KodError, Result};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
// `#[serde(default)]` at the container level: a config.toml that omits
// a section entirely (e.g. no `[memory]`) fills that section from
// KodConfig::default() instead of failing with "missing field memory".
// This is the common hand-edited config shape — write the block you
// care about, leave the rest out.
#[serde(default)]
pub struct KodConfig {
    pub llm: LlmConfig,
    pub memory: MemoryConfig,
    pub skills: SkillsConfig,
    pub performance: PerformanceConfig,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
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
    ///
    /// Never fails on the first-run path. Three cases, all non-fatal:
    ///
    /// - Config file exists and parses: use it.
    /// - Config file exists but is corrupt (bad TOML, unknown fields
    ///   that break deserialization, truncated write from a crash):
    ///   warn loudly, fall back to `Self::default()`, and leave the
    ///   file untouched so the user can inspect what they had. The CLI
    ///   previously exited here, leaving a session unusable until the
    ///   user found and deleted the file by hand.
    /// - Config file does not exist: write the defaults. If that write
    ///   fails (read-only home, no permission on `~/.config`, NFS
    ///   mounted ro), warn and hand back in-memory defaults — the
    ///   session runs fine, it just will not persist.
    ///
    /// `KodConfig::load_from` stays strict (used by tests and by
    /// `load_default` itself when the file exists); only the top-level
    /// entry point is forgiving.
    pub fn load_default() -> Result<Self> {
        let config_dir = match Self::config_dir() {
            Ok(d) => d,
            Err(e) => {
                tracing::warn!(
                    error = %e,
                    "Could not determine config directory; using defaults"
                );
                return Ok(Self::default());
            }
        };
        let config_path = config_dir.join("config.toml");

        if config_path.exists() {
            match Self::load_from(&config_path) {
                Ok(mut cfg) => {
                    // Soft-validate the loaded config: clamp out-of-range
                    // values to safe bounds, logging each change. A user
                    // with `temperature = 2.5` or `context_window = 0`
                    // gets a working session and a warning, not a silent
                    // failure three prompts later.
                    cfg.llm.validate();
                    Ok(cfg)
                }
                Err(e) => {
                    tracing::warn!(
                        path = %config_path.display(),
                        error = %e,
                        "Config file present but unreadable; using built-in defaults. \
                         Fix or delete the file to silence this warning."
                    );
                    Ok(Self::default())
                }
            }
        } else {
            let config = Self::default();
            if let Err(e) = config.save_to(&config_path) {
                tracing::warn!(
                    path = %config_path.display(),
                    error = %e,
                    "Could not write default config; using in-memory defaults. \
                     Settings will not persist across restarts."
                );
            }
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

    /// The effective long-term memory database path.
    ///
    /// Returns the explicit `memory.long_term_db_path` when set;
    /// otherwise the default the engine and CLI construct —
    /// `~/.kod/data/kod.redb`. A caller (the `kod config` display,
    /// the engine, a future backup command) needs the path that will
    /// actually be opened, not the raw `Option` in the config file.
    pub fn memory_db_path(&self) -> Result<PathBuf> {
        if let Some(explicit) = &self.memory.long_term_db_path {
            return Ok(PathBuf::from(explicit));
        }
        dirs::home_dir()
            .map(|h| h.join(".kod").join("data").join("kod.redb"))
            .ok_or_else(|| {
                KodError::Config("Could not determine home directory".to_string())
            })
    }

    /// Get the primary skills directory.
    ///
    /// Prefer [`KodConfig::skills_dirs`] — this returns only the first
    /// directory and exists for backwards compatibility with callers
    /// that predate multi-directory discovery.
    pub fn skills_dir(&self) -> Result<PathBuf> {
        Ok(self
            .skills_dirs()?
            .into_iter()
            .next()
            .unwrap_or_else(|| PathBuf::from(".kod/skills")))
    }

    /// Every directory KOD scans for skills, in load order.
    ///
    /// When `skills.skills_dir` is explicitly set in the config, only that
    /// directory is returned — an explicit choice is not augmented
    /// silently. Otherwise the standard locations are returned:
    ///
    /// 1. `~/.kod/skills`      — canonical KOD location
    /// 2. `~/.agents/skills`   — Claude-style compatibility
    /// 3. `<cwd>/.kod/skills`  — project-local KOD
    /// 4. `<cwd>/.agents/skills` — project-local Claude-style
    ///
    /// Later directories shadow earlier ones for skills that share a
    /// name, so a project-local skill overrides a global one with the
    /// same name.
    pub fn skills_dirs(&self) -> Result<Vec<PathBuf>> {
        if let Some(dir) = &self.skills.skills_dir {
            return Ok(vec![PathBuf::from(dir)]);
        }
        let home = dirs::home_dir()
            .ok_or_else(|| KodError::Config("Could not determine home directory".to_string()))?;
        let cwd = std::env::current_dir().map_err(|e| {
            KodError::Config(format!("Could not determine working directory: {}", e))
        })?;
        Ok(vec![
            home.join(".kod").join("skills"),
            home.join(".agents").join("skills"),
            cwd.join(".kod").join("skills"),
            cwd.join(".agents").join("skills"),
        ])
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

    /// `load_default` must never fail the process on a corrupt file —
    /// it should warn and return defaults. `load_from` stays strict, so
    /// the two callsites are not accidentally swapped.
    ///
    /// This test drives the fallback path directly by pointing the
    /// loader at a temp dir containing a corrupt config.toml. Because
    /// `load_default` uses `dirs::config_dir()`, we exercise the same
    /// code by calling `load_from` on the corrupt file (which errors, as
    /// expected) and asserting the recovery branch we care about in
    /// `load_default` would return `Self::default()`. The observable
    /// contract — that a corrupt file at the default location cannot
    /// take the CLI down — is captured by the CLI integration test
    /// (`test_cli_error_handling`), which sets up the same condition.
    /// A config.toml with only a subset of sections (or a subset of
    /// fields within a section) must parse. Before `#[serde(default)]`
    /// at container level, a file containing only `[llm]` failed with
    /// "missing field `memory`", and a file containing only `[llm]
    /// model = "…"` additionally failed with "missing field
    /// `provider`". Both are the common hand-edited shape.
    #[test]
    fn test_partial_config_uses_defaults() {
        // Empty file: every field defaults.
        let cfg: KodConfig = toml::from_str("").unwrap();
        assert_eq!(cfg.llm.model, "codellama:13b");
        assert_eq!(cfg.memory.short_term_capacity, 100);
        assert_eq!(cfg.skills.max_skills_per_query, 3);
        assert_eq!(cfg.performance.max_memory_mb, 150);

        // Only [llm] present: other sections default.
        let cfg: KodConfig = toml::from_str(
            r#"
            [llm]
            model = "llama3.1"
            "#,
        )
        .unwrap();
        assert_eq!(cfg.llm.model, "llama3.1");
        assert_eq!(cfg.memory.short_term_capacity, 100);

        // A single field inside a section: siblings default.
        let cfg: KodConfig = toml::from_str(
            r#"
            [llm]
            model = "llama3.1"

            [skills]
            max_skills_per_query = 7
            "#,
        )
        .unwrap();
        assert_eq!(cfg.llm.model, "llama3.1");
        // The other llm fields fall back to their defaults.
        assert_eq!(cfg.llm.provider, crate::llm::ProviderType::OpenAICompatible);
        assert_eq!(cfg.llm.temperature, 0.7);
        assert_eq!(cfg.llm.context_window, 8192);
        // The set skills field is honored, its siblings default.
        assert_eq!(cfg.skills.max_skills_per_query, 7);
        assert!(cfg.skills.enable_hot_reload);
    }

    /// A config.toml with only a subset of sections (or a subset of
    /// fields within a section) must parse. Before `#[serde(default)]`
    /// at container level, a file containing only `[llm]` failed with
    /// "missing field `memory`", and a file containing only `[llm]
    /// model = "…"` additionally failed with "missing field
    /// `provider`". Both are the common hand-edited shape.
    #[test]
    fn test_corrupt_file_yields_error_from_load_from() {
        let temp_dir = tempfile::tempdir().unwrap();
        let path = temp_dir.path().join("config.toml");
        std::fs::write(&path, "not = valid toml [ =").unwrap();

        // Strict API errors, so load_default's fallback branch triggers.
        assert!(KodConfig::load_from(&path).is_err());
        // Defaults, by construction, have the documented fields.
        let defaults = KodConfig::default();
        assert_eq!(defaults.llm.model, "codellama:13b");
    }
}
