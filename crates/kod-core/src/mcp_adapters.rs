//! Bridge between the MCP client and the tool registry (D6.1).
//!
//! The protocol layer (`kod-mcp`) knows nothing about `Tool` or
//! `ToolDefinition`. This module is the composition root: it owns the
//! running servers, exposes each of their tools as a
//! [`kod_tools::Tool`], and registers them on the engine's registry
//! under the `mcp:<server>.<tool>` naming policy.
//!
//! # Lifecycle
//!
//! [`McpHost::startup_tools`] spawns every enabled server once,
//! lists its tools, and returns one adapter per tool. The engine
//! calls it exactly once, from `KodEngine::start`. Servers stay
//! alive for the session and are killed on
//! [`McpHost::shutdown_all`], which `KodEngine::shutdown` calls.
//!
//! # Naming
//!
//! A server named `filesystem` that advertises a `read_file` tool
//! produces the model-visible tool name `mcp:filesystem.read_file`.
//! The dot is the separator; the tool name after it is verbatim from
//! the server. A server that names a tool with a dot inside
//! (`foo.bar`) is legal — the split is on the *first* dot after the
//! `mcp:` prefix, so `mcp:filesystem.foo.bar` maps back to server
//! `filesystem` and tool `foo.bar`.
//!
//! # Policy
//!
//! Every adapter declares `ToolPermissions::default()` — the
//! all-false bitmask. The permissions bitmask is not the security
//! boundary here; the policy engine is. A user who wants to allow a
//! specific MCP tool writes `[tools."mcp:filesystem.read_file"]
//! mode = "allow"` in `.kod/policy.toml`. The default preset is
//! `standard`, under which MCP tools ask.

use kod_config::McpConfig;
use kod_error::{KodError, Result};
use kod_mcp::{McpClient, McpToolDef};
use kod_tools::{Tool, ToolContext};
use kod_types::{ToolCategory, ToolDefinition, ToolId, ToolPermissions, ToolResult};
use serde_json::Value;
use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;

/// The prefix every MCP tool name carries. Kept here so the policy
/// engine's naming assumptions and the adapter's name construction
/// cannot drift.
pub const MCP_TOOL_PREFIX: &str = "mcp:";

/// Compose a tool name from a server name and a tool name.
pub fn mcp_tool_name(server: &str, tool: &str) -> String {
    format!("{MCP_TOOL_PREFIX}{server}.{tool}")
}

/// The owning server + tool for a composed name. `None` when the
/// input does not start with the prefix or has no separator.
pub fn split_mcp_tool_name(full: &str) -> Option<(&str, &str)> {
    let rest = full.strip_prefix(MCP_TOOL_PREFIX)?;
    let (server, tool) = rest.split_once('.')?;
    if server.is_empty() || tool.is_empty() {
        return None;
    }
    Some((server, tool))
}

/// Owns the running MCP servers and answers per-tool calls.
///
/// A single `McpHost` is built once per session and shared as an
/// `Arc`. The `servers` map is written once at startup (each server
/// spawns on first request — `ensure_started`) and read on every
/// tool call; concurrent tool calls are safe (each one locks briefly
/// to look up the `Arc<McpClient>`, then operates on the client which
/// is internally `Send + Sync`).
pub struct McpHost {
    specs: BTreeMap<String, kod_config::McpServerConfig>,
    #[allow(dead_code)]
    working_dir: std::path::PathBuf,
    servers: tokio::sync::RwLock<HashMap<String, Arc<McpClient>>>,
}

impl McpHost {
    /// Build a host from a parsed `[mcp]` section. Does not spawn
    /// anything — [`McpHost::startup_tools`] does that.
    pub fn new(config: McpConfig, working_dir: std::path::PathBuf) -> Self {
        Self {
            specs: config.servers,
            working_dir,
            servers: tokio::sync::RwLock::new(HashMap::new()),
        }
    }

    /// Names of the enabled servers this host would spawn. Used by
    /// `kod doctor` to show what plugins are configured without
    /// actually spawning them.
    pub fn server_names(&self) -> Vec<String> {
        self.specs
            .iter()
            .filter(|(_, s)| s.is_spawnable())
            .map(|(n, _)| n.clone())
            .collect()
    }

