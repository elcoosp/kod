//! Main configuration for KOD.

use crate::{LlmConfig, MemoryConfig, SkillsConfig, SwarmConfig};
use kod_error::{KodError, Result};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// The in-memory default config. Stamped `config_version = 2` so any
/// caller that writes `KodConfig::default()` to disk produces the
/// current release's shape, not the pre-v2 shape. `load_default` used
/// to paper over this for the first-run path only; the invariant now
/// holds everywhere — tests, library code, future subcommands.
impl Default for KodConfig {
    fn default() -> Self {
        Self {
            config_version: 2,
            llm: LlmConfig::default(),
            memory: MemoryConfig::default(),
            skills: SkillsConfig::default(),
            swarm: SwarmConfig::default(),
            hooks: HooksConfig::default(),
            tools: ToolsConfig::default(),
            lsp: LspConfig::default(),
            mcp: crate::McpConfig::default(),
            jev: crate::JevConfig::default(),
            security: crate::SecurityConfig::default(),
            limits: crate::LimitsConfig::default(),
            commands: Default::default(),
        }
    }
}

/// `[security]` — the reserved block from the config reference.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct SecurityConfig {
    /// Prompt-path redaction.
    pub redact: RedactConfig,
}

/// `[security.redact]` — in-prompt secret redaction (Tier 1.3).
///
/// Log-write redaction is always on and not configurable. This block
/// controls the *prompt-path* redactor, which runs over every message
/// and tool result before it reaches the model.
///
/// The default is off. Turning it on is the right choice when the
/// workspace may contain live credentials the model does not need to
/// see; turning it off (the default) is the right choice for a normal
/// code-editing session, where a redacted `read_file` result would
/// prevent the model from proposing the change the user asked for.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct RedactConfig {
    /// Redact secrets in the prompt before the model sees it.
    pub in_prompt: bool,
    /// When set, paths that match this list are additionally redacted
    /// in `ToolCall.arguments` before being rendered into the prompt.
    /// Same glob semantics as `[policy.read_protection].deny`.
    ///
    /// Redacting arguments is stronger than redacting results: the
    /// model cannot see the arguments at all, which is what you want
    /// when a `write_file` is about to place a literal credential.
    #[serde(default)]
    pub redact_argument_paths: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
// `#[serde(default)]` at the container level: a config.toml that omits
// a section entirely (e.g. no `[memory]`) fills that section from
// KodConfig::default() instead of failing with "missing field memory".
// This is the common hand-edited config shape — write the block you
// care about, leave the rest out.
#[serde(default)]
pub struct KodConfig {
    /// Config schema version. Absent means "v1" — the shape that
    /// predates named endpoints, `[mcp]`, `[tools]`, and the
    /// policy engine's layering. The loader accepts both; `kod config
    /// migrate` writes a `2` here and rewrites the file into the v2
    /// shape.
    ///
    /// `#[serde(default)]` rather than a bare `u32`: a file without
    /// the key must still parse. `0` means "unset"; the effective
    /// version is `version == 0 ? 1 : version`.
    #[serde(default)]
    pub config_version: u32,
    pub llm: LlmConfig,
    pub memory: MemoryConfig,
    pub skills: SkillsConfig,
    pub swarm: SwarmConfig,
    pub hooks: HooksConfig,
    pub tools: ToolsConfig,
    pub lsp: LspConfig,
    /// MCP servers (D6.1). Empty when the config has no `[mcp]`
    /// section, which is the default — no plugin is registered
    /// unless the user writes a block for it.
    pub mcp: crate::McpConfig,
    /// TypeSafe AI / Jev integration (design P0.1). Disabled by
    /// default — an unconfigured KOD runs identically to a
    /// build that predates the integration. The CLI and TUI
    /// construct a `JevClient` from this block at startup and
    /// install it on the engine.
    pub jev: crate::JevConfig,
    /// In-prompt secret redaction (Tier 1.3).
    pub security: SecurityConfig,
    /// Session cost and token limits (Tier 1.2).
    pub limits: crate::LimitsConfig,
    /// User-defined slash commands. A key `foo` registers `/foo <args>`,
    /// whose body is the prompt sent to the model. `{args}` in the
    /// body is replaced by everything after the command name; `{cwd}`
    /// by the current working directory. Example:
    ///
    /// ```toml
    /// [commands]
    /// review = "Review the last change in {cwd} and point out risks."
    /// explain = "Explain what this code does:\n\n{args}"
    /// ```
    #[serde(default)]
    pub commands: std::collections::HashMap<String, String>,
}

