//! Per-project policy (D3-C1, AD-08).
//!
//! A `Policy` says what the agent is allowed to do, project by project.
//! It is read from `.kod/policy.toml` in the project root, merged over
//! a preset (read-only / standard / yolo), and consulted by the engine
//! before every tool call. The decision replaces the ad-hoc
//! `ToolsConfig::confirm_writes` flag and the scattered
//! `ToolPermissions` bitmask that used to gate behaviour.
//!
//! # Layers and precedence
//!
//! Four layers, later overriding earlier:
//!
//! 1. **Preset** — `read-only`, `standard`, `yolo`. The default preset
//!    is `standard`.
//! 2. **Global config** — the `[tools]` section of
//!    `~/.config/kod/config.toml`. Today this carries
//!    `confirm_writes`, which maps to `standard` (true) or `yolo`
//!    (false) for backward compatibility.
//! 3. **Project policy** — `.kod/policy.toml`, committed to the repo
//!    like an `.editorconfig`.
//! 4. **CLI override** — `--preset yolo` on the command line.
//!
//! Within a layer, `[tools.<name>]` entries give a per-tool decision
//! (`allow` / `deny` / `ask`), plus optional path globs and command
//! allowlists. A `deny` always wins over an `allow` at glob-conflict
//! time — the same rule `ToolContext::is_path_allowed` applies.
//!
//! # Contract
//!
//! `PolicyEngine::decide` is pure with respect to the tool call: it
//! takes the tool name, the JSON arguments, and the effective
//! `ToolContext` (whose `working_dir` anchors relative paths), and
//! returns a `PolicyDecision { outcome, rule, source }`. The engine
//! logs this as `SessionEntry::PolicyDecision` and either runs,
//! refuses, or asks for approval.

use crate::KodConfig;
use kod_error::{KodError, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::{BTreeMap, HashSet};
use std::path::{Path, PathBuf};

/// Which preset a policy was seeded from. Presets are the coarse
/// "what kind of session is this" choice; per-tool overrides live in
/// `[tools.<name>]` on top.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Preset {
    /// Reads allowed, everything else denied. `write_file`,
    /// `patch_file`, `execute_command` all return Deny without asking.
    ReadOnly,
    /// Reads and non-mutating commands allowed; writes and shell
    /// commands require approval. The default.
    Standard,
    /// Everything allowed without a prompt. The pre-PolicyEngine
    /// behaviour of a config with `confirm_writes = false`.
    Yolo,
}

impl Default for Preset {
    fn default() -> Self {
        Preset::Standard
    }
}

/// What a `PolicyEngine::decide` call returns for one tool call.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Decision {
    Allow,
    Deny,
    Ask,
}

/// Which layer produced a decision. Used for logging and for
/// `kod policy explain`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum PolicySource {
    Preset,
    GlobalConfig,
    ProjectPolicy,
    CliOverride,
    SessionDeny,
}

/// Per-tool policy override.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ToolPolicy {
    /// Override the preset's decision for this tool.
    #[serde(default)]
    pub mode: Option<Decision>,
    /// Allow-list of path globs. When present, a path that does not
    /// match any of them is denied.
    #[serde(default)]
    pub paths: Option<Vec<String>>,
    /// Forbidden path globs. Deny always wins over an allow.
    #[serde(default)]
    pub forbidden: Option<Vec<String>>,
    /// Command allow-list. Meaningful for `execute_command` only; a
    /// command whose first token is not in this list is denied. Never a
    /// security guarantee — the sandbox is (AD-10).
    #[serde(default)]
    pub binaries: Option<Vec<String>>,
    /// Command deny-list. Deny wins over the allow-list.
    #[serde(default)]
    pub forbidden_binaries: Option<Vec<String>>,
    /// `network_access` for tools that reach the network. Overrides
    /// the global `llm.network_access` for this tool.
    #[serde(default)]
    pub network: Option<bool>,
    /// Domain allow-list for `web_fetch` when network is enabled.
    #[serde(default)]
    pub domains: Option<Vec<String>>,
}

/// Git-specific policy.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GitPolicy {
    /// `.git` is mounted read-only at the sandbox. Default true.
    #[serde(default = "default_true")]
    pub history_protected: bool,
}

impl Default for GitPolicy {
    fn default() -> Self {
        Self {
            history_protected: true,
        }
    }
}

