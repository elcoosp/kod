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
        format!("{} (model {})", config.llm.base_url, config.llm.model),
    );

    match config.skills_dirs() {
        Ok(dirs) => {
            let existing: Vec<&std::path::PathBuf> =
                dirs.iter().filter(|d| d.is_dir()).collect();
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
                report.push(
                    "skills",
                    CheckStatus::Ok,
                    format!(
                        "{} of {} standard director{} present",
                        existing.len(),
                        dirs.len(),
                        if dirs.len() == 1 { "y" } else { "ies" },
                    ),
                );
            }
        }
        Err(e) => report.push("skills", CheckStatus::Fail, format!("{e}")),
    }

    match config.memory_db_path() {
        Ok(path) => {
            let parent_ok = path
                .parent()
                .map(|p| p.exists())
                .unwrap_or(false);
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

    // Write confirmation. Informational.
    report.push(
        "tools.confirm_writes",
        CheckStatus::Ok,
        if config.tools.confirm_writes {
            "enabled — write_file / patch_file ask for approval"
        } else {
            "disabled — writes proceed without a prompt (checkpoint rollback still available)"
        },
    );

    report
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
        assert!(names.contains(&"llm.base_url"), "missing llm check: {names:?}");
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
            check.message.contains(&cfg.llm.model),
            "llm check should name the model: {}",
            check.message
        );
        assert!(
            check.message.contains(&cfg.llm.base_url),
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
    fn has_failures_only_when_a_check_failed() {
        let mut report = DiagnosticReport::default();
        report.push("a", CheckStatus::Ok, "fine");
        report.push("b", CheckStatus::Warn, "meh");
        assert!(!report.has_failures());
        report.push("c", CheckStatus::Fail, "broken");
        assert!(report.has_failures());
    }
}
