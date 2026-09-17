//! MCP server configuration (D6.1, AD-12).
//!
//! One `[mcp.servers.<name>]` block per plugin. The name is the
//! routing key: a tool the server advertises as `read_file` is
//! exposed to the model as `mcp:<name>.read_file`. The dot is the
//! naming separator, so a server name is an identifier and the tool
//! name is whatever the server says.
//!
//! # Example
//!
//! ```toml
//! [mcp.servers.filesystem]
//! command = "npx"
//! args = ["-y", "@modelcontextprotocol/server-filesystem", "/tmp"]
//! enabled = true
//! call_timeout_secs = 60
//!
//! [mcp.servers.filesystem.env]
//! LOG_LEVEL = "info"
//! ```
//!
//! A missing `[mcp]` section means no MCP tools, which is the
//! default. A server marked `enabled = false` is configured but not
//! spawned; the block stays in the file as a record.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// The `[mcp]` section. Empty by default.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct McpConfig {
    /// One entry per configured server. A `BTreeMap` (not `HashMap`)
    /// so iteration order — and therefore the order tools register,
    /// and therefore the order they appear in the prompt's tool
    /// inventory — is deterministic across runs.
    pub servers: BTreeMap<String, McpServerConfig>,
}

/// One `[mcp.servers.<name>]` block.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct McpServerConfig {
    /// The program to spawn. Resolved against `$PATH` by the OS, so
    /// the value is a program name (`npx`, `python3`, `uvx`, an
    /// absolute path — the caller's choice).
    pub command: String,
    /// Arguments to the program. Empty when the command needs none.
    pub args: Vec<String>,
    /// Environment variables to add to the child's environment. The
    /// child inherits the parent's environment; this map adds to it,
    /// so a `PATH` the caller set in their shell reaches the server.
    pub env: BTreeMap<String, String>,
    /// When false, the block is configured but no process is spawned
    /// and no tools are registered. Default true.
    pub enabled: bool,
    /// Per-call timeout in seconds. Applied to `tools/call`; the
    /// `initialize` and `tools/list` timeouts are fixed by the
    /// client. Default 60.
    pub call_timeout_secs: u64,
}

impl Default for McpServerConfig {
    fn default() -> Self {
        Self {
            command: String::new(),
            args: Vec::new(),
            env: BTreeMap::new(),
            enabled: true,
            call_timeout_secs: 60,
        }
    }
}

impl McpServerConfig {
    /// A server is "spawnable" when it has a command and is enabled.
    /// A block with an empty command is a config the user drafted but
    /// never filled in; skipping it is the honest behaviour.
    pub fn is_spawnable(&self) -> bool {
        self.enabled && !self.command.trim().is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_config_has_no_servers() {
        let c = McpConfig::default();
        assert!(c.servers.is_empty());
    }

    #[test]
    fn parses_full_server_block() {
        let raw = r#"
            [servers.filesystem]
            command = "npx"
            args = ["-y", "@modelcontextprotocol/server-filesystem", "/tmp"]
            enabled = true
            call_timeout_secs = 120

            [servers.filesystem.env]
            LOG_LEVEL = "info"
        "#;
        let c: McpConfig = toml::from_str(raw).unwrap();
        assert_eq!(c.servers.len(), 1);
        let s = c.servers.get("filesystem").unwrap();
        assert_eq!(s.command, "npx");
        assert_eq!(s.args.len(), 3);
        assert_eq!(s.call_timeout_secs, 120);
        assert_eq!(s.env.get("LOG_LEVEL").map(String::as_str), Some("info"));
        assert!(s.is_spawnable());
    }

    #[test]
    fn defaults_apply_to_minimal_block() {
        let raw = r#"
            [servers.echo]
            command = "echo-server"
        "#;
        let c: McpConfig = toml::from_str(raw).unwrap();
        let s = c.servers.get("echo").unwrap();
        assert_eq!(s.command, "echo-server");
        assert!(s.args.is_empty());
        assert!(s.env.is_empty());
        assert!(s.enabled, "enabled defaults to true");
        assert_eq!(s.call_timeout_secs, 60);
        assert!(s.is_spawnable());
    }

    #[test]
    fn disabled_server_is_not_spawnable() {
        let raw = r#"
            [servers.x]
            command = "x"
            enabled = false
        "#;
        let c: McpConfig = toml::from_str(raw).unwrap();
        assert!(!c.servers.get("x").unwrap().is_spawnable());
    }

    #[test]
    fn empty_command_is_not_spawnable() {
        let raw = r#"
            [servers.draft]
            args = ["--nothing"]
        "#;
        let c: McpConfig = toml::from_str(raw).unwrap();
        assert!(!c.servers.get("draft").unwrap().is_spawnable());
    }

    #[test]
    fn absent_section_defaults_to_empty() {
        let c: McpConfig = toml::from_str("").unwrap();
        assert!(c.servers.is_empty());
    }
}