fn default_true() -> bool {
    true
}

/// The full policy: a preset, per-tool overrides, and a git block.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Policy {
    #[serde(default)]
    pub preset: Preset,
    #[serde(default)]
    pub tools: BTreeMap<String, ToolPolicy>,
    #[serde(default)]
    pub git: GitPolicy,
}

impl Default for Policy {
    fn default() -> Self {
        Self {
            preset: Preset::Standard,
            tools: BTreeMap::new(),
            git: GitPolicy::default(),
        }
    }
}

impl Policy {
    /// Parse a policy from a TOML string.
    pub fn from_toml(s: &str) -> Result<Self> {
        toml::from_str(s)
            .map_err(|e| KodError::Config(format!("policy.toml: {e}")))
    }

    /// Load a policy from `.kod/policy.toml` under `root`. `Ok(None)`
    /// when the file does not exist — the absence is normal; the
    /// caller merges with what the preset supplies.
    pub fn load_project(root: &Path) -> Result<Option<(Self, PathBuf)>> {
        let path = root.join(".kod").join("policy.toml");
        if !path.is_file() {
            return Ok(None);
        }
        let raw = std::fs::read_to_string(&path).map_err(|e| {
            KodError::Config(format!("{}: {e}", path.display()))
        })?;
        Ok(Some((Self::from_toml(&raw)?, path)))
    }
}

/// One resolved decision with provenance.
#[derive(Debug, Clone)]
pub struct PolicyDecision {
    pub outcome: Decision,
    /// A one-line description of the rule that fired, for
    /// `kod policy explain` and the SessionEntry log.
    pub rule: String,
    pub source: PolicySource,
}

/// The effective policy after merging all four layers, ready to
/// answer `decide` questions.
pub struct PolicyEngine {
    effective: Policy,
    /// Provenance per (tool, field) — used by `policy explain`.
    sources: BTreeMap<String, PolicySource>,
}

impl PolicyEngine {
    /// Build the effective policy from a full `KodConfig` (which
    /// carries the global `[tools]` section), an optional project
    /// root, and an optional CLI `--preset` override.
    ///
    /// `KodConfig::tools.confirm_writes` is interpreted as a legacy
    /// alias: `true` maps to `Preset::Standard`, `false` to
    /// `Preset::Yolo`. It is only consulted when the preset layer is
    /// not otherwise set (i.e. the policy.toml has no `preset =` line
    /// and the CLI has not supplied one).
    pub fn load(
        cfg: &KodConfig,
        project_root: Option<&Path>,
        cli_preset: Option<Preset>,
    ) -> Result<Self> {
        let mut effective = Policy::default();
        let mut sources: BTreeMap<String, PolicySource> = BTreeMap::new();

        // Layer 1: preset, defaulting to the global config's
        // confirm_writes interpretation.
        effective.preset = match cli_preset {
            Some(_) => Preset::Standard, // CLI override applied below
            None => {
                if cfg.tools.confirm_writes {
                    Preset::Standard
                } else {
                    // confirm_writes was false: the pre-PolicyEngine
                    // behaviour is Yolo (no prompts). Preserve it for
                    // a config that has not opted in to the new
                    // system.
                    Preset::Yolo
                }
            }
        };
        sources.insert("preset".to_string(), PolicySource::GlobalConfig);

        // Layer 3: project policy overrides the preset and merges
        // the tools map.
        if let Some(root) = project_root
            && let Some((project, _path)) = Policy::load_project(root)?
        {
            // A project can override the preset explicitly.
            if !matches!(project.preset, Preset::Standard) {
                // Only treat a non-Standard value as an override —
                // otherwise the default of `Policy::default()`
                // (Standard) would always win over the global config's
                // derived preset, which is not the intent.
                effective.preset = project.preset;
                sources.insert("preset".to_string(), PolicySource::ProjectPolicy);
            }
            for (tool, tp) in project.tools {
                effective.tools.insert(tool.clone(), tp);
                sources.insert(tool, PolicySource::ProjectPolicy);
            }
            effective.git = project.git;
        }

        // Layer 4: CLI override wins.
        if let Some(p) = cli_preset {
            effective.preset = p;
            sources.insert("preset".to_string(), PolicySource::CliOverride);
        }

        Ok(Self {
            effective,
            sources,
        })
    }