/// Language-server configuration (design §4 D5.2).
///
/// Two independent knobs:
///
/// - `auto_diagnostics` — whether a successful `write_file` /
///   `patch_file` triggers the post-write diagnostics pass. Independent
///   of `[tools] auto_lsp`, which says whether the pass *tries* LSP or
///   falls straight to the compiler. The composition is intentional:
///   `auto_lsp = false, auto_diagnostics = true` gives a
///   compiler-only feedback loop (fast on a project with no language
///   server); `auto_diagnostics = false` disables both.
///
/// - `settle_ms` — how long the *inline* diagnostics pass waits for
///   the server to publish its list after a write before accepting an
///   empty answer as final. The inline pass runs on the write's own
///   turn and the model is waiting on it, so the default is short
///   (500 ms, the design's value). A slower answer is not lost: see
///   `deferred_settle_ms`.
///
/// - `deferred_settle_ms` — the design's "keep fetching up to 12 s".
///   When the inline wait produces nothing, a background task keeps
///   watching the server for up to this long. Any late diagnostics
///   are queued and prepended to the next turn's prompt block as a
///   `## LSP diagnostics (late)` section. `0` disables the deferred
///   pass; `deferred_enabled = false` is the same but clearer.
///
/// - `deferred_enabled` — the master switch for the deferred pass.
///   `false` makes the write path pure inline: it waits `settle_ms`,
///   emits whatever it saw (possibly nothing), and moves on. The
///   default is on — the deferred pass is what plugs a slow
///   rust-analyzer's late answer into the model's next turn.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct LspConfig {
    pub auto_diagnostics: bool,
    pub settle_ms: u64,
    pub deferred_settle_ms: u64,
    pub deferred_enabled: bool,
}

impl Default for LspConfig {
    fn default() -> Self {
        Self {
            auto_diagnostics: true,
            settle_ms: 500,
            deferred_settle_ms: 12_000,
            deferred_enabled: true,
        }
    }
}

/// Shell hooks run around tool execution. See `kod-core`'s `hooks`
/// module for the runtime side. Disabled by default; a config that
/// wants them sets `enabled = true` and adds at least one hook.
/// Tool-execution policy. Currently one flag; the struct exists so
/// future toggles (a per-tool allowlist, a bulk-write size cap) have a
/// home and do not silently land in `LlmConfig` — which they do not
/// belong in.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct ToolsConfig {
    /// The default policy preset (design §6.1). Written as a string —
    /// `"read-only"`, `"standard"`, or `"yolo"` — and parsed at engine
    /// startup. `None` (the default) selects `Standard`. A project's
    /// `.kod/policy.toml` and a CLI `--preset` override this; it is
    /// the same layer ordering the design documents.
    #[serde(default)]
    pub preset: Option<String>,
    /// When true, a successful `write_file` / `patch_file` in a tool
    /// round triggers a project check (Cargo, tsc, ruff, go vet) and
    /// the diagnostics are appended to the prompt the model sees on
    /// its next turn. Default false — running `cargo check` after
    /// every write is not free, and a session on a large workspace
    /// may prefer to run `check` only when it decides to.
    pub auto_check: bool,
    /// When true (the default), a successful write in a tool round
    /// also runs the language server's diagnostics pass on the
    /// touched file(s) and appends a `## LSP diagnostics` block to
    /// the next turn's prompt. Unlike `auto_check` this is cheap
    /// (sub-second per file) and only runs for languages that have
    /// a server on PATH; a session with no LSP installed simply sees
    /// no block.
    pub auto_lsp: bool,
}

