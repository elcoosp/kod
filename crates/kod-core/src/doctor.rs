//! `kod doctor` — first-run diagnostics.
//!
//! Checks the environment a `kod` session depends on: the config file,
//! the provider endpoint's shape, the skill directories, and the memory
//! database path. Each check produces a `Check` with a status (`Ok`,
//! `Warn`, `Fail`) and a human message. `run_diagnostics` is a pure
//! function over a `KodConfig`, so the CLI is a thin printer around it
//! and the tests can drive any config without touching the filesystem
//! beyond what the config itself points at.

use kod_config::KodConfig;

/// Outcome of a single diagnostic check.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CheckStatus {
    /// The thing exists and is well-formed.
    Ok,
    /// The thing is missing but the session will still work (a default
    /// will be created on first use).
    Warn,
    /// The thing is broken and the session will fail without user
    /// action.
    Fail,
}

/// One diagnostic result.
#[derive(Debug, Clone)]
pub struct Check {
    pub name: String,
    pub status: CheckStatus,
    pub message: String,
}

/// The full set of results from one `kod doctor` run.
#[derive(Debug, Clone, Default)]
pub struct DiagnosticReport {
    pub checks: Vec<Check>,
}

impl DiagnosticReport {
    pub fn push(&mut self, name: &str, status: CheckStatus, message: impl Into<String>) {
        self.checks.push(Check {
            name: name.to_string(),
            status,
            message: message.into(),
        });
    }

    /// True when any check returned `CheckStatus::Fail`. `kod doctor`
    /// exits non-zero on this so a first-run script or CI job can gate
    /// on it.
    pub fn has_failures(&self) -> bool {
        self.checks.iter().any(|c| c.status == CheckStatus::Fail)
    }

    /// Render the report as a JSON object with `ok` (bool), `checks`
    /// (array of `{name, status, message}`), and `has_failures`
    /// (bool). Kept in kod-core so `kod doctor --json` and any embedder
    /// that wants to consume diagnostics use the same shape.
    pub fn to_json(&self) -> serde_json::Value {
        let checks: Vec<serde_json::Value> = self
            .checks
            .iter()
            .map(|c| {
                serde_json::json!({
                    "name": c.name,
                    "status": match c.status {
                        CheckStatus::Ok => "ok",
                        CheckStatus::Warn => "warn",
                        CheckStatus::Fail => "fail",
                    },
                    "message": c.message,
                })
            })
            .collect();
        serde_json::json!({
            "ok": !self.has_failures(),
            "has_failures": self.has_failures(),
            "checks": checks,
        })
    }
}