    /// Build a `PolicyEngine` directly from an already-merged
    /// `Policy`. Public so tests — and callers that construct a
    /// policy programmatically rather than via the config layering —
    /// can bypass `load`.
    pub fn from_effective_for_tests(policy: Policy) -> Self {
        Self {
            effective: policy,
            sources: BTreeMap::new(),
        }
    }

    /// The effective policy.
    pub fn effective(&self) -> &Policy {
        &self.effective
    }

    /// What layer set the policy for `tool`, if there is a per-tool
    /// entry. For preset-only decisions the answer is `Preset`.
    pub fn source_for(&self, tool: &str) -> PolicySource {
        self.sources
            .get(tool)
            .cloned()
            .unwrap_or(PolicySource::Preset)
    }

    /// Decide a tool call.
    ///
    /// `working_dir` is the context's working directory; relative
    /// path arguments are resolved against it before glob matching,
    /// matching the tool's own resolution. `session_denies` is the
    /// set of `(tool, path-pattern)` pairs the user said "never" to
    /// during this session — always checked first.
    pub fn decide(
        &self,
        tool: &str,
        args: &Value,
        working_dir: &Path,
        session_denies: &HashSet<SessionDeny>,
    ) -> PolicyDecision {
        // 0. Session "never" wins everything.
        let path_arg = extract_path_arg(args);
        let resolved_path = path_arg
            .as_deref()
            .map(|p| resolve_path(working_dir, p));
        for deny in session_denies {
            if deny.tool == tool {
                match (&deny.path_pattern, &resolved_path) {
                    (None, _) => {
                        return PolicyDecision {
                            outcome: Decision::Deny,
                            rule: format!("session deny for tool {tool}"),
                            source: PolicySource::SessionDeny,
                        };
                    }
                    (Some(pattern), Some(p)) if glob_matches(pattern, p, working_dir) => {
                        return PolicyDecision {
                            outcome: Decision::Deny,
                            rule: format!(
                                "session deny for tool {tool} at path pattern {pattern:?}"
                            ),
                            source: PolicySource::SessionDeny,
                        };
                    }
                    _ => {}
                }
            }
        }

        // 1. Per-tool policy overrides the preset.
        if let Some(tp) = self.effective.tools.get(tool) {
            // Forbidden paths: any match is a hard deny.
            if let (Some(forbidden), Some(p)) = (&tp.forbidden, &resolved_path) {
                for g in forbidden {
                    if glob_matches(g, p, working_dir) {
                        return PolicyDecision {
                            outcome: Decision::Deny,
                            rule: format!("{tool} forbids path pattern {g:?}"),
                            source: self.source_for(tool),
                        };
                    }
                }
            }
            // Command binaries (execute_command only): a forbidden
            // binary is a hard deny; a non-matching binary when an
            // allow-list is present is also a deny.
            if tool == "execute_command"
                && let Some(cmd) = args.get("command").and_then(|v| v.as_str())
            {
                let first = cmd
                    .trim_start()
                    .split_whitespace()
                    .next()
                    .unwrap_or("")
                    .rsplit('/')
                    .next()
                    .unwrap_or("");
                if let Some(forbidden) = &tp.forbidden_binaries
                    && forbidden.iter().any(|b| b == first)
                {
                    return PolicyDecision {
                        outcome: Decision::Deny,
                        rule: format!("{tool} forbids binary {first:?}"),
                        source: self.source_for(tool),
                    };
                }
                if let Some(allow) = &tp.binaries
                    && !allow.iter().any(|b| b == first)
                {
                    return PolicyDecision {
                        outcome: Decision::Deny,
                        rule: format!(
                            "{tool} allow-list does not include binary {first:?}"
                        ),
                        source: self.source_for(tool),
                    };
                }
            }
            // Path allow-list: if set, a path not matching any glob is
            // a deny.
            if let (Some(allowed), Some(p)) = (&tp.paths, &resolved_path)
                && !allowed.iter().any(|g| glob_matches(g, p, working_dir))
            {
                return PolicyDecision {
                    outcome: Decision::Deny,
                    rule: format!(
                        "{tool} allow-list {allowed:?} does not include {p:?}",
                    ),
                    source: self.source_for(tool),
                };
            }
            // Explicit mode wins.
            if let Some(mode) = tp.mode {
                return PolicyDecision {
                    outcome: mode,
                    rule: format!("{tool} mode = {mode:?}"),
                    source: self.source_for(tool),
                };
            }
        }

        // 2. Preset fallback.
        let outcome = preset_decision(self.effective.preset, tool);
        PolicyDecision {
            outcome,
            rule: format!("preset {:?} applies", self.effective.preset),
            source: PolicySource::Preset,
        }
    }

