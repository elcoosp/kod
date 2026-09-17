//! Wire-level types for the LSP client.
//!
//! Deliberately small: this crate speaks just enough of the protocol
//! to get diagnostics from a server. The `Diagnostic` type is a
//! workspace-native shape — not the LSP wire form — so a caller does
//! not have to know about `0`-based line numbers, `Position` structs,
//! or numeric severities.

use serde::{Deserialize, Serialize};

/// One diagnostic, in a shape that matches the rest of the workspace
/// (see `kod_tools::check::Diagnostic`).
///
/// Line and column are 1-based to match every tool a user interacts
/// with (`grep`, compilers, editors). The LSP server sends 0-based;
/// `client.rs` converts on the way in.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Diagnostic {
    /// Filesystem path, decoded from the LSP `file://` URI.
    pub file: String,
    /// 1-based.
    pub line: u32,
    /// 1-based.
    pub column: u32,
    /// `"error"`, `"warning"`, `"info"`, or `"hint"`.
    pub severity: String,
    /// Server-specific code (`E0308` for rust-analyzer, `reportGeneralTypeIssues`
    /// for pyright). `None` when the server does not send one.
    pub code: Option<String>,
    pub message: String,
}

/// Which server speaks for a given file.
///
/// Detection is by extension; the caller decides which binary to
/// spawn. The mapping lives here so `check` and any future editor
/// integration agree on the same answer.
pub fn language_id_for(path: &std::path::Path) -> &'static str {
    match path.extension().and_then(|s| s.to_str()) {
        Some("rs") => "rust",
        Some("py") => "python",
        Some("ts") | Some("tsx") => "typescript",
        Some("js") | Some("jsx") => "javascript",
        Some("go") => "go",
        Some("c") | Some("h") => "c",
        Some("cc") | Some("cpp") | Some("hpp") | Some("cxx") => "cpp",
        _ => "plaintext",
    }
}
