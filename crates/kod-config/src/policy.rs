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
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
#[derive(Default)]
pub enum Preset {
    /// Reads allowed, everything else denied. `write_file`,
    /// `patch_file`, `execute_command` all return Deny without asking.
    ReadOnly,
    /// Reads and non-mutating commands allowed; writes and shell
    /// commands require approval. The default.
    #[default]
    Standard,
    /// Everything allowed without a prompt. The pre-PolicyEngine
    /// behaviour of a config with `confirm_writes = false`.
    Yolo,
}

/// What a `PolicyEngine::decide` call returns for one tool call.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Decision {
    /// Most permissive.
    Allow,
    Ask,
    /// Most restrictive. Ordering: Allow < Ask < Deny, so the
    /// "narrower of two decisions" is `max(a, b)`.
    Deny,
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

/// How `read_file` / `list_files` treat known-secret paths (Tier 1.3).
///
/// The default is `redact`: the file is read, matches from the
/// redactor's rule set are replaced by `[REDACTED:<rule>]` markers,
/// and a one-line header is prepended so the model knows content was
/// removed. `Refuse` returns a `ToolResult::Error` for the matched
/// path instead — a stricter posture for a shared or regulated
/// environment. `Allow` disables the check for this session; it is
/// the right choice for a caller that has already reviewed its own
/// secret handling.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum ReadMode {
    #[default]
    Redact,
    Refuse,
    Allow,
}

/// Read-protection rules for high-risk paths. Patterns are matched
/// as globs against the *resolved* path (same engine as forbidden
/// paths, so a project-relative glob and an absolute pattern both
/// work). The default list covers the conventional locations of
/// long-lived credentials in a developer's home directory and any
/// project tree.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct ReadProtection {
    pub enabled: bool,
    pub mode: ReadMode,
    pub deny: Vec<String>,
}

impl Default for ReadProtection {
    fn default() -> Self {
        Self {
            enabled: true,
            mode: ReadMode::Redact,
            deny: vec![
                ".env".to_string(),
                ".env.*".to_string(),
                "**/.env".to_string(),
                "**/.env.*".to_string(),
                "**/*.pem".to_string(),
                "**/*.key".to_string(),
                "**/id_rsa*".to_string(),
                "**/id_ed25519*".to_string(),
                "**/id_ecdsa*".to_string(),
                "**/.aws/credentials".to_string(),
                "**/.aws/config".to_string(),
                "**/.ssh/**".to_string(),
                "**/.docker/config.json".to_string(),
                "**/.netrc".to_string(),
                "**/.pgpass".to_string(),
                "**/credentials.json".to_string(),
                "**/service-account*.json".to_string(),
            ],
        }
    }
}