    /// The git history protection flag.
    pub fn git_history_protected(&self) -> bool {
        self.effective.git.history_protected
    }

    /// One-line summary of the effective policy, for `policy show`.
    pub fn describe(&self) -> String {
        let mut lines = Vec::new();
        lines.push(format!("preset: {:?}", self.effective.preset));
        if self.effective.tools.is_empty() {
            lines.push("(no per-tool overrides)".to_string());
        } else {
            for (name, tp) in &self.effective.tools {
                let mode = tp
                    .mode
                    .map(|m| format!("{m:?}"))
                    .unwrap_or_else(|| "(preset)".to_string());
                lines.push(format!("  {name}: mode = {mode}"));
            }
        }
        lines.push(format!(
            "git.history_protected: {}",
            self.effective.git.history_protected
        ));
        lines.join("\n")
    }
}

/// A session-scoped "never" rule. `path_pattern` is `None` for a
/// tool-wide rule (`a` was pressed without a path-specific context —
/// e.g. execute_command on an unknown binary).
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct SessionDeny {
    pub tool: String,
    pub path_pattern: Option<String>,
}

/// The preset decision for a tool name when no `[tools.<name>]`
/// override applies. The mapping is intentionally conservative:
///
/// - `ReadOnly`: read-only tools allow, everything else denies.
/// - `Standard`: read-only tools allow, mutating tools ask.
/// - `Yolo`: everything allows.
fn preset_decision(preset: Preset, tool: &str) -> Decision {
    match preset {
        Preset::Yolo => Decision::Allow,
        Preset::Standard | Preset::ReadOnly => {
            // A small read-only set. `web_fetch` is read-only in
            // spirit but crosses a network boundary, so it asks in
            // Standard and denies in ReadOnly — the caller decides via
            // an explicit `[tools.web_fetch]` override to allow it.
            let read_only = matches!(
                tool,
                "read_file"
                    | "list_files"
                    | "grep"
                    | "search_files"
                    | "file_info"
                    | "git_status"
                    | "git_diff"
                    | "lsp_diagnostics"
                    | "lsp_definition"
                    | "lsp_references"
                    | "lsp_hover"
                    | "check"
            );
            if read_only {
                Decision::Allow
            } else {
                match preset {
                    Preset::Standard => Decision::Ask,
                    Preset::ReadOnly => Decision::Deny,
                    Preset::Yolo => Decision::Allow,
                }
            }
        }
    }
}

/// Extract the `path` or `file` argument from a tool call, if any.
fn extract_path_arg(args: &Value) -> Option<String> {
    for key in ["path", "file"] {
        if let Some(s) = args.get(key).and_then(|v| v.as_str()) {
            return Some(s.to_string());
        }
    }
    None
}

/// Resolve a possibly-relative path against `working_dir`. No
/// canonicalization: the policy engine compares lexically against
/// globs the user wrote with `src/**` etc., and canonicalization
/// would fail on paths that do not exist yet (the exact case a write
/// pre-approval cares about).
fn resolve_path(working_dir: &Path, p: &str) -> PathBuf {
    let path = Path::new(p);
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        working_dir.join(path)
    }
}

