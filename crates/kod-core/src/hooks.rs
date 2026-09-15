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
        for (key, template) in &self.config.pre_tool_use {
            if !matches_key(key, &call.tool_name) {
                continue;
            }
            let command = substitute(template, call);
            let outcome = run_shell(&command).await?;
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
        for (key, template) in &self.config.post_tool_use {
            if !matches_key(key, &call.tool_name) {
                continue;
            }
            let command = substitute(template, call);
            match run_shell(&command).await {
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

/// Substitute `{field}` tokens with values from the call's arguments.
/// Unknown tokens are left in place.
fn substitute(template: &str, call: &ToolCall) -> String {
    let mut out = template.to_string();
    for key in ["path", "command", "pattern", "content", "file"] {
        let placeholder = format!("{{{key}}}");
        if !out.contains(&placeholder) {
            continue;
        }
        let value = call
            .arguments
            .get(key)
            .and_then(|v| v.as_str())
            .unwrap_or("");
        out = out.replace(&placeholder, value);
    }
    out
}

async fn run_shell(command: &str) -> Result<HookOutcome> {
    let (shell, flag) = if cfg!(windows) {
        ("cmd", "/C")
    } else {
        ("sh", "-c")
    };
    let output = tokio::process::Command::new(shell)
        .arg(flag)
        .arg(command)
        .stdin(std::process::Stdio::null())
        .output()
        .await
        .map_err(KodError::Io)?;
    Ok(HookOutcome {
        exit_code: output.status.code().unwrap_or(-1),
        stdout: String::from_utf8_lossy(&output.stdout).to_string(),
        stderr: String::from_utf8_lossy(&output.stderr).to_string(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn call(name: &str, args: serde_json::Value) -> ToolCall {
        ToolCall {
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

    #[test]
    fn substitute_replaces_known_fields() {
        let c = call("write_file", json!({"path": "/tmp/a.rs", "content": "hi"}));
        let out = substitute("rustfmt {path} # content={content}", &c);
        assert_eq!(out, "rustfmt /tmp/a.rs # content=hi");
    }

    #[test]
    fn substitute_leaves_unknown_tokens() {
        let c = call("write_file", json!({"path": "/tmp/a.rs"}));
        assert_eq!(substitute("run {unknown}", &c), "run {unknown}");
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
