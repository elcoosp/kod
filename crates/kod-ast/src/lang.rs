//! Language dispatch for the parse cache.
//!
//! The nine grammars are the languages kod's repo map already
//! understands (`repomap::extract_symbols_and_imports`). The mapping
//! from file extension to [`Lang`] mirrors that function's table so
//! the two agree on what a ".rs" file is.

use std::path::Path;

/// A tree-sitter grammar kod bundles.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Lang {
    Rust,
    Python,
    TypeScript,
    Tsx,
    JavaScript,
    Go,
    Ruby,
    Java,
    C,
}

impl Lang {
    /// The language for `path`, by extension. `None` for a file the
    /// parser set does not cover.
    pub fn from_path(path: &Path) -> Option<Lang> {
        Self::from_extension(path.extension().and_then(|e| e.to_str())?)
    }

    /// The language for a bare extension (no dot). Lower-cased by the
    /// caller's `to_str` when it came from a path; an explicitly
    /// upper-case extension is accepted too.
    pub fn from_extension(ext: &str) -> Option<Lang> {
        match ext.to_ascii_lowercase().as_str() {
            "rs" => Some(Lang::Rust),
            "py" | "pyi" => Some(Lang::Python),
            "ts" | "mts" | "cts" => Some(Lang::TypeScript),
            "tsx" => Some(Lang::Tsx),
            "js" | "mjs" | "cjs" | "jsx" => Some(Lang::JavaScript),
            "go" => Some(Lang::Go),
            "rb" => Some(Lang::Ruby),
            "java" => Some(Lang::Java),
            "c" | "h" => Some(Lang::C),
            _ => None,
        }
    }

    /// The tree-sitter grammar. Each arm returns the modern
    /// `LanguageFn` constant (tree-sitter 0.23+ convention) converted
    /// to a `Language`.
    pub fn language(self) -> tree_sitter::Language {
        match self {
            Lang::Rust => tree_sitter_rust::LANGUAGE.into(),
            Lang::Python => tree_sitter_python::LANGUAGE.into(),
            Lang::TypeScript => tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into(),
            Lang::Tsx => tree_sitter_typescript::LANGUAGE_TSX.into(),
            Lang::JavaScript => tree_sitter_javascript::LANGUAGE.into(),
            Lang::Go => tree_sitter_go::LANGUAGE.into(),
            Lang::Ruby => tree_sitter_ruby::LANGUAGE.into(),
            Lang::Java => tree_sitter_java::LANGUAGE.into(),
            Lang::C => tree_sitter_c::LANGUAGE.into(),
        }
    }

    /// The stable name used in the cache key and in logs.
    pub fn name(self) -> &'static str {
        match self {
            Lang::Rust => "rust",
            Lang::Python => "python",
            Lang::TypeScript => "typescript",
            Lang::Tsx => "tsx",
            Lang::JavaScript => "javascript",
            Lang::Go => "go",
            Lang::Ruby => "ruby",
            Lang::Java => "java",
            Lang::C => "c",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extension_table_matches_the_repo_map() {
        // Every extension `repomap` extracts gets a Lang.
        for (ext, want) in [
            ("rs", Lang::Rust),
            ("py", Lang::Python),
            ("ts", Lang::TypeScript),
            ("tsx", Lang::Tsx),
            ("js", Lang::JavaScript),
            ("go", Lang::Go),
            ("rb", Lang::Ruby),
            ("java", Lang::Java),
            ("c", Lang::C),
            ("h", Lang::C),
        ] {
            assert_eq!(Lang::from_extension(ext), Some(want), "ext: {ext}");
        }
    }

    #[test]
    fn unknown_extension_is_none() {
        assert_eq!(Lang::from_extension("toml"), None);
        assert_eq!(Lang::from_extension("md"), None);
        assert_eq!(Lang::from_extension(""), None);
    }

    #[test]
    fn from_path_reads_the_extension() {
        assert_eq!(
            Lang::from_path(std::path::Path::new("src/main.rs")),
            Some(Lang::Rust),
        );
        assert_eq!(Lang::from_path(std::path::Path::new("Makefile")), None);
    }

    #[test]
    fn every_language_loads() {
        // A grammar that fails to load panics in `language()`; this
        // proves all nine are wired.
        for l in [
            Lang::Rust,
            Lang::Python,
            Lang::TypeScript,
            Lang::Tsx,
            Lang::JavaScript,
            Lang::Go,
            Lang::Ruby,
            Lang::Java,
            Lang::C,
        ] {
            let _ = l.language();
            assert!(!l.name().is_empty());
        }
    }
}