impl Default for ToolsConfig {
    fn default() -> Self {
        Self {
            preset: None,
            auto_check: false,
            auto_lsp: true,
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct HooksConfig {
    pub pre_tool_use: std::collections::HashMap<String, String>,
    pub post_tool_use: std::collections::HashMap<String, String>,
    pub enabled: bool,
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
    /// H-D9: parse `path` as a `toml::Table` and deserialize each
    /// known top-level section independently. A section that fails
    /// to parse falls back to `Self::default()`'s value for that
    /// section; the others are preserved.
    ///
    /// Returns `Ok(None)` when even the TOML syntax is broken
    /// (recovery cannot salvage anything), `Ok(Some(cfg))` when the
    /// file is valid TOML but at least one section failed.
    fn recover_sections(path: &std::path::Path) -> Result<Option<Self>> {
        let raw = match std::fs::read_to_string(path) {
            Ok(s) => s,
            Err(_) => return Ok(None),
        };
        let table: toml::Table = match raw.parse::<toml::Table>() {
            Ok(t) => t,
            Err(_) => return Ok(None),
        };

        // Start from the default and override each section we can
        // deserialize. Unknown keys are ignored (a warning is the
        // job of a later `kod config validate` pass).
        let mut cfg = Self::default();
        cfg.config_version = table
            .get("config_version")
            .and_then(|v| v.as_integer())
            .map(|n| n as u32)
            .unwrap_or(cfg.config_version);

        macro_rules! recover {
            ($field:ident, $ty:ty) => {
                if let Some(section) = table.get(stringify!($field)) {
                    match section.clone().try_into::<$ty>() {
                        Ok(v) => cfg.$field = v,
                        Err(e) => {
                            tracing::error!(
                                section = stringify!($field),
                                error = %e,
                                "config section failed to parse; using default",
                            );
                        }
                    }
                }
            };
        }
        recover!(llm, crate::llm::LlmConfig);
        recover!(tools, ToolsConfig);
        recover!(mcp, crate::mcp::McpConfig);
        recover!(limits, crate::limits::LimitsConfig);
        recover!(hooks, HooksConfig);
        recover!(lsp, LspConfig);
        recover!(memory, crate::memory::MemoryConfig);
        recover!(skills, crate::skills::SkillsConfig);
        recover!(swarm, crate::swarm::SwarmConfig);
        recover!(jev, crate::jev::JevConfig);
        recover!(security, SecurityConfig);

        Ok(Some(cfg))
    }

    /// M-11: mtime-validated config cache for hot paths. `load_default`
    /// re-reads and re-parses `config.toml` on every call — the engine
    /// hits it ~3x per tool-write round. This parses once and re-parses
    /// only when the file's mtime changes. Startup side-effects
    /// (creating a default config) stay in `load_default`.
    pub fn load_cached() -> Result<std::sync::Arc<Self>> {
        use std::sync::{Mutex, OnceLock};
        static CACHE: OnceLock<
            Mutex<Option<(PathBuf, Option<std::time::SystemTime>, std::sync::Arc<KodConfig>)>>,
        > = OnceLock::new();
        let path = match Self::config_dir() {
            Ok(d) => d.join("config.toml"),
            Err(_) => return Ok(std::sync::Arc::new(Self::default())),
        };
        let mtime = std::fs::metadata(&path).and_then(|m| m.modified()).ok();
        let cache = CACHE.get_or_init(|| Mutex::new(None));
        if let Ok(g) = cache.lock()
            && let Some((p, t, cfg)) = g.as_ref()
            && *p == path
            && *t == mtime
        {
            return Ok(cfg.clone());
        }
        let cfg = std::sync::Arc::new(Self::load_default()?);
        if let Ok(mut g) = cache.lock() {
            *g = Some((path, mtime, cfg.clone()));
        }
        Ok(cfg)
    }

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
                    // values to safe bounds, logging each change.
                    cfg.llm.validate();
                    cfg.skills.validate();
                    cfg.limits.clamp();
                    // Out-of-range thresholds (auto_approve_min = -1.0)
                    // otherwise made every gated decision auto-approve.
                    cfg.jev.thresholds.clamp();
                    Ok(cfg)
                }
                Err(whole_file_err) => {
                    // H-D9: try per-section recovery. The pre-fix
                    // fallback discarded the *entire* user config on
                    // any single-field parse error — a typo in one
                    // endpoint's `context_window` replaced every
                    // endpoint, hook, policy preset, and memory
                    // setting with the built-in default. Here we
                    // parse the file into a `toml::Table`, deserialize
                    // each known section independently, and keep the
                    // ones that work.
                    match Self::recover_sections(&config_path) {
                        Ok(Some(mut cfg)) => {
                            tracing::error!(
                                path = %config_path.display(),
                                error = %whole_file_err,
                                "Config file could not be parsed as a whole; \
                                 recovered the sections that parse cleanly. \
                                 Broken fields use their built-in defaults.",
                            );
                            cfg.llm.validate();
                            cfg.skills.validate();
                            cfg.limits.clamp();
                            // Preserve the broken file for inspection.
                            let backup = config_path.with_extension("toml.broken");
                            let _ = std::fs::rename(&config_path, &backup);
                            eprintln!(
                                "kod: config parse error in {}: {}. The \
                                 recoverable sections were kept; the broken \
                                 file is saved as {}.",
                                config_path.display(),
                                whole_file_err,
                                backup.display(),
                            );
                            Ok(cfg)
                        }
                        _ => {
                            tracing::warn!(
                                path = %config_path.display(),
                                error = %whole_file_err,
                                "Config file present but unreadable; using built-in defaults. \
                                 Fix or delete the file to silence this warning."
                            );
                            Ok(Self::default())
                        }
                    }
                }
            }
        } else {
            // Fresh install: stamp v2 so the file the user opens is
            // the shape the current release reads. A file that
            // predates this change and lacks `config_version` loads
            // as v1 via `effective_version()`.
            let config = Self {
                config_version: 2,
                ..Self::default()
            };
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

        // Atomic write: a crash/ENOSPC mid-write previously truncated
        // the user's config.toml. tmp + fsync + rename.
        let tmp = path.with_extension("toml.tmp");
        {
            use std::io::Write as _;
            let mut f = std::fs::File::create(&tmp)
                .map_err(|e| KodError::Config(format!("Failed to create temp config: {}", e)))?;
            f.write_all(content.as_bytes())
                .and_then(|_| f.sync_all())
                .map_err(|e| KodError::Config(format!("Failed to write temp config: {}", e)))?;
        }
        std::fs::rename(&tmp, path)
            .map_err(|e| KodError::Config(format!("Failed to commit config: {}", e)))?;

        Ok(())
    }

