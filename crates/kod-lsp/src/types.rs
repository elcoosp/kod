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

/// A cursor position in a document. 1-based to match
/// `Diagnostic`. `LspClient`'s request methods translate to the LSP
/// 0-based wire form.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Position {
    pub line: u32,
    pub column: u32,
}

/// A range inside one file, both ends 1-based.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Range {
    pub start: Position,
    pub end: Position,
}

/// A location the server returned: a file plus a range in it.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Location {
    pub file: std::path::PathBuf,
    pub range: Range,
}

/// What `textDocument/hover` returns: the text the server wants to
/// show plus the range it applies to. `text` may be empty when the
/// server has nothing to say about the position.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Hover {
    pub text: String,
    pub range: Option<Range>,
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

#[cfg(test)]
mod coverage_language_id {
    //! `language_id_for` is the single point that decides which LSP
    //! language a file belongs to. A regression mislabels a file,
    //! the server reports no diagnostics for it, and the model reads
    //! a clean build where there is one.
    use super::*;
    use std::path::Path;

    #[test]
    fn supported_extensions_map_to_the_expected_id() {
        let cases = [
            ("a.rs", "rust"),
            ("a.py", "python"),
            ("a.ts", "typescript"),
            ("a.tsx", "typescript"),
            ("a.js", "javascript"),
            ("a.jsx", "javascript"),
            ("a.go", "go"),
            ("a.c", "c"),
            ("a.h", "c"),
            ("a.cc", "cpp"),
            ("a.cpp", "cpp"),
            ("a.hpp", "cpp"),
            ("a.cxx", "cpp"),
        ];
        for (file, expected) in cases {
            assert_eq!(
                language_id_for(Path::new(file)),
                expected,
                "{file} mapped wrong",
            );
        }
    }

    #[test]
    fn unknown_extension_falls_back_to_plaintext() {
        assert_eq!(language_id_for(Path::new("a.txt")), "plaintext");
        assert_eq!(language_id_for(Path::new("a.json")), "plaintext");
        assert_eq!(language_id_for(Path::new("a.md")), "plaintext");
    }

    #[test]
    fn no_extension_is_plaintext() {
        // A file with no extension (`Makefile`, `LICENSE`) must not
        // panic or return an empty string.
        assert_eq!(language_id_for(Path::new("Makefile")), "plaintext");
        assert_eq!(language_id_for(Path::new("LICENSE")), "plaintext");
    }

    #[test]
    fn uppercase_extension_is_not_recognized() {
        // The mapping is deliberately case-sensitive: `.RS` is
        // almost always a typo, and silently mapping it to Rust
        // would hide the mistake.
        assert_eq!(language_id_for(Path::new("a.RS")), "plaintext");
    }
}
