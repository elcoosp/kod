//! Minimal Model Context Protocol (MCP) client over stdio.
//!
//! # Scope
//!
//! One transport: newline-delimited JSON-RPC 2.0 over a child process's
//! stdin/stdout — the transport the MCP specification calls "stdio". No
//! HTTP, no SSE, no WebSocket: an agent harness spawns servers, not
//! connects to them.
//!
//! One role: client. This crate spawns servers, performs the
//! `initialize` handshake, and exposes `tools/list` and `tools/call`.
//! Resources, prompts, sampling, and roots are not implemented — the
//! four tools kod needs from a plugin are listable and callable.
//!
//! # Design
//!
//! - `McpClient` owns one child process. Lifetime = the client.
//! - A single background reader task parses incoming lines and
//!   dispatches responses to pending oneshot senders keyed by request
//!   id. Notifications (server → client, no id) are logged at debug.
//! - `request` inserts a oneshot into the pending map, writes the
//!   request, and awaits with a bounded timeout. A timed-out request
//!   removes its own entry so a late response does not leak.
//! - No retry. An MCP server that dies mid-call is removed by the
//!   caller (see `kod-core/src/mcp_adapters.rs`); a transient protocol
//!   failure does not have a well-defined recovery.
//!
//! # Zero kod-core dependency
//!
//! This crate speaks the protocol and nothing else. It does not know
//! about `ToolDefinition`, `ToolResult`, or the engine. The adapter
//! that turns `McpToolDef` into `kod_tools::Tool` lives in kod-core,
//! which is the only crate that already depends on both.

pub mod client;
pub mod types;

pub use client::{McpClient, McpError};
pub use types::{McpContent, McpToolDef, McpToolResult, ServerInfo};