    /// Get the configuration directory
    pub fn config_dir() -> Result<PathBuf> {
        // `KOD_CONFIG_DIR` is the kod config directory *itself* (the
        // directory that holds `config.toml`), not a parent. It is
        // consulted before the platform default so a caller can
        // localise the config path without mutating `HOME` or
        // `XDG_CONFIG_HOME` — both process-wide, both racing under a
        // parallel test runner, and `XDG_CONFIG_HOME` is ignored by
        // `dirs::config_dir` on macOS anyway. An empty value falls
        // through to the default rather than producing an empty path.
        if let Ok(dir) = std::env::var("KOD_CONFIG_DIR") {
            let dir = dir.trim();
            if !dir.is_empty() {
                return Ok(PathBuf::from(dir));
            }
        }
        dirs::config_dir()
            .map(|d| d.join("kod"))
            .ok_or_else(|| KodError::Config("Could not determine config directory".to_string()))
    }

    /// The effective long-term memory database path.
    ///
    /// Three cases, in order:
    ///
    /// 1. `memory.long_term_db_path` is set: use it. An explicit path is
    ///    the caller's intent, and no scope should override it.
    /// 2. `memory.scope = "global"` (the default): `~/.kod/data/kod.redb`.
    ///    One shared database across every project the user opens.
    /// 3. `memory.scope = "project"`: `<cwd>/.kod/memory.redb`. One
    ///    database per project, so a fact learned while working on
    ///    project A cannot leak into a prompt for project B.
    ///
    /// A caller (the `kod config` display, the engine, a future backup
    /// command) needs the path that will actually be opened, not the
    /// raw `Option` in the config file.
    pub fn memory_db_path(&self) -> Result<PathBuf> {
        if let Some(explicit) = &self.memory.long_term_db_path {
            return Ok(PathBuf::from(explicit));
        }
        match self.memory.scope {
            crate::MemoryScope::Global => dirs::home_dir()
                .map(|h| h.join(".kod").join("data").join("kod.redb"))
                .ok_or_else(|| KodError::Config("Could not determine home directory".to_string())),
            crate::MemoryScope::Project => {
                let cwd = std::env::current_dir().map_err(|e| {
                    KodError::Config(format!(
                        "Could not determine working directory for project-scoped memory: {}",
                        e
                    ))
                })?;
                Ok(cwd.join(".kod").join("memory.redb"))
            }
        }
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
        // Delta §14.2: include the common foreign skill directories
        // so a user who already curates `.claude/skills` (or
        // `.cursor/skills`) does not have to copy into kod's own tree.
        // The order is lowest-priority first: `load_from_dirs`
        // shadows by name, so a project directory beats a global one
        // of the same scope, and a kod-native directory beats a
        // foreign directory in the same scope.
        Ok(vec![
            // Foreign globals (lowest priority overall).
            home.join(".claude").join("skills"),
            home.join(".cursor").join("skills"),
            home.join(".gemini").join("skills"),
            home.join(".codex").join("skills"),
            // kod / agents globals.
            home.join(".kod").join("skills"),
            home.join(".agents").join("skills"),
            // Foreign project.
            cwd.join(".claude").join("skills"),
            cwd.join(".cursor").join("skills"),
            cwd.join(".gemini").join("skills"),
            cwd.join(".codex").join("skills"),
            // kod / agents project (highest priority).
            cwd.join(".kod").join("skills"),
            cwd.join(".agents").join("skills"),
        ])
    }