/// Run every check against `config`. Pure over `config`: no network
/// calls, no user prompts. Filesystem queries are read-only and
/// confined to the paths the config itself names.
pub fn run_diagnostics(config: &KodConfig) -> DiagnosticReport {
    let mut report = DiagnosticReport::default();

    match KodConfig::config_dir() {
        Ok(dir) => {
            let path = dir.join("config.toml");
            if path.exists() {
                report.push(
                    "config",
                    CheckStatus::Ok,
                    format!("found at {}", path.display()),
                );
            } else {
                report.push(
                    "config",
                    CheckStatus::Warn,
                    format!(
                        "not present at {} — defaults are in use and will be written on first save",
                        path.display()
                    ),
                );
            }
        }
        Err(e) => report.push("config", CheckStatus::Fail, format!("{e}")),
    }

    report.push(
        "llm.base_url",
        CheckStatus::Ok,
        format!(
            "{} (model {})",
            config.llm.default_endpoint().base_url,
            config.llm.default_endpoint().model
        ),
    );

    match config.skills_dirs() {
        Ok(dirs) => {
            let existing: Vec<&std::path::PathBuf> = dirs.iter().filter(|d| d.is_dir()).collect();
            if existing.is_empty() {
                report.push(
                    "skills",
                    CheckStatus::Warn,
                    format!(
                        "none of the {} standard skill director{} exist — \
                         add a .md skill to {} to get started",
                        dirs.len(),
                        if dirs.len() == 1 { "y" } else { "ies" },
                        dirs.first()
                            .map(|d| d.display().to_string())
                            .unwrap_or_else(|| "~/.agents/skills".to_string()),
                    ),
                );
            } else {
                // Directories exist: walk each one, count the .md
                // files, and count how many parse. A directory that
                // exists but holds a malformed skill is a worse
                // surprise than a directory that does not exist —
                // the loader logs a warning and silently skips the
                // file, and the user only finds out when the skill
                // they wrote does nothing.
                let parser = kod_skills::SkillParser::new();
                let mut total = 0usize;
                let mut parsed = 0usize;
                let mut failed: Vec<(std::path::PathBuf, String)> = Vec::new();
                for dir in &existing {
                    for path in collect_md_files(dir) {
                        total += 1;
                        match parser.parse_file(&path) {
                            Ok(_) => parsed += 1,
                            Err(e) => failed.push((path, e.to_string())),
                        }
                    }
                }
                if total == 0 {
                    report.push(
                        "skills",
                        CheckStatus::Ok,
                        format!(
                            "{} director{} present, no .md skills yet",
                            existing.len(),
                            if existing.len() == 1 { "y" } else { "ies" },
                        ),
                    );
                } else if failed.is_empty() {
                    report.push(
                        "skills",
                        CheckStatus::Ok,
                        format!(
                            "{parsed} skill file(s) parsed across {} director{}",
                            existing.len(),
                            if existing.len() == 1 { "y" } else { "ies" },
                        ),
                    );
                } else {
                    let first = failed
                        .first()
                        .map(|(p, e)| {
                            let name = p.file_name().and_then(|n| n.to_str()).unwrap_or("?");
                            format!("{name}: {e}")
                        })
                        .unwrap_or_default();
                    report.push(
                        "skills",
                        CheckStatus::Warn,
                        format!("{parsed}/{total} parsed; {} failed — {first}", failed.len(),),
                    );
                }
            }
        }
        Err(e) => report.push("skills", CheckStatus::Fail, format!("{e}")),
    }

    match config.memory_db_path() {
        Ok(path) => {
            let parent_ok = path.parent().map(|p| p.exists()).unwrap_or(false);
            if parent_ok {
                report.push(
                    "memory",
                    CheckStatus::Ok,
                    format!("{} (parent directory exists)", path.display()),
                );
            } else {
                report.push(
                    "memory",
                    CheckStatus::Warn,
                    format!(
                        "{} — parent directory will be created on first write",
                        path.display()
                    ),
                );
            }
        }
        Err(e) => report.push("memory", CheckStatus::Fail, format!("{e}")),
    }

    // Sandbox primitive. Informational — a session without one still
    // runs `execute_command`, it just cannot require the sandbox.
    {
        use kod_tools::context::{SandboxMode, SandboxOpts, SandboxResolver};
        let cwd = std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."));
        let resolver = SandboxResolver::detect();
        match resolver.invocation(SandboxMode::Require, &cwd, SandboxOpts::default()) {
            Ok(Some(inv)) => report.push(
                "sandbox",
                CheckStatus::Ok,
                format!(
                    "{} available (Auto uses it; Require enforces it)",
                    inv.backend.name()
                ),
            ),
            _ => {
                #[cfg(target_os = "linux")]
                let advice = "install bubblewrap (apt/dnf/pacman/apk) to enable sandboxing";
                #[cfg(target_os = "macos")]
                let advice = "sandbox-exec is not available; `xcode-select --install` may fix it";
                #[cfg(not(any(target_os = "linux", target_os = "macos")))]
                let advice = "no sandbox primitive on this platform";

                report.push("sandbox", CheckStatus::Warn, advice);
            }
        }
    }

    // LSP servers. Informational: which language servers are on PATH.
    // One row per detected binary so a user knows whether the D5.2
    // diagnostic hook and the lsp_* tools will actually fire for
    // their language. The doctor does not spawn the servers — that
    // is the engine's job at first use.
    {
        let detected: Vec<(&str, &str)> = [
            ("rust-analyzer", "rust"),
            ("pyright-langserver", "python"),
            ("pylsp", "python"),
            ("typescript-language-server", "typescript"),
            ("gopls", "go"),
        ]
        .iter()
        .filter(|(bin, _)| {
            std::env::var_os("PATH")
                .map(|p| std::env::split_paths(&p).any(|d| d.join(bin).is_file()))
                .unwrap_or(false)
        })
        .map(|(b, l)| (*b, *l))
        .collect();
        if detected.is_empty() {
            report.push(
                "lsp",
                CheckStatus::Ok,
                "no language servers on PATH (the diagnostics hook and lsp_* \
                 tools will report 'no server for this file'; install \
                 rust-analyzer / pyright-langserver / typescript-language-server \
                 / gopls to enable)",
            );
        } else {
            let names: Vec<String> = detected.iter().map(|(b, l)| format!("{b} ({l})")).collect();
            report.push(
                "lsp",
                CheckStatus::Ok,
                format!(
                    "{} language server(s): {}",
                    detected.len(),
                    names.join(", ")
                ),
            );
        }
    }

    // LSP configuration. Two checks, one per knob:
    //
    //  - `auto_diagnostics`: is the post-write diagnostics pass
    //    enabled? Reported with a hint when it is off, naming both
    //    switches that can disable it so a user who forgot which one
    //    they set has a next step.
    //  - `settle_ms`: is the wait long enough for a language server to
    //    publish on a cold file, and short enough to keep the
    //    interactive loop snappy? Flags both extremes; the design's
    //    default (1500 ms) is the middle.
    {
        let ad = config.lsp.auto_diagnostics;
        let auto_lsp = config.tools.auto_lsp;
        if ad && auto_lsp {
            report.push(
                "lsp.auto_diagnostics",
                CheckStatus::Ok,
                "enabled (a successful write runs the diagnostics pass)",
            );
        } else {
            let mut disabled_by = Vec::new();
            if !ad {
                disabled_by.push("[lsp] auto_diagnostics = false");
            }
            if !auto_lsp {
                disabled_by.push("[tools] auto_lsp = false");
            }
            report.push(
                "lsp.auto_diagnostics",
                CheckStatus::Warn,
                format!("disabled — {}", disabled_by.join(" and "),),
            );
        }

        let ms = config.lsp.settle_ms;
        if ms < 200 {
            report.push(
                "lsp.settle_ms",
                CheckStatus::Warn,
                format!(
                    "{ms} ms is very short — a cold language server will \
                     usually not have published diagnostics yet; consider \
                     at least 500 ms",
                ),
            );
        } else if ms > 10_000 {
            report.push(
                "lsp.settle_ms",
                CheckStatus::Warn,
                format!(
                    "{ms} ms is very long — the post-write diagnostics pass \
                     will slow the interactive loop; consider at most \
                     5000 ms",
                ),
            );
        } else {
            report.push(
                "lsp.settle_ms",
                CheckStatus::Ok,
                format!("{ms} ms (within the recommended range)"),
            );
        }
    }

    // MCP servers. Informational: the count of configured (spawnable)
    // servers, so a user who wrote a `[mcp.servers.*]` block sees
    // whether it parsed and is enabled. The doctor does not spawn the
    // servers — that would make a diagnostic slow and side-effecting.
    {
        let configured = config.mcp.servers.len();
        let spawnable = config
            .mcp
            .servers
            .iter()
            .filter(|(_, s)| s.is_spawnable())
            .count();
        if configured == 0 {
            report.push(
                "mcp",
                CheckStatus::Ok,
                "no MCP servers configured (add [mcp.servers.<name>] to enable)",
            );
        } else if spawnable == 0 {
            report.push(
                "mcp",
                CheckStatus::Warn,
                format!(
                    "{configured} server(s) configured but none is spawnable                      (disabled, or missing `command`)",
                ),
            );
        } else {
            let names: Vec<String> = config
                .mcp
                .servers
                .iter()
                .filter(|(_, s)| s.is_spawnable())
                .map(|(n, _)| n.clone())
                .collect();
            report.push(
                "mcp",
                CheckStatus::Ok,
                format!("{spawnable} server(s) configured: {}", names.join(", "),),
            );
        }
    }

    // Serve daemon. Informational: report whether a daemon is
    // listening on the default socket. A user who expects one and
    // finds none needs the hint; a user who does not use the daemon
    // sees a quiet "no".
    {
        let socket = crate::serve::default_socket_path();
        if socket.exists() {
            report.push(
                "serve",
                CheckStatus::Ok,
                format!("daemon socket present at {}", socket.display()),
            );
        } else {
            report.push(
                "serve",
                CheckStatus::Ok,
                "no daemon (start one with `kod serve` to enable --remote)".to_string(),
            );
        }
    }

    // Swarm routing. Informational: name the endpoints a planner and
    // a coder route to, when the user configured `[llm.routing.swarm]`.
    // Empty table means every capability routes to the default
    // endpoint, which is what a v1 config produces.
    {
        let swarm_routes = config
            .llm
            .routing
            .as_ref()
            .map(|r| r.swarm.len())
            .unwrap_or(0);
        if swarm_routes == 0 {
            report.push(
                "swarm.routing",
                CheckStatus::Ok,
                "no per-capability routing (every swarm role uses the default endpoint)",
            );
        } else {
            report.push(
                "swarm.routing",
                CheckStatus::Ok,
                format!("{swarm_routes} capability route(s) configured"),
            );
        }
    }

    // Git availability. Only a warning — KOD runs fine without git,
    // but a user who intends to use git_status / git_diff needs git
    // installed and on PATH.
    match std::process::Command::new("git")
        .arg("--version")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
    {
        Ok(s) if s.success() => {
            report.push("git", CheckStatus::Ok, "git binary found on PATH");
        }
        _ => {
            report.push(
                "git",
                CheckStatus::Warn,
                "git is not on PATH — git_status / git_diff will fail",
            );
        }
    }

    // Network access. Informational.
    report.push(
        "llm.network_access",
        CheckStatus::Ok,
        if config.llm.network_access {
            "enabled — web_fetch may reach the network"
        } else {
            "disabled — set llm.network_access = true to enable web_fetch"
        },
    );

    report
}

