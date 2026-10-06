//! kod-core-tools: adapter layer wrapping the built-in tools
//! (memory, lsp, mcp, hub, jfind) for use by the agent engine.

#![allow(clippy::all)]

pub mod jfind;
pub mod lsp_tools;
pub mod prewalk;
pub mod speculation;
pub mod tool_loop_guard;