    /// Effective schema version: `1` for a config that never named
    /// one, the stored value otherwise. Callers that need to branch
    /// on the shape check this, not the raw field.
    pub fn effective_version(&self) -> u32 {
        if self.config_version == 0 {
            1
        } else {
            self.config_version
        }
    }

    /// True when the config file (as loaded) predates the v2 layout.
    pub fn needs_migration(&self) -> bool {
        self.effective_version() < 2
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn default_config_is_v2() {
        // Regression: `KodConfig::default()` used to return
        // `config_version: 0` (the derive), so any caller that saved
        // a default to disk wrote the pre-v2 shape. The explicit
        // `Default` impl stamps 2.
        let cfg = KodConfig::default();
        assert_eq!(
            cfg.config_version, 2,
            "the in-memory default must match the schema the current release writes",
        );
        assert!(
            !cfg.needs_migration(),
            "a freshly constructed default must not look like a v1 config",
        );
    }

    #[test]
    fn default_config_round_trip_preserves_v2() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("config.toml");
        KodConfig::default().save_to(&path).unwrap();
        let loaded = KodConfig::load_from(&path).unwrap();
        assert_eq!(loaded.config_version, 2);
    }

    #[test]
    fn test_default_config() {
        let config = KodConfig::default();
        assert_eq!(config.llm.default_endpoint().model, "codellama:13b");
    }

    #[test]
    fn test_config_serialization_roundtrip() {
        let config = KodConfig::default();
        let toml_str = toml::to_string(&config).unwrap();
        let deserialized: KodConfig = toml::from_str(&toml_str).unwrap();
        assert_eq!(
            config.llm.default_endpoint().model,
            deserialized.llm.default_endpoint().model
        );
    }