/// Match a glob pattern against a path.
///
/// `path` is expected to have been resolved against `working_dir`
/// (see `resolve_path`), so it is usually absolute. The matcher
/// tries three target forms for each pattern:
///
/// 1. The path verbatim — the right form for absolute patterns like
///    `/home/user/secret/**`.
/// 2. The path relative to `working_dir` — the right form for
///    project-relative patterns like `src/**` and `**/.env`, which
///    is what a `.kod/policy.toml` author writes.
/// 3. The path verbatim or relative, prefixed with `**/` — because
///    `globset`'s `src/**` does not match a `src/main.rs` relative
///    path at the top level in the way a shell glob would. The
///    `**/` prefix is the standard "anywhere in the tree" idiom.
///
/// Uses `globset` with `literal_separator(true)`, the default, so
/// `*` does not cross `/` and `**` does — the same semantics git
/// uses for `.gitignore`.
fn glob_matches(pattern: &str, path: &Path, working_dir: &Path) -> bool {
    use globset::GlobBuilder;

    let build = |pat: &str| -> Option<globset::GlobMatcher> {
        GlobBuilder::new(pat)
            .literal_separator(true)
            .build()
            .ok()
            .map(|g| g.compile_matcher())
    };

    let matcher = match build(pattern) {
        Some(m) => m,
        None => return false,
    };

    // Form 1: verbatim.
    if matcher.is_match(path) {
        return true;
    }
    // Form 2: relative to working_dir.
    let relative = path.strip_prefix(working_dir).unwrap_or(path);
    if matcher.is_match(relative) {
        return true;
    }
    // Form 3: prepend `**/` unless the pattern is already anchored.
    if !pattern.starts_with("**") && !pattern.starts_with('/')
        && let Some(m2) = build(&format!("**/{pattern}"))
    {
        if m2.is_match(path) || m2.is_match(relative) {
            return true;
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg_with_confirm(confirm: bool) -> KodConfig {
        let mut c = KodConfig::default();
        c.tools.confirm_writes = confirm;
        c
    }

    fn engine(preset: Preset) -> PolicyEngine {
        PolicyEngine {
            effective: Policy {
                preset,
                ..Policy::default()
            },
            sources: BTreeMap::new(),
        }
    }

    #[test]
    fn read_only_preset_allows_reads_denies_writes() {
        let e = engine(Preset::ReadOnly);
        let wd = Path::new("/tmp");
        let d = e.decide("read_file", &serde_json::json!({"path": "a.txt"}), wd, &HashSet::new());
        assert_eq!(d.outcome, Decision::Allow);
        let d = e.decide("write_file", &serde_json::json!({"path": "a.txt"}), wd, &HashSet::new());
        assert_eq!(d.outcome, Decision::Deny);
    }

    #[test]
    fn standard_preset_asks_for_writes() {
        let e = engine(Preset::Standard);
        let wd = Path::new("/tmp");
        let d = e.decide("write_file", &serde_json::json!({"path": "a.txt"}), wd, &HashSet::new());
        assert_eq!(d.outcome, Decision::Ask);
        let d = e.decide("execute_command", &serde_json::json!({"command": "ls"}), wd, &HashSet::new());
        assert_eq!(d.outcome, Decision::Ask);
    }

    #[test]
    fn yolo_preset_allows_everything() {
        let e = engine(Preset::Yolo);
        let wd = Path::new("/tmp");
        for tool in ["write_file", "execute_command", "patch_file"] {
            let d = e.decide(tool, &serde_json::json!({}), wd, &HashSet::new());
            assert_eq!(d.outcome, Decision::Allow, "{tool}");
        }
    }

    #[test]
    fn per_tool_mode_overrides_preset() {
        let mut effective = Policy::default();
        effective.preset = Preset::ReadOnly;
        effective.tools.insert(
            "write_file".to_string(),
            ToolPolicy {
                mode: Some(Decision::Allow),
                ..Default::default()
            },
        );
        let e = PolicyEngine {
            effective,
            sources: BTreeMap::new(),
        };
        let wd = Path::new("/tmp");
        let d = e.decide("write_file", &serde_json::json!({"path": "a.txt"}), wd, &HashSet::new());
        assert_eq!(d.outcome, Decision::Allow);
    }

    #[test]
    fn forbidden_path_wins_over_mode() {
        let mut effective = Policy::default();
        effective.preset = Preset::Yolo;
        effective.tools.insert(
            "write_file".to_string(),
            ToolPolicy {
                mode: Some(Decision::Allow),
                forbidden: Some(vec!["**/.env".to_string()]),
                ..Default::default()
            },
        );
        let e = PolicyEngine {
            effective,
            sources: BTreeMap::new(),
        };
        let wd = Path::new("/tmp/proj");
        let d = e.decide("write_file", &serde_json::json!({"path": ".env"}), wd, &HashSet::new());
        assert_eq!(d.outcome, Decision::Deny);
        let d = e.decide("write_file", &serde_json::json!({"path": "src/main.rs"}), wd, &HashSet::new());
        assert_eq!(d.outcome, Decision::Allow);
    }

    #[test]
    fn execute_command_binary_allowlist() {
        let mut effective = Policy::default();
        effective.preset = Preset::Yolo;
        effective.tools.insert(
            "execute_command".to_string(),
            ToolPolicy {
                binaries: Some(vec!["cargo".to_string(), "rustc".to_string()]),
                ..Default::default()
            },
        );
        let e = PolicyEngine {
            effective,
            sources: BTreeMap::new(),
        };
        let wd = Path::new("/tmp");
        let d = e.decide("execute_command", &serde_json::json!({"command": "cargo test"}), wd, &HashSet::new());
        assert_eq!(d.outcome, Decision::Allow);
        let d = e.decide("execute_command", &serde_json::json!({"command": "rm -rf"}), wd, &HashSet::new());
        assert_eq!(d.outcome, Decision::Deny);
    }

    #[test]
    fn session_deny_wins_over_everything() {
        let e = engine(Preset::Yolo);
        let wd = Path::new("/tmp");
        let mut denies = HashSet::new();
        denies.insert(SessionDeny {
            tool: "write_file".to_string(),
            path_pattern: Some("src/**".to_string()),
        });
        let d = e.decide("write_file", &serde_json::json!({"path": "src/main.rs"}), wd, &denies);
        assert_eq!(d.outcome, Decision::Deny);
        assert_eq!(d.source, PolicySource::SessionDeny);
        // Another path is unaffected.
        let d = e.decide("write_file", &serde_json::json!({"path": "docs/readme.md"}), wd, &denies);
        assert_eq!(d.outcome, Decision::Allow);
    }

    #[test]
    fn policy_parses_from_toml() {
        let p = Policy::from_toml(
            r#"
            preset = "read-only"

            [tools.write_file]
            mode = "ask"
            paths = ["src/**", "docs/**"]
            forbidden = ["**/.env"]

            [git]
            history_protected = true
            "#,
        )
        .unwrap();
        assert_eq!(p.preset, Preset::ReadOnly);
        let tp = p.tools.get("write_file").unwrap();
        assert_eq!(tp.mode, Some(Decision::Ask));
        assert_eq!(tp.paths.as_ref().unwrap().len(), 2);
    }

    #[test]
    fn missing_policy_file_is_none() {
        let tmp = tempfile::TempDir::new().unwrap();
        assert!(Policy::load_project(tmp.path()).unwrap().is_none());
    }

    #[test]
    fn legacy_confirm_writes_false_maps_to_yolo() {
        let cfg = cfg_with_confirm(false);
        let e = PolicyEngine::load(&cfg, None, None).unwrap();
        assert_eq!(e.effective().preset, Preset::Yolo);
    }

    #[test]
    fn legacy_confirm_writes_true_maps_to_standard() {
        let cfg = cfg_with_confirm(true);
        let e = PolicyEngine::load(&cfg, None, None).unwrap();
        assert_eq!(e.effective().preset, Preset::Standard);
    }

    #[test]
    fn cli_preset_wins_over_project_policy() {
        let tmp = tempfile::TempDir::new().unwrap();
        std::fs::create_dir_all(tmp.path().join(".kod")).unwrap();
        std::fs::write(
            tmp.path().join(".kod").join("policy.toml"),
            "preset = \"read-only\"\n",
        )
        .unwrap();
        let cfg = cfg_with_confirm(false);
        let e = PolicyEngine::load(&cfg, Some(tmp.path()), Some(Preset::Yolo)).unwrap();
        assert_eq!(e.effective().preset, Preset::Yolo);
    }

    #[test]
    fn project_policy_overrides_global_config() {
        let tmp = tempfile::TempDir::new().unwrap();
        std::fs::create_dir_all(tmp.path().join(".kod")).unwrap();
        std::fs::write(
            tmp.path().join(".kod").join("policy.toml"),
            "preset = \"read-only\"\n",
        )
        .unwrap();
        let cfg = cfg_with_confirm(false); // would be Yolo alone
        let e = PolicyEngine::load(&cfg, Some(tmp.path()), None).unwrap();
        assert_eq!(e.effective().preset, Preset::ReadOnly);
    }
}