impl ReadProtection {
    /// Does `path` match any deny glob?
    pub fn matches(&self, path: &std::path::Path) -> bool {
        if !self.enabled {
            return false;
        }
        let path_str = path.to_string_lossy();
        let basename = path.file_name().and_then(|s| s.to_str()).unwrap_or("");
        for pat in &self.deny {
            // Same glob semantics as the policy engine's forbidden
            // paths: a pattern with a slash is matched against the
            // full path; a pattern without is matched against the
            // basename at any depth.
            let (target, pat_use) = if pat.contains('/') {
                (path_str.as_ref(), pat.as_str())
            } else {
                (basename, pat.as_str())
            };
            if let Ok(glob) = globset::Glob::new(pat_use)
                && glob.compile_matcher().is_match(target)
            {
                return true;
            }
        }
        false
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Policy {
    #[serde(default)]
    pub preset: Preset,
    /// Non-serialized. `true` when the source TOML contained an
    /// explicit `preset = ` line. `Policy::from_toml` sets it; the
    /// engine's layering uses it to tell "the project chose
    /// Standard" from "the project did not choose, and Standard is
    /// the struct's default". Without this flag, a project that
    /// wants to pin `standard` cannot override a global
    /// `[tools] preset = "yolo"`.
    #[serde(skip)]
    pub preset_explicit: bool,
    #[serde(default)]
    pub tools: BTreeMap<String, ToolPolicy>,
    #[serde(default)]
    pub git: GitPolicy,

    /// Read-protection rules for known-secret paths (Tier 1.3).
    #[serde(default)]
    pub read_protection: ReadProtection,
}

impl Default for Policy {
    fn default() -> Self {
        Self {
            preset: Preset::Standard,
            preset_explicit: false,
            tools: BTreeMap::new(),
            git: GitPolicy::default(),
            read_protection: ReadProtection::default(),
        }
    }
}

impl Policy {
    /// Parse a policy from a TOML string.
    pub fn from_toml(s: &str) -> Result<Self> {
        let mut p: Self =
            toml::from_str(s).map_err(|e| KodError::Config(format!("policy.toml: {e}")))?;
        // Presence check for `preset =` at the start of a line. A
        // crude scan is sufficient: TOML keys start a line (after
        // whitespace); the substring cannot appear in a value that
        // matters for this file.
        p.preset_explicit = s.lines().any(|l| l.trim_start().starts_with("preset"));
        Ok(p)
    }

    /// Load a policy from `.kod/policy.toml` under `root`. `Ok(None)`
    /// when the file does not exist — the absence is normal; the
    /// caller merges with what the preset supplies.
    pub fn load_project(root: &Path) -> Result<Option<(Self, PathBuf)>> {
        let path = root.join(".kod").join("policy.toml");
        if !path.is_file() {
            return Ok(None);
        }
        let raw = std::fs::read_to_string(&path)
            .map_err(|e| KodError::Config(format!("{}: {e}", path.display())))?;
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
    /// The effective read-protection rules — the highest layer that
    /// set the block wins, matching the policy engine's standard
    /// layering.
    pub fn read_protection(&self) -> &ReadProtection {
        &self.effective().read_protection
    }

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

        // Layer 1: preset. Three sources, in priority order:
        //
        //   1. `[tools] preset = "…"` in the global config (parsed from
        //      a string; a bad value is a warning and falls through to
        //      the default, not a hard error — a typo in a config
        //      field must not make the session unusable).
        //   2. Built-in default: `Standard`. The pre-policy default
        //      (`Yolo`) was the wrong side of the trade for a
        //      user-facing agent; the design's §12 migration makes
        //      "writes require approval" the default, and a user who
        //      wants the old behaviour writes `preset = "yolo"` or
        //      passes `--preset yolo`.
        //   3. CLI override applied below (the last writer wins).
        effective.preset = match cfg.tools.preset.as_deref() {
            Some("read-only") | Some("readonly") => Preset::ReadOnly,
            Some("standard") | Some("default") => Preset::Standard,
            Some("yolo") | Some("unrestricted") => Preset::Yolo,
            Some(other) => {
                tracing::warn!(
                    value = %other,
                    "unknown [tools] preset; falling back to Standard.                      Known values: read-only, standard, yolo",
                );
                Preset::Standard
            }
            None => Preset::Standard,
        };
        sources.insert("preset".to_string(), PolicySource::GlobalConfig);
        // Note: the CLI override is applied further down (Layer 4); the
        // priority above already establishes the correct layering.

        // Layer 3: project policy. H-S2 — a project layer may narrow
        // the effective policy, never widen it. The pre-fix behaviour
        // let a repo shipping `preset = "yolo"` (or
        // `[tools.write_file] mode = "allow"`) escalate past the
        // user's global `standard` with no consent gate. An
        // `.editorconfig`-shaped file should not be able to grant
        // shell-exec rights.
        //
        // Narrowing rules:
        //   - Preset: `min(global, project)` in the Preset ordering
        //     (`ReadOnly < Standard < Yolo`), so a project may only
        //     choose the same or a *stricter* preset.
        //   - Per-tool mode: `max(global_decision, project_decision)`
        //     in the Decision ordering (`Allow < Ask < Deny`), so a
        //     project may only ask more or deny more.
        //
        // An attempt to widen is logged at WARN and ignored. A future
        // opt-in flag could honour the widening; today the safe
        // direction is taken unconditionally.
        if let Some(root) = project_root
            && let Some((project, _path)) = Policy::load_project(root)?
        {
            if project.preset_explicit {
                if project.preset > effective.preset {
                    tracing::warn!(
                        global = ?effective.preset,
                        project = ?project.preset,
                        "project .kod/policy.toml tries to widen the preset; \
                         keeping the global (stricter) choice",
                    );
                } else {
                    effective.preset = project.preset;
                    sources.insert("preset".to_string(), PolicySource::ProjectPolicy);
                }
            }
            for (tool, tp) in project.tools {
                // Widen check on the per-tool mode. The "current
                // effective decision" for a tool is its explicit
                // override if one exists, otherwise the preset
                // fallback. A project entry that is strictly *less*
                // restrictive than that is a widening and is dropped;
                // a same-or-stricter entry is honoured.
                let mut accepted = tp;
                if let Some(proj_mode) = accepted.mode {
                    let current_effective = effective
                        .tools
                        .get(&tool)
                        .and_then(|g| g.mode)
                        .unwrap_or_else(|| preset_decision(effective.preset, &tool));
                    if proj_mode < current_effective {
                        tracing::warn!(
                            tool = %tool,
                            current = ?current_effective,
                            project = ?proj_mode,
                            "project .kod/policy.toml widens a tool's mode; \
                             keeping the stricter effective choice",
                        );
                        // Drop the widening field, keep the rest of the
                        // project's entry (paths/forbidden/etc. may
                        // still narrow usefully).
                        accepted.mode = None;
                    }
                }
                effective.tools.insert(tool.clone(), accepted);
                sources.insert(tool, PolicySource::ProjectPolicy);
            }
            effective.git = project.git;
        }

        // Layer 4: CLI override wins.
        if let Some(p) = cli_preset {
            effective.preset = p;
            sources.insert("preset".to_string(), PolicySource::CliOverride);
        }

        Ok(Self { effective, sources })
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
        let resolved_path = path_arg.as_deref().map(|p| resolve_path(working_dir, p));
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
                        rule: format!("{tool} allow-list does not include binary {first:?}"),
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
                    rule: format!("{tool} allow-list {allowed:?} does not include {p:?}",),
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

/// Resolve a possibly-relative path against `working_dir`, with
/// lexical `..` / `.` normalization.
///
/// H-S3: the previous implementation was a bare join. The bypass it
/// opened is exactly the one the review names: `src/../secrets/x`
/// matched a `src/**` allowlist lexically while the tool's own
/// `resolve_path` (which *does* normalize) wrote `<wd>/secrets/x`.
/// The two implementations disagreed on what path a call touched.
///
/// Full canonicalization (symlink resolution) is deliberately *not*
/// done here: the policy engine runs before the tool, and a write
/// pre-approval is the case for a path that does not exist yet —
/// `fs::canonicalize` would fail on the write's own destination. The
/// tool's `resolve_path` still canonicalizes for symlink safety, so a
/// symlink that escapes the root is caught by the tool layer. This
/// function removes the "different lexical answer" hole.
fn resolve_path(working_dir: &Path, p: &str) -> PathBuf {
    let path = Path::new(p);
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        working_dir.join(path)
    };

    // Lexical `..` / `.` normalization. Mirrors `Path::components()`'s
    // own handling of CurDir and ParentDir when the path is not yet
    // resolved on disk.
    let mut stack: Vec<std::ffi::OsString> = Vec::new();
    let mut root: Option<PathBuf> = None;
    for c in absolute.components() {
        use std::path::Component;
        match c {
            Component::Prefix(..) | Component::RootDir => {
                root = Some(PathBuf::from(c.as_os_str()));
            }
            Component::CurDir => {}
            Component::ParentDir => {
                // Pop the last normal element; a parent *above* the
                // root is a no-op (`/..` == `/`). With `proj` on the
                // stack, the first `..` pops it, a second is a no-op.
                stack.pop();
            }
            Component::Normal(seg) => {
                stack.push(seg.to_os_string());
            }
        }
    }
    let mut out = root.unwrap_or_default();
    for s in stack {
        out.push(s);
    }
    out
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
    if !pattern.starts_with("**")
        && !pattern.starts_with('/')
        && let Some(m2) = build(&format!("**/{pattern}"))
        && (m2.is_match(path) || m2.is_match(relative))
    {
        return true;
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

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
        let d = e.decide(
            "read_file",
            &serde_json::json!({"path": "a.txt"}),
            wd,
            &HashSet::new(),
        );
        assert_eq!(d.outcome, Decision::Allow);
        let d = e.decide(
            "write_file",
            &serde_json::json!({"path": "a.txt"}),
            wd,
            &HashSet::new(),
        );
        assert_eq!(d.outcome, Decision::Deny);
    }

    #[test]
    fn standard_preset_asks_for_writes() {
        let e = engine(Preset::Standard);
        let wd = Path::new("/tmp");
        let d = e.decide(
            "write_file",
            &serde_json::json!({"path": "a.txt"}),
            wd,
            &HashSet::new(),
        );
        assert_eq!(d.outcome, Decision::Ask);
        let d = e.decide(
            "execute_command",
            &serde_json::json!({"command": "ls"}),
            wd,
            &HashSet::new(),
        );
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
        let mut effective = Policy {
            preset: Preset::ReadOnly,
            ..Policy::default()
        };
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
        let d = e.decide(
            "write_file",
            &serde_json::json!({"path": "a.txt"}),
            wd,
            &HashSet::new(),
        );
        assert_eq!(d.outcome, Decision::Allow);
    }

    #[test]
    fn forbidden_path_wins_over_mode() {
        let mut effective = Policy {
            preset: Preset::Yolo,
            ..Policy::default()
        };
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
        let d = e.decide(
            "write_file",
            &serde_json::json!({"path": ".env"}),
            wd,
            &HashSet::new(),
        );
        assert_eq!(d.outcome, Decision::Deny);
        let d = e.decide(
            "write_file",
            &serde_json::json!({"path": "src/main.rs"}),
            wd,
            &HashSet::new(),
        );
        assert_eq!(d.outcome, Decision::Allow);
    }

    #[test]
    fn execute_command_binary_allowlist() {
        let mut effective = Policy {
            preset: Preset::Yolo,
            ..Policy::default()
        };
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
        let d = e.decide(
            "execute_command",
            &serde_json::json!({"command": "cargo test"}),
            wd,
            &HashSet::new(),
        );
        assert_eq!(d.outcome, Decision::Allow);
        let d = e.decide(
            "execute_command",
            &serde_json::json!({"command": "rm -rf"}),
            wd,
            &HashSet::new(),
        );
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
        let d = e.decide(
            "write_file",
            &serde_json::json!({"path": "src/main.rs"}),
            wd,
            &denies,
        );
        assert_eq!(d.outcome, Decision::Deny);
        assert_eq!(d.source, PolicySource::SessionDeny);
        // Another path is unaffected.
        let d = e.decide(
            "write_file",
            &serde_json::json!({"path": "docs/readme.md"}),
            wd,
            &denies,
        );
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
    fn project_can_explicitly_pin_standard() {
        // Regression: a project that writes `preset = "standard"` must
        // be able to override a global config that set Yolo, because
        // "I want the middle preset here" is a real decision. The
        // `preset_explicit` flag is what makes that possible.
        let tmp = tempfile::TempDir::new().unwrap();
        std::fs::create_dir_all(tmp.path().join(".kod")).unwrap();
        std::fs::write(
            tmp.path().join(".kod").join("policy.toml"),
            "preset = \"standard\"\n",
        )
        .unwrap();

        let mut cfg = KodConfig::default();
        cfg.tools.preset = Some("yolo".to_string());
        let engine = PolicyEngine::load(&cfg, Some(tmp.path()), None).expect("load");
        assert_eq!(
            engine.effective().preset,
            Preset::Standard,
            "an explicit `preset = \"standard\"` in .kod/policy.toml must \
             override the global config's yolo",
        );
    }

    #[test]
    fn project_without_preset_line_does_not_override() {
        // Counterpart: a project that only carries per-tool rules and
        // no `preset = ` line must not silently reset a global Yolo to
        // Standard. The flag is false when the line is absent.
        let tmp = tempfile::TempDir::new().unwrap();
        std::fs::create_dir_all(tmp.path().join(".kod")).unwrap();
        std::fs::write(
            tmp.path().join(".kod").join("policy.toml"),
            "[tools.write_file]\nmode = \"deny\"\n",
        )
        .unwrap();

        let mut cfg = KodConfig::default();
        cfg.tools.preset = Some("yolo".to_string());
        let engine = PolicyEngine::load(&cfg, Some(tmp.path()), None).expect("load");
        assert_eq!(
            engine.effective().preset,
            Preset::Yolo,
            "a project with no explicit preset must not change the global one",
        );
        // The project's per-tool rule still applies.
        assert!(
            engine.effective().tools.contains_key("write_file"),
            "the project's tools map must merge regardless of the preset flag",
        );
    }

    #[test]
    fn missing_policy_file_is_none() {
        let tmp = tempfile::TempDir::new().unwrap();
        assert!(Policy::load_project(tmp.path()).unwrap().is_none());
    }
}

#[cfg(test)]
mod coverage_glob_matching {
    //! `glob_matches` is the single point through which every
    //! path-based policy decision flows: forbidden paths in a
    //! `[tools.*]` block, session deny patterns from an approval
    //! dialog, and the swarm runner's write-set enforcement all
    //! compile their patterns through it. The current tests use only
    //! single-segment patterns; this module pins the multi-segment,
    //! `**`, and single-`*` behaviour the .gitignore semantics rely
    //! on.
    use super::*;
    use std::path::Path;

    #[test]
    fn relative_pattern_matches_relative_path() {
        let wd = Path::new("/tmp/proj");
        assert!(glob_matches("src/**", Path::new("/tmp/proj/src/a.rs"), wd));
        assert!(glob_matches(
            "src/**",
            Path::new("/tmp/proj/src/deep/b.rs"),
            wd
        ));
        assert!(!glob_matches(
            "src/**",
            Path::new("/tmp/proj/tests/a.rs"),
            wd
        ));
    }

    #[test]
    fn basename_pattern_matches_anywhere_in_tree() {
        let wd = Path::new("/tmp/proj");
        // `**/.env` is the canonical "anywhere in the tree" idiom a
        // policy author reaches for. A single-`*` pattern would only
        // match the top level, which is the wrong guarantee.
        assert!(glob_matches("**/.env", Path::new("/tmp/proj/.env"), wd));
        assert!(glob_matches("**/.env", Path::new("/tmp/proj/a/.env"), wd));
        assert!(!glob_matches(
            "**/.env",
            Path::new("/tmp/proj/a/.env.local"),
            wd
        ));
    }

    #[test]
    fn absolute_pattern_is_matched_verbatim() {
        let wd = Path::new("/tmp/proj");
        assert!(glob_matches(
            "/tmp/proj/secret",
            Path::new("/tmp/proj/secret"),
            wd
        ));
        assert!(!glob_matches(
            "/tmp/other/secret",
            Path::new("/tmp/proj/secret"),
            wd
        ));
    }

    #[test]
    fn single_star_does_not_cross_directory_boundary() {
        // The `.gitignore` contract: `*` matches within a segment,
        // `**` crosses segments. A regression that let `*` cross
        // would silently widen every pattern a user wrote.
        let wd = Path::new("/tmp/proj");
        assert!(glob_matches(
            "src/*.rs",
            Path::new("/tmp/proj/src/a.rs"),
            wd
        ));
        assert!(!glob_matches(
            "src/*.rs",
            Path::new("/tmp/proj/src/sub/a.rs"),
            wd
        ));
    }

    #[test]
    fn double_star_crosses_directories() {
        let wd = Path::new("/tmp/proj");
        assert!(glob_matches(
            "src/**/a.rs",
            Path::new("/tmp/proj/src/x/y/a.rs"),
            wd
        ));
        assert!(glob_matches(
            "src/**/a.rs",
            Path::new("/tmp/proj/src/a.rs"),
            wd
        ));
    }

    #[test]
    fn invalid_pattern_returns_false_without_panicking() {
        // A malformed glob (unbalanced bracket) is a hand-written
        // policy typo. Returning `false` means the pattern does not
        // fire; the tool's permission gate then falls through to the
        // preset, which is the safe direction.
        let wd = Path::new("/tmp/proj");
        assert!(!glob_matches("[unterminated", Path::new("/tmp/proj/a"), wd));
    }
}

#[cfg(test)]
mod coverage_policy_deny_rules {
    //! `SessionDeny` is keyed in a `HashSet` by the engine's
    //! session deny store. Its `Eq` and `Hash` impls are the
    //! contract that makes `kod policy forget <n>` and the
    //! approval dialog's "never" choice agree on which rule is
    //! which. `Decision` and `PolicySource` are the two enums a
    //! policy decision carries; their serde spellings are the
    //! on-disk and on-the-wire shapes a downstream consumer
    //! relies on.
    use super::*;

    #[test]
    fn session_deny_equality_uses_both_fields() {
        let a = SessionDeny {
            tool: "write_file".into(),
            path_pattern: Some("src/**".into()),
        };
        let b = SessionDeny {
            tool: "write_file".into(),
            path_pattern: Some("src/**".into()),
        };
        let c = SessionDeny {
            tool: "write_file".into(),
            path_pattern: Some("docs/**".into()),
        };
        let d = SessionDeny {
            tool: "execute_command".into(),
            path_pattern: Some("src/**".into()),
        };
        assert_eq!(a, b);
        assert_ne!(a, c);
        assert_ne!(a, d);
    }

    #[test]
    fn session_deny_hashes_the_same_for_equal_values() {
        use std::collections::HashSet;
        let a = SessionDeny {
            tool: "write_file".into(),
            path_pattern: Some("src/**".into()),
        };
        let b = a.clone();
        let mut set = HashSet::new();
        set.insert(a);
        assert!(set.contains(&b));
    }

    #[test]
    fn session_deny_with_none_pattern_is_distinct_from_wildcard() {
        // A tool-wide deny (`path_pattern: None`) and a
        // `path_pattern: Some("*")` are semantically different:
        // the first denies every call of the tool, the second
        // denies every path the glob happens to match. The
        // equality test pins that they are not conflated.
        let a = SessionDeny {
            tool: "write_file".into(),
            path_pattern: None,
        };
        let b = SessionDeny {
            tool: "write_file".into(),
            path_pattern: Some("*".into()),
        };
        assert_ne!(a, b);
    }

    #[test]
    fn decision_variants_serialize_lowercase() {
        // The serde rename_all = "lowercase" is the on-disk
        // contract; a downstream viewer that reads a policy
        // decision from the session log depends on the exact
        // spelling.
        assert_eq!(
            serde_json::to_string(&Decision::Allow).unwrap(),
            "\"allow\""
        );
        assert_eq!(serde_json::to_string(&Decision::Deny).unwrap(), "\"deny\"");
        assert_eq!(serde_json::to_string(&Decision::Ask).unwrap(), "\"ask\"");
    }

    #[test]
    fn decision_round_trips_every_variant() {
        for d in [Decision::Allow, Decision::Deny, Decision::Ask] {
            let json = serde_json::to_string(&d).unwrap();
            let parsed: Decision = serde_json::from_str(&json).unwrap();
            assert_eq!(d, parsed, "roundtrip mismatch for {json}");
        }
    }

    #[test]
    fn policy_source_variants_use_kebab_case() {
        // The rename_all = "kebab-case" is the on-disk contract
        // for the session log's policy decision entries.
        assert_eq!(
            serde_json::to_string(&PolicySource::Preset).unwrap(),
            "\"preset\"",
        );
        assert_eq!(
            serde_json::to_string(&PolicySource::GlobalConfig).unwrap(),
            "\"global-config\"",
        );
        assert_eq!(
            serde_json::to_string(&PolicySource::ProjectPolicy).unwrap(),
            "\"project-policy\"",
        );
        assert_eq!(
            serde_json::to_string(&PolicySource::CliOverride).unwrap(),
            "\"cli-override\"",
        );
        assert_eq!(
            serde_json::to_string(&PolicySource::SessionDeny).unwrap(),
            "\"session-deny\"",
        );
    }

    #[test]
    fn policy_source_round_trips_every_variant() {
        for s in [
            PolicySource::Preset,
            PolicySource::GlobalConfig,
            PolicySource::ProjectPolicy,
            PolicySource::CliOverride,
            PolicySource::SessionDeny,
        ] {
            let json = serde_json::to_string(&s).unwrap();
            let parsed: PolicySource = serde_json::from_str(&json).unwrap();
            assert_eq!(s, parsed, "roundtrip mismatch for {json}");
        }
    }

    #[test]
    fn tool_policy_default_has_every_field_none() {
        // An empty `[tools.write_file]` block must default to
        // "preset applies" — every override is `None`. A
        // regression that pre-populated a field would silently
        // change the effective policy for that tool.
        let p = ToolPolicy::default();
        assert!(p.mode.is_none());
        assert!(p.paths.is_none());
        assert!(p.forbidden.is_none());
        assert!(p.binaries.is_none());
        assert!(p.forbidden_binaries.is_none());
        assert!(p.network.is_none());
        assert!(p.domains.is_none());
    }

    #[test]
    fn git_policy_defaults_to_protecting_history() {
        // `.git` read-only is the safe default. A regression that
        // flipped it would let a sandboxed agent rewrite git
        // history, which is the exact class of accident the design
        // calls out.
        let g = GitPolicy::default();
        assert!(g.history_protected);
    }

    #[test]
    fn empty_tool_policy_block_parses_with_all_none() {
        // The TOML spelling a user writes for "just use the
        // preset" must parse cleanly. Every field is
        // `#[serde(default)]`.
        let p: ToolPolicy = toml::from_str("").unwrap();
        assert!(p.mode.is_none());
        assert!(p.paths.is_none());
    }

    #[test]
    fn tool_policy_parses_each_field_independently() {
        let p: ToolPolicy = toml::from_str(
            r#"
            mode = "ask"
            paths = ["src/**", "docs/**"]
            forbidden = ["**/.env"]
            binaries = ["cargo", "rustc"]
            forbidden_binaries = ["rm"]
            network = false
            domains = ["docs.rs"]
            "#,
        )
        .unwrap();
        assert_eq!(p.mode, Some(Decision::Ask));
        assert_eq!(p.paths.unwrap().len(), 2);
        assert_eq!(p.forbidden.unwrap().len(), 1);
        assert_eq!(p.binaries.unwrap().len(), 2);
        assert_eq!(p.forbidden_binaries.unwrap().len(), 1);
        assert_eq!(p.network, Some(false));
        assert_eq!(p.domains.unwrap().len(), 1);
    }

    #[test]
    fn policy_default_preset_is_standard() {
        // The design's migration made Standard the default; a
        // regression to ReadOnly or Yolo would silently change
        // every session's behaviour.
        let p = Policy::default();
        assert_eq!(p.preset, Preset::Standard);
        assert!(!p.preset_explicit, "the flag must default to false");
        assert!(p.tools.is_empty());
    }

    #[test]
    fn project_cannot_widen_the_preset() {
        // H-S2: a repo shipping `.kod/policy.toml` with
        // `preset = "yolo"` must NOT be able to escalate past the
        // user's global `standard`.
        let tmp = tempfile::TempDir::new().unwrap();
        std::fs::create_dir_all(tmp.path().join(".kod")).unwrap();
        std::fs::write(
            tmp.path().join(".kod").join("policy.toml"),
            "preset = \"yolo\"\n",
        )
        .unwrap();

        let mut cfg = KodConfig::default();
        cfg.tools.preset = Some("standard".to_string());
        let engine = PolicyEngine::load(&cfg, Some(tmp.path()), None).expect("load");
        assert_eq!(
            engine.effective().preset,
            Preset::Standard,
            "project yolo must not widen the global standard preset",
        );
    }

    #[test]
    fn project_can_narrow_the_preset() {
        // The flip side: a project asking for `read-only` under a
        // global `standard` is a narrowing and must be honoured.
        let tmp = tempfile::TempDir::new().unwrap();
        std::fs::create_dir_all(tmp.path().join(".kod")).unwrap();
        std::fs::write(
            tmp.path().join(".kod").join("policy.toml"),
            "preset = \"read-only\"\n",
        )
        .unwrap();

        let mut cfg = KodConfig::default();
        cfg.tools.preset = Some("standard".to_string());
        let engine = PolicyEngine::load(&cfg, Some(tmp.path()), None).expect("load");
        assert_eq!(engine.effective().preset, Preset::ReadOnly);
    }

    #[test]
    fn project_cannot_widen_a_tool_mode() {
        // A project `[tools.write_file] mode = "allow"` under a global
        // `Ask` (Standard preset) must not silently grant write
        // permission. The per-tool override is dropped, the preset
        // fallback applies.
        let tmp = tempfile::TempDir::new().unwrap();
        std::fs::create_dir_all(tmp.path().join(".kod")).unwrap();
        std::fs::write(
            tmp.path().join(".kod").join("policy.toml"),
            "[tools.write_file]\nmode = \"allow\"\n",
        )
        .unwrap();

        let cfg = KodConfig::default(); // preset defaults to Standard
        let engine = PolicyEngine::load(&cfg, Some(tmp.path()), None).expect("load");
        let wd = std::path::Path::new("/tmp");
        let d = engine.decide(
            "write_file",
            &serde_json::json!({"path": "a"}),
            wd,
            &HashSet::new(),
        );
        assert_eq!(d.outcome, Decision::Ask, "project allow must not widen");
    }

    #[test]
    fn project_can_narrow_a_tool_mode() {
        // Reverse: project `mode = "deny"` under global `Ask` is a
        // narrowing and must be honoured.
        let tmp = tempfile::TempDir::new().unwrap();
        std::fs::create_dir_all(tmp.path().join(".kod")).unwrap();
        std::fs::write(
            tmp.path().join(".kod").join("policy.toml"),
            "[tools.write_file]\nmode = \"deny\"\n",
        )
        .unwrap();

        let cfg = KodConfig::default();
        let engine = PolicyEngine::load(&cfg, Some(tmp.path()), None).expect("load");
        let wd = std::path::Path::new("/tmp");
        let d = engine.decide(
            "write_file",
            &serde_json::json!({"path": "a"}),
            wd,
            &HashSet::new(),
        );
        assert_eq!(d.outcome, Decision::Deny);
    }

    #[test]
    fn traversal_is_normalized_before_glob_matching() {
        // H-S3 regression: a path with `..` that escapes an allowlist
        // prefix must NOT match that prefix. `src/../secrets/x` is
        // `<wd>/secrets/x`, not `<wd>/src/secrets/x`.
        let mut effective = Policy {
            preset: Preset::Yolo,
            ..Policy::default()
        };
        effective.tools.insert(
            "write_file".to_string(),
            ToolPolicy {
                paths: Some(vec!["src/**".to_string()]),
                ..Default::default()
            },
        );
        let e = PolicyEngine {
            effective,
            sources: BTreeMap::new(),
        };
        let wd = Path::new("/proj");
        // A legitimate path under src/ is allowed.
        let d = e.decide(
            "write_file",
            &serde_json::json!({"path": "src/main.rs"}),
            wd,
            &HashSet::new(),
        );
        assert_eq!(d.outcome, Decision::Allow);
        // The traversal must not match the src/** allowlist.
        let d = e.decide(
            "write_file",
            &serde_json::json!({"path": "src/../secrets/x"}),
            wd,
            &HashSet::new(),
        );
        assert_eq!(
            d.outcome,
            Decision::Deny,
            "src/../secrets/x must not match src/**",
        );
    }

    #[test]
    fn dot_segments_are_stripped_from_the_resolved_path() {
        let wd = Path::new("/proj");
        // `.` segments are dropped.
        assert_eq!(
            resolve_path(wd, "src/./main.rs"),
            PathBuf::from("/proj/src/main.rs"),
        );
        // `..` pops the previous segment.
        assert_eq!(
            resolve_path(wd, "src/sub/../main.rs"),
            PathBuf::from("/proj/src/main.rs"),
        );
        // `..` above the root is a no-op.
        assert_eq!(resolve_path(wd, "../../etc/x"), PathBuf::from("/etc/x"));
        // A `..` above an absolute root stays at the root.
        assert_eq!(resolve_path(wd, "/.."), PathBuf::from("/"));
    }
}