    #[test]
    fn test_config_load_from_file() {
        let temp_dir = tempfile::tempdir().unwrap();
        let config_path = temp_dir.path().join("config.toml");

        let config = KodConfig::default();
        config.save_to(&config_path).unwrap();

        let loaded = KodConfig::load_from(&config_path).unwrap();
        assert_eq!(
            config.llm.default_endpoint().model,
            loaded.llm.default_endpoint().model
        );
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

    /// A config.toml with only a subset of sections (or a subset of
    /// fields within a section) must parse. The v1 shape — a bare
    /// `[llm] model = "…"` at the top of the `[llm]` table — is no
    /// longer a supported spelling: in v2 a model lives on an
    /// endpoint, and the shape below names that endpoint explicitly.
    /// The test verifies that omitting sections and omitting
    /// optional fields still works.
    #[test]
    fn test_partial_config_uses_defaults() {
        // Empty file: every field defaults.
        let cfg: KodConfig = toml::from_str("").unwrap();
        assert_eq!(cfg.llm.default_endpoint().model, "codellama:13b");
        assert_eq!(cfg.memory.short_term_capacity, 100);
        assert_eq!(cfg.skills.max_skills_per_query, 3);

        // Only [skills] present: every other section defaults.
        let cfg: KodConfig = toml::from_str(
            r#"
            [skills]
            max_skills_per_query = 7
            "#,
        )
        .unwrap();
        assert_eq!(cfg.memory.short_term_capacity, 100);
        assert_eq!(cfg.skills.max_skills_per_query, 7);

        // A v2 config with a single endpoint and an explicit skills
        // block. Required endpoint fields (name, provider, base_url,
        // model, context_window) are given; optional ones
        // (temperature, max_tokens, timeout_secs) fall back to their
        // defaults.
        let cfg: KodConfig = toml::from_str(
            r#"
            [skills]
            max_skills_per_query = 7

            [[llm.endpoints]]
            name = "default"
            provider = "openai-compatible"
            base_url = "http://localhost:11434/v1"
            model = "llama3.1"
            context_window = 8192
            "#,
        )
        .unwrap();
        assert_eq!(cfg.llm.default_endpoint().model, "llama3.1");
        assert_eq!(
            cfg.llm.default_endpoint().provider,
            crate::llm::ProviderKind::OpenAICompatible,
        );
        assert_eq!(cfg.llm.default_endpoint().temperature.unwrap_or(0.7), 0.7);
        assert_eq!(cfg.llm.default_endpoint().context_window, 8192);
        assert_eq!(cfg.skills.max_skills_per_query, 7);
        assert!(cfg.skills.enable_hot_reload);
    }

    /// `memory.scope = "project"` must produce a path rooted at the
    /// current working directory, not under the home directory.
    /// Regression: the previous implementation only knew about
    /// `~/.kod/data/kod.redb`, so there was no way to scope memory per
    /// project — a fact learned on project A leaked into project B's
    /// prompts.
    #[test]
    fn test_memory_db_path_respects_project_scope() {
        let mut cfg = KodConfig::default();
        cfg.memory.scope = crate::MemoryScope::Project;
        let path = cfg.memory_db_path().unwrap();
        let cwd = std::env::current_dir().unwrap();
        assert!(
            path.starts_with(&cwd),
            "project scope must root at cwd, got {}",
            path.display()
        );
        assert!(
            path.to_string_lossy().ends_with(".kod/memory.redb")
                || path.to_string_lossy().ends_with(".kod\\memory.redb"),
            "expected .kod/memory.redb suffix, got {}",
            path.display()
        );
    }

    /// Global scope (the default) keeps the existing behavior:
    /// `~/.kod/data/kod.redb`.
    #[test]
    fn test_memory_db_path_global_scope_under_home() {
        let cfg = KodConfig::default();
        assert_eq!(cfg.memory.scope, crate::MemoryScope::Global);
        let path = cfg.memory_db_path().unwrap();
        if let Some(home) = dirs::home_dir() {
            assert!(
                path.starts_with(&home),
                "global scope must root at home, got {}",
                path.display()
            );
        }
        assert!(path.to_string_lossy().contains(".kod"));
        assert!(path.to_string_lossy().contains("data"));
    }

    /// An explicit `long_term_db_path` wins over either scope — a user
    /// who named a specific file meant it.
    #[test]
    fn test_memory_db_path_explicit_overrides_scope() {
        let mut cfg = KodConfig::default();
        cfg.memory.scope = crate::MemoryScope::Project;
        cfg.memory.long_term_db_path = Some("/tmp/custom-memory.redb".to_string());
        let path = cfg.memory_db_path().unwrap();
        assert_eq!(path, PathBuf::from("/tmp/custom-memory.redb"));
    }

    /// A config.toml with only a subset of sections (or a subset of
    /// fields within a section) must parse. Before `#[serde(default)]`
    /// at container level, a file containing only `[llm]` failed with
    /// "missing field `memory`", and a file containing only `[llm]
    /// model = "…"` additionally failed with "missing field
    /// `provider`". Both are the common hand-edited shape.
    #[test]
    fn lsp_section_defaults_when_absent() {
        // A config.toml with no `[lsp]` block still parses and gets
        // the documented defaults: auto_diagnostics = true,
        // settle_ms = 500 (short inline wait), deferred_settle_ms =
        // 12000, deferred_enabled = true.
        let cfg: KodConfig = toml::from_str("").unwrap();
        assert!(cfg.lsp.auto_diagnostics);
        assert_eq!(cfg.lsp.settle_ms, 500);
        assert_eq!(cfg.lsp.deferred_settle_ms, 12_000);
        assert!(cfg.lsp.deferred_enabled);
    }

    #[test]
    fn lsp_section_overrides_are_honoured() {
        let cfg: KodConfig = toml::from_str(
            r#"
            [lsp]
            auto_diagnostics = false
            settle_ms = 400
            deferred_settle_ms = 8000
            deferred_enabled = false
            "#,
        )
        .unwrap();
        assert!(!cfg.lsp.auto_diagnostics);
        assert_eq!(cfg.lsp.settle_ms, 400);
        assert_eq!(cfg.lsp.deferred_settle_ms, 8_000);
        assert!(!cfg.lsp.deferred_enabled);
    }

    #[test]
    fn test_corrupt_file_yields_error_from_load_from() {
        let temp_dir = tempfile::tempdir().unwrap();
        let path = temp_dir.path().join("config.toml");
        std::fs::write(&path, "not = valid toml [ =").unwrap();

        // Strict API errors, so load_default's fallback branch triggers.
        assert!(KodConfig::load_from(&path).is_err());
        // Defaults, by construction, have the documented fields.
        let defaults = KodConfig::default();
        assert_eq!(defaults.llm.default_endpoint().model, "codellama:13b");
    }
}

#[cfg(test)]
mod coverage_hooks_config {
    //! `HooksConfig` is empty and disabled by default. A regression
    //! that flipped `enabled` or seeded a default hook would make
    //! every session shell out to a command the user never
    //! configured.
    use super::*;