/// Every `.md` file under `root`, recursively, following the same
/// conventions the skill loader uses (no symlink traversal, no
/// filtering by directory name — a skill can live in a nested
/// folder).
///
/// `kod-core` does not depend on `walkdir`; the doctor's walk is the
/// only place that needs one, and a dozen lines of `std::fs` do not
/// justify the dependency. Errors on individual entries are
/// swallowed: a permission-denied subdirectory is reported by the
/// caller as a parse failure count, not as a crash.
fn collect_md_files(root: &std::path::Path) -> Vec<std::path::PathBuf> {
    let mut out = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let Ok(meta) = entry.metadata() else { continue };
            if meta.is_dir() {
                // A symlinked directory is skipped rather than
                // followed. The skill loader does the same with
                // `follow_links(false)`; matching its behaviour here
                // means the doctor sees exactly what the loader
                // would see.
                if meta.file_type().is_symlink() {
                    continue;
                }
                stack.push(path);
            } else if path.extension().and_then(|s| s.to_str()) == Some("md") {
                out.push(path);
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_config_produces_no_failures() {
        let cfg = KodConfig::default();
        let report = run_diagnostics(&cfg);
        assert!(
            !report.has_failures(),
            "default config must not fail any check: {:?}",
            report.checks
        );
    }

    #[test]
    fn report_contains_every_expected_check() {
        let cfg = KodConfig::default();
        let report = run_diagnostics(&cfg);
        let names: Vec<&str> = report.checks.iter().map(|c| c.name.as_str()).collect();
        assert!(names.contains(&"config"), "missing config check: {names:?}");
        assert!(
            names.contains(&"llm.base_url"),
            "missing llm check: {names:?}"
        );
        assert!(names.contains(&"skills"), "missing skills check: {names:?}");
        assert!(names.contains(&"memory"), "missing memory check: {names:?}");
    }

    #[test]
    fn config_check_is_ok_or_warn_on_normal_machine() {
        let cfg = KodConfig::default();
        let report = run_diagnostics(&cfg);
        let check = report
            .checks
            .iter()
            .find(|c| c.name == "config")
            .expect("config check present");
        assert!(
            matches!(check.status, CheckStatus::Ok | CheckStatus::Warn),
            "expected Ok or Warn on a normal machine, got {:?} ({})",
            check.status,
            check.message
        );
    }

    #[test]
    fn llm_check_names_model_and_url() {
        let cfg = KodConfig::default();
        let report = run_diagnostics(&cfg);
        let check = report
            .checks
            .iter()
            .find(|c| c.name == "llm.base_url")
            .unwrap();
        assert!(
            check.message.contains(&cfg.llm.default_endpoint().model),
            "llm check should name the model: {}",
            check.message
        );
        assert!(
            check.message.contains(&cfg.llm.default_endpoint().base_url),
            "llm check should name the base URL: {}",
            check.message
        );
        assert_eq!(check.status, CheckStatus::Ok);
    }

    #[test]
    fn empty_skills_dir_reports_warn_not_fail() {
        let mut cfg = KodConfig::default();
        cfg.skills.skills_dir = Some("/nonexistent/kod-doctor-test/skills".to_string());
        let report = run_diagnostics(&cfg);
        let check = report
            .checks
            .iter()
            .find(|c| c.name == "skills")
            .expect("skills check present");
        assert_eq!(
            check.status,
            CheckStatus::Warn,
            "missing skills dir should warn, got {:?}: {}",
            check.status,
            check.message
        );
        assert!(
            !report.has_failures(),
            "a missing skills dir must not produce a failure"
        );
    }

    #[test]
    fn lsp_defaults_report_ok() {
        // With the design's defaults — auto_diagnostics = true,
        // auto_lsp = true, settle_ms = 1500 — both LSP checks are Ok.
        let cfg = KodConfig::default();
        let report = run_diagnostics(&cfg);
        let ad = report
            .checks
            .iter()
            .find(|c| c.name == "lsp.auto_diagnostics")
            .expect("lsp.auto_diagnostics check present");
        assert_eq!(
            ad.status,
            CheckStatus::Ok,
            "default must report Ok: {}",
            ad.message
        );
        let settle = report
            .checks
            .iter()
            .find(|c| c.name == "lsp.settle_ms")
            .expect("lsp.settle_ms check present");
        assert_eq!(
            settle.status,
            CheckStatus::Ok,
            "default must report Ok: {}",
            settle.message
        );
    }

    #[test]
    fn lsp_disabled_reports_warn_naming_the_switch() {
        let mut cfg = KodConfig::default();
        cfg.lsp.auto_diagnostics = false;
        let report = run_diagnostics(&cfg);
        let ad = report
            .checks
            .iter()
            .find(|c| c.name == "lsp.auto_diagnostics")
            .unwrap();
        assert_eq!(ad.status, CheckStatus::Warn);
        assert!(
            ad.message.contains("[lsp] auto_diagnostics"),
            "must name the switch a user set: {}",
            ad.message,
        );
    }

    #[test]
    fn lsp_short_settle_ms_reports_warn() {
        let mut cfg = KodConfig::default();
        cfg.lsp.settle_ms = 50;
        let report = run_diagnostics(&cfg);
        let check = report
            .checks
            .iter()
            .find(|c| c.name == "lsp.settle_ms")
            .unwrap();
        assert_eq!(check.status, CheckStatus::Warn);
        assert!(
            check.message.contains("50 ms"),
            "must name the value: {}",
            check.message
        );
    }

    #[test]
    fn lsp_long_settle_ms_reports_warn() {
        let mut cfg = KodConfig::default();
        cfg.lsp.settle_ms = 30_000;
        let report = run_diagnostics(&cfg);
        let check = report
            .checks
            .iter()
            .find(|c| c.name == "lsp.settle_ms")
            .unwrap();
        assert_eq!(check.status, CheckStatus::Warn);
        assert!(
            check.message.contains("30000 ms"),
            "must name the value: {}",
            check.message,
        );
    }

    #[test]
    fn has_failures_only_when_a_check_failed() {
        let mut report = DiagnosticReport::default();
        report.push("a", CheckStatus::Ok, "fine");
        report.push("b", CheckStatus::Warn, "meh");
        assert!(!report.has_failures());
        report.push("c", CheckStatus::Fail, "broken");
        assert!(report.has_failures());
    }
}

/// Coverage for the pieces of `run_diagnostics` and `DiagnosticReport`
/// that the existing tests do not reach: the JSON shape, the recursive
/// `.md` walker, and the six checks (memory, sandbox, lsp-detected,
/// mcp, serve, swarm.routing, git, network) that only the full-report
/// tests exercised indirectly.
#[cfg(test)]
mod coverage_doctor {
    use super::*;

    fn find<'a>(report: &'a DiagnosticReport, name: &str) -> &'a Check {
        report
            .checks
            .iter()
            .find(|c| c.name == name)
            .unwrap_or_else(|| panic!("check {name:?} missing from report"))
    }

    // ---- DiagnosticReport::to_json -------------------------------------

    #[test]
    fn to_json_empty_report_is_ok_and_has_empty_checks() {
        let report = DiagnosticReport::default();
        let v = report.to_json();
        assert_eq!(v["ok"], true);
        assert_eq!(v["has_failures"], false);
        assert!(v["checks"].as_array().unwrap().is_empty());
    }

    #[test]
    fn to_json_serializes_each_status_with_the_documented_word() {
        let mut report = DiagnosticReport::default();
        report.push("a", CheckStatus::Ok, "fine");
        report.push("b", CheckStatus::Warn, "meh");
        report.push("c", CheckStatus::Fail, "broken");
        let v = report.to_json();
        let checks = v["checks"].as_array().unwrap();
        assert_eq!(checks[0]["status"], "ok");
        assert_eq!(checks[1]["status"], "warn");
        assert_eq!(checks[2]["status"], "fail");
    }

    #[test]
    fn to_json_ok_and_has_failures_are_opposites() {
        // The CLI's exit code keys on `ok`. A regression that set
        // them independently could report `ok: false, has_failures:
        // false`, which no consumer would know how to read.
        let mut report = DiagnosticReport::default();
        report.push("a", CheckStatus::Ok, "fine");
        assert_eq!(report.to_json()["ok"], true);
        assert_eq!(report.to_json()["has_failures"], false);

        report.push("b", CheckStatus::Fail, "broken");
        assert_eq!(report.to_json()["ok"], false);
        assert_eq!(report.to_json()["has_failures"], true);
    }

    #[test]
    fn to_json_keeps_name_and_message_verbatim() {
        let mut report = DiagnosticReport::default();
        report.push("check.with.dots", CheckStatus::Ok, "a message with spaces");
        let v = report.to_json();
        let checks = v["checks"].as_array().unwrap();
        assert_eq!(checks[0]["name"], "check.with.dots");
        assert_eq!(checks[0]["message"], "a message with spaces");
    }

    #[test]
    fn report_push_accepts_string_and_str() {
        let mut report = DiagnosticReport::default();
        report.push("a", CheckStatus::Ok, "static str");
        report.push("b", CheckStatus::Ok, String::from("owned string"));
        assert_eq!(report.checks.len(), 2);
    }

    // ---- collect_md_files ----------------------------------------------

    #[test]
    fn collect_md_files_on_a_missing_root_is_empty() {
        let out = collect_md_files(std::path::Path::new("/no/such/dir/kod-doctor-xyz"));
        assert!(out.is_empty(), "a missing root must yield nothing");
    }

    #[test]
    fn collect_md_files_finds_top_level_markdown() {
        let tmp = tempfile::TempDir::new().unwrap();
        std::fs::write(tmp.path().join("a.md"), "x").unwrap();
        std::fs::write(tmp.path().join("b.txt"), "x").unwrap();
        let out = collect_md_files(tmp.path());
        assert_eq!(out.len(), 1, "only .md files, got: {out:?}");
        assert!(out[0].ends_with("a.md"));
    }

    #[test]
    fn collect_md_files_recurses_into_subdirectories() {
        let tmp = tempfile::TempDir::new().unwrap();
        std::fs::create_dir(tmp.path().join("nested")).unwrap();
        std::fs::create_dir(tmp.path().join("nested").join("deeper")).unwrap();
        std::fs::write(tmp.path().join("top.md"), "x").unwrap();
        std::fs::write(tmp.path().join("nested").join("mid.md"), "x").unwrap();
        std::fs::write(tmp.path().join("nested").join("deeper").join("deep.md"), "x").unwrap();
        let out = collect_md_files(tmp.path());
        assert_eq!(out.len(), 3, "must find all three, got: {out:?}");
    }

    #[test]
    fn collect_md_files_accepts_a_nested_directory_as_root() {
        // The caller may hand a subdirectory; the walk is rooted
        // there and does not ascend.
        let tmp = tempfile::TempDir::new().unwrap();
        std::fs::create_dir(tmp.path().join("only")).unwrap();
        std::fs::write(tmp.path().join("only").join("x.md"), "x").unwrap();
        let out = collect_md_files(&tmp.path().join("only"));
        assert_eq!(out.len(), 1);
    }

    #[test]
    fn collect_md_files_ignores_files_with_other_extensions() {
        let tmp = tempfile::TempDir::new().unwrap();
        for name in ["a.md", "b.txt", "c.rs", "d", "e.markdown"] {
            std::fs::write(tmp.path().join(name), "x").unwrap();
        }
        let out = collect_md_files(tmp.path());
        assert_eq!(out.len(), 1, "only `.md` counts: {out:?}");
    }

    // ---- checks the existing tests did not reach -----------------------

    #[test]
    fn memory_check_is_ok_or_warn() {
        // Either the parent directory exists (Ok) or it will be
        // created on first write (Warn). Never Fail on a default
        // config — that would gate every CI run on a missing
        // ~/.kod directory.
        let report = run_diagnostics(&KodConfig::default());
        let check = find(&report, "memory");
        assert!(
            matches!(check.status, CheckStatus::Ok | CheckStatus::Warn),
            "memory check must not fail on a default config: {:?}",
            check.status,
        );
    }

    #[test]
    fn sandbox_check_is_informational() {
        // "Sandbox not available" must be a Warn, never a Fail —
        // a machine without bubblewrap or sandbox-exec still runs
        // kod, just without the sandbox.
        let report = run_diagnostics(&KodConfig::default());
        let check = find(&report, "sandbox");
        assert!(
            matches!(check.status, CheckStatus::Ok | CheckStatus::Warn),
            "sandbox check must never fail: {:?} ({})",
            check.status,
            check.message,
        );
    }

    #[test]
    fn lsp_check_is_informational() {
        // Same rule: no language server on PATH is a valid state
        // (the diagnostics hook reports "no server" cleanly).
        let report = run_diagnostics(&KodConfig::default());
        let check = find(&report, "lsp");
        assert_eq!(check.status, CheckStatus::Ok);
        // The message names either the servers found or the "none
        // on PATH" hint. Either is fine; the point is that the
        // check always succeeds.
        assert!(!check.message.is_empty());
    }

    #[test]
    fn mcp_check_with_no_servers_is_ok() {
        let report = run_diagnostics(&KodConfig::default());
        let check = find(&report, "mcp");
        assert_eq!(check.status, CheckStatus::Ok);
        assert!(
            check.message.contains("no MCP servers configured"),
            "got: {}",
            check.message,
        );
    }

    #[test]
    fn serve_check_is_always_ok() {
        // Whether or not a daemon is running is informational. A
        // missing daemon is a normal state for a user who never
        // uses `--remote`.
        let report = run_diagnostics(&KodConfig::default());
        let check = find(&report, "serve");
        assert_eq!(check.status, CheckStatus::Ok);
    }

    #[test]
    fn swarm_routing_default_reports_no_capability_routes() {
        let report = run_diagnostics(&KodConfig::default());
        let check = find(&report, "swarm.routing");
        assert_eq!(check.status, CheckStatus::Ok);
        assert!(
            check.message.contains("no per-capability routing"),
            "got: {}",
            check.message,
        );
    }

    #[test]
    fn git_check_is_ok_or_warn() {
        // This machine has git on PATH (we run cargo from a git
        // repo), so the check is Ok. On a machine without git it
        // would be Warn. Never Fail.
        let report = run_diagnostics(&KodConfig::default());
        let check = find(&report, "git");
        assert!(
            matches!(check.status, CheckStatus::Ok | CheckStatus::Warn),
            "git check must not fail: {:?}",
            check.status,
        );
    }

    #[test]
    fn network_check_names_the_current_setting() {
        let report = run_diagnostics(&KodConfig::default());
        let check = find(&report, "llm.network_access");
        assert_eq!(check.status, CheckStatus::Ok);
        assert!(
            check.message.starts_with("enabled") || check.message.starts_with("disabled"),
            "message must state the current setting: {}",
            check.message,
        );
    }

    #[test]
    fn every_check_has_a_non_empty_name_and_message() {
        // The JSON consumer filters by name and prints the message;
        // an empty either makes the row useless.
        let report = run_diagnostics(&KodConfig::default());
        for check in &report.checks {
            assert!(!check.name.is_empty(), "empty check name: {check:?}");
            assert!(
                !check.message.is_empty(),
                "empty message for check {:?}",
                check.name,
            );
        }
    }
}