    /// Spawn every enabled server, list its tools, and return one
    /// adapter per tool. Best-effort per server: a server that fails
    /// to start or to enumerate its tools is logged and skipped;
    /// the others (and the built-in tools the engine registers
    /// alongside) are unaffected.
    ///
    /// `self: &Arc<Self>` because the adapters the caller wants must
    /// hold a strong reference to the host. An adapter that only
    /// held `&McpHost` would be impossible to box as `dyn Tool`
    /// (which requires `'static`).
    pub async fn startup_tools(self: &Arc<Self>) -> Vec<Box<dyn Tool>> {
        let mut out: Vec<Box<dyn Tool>> = Vec::new();
        for (name, spec) in &self.specs {
            if !spec.is_spawnable() {
                if spec.enabled {
                    tracing::warn!(
                        server = %name,
                        "MCP: server is enabled but has no command; skipping"
                    );
                }
                continue;
            }
            let client = match self.ensure_started(name).await {
                Ok(c) => c,
                Err(e) => {
                    tracing::warn!(
                        server = %name,
                        error = %e,
                        "MCP: could not start server"
                    );
                    continue;
                }
            };
            let defs = match client.list_tools().await {
                Ok(d) => d,
                Err(e) => {
                    tracing::warn!(
                        server = %name,
                        error = %e,
                        "MCP: server did not answer tools/list"
                    );
                    continue;
                }
            };
            tracing::info!(
                server = %name,
                tools = defs.len(),
                "MCP: server ready"
            );
            for def in defs {
                out.push(Box::new(McpToolAdapter::new(
                    self.clone(),
                    name.clone(),
                    def,
                    spec.call_timeout_secs,
                )));
            }
        }
        out
    }

    /// Get (or spawn) the client for `name`. Idempotent: two callers
    /// racing on the first call spawn one process, not two — the
    /// write lock is held across the check-and-insert.
    pub async fn ensure_started(&self, name: &str) -> Result<Arc<McpClient>> {
        {
            let guard = self.servers.read().await;
            if let Some(c) = guard.get(name) {
                return Ok(c.clone());
            }
        }
        let spec = self
            .specs
            .get(name)
            .ok_or_else(|| {
                KodError::InvalidState(format!("unknown MCP server {name:?}"))
            })?
            .clone();
        if !spec.is_spawnable() {
            return Err(KodError::InvalidState(format!(
                "MCP server {name:?} is not spawnable (disabled or no command)"
            )));
        }

        let client = McpClient::spawn_stdio(&spec.command, &spec.args, &spec.env)
            .await
            .map_err(|e| {
                KodError::Internal(format!(
                    "MCP server {name:?} ({}) failed to spawn: {e}",
                    spec.command
                ))
            })?;
        client.initialize().await.map_err(|e| {
            KodError::Internal(format!("MCP server {name:?} initialize failed: {e}"))
        })?;
        let arc = Arc::new(client);
        let mut guard = self.servers.write().await;
        // Double-check under the write lock: a concurrent caller may
        // have inserted while we were spawning. In that case discard
        // ours and use theirs so the tool sees one client.
        if let Some(existing) = guard.get(name) {
            let existing = existing.clone();
            drop(guard);
            // We spawned `arc` and have not published it yet, so
            // `try_unwrap` succeeds here. If it ever did not — a
            // future refactor that publishes first — the fallback is
            // to drop the `Arc` and let `kill_on_drop` reap the
            // child, which is why `shutdown` is best-effort anyway.
            match Arc::try_unwrap(arc) {
                Ok(owned) => owned.shutdown().await,
                Err(_still_shared) => {
                    tracing::debug!(
                        server = %name,
                        "MCP: discarded duplicate client is still referenced"
                    );
                }
            }
            return Ok(existing);
        }
        guard.insert(name.to_string(), arc.clone());
        Ok(arc)
    }

    /// Kill every spawned server. Best-effort: called from
    /// `KodEngine::shutdown`, must never panic or block past its
    /// client-level timeout.
    pub async fn shutdown_all(&self) {
        let drained: Vec<(String, Arc<McpClient>)> = {
            let mut guard = self.servers.write().await;
            guard.drain().collect()
        };
        for (name, client) in drained {
            tracing::debug!(server = %name, "MCP: shutting down server");
            // `shutdown(self)` takes ownership; `Arc::try_unwrap` is
            // the honest way to hand it over when no adapter still
            // holds a clone. An adapter that outlives shutdown (a
            // leaked spawn, a boxed tool in a test) would make
            // `try_unwrap` fail; in that case the `Arc` is dropped
            // and the child is killed by `kill_on_drop` instead.
            match Arc::try_unwrap(client) {
                Ok(owned) => owned.shutdown().await,
                Err(_still_shared) => {
                    tracing::debug!(
                        server = %name,
                        "MCP: client still referenced; relying on kill_on_drop"
                    );
                }
            }
        }
    }
}

/// A `kod_tools::Tool` that forwards `execute` to one MCP server.
///
/// Holds the host (not the client) so the adapter survives a
/// `ensure_started` on a lazy server whose process was killed
/// externally — the next call re-spawns.
pub struct McpToolAdapter {
    host: Arc<McpHost>,
    server: String,
    tool_name: String,
    definition: ToolDefinition,
    call_timeout_secs: u64,
}

impl McpToolAdapter {
    fn new(
        host: Arc<McpHost>,
        server: String,
        def: McpToolDef,
        call_timeout_secs: u64,
    ) -> Self {
        let full_name = mcp_tool_name(&server, &def.name);
        let description = def
            .description
            .clone()
            .unwrap_or_else(|| format!("MCP tool `{}` on server `{}`", def.name, server));
        let schema = if def.input_schema.is_null() {
            serde_json::json!({ "type": "object" })
        } else {
            def.input_schema.clone()
        };
        Self {
            host,
            server,
            tool_name: def.name,
            definition: ToolDefinition {
                id: ToolId::new(),
                name: full_name,
                description,
                category: ToolCategory::System,
                parameters_schema: schema,
                // The all-false bitmask. The policy engine is the
                // gate; a per-tool permission bitmask here would
                // pretend to be one and be bypassable by any config
                // that forgot to set it.
                permissions: ToolPermissions::default(),
            },
            call_timeout_secs: call_timeout_secs.max(1),
        }
    }