    #[test]
    fn default_is_empty_and_disabled() {
        let h = HooksConfig::default();
        assert!(!h.enabled);
        assert!(h.pre_tool_use.is_empty());
        assert!(h.post_tool_use.is_empty());
    }

    #[test]
    fn absent_section_in_toml_defaults_to_disabled() {
        // A config.toml without a `[hooks]` block produces the
        // default. The container-level `#[serde(default)]` is what
        // makes that work.
        let cfg: KodConfig = toml::from_str("").unwrap();
        assert!(!cfg.hooks.enabled);
        assert!(cfg.hooks.pre_tool_use.is_empty());
        assert!(cfg.hooks.post_tool_use.is_empty());
    }

    #[test]
    fn a_partial_hooks_block_keeps_the_missing_fields_at_their_defaults() {
        let cfg: KodConfig = toml::from_str(
            r#"
            [hooks]
            enabled = true
            "#,
        )
        .unwrap();
        assert!(cfg.hooks.enabled);
        assert!(cfg.hooks.pre_tool_use.is_empty());
        assert!(cfg.hooks.post_tool_use.is_empty());
    }

    #[test]
    fn hook_maps_parse_from_toml() {
        let cfg: KodConfig = toml::from_str(
            r#"
            [hooks]
            enabled = true

            [hooks.pre_tool_use]
            write_file = "rustfmt {path}"

            [hooks.post_tool_use]
            "write_file.fmt" = "cargo check"
            "#,
        )
        .unwrap();
        assert!(cfg.hooks.enabled);
        assert_eq!(
            cfg.hooks.pre_tool_use.get("write_file").map(String::as_str),
            Some("rustfmt {path}"),
        );
        assert_eq!(
            cfg.hooks
                .post_tool_use
                .get("write_file.fmt")
                .map(String::as_str),
            Some("cargo check"),
        );
    }

    #[test]
    fn hooks_config_round_trips_through_toml() {
        let mut h = HooksConfig::default();
        h.enabled = true;
        h.pre_tool_use.insert("write_file".into(), "fmt".into());
        let s = toml::to_string(&h).unwrap();
        let parsed: HooksConfig = toml::from_str(&s).unwrap();
        assert!(parsed.enabled);
        assert_eq!(
            parsed.pre_tool_use.get("write_file").map(String::as_str),
            Some("fmt"),
        );
    }
}