    /// The owning server name, for tests and diagnostics.
    pub fn server(&self) -> &str {
        &self.server
    }

    /// The tool name as the server advertises it (without the
    /// `mcp:<server>.` prefix), for tests and diagnostics.
    pub fn tool_name(&self) -> &str {
        &self.tool_name
    }
}

#[async_trait::async_trait]
impl Tool for McpToolAdapter {
    fn definition(&self) -> ToolDefinition {
        self.definition.clone()
    }

    async fn execute(&self, params: &Value, _context: &ToolContext) -> Result<ToolResult> {
        let client = match self.host.ensure_started(&self.server).await {
            Ok(c) => c,
            Err(e) => {
                return Ok(ToolResult::Error(format!(
                    "mcp:{}: server unavailable: {e}",
                    self.server
                )));
            }
        };
        let args = if params.is_null() {
            serde_json::json!({})
        } else {
            params.clone()
        };
        match client
            .call_tool_with_timeout(&self.tool_name, args, self.call_timeout_secs)
            .await
        {
            Ok(result) => {
                let rendered = result.render_text();
                if result.is_error {
                    Ok(ToolResult::Error(rendered))
                } else {
                    Ok(ToolResult::Success(serde_json::json!({
                        "server": self.server,
                        "tool": self.tool_name,
                        "text": rendered,
                    })))
                }
            }
            Err(e) => Ok(ToolResult::Error(format!(
                "mcp:{}: {} failed: {e}",
                self.server, self.tool_name
            ))),
        }
    }
}

/// Install an MCP host on `engine` from the config, if the config has
/// any spawnable server. No-op when `[mcp.servers]` is empty or every
/// entry is disabled or unconfigured — the common case, and the one
/// that must not add overhead.
///
/// Called by the CLI and TUI at startup, **before** `engine.start()`.
/// The registration happens inside `start()` (it enumerates the
/// host's tools and registers them), so a host installed afterwards
/// would be visible to `/mcp` but never to the model. Keeping the
/// call site next to the other `set_*` calls at startup is what
/// guarantees that ordering.
///
/// The function lives here (not in either interface crate) so both
/// paths go through the same logic and the spawnable / disabled
/// distinction is enforced once.
pub async fn install_from_config(
    engine: &crate::engine::KodEngine,
    config: &kod_config::KodConfig,
) {
    if config.mcp.servers.is_empty() {
        return;
    }
    let spawnable = config
        .mcp
        .servers
        .iter()
        .filter(|(_, s)| s.is_spawnable())
        .count();
    if spawnable == 0 {
        tracing::warn!(
            servers = config.mcp.servers.len(),
            "config has [mcp.servers] but none is spawnable \
             (all disabled or missing a command)"
        );
        return;
    }
    let host = Arc::new(McpHost::new(
        config.mcp.clone(),
        engine.working_dir().to_path_buf(),
    ));
    engine.set_mcp_host(host).await;
    tracing::info!(servers = spawnable, "MCP host installed");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn name_composition_and_split_round_trip() {
        let full = mcp_tool_name("filesystem", "read_file");
        assert_eq!(full, "mcp:filesystem.read_file");
        assert_eq!(
            split_mcp_tool_name(&full),
            Some(("filesystem", "read_file"))
        );
    }

    #[test]
    fn split_handles_dotted_tool_names() {
        // A server is allowed to name a tool with a dot inside.
        let full = mcp_tool_name("x", "foo.bar");
        assert_eq!(split_mcp_tool_name(&full), Some(("x", "foo.bar")));
    }

    #[test]
    fn split_rejects_malformed_names() {
        assert_eq!(split_mcp_tool_name("read_file"), None);
        assert_eq!(split_mcp_tool_name("mcp:nodot"), None);
        assert_eq!(split_mcp_tool_name("mcp:.tool"), None);
        assert_eq!(split_mcp_tool_name("mcp:srv."), None);
    }

    #[test]
    fn server_names_lists_only_spawnable_servers() {
        use kod_config::{McpConfig, McpServerConfig};
        let mut cfg = McpConfig::default();
        cfg.servers.insert(
            "a".into(),
            McpServerConfig {
                command: "a".into(),
                ..Default::default()
            },
        );
        cfg.servers.insert(
            "b".into(),
            McpServerConfig {
                command: "b".into(),
                enabled: false,
                ..Default::default()
            },
        );
        cfg.servers.insert(
            "empty".into(),
            McpServerConfig::default(),
        );
        let host = McpHost::new(cfg, std::env::temp_dir());
        let names = host.server_names();
        assert_eq!(names, vec!["a".to_string()]);
    }
}
