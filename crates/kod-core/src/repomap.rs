//! Repository map generation.
//!
//! Extracts top-level symbols from source files using language-specific
//! regex patterns, producing a compact map suitable for inclusion in
//! every prompt. The agent knows what exists in the repository without
//! having to grep for it.
//!
//! The map is the highest-leverage context-selection technique in this
//! category (Aider's repo map is the canonical example). It is
//! deliberately cheap: regex extraction over text files, capped at a
//! character budget, cached once per `TaskRouter` lifetime.

use regex::Regex;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

/// One extracted top-level symbol.
#[derive(Debug, Clone)]
pub struct Symbol {
    pub kind: &'static str,
    pub name: String,
    pub line: usize,
}

/// The full repository map.
#[derive(Debug, Clone, Default)]
pub struct RepoMap {
    pub entries: BTreeMap<PathBuf, Vec<Symbol>>,
}

/// Default character budget for the rendered map. At the workspace's
/// 4-chars-per-token rule of thumb this is ~4k tokens, comfortable
/// inside an 8k-context model's prompt alongside everything else.
pub const DEFAULT_MAP_CHARS: usize = 16_000;

impl RepoMap {
    /// Render as a compact text block, one file per line.
    pub fn render(&self, max_chars: usize) -> String {
        let mut out = String::new();
        for (path, symbols) in &self.entries {
            let mut line = format!("{}:", path.display());
            for s in symbols {
                line.push_str(&format!(" {} {}", s.kind, s.name));
            }
            if out.len() + line.len() + 1 > max_chars {
                out.push_str("\n… (map truncated)\n");
                break;
            }
            out.push_str(&line);
            out.push('\n');
        }
        out
    }

    pub fn file_count(&self) -> usize {
        self.entries.len()
    }

    pub fn symbol_count(&self) -> usize {
        self.entries.values().map(|v| v.len()).sum()
    }
}

/// Walk `root` honoring .gitignore and skipping the usual build trees,
/// extracting top-level symbols per recognized source file.
pub fn build_repo_map(root: &Path) -> RepoMap {
    let mut entries = BTreeMap::new();
    let mut builder = ignore::WalkBuilder::new(root);
    builder
        .hidden(false)
        .git_ignore(true)
        .ignore(true)
        .git_global(true)
        .git_exclude(true)
        .require_git(false)
        .filter_entry(|e| {
            let name = e.file_name().to_str().unwrap_or("");
            name != ".git" && name != "target" && name != "node_modules"
        });
    for entry in builder.build().filter_map(|e| e.ok()) {
        let path = entry.path();
        if !path.is_file() {
            continue;
        }
        let rel = path.strip_prefix(root).unwrap_or(path).to_path_buf();
        let symbols = extract_symbols(path);
        if !symbols.is_empty() {
            entries.insert(rel, symbols);
        }
    }
    RepoMap { entries }
}

fn extract_symbols(path: &Path) -> Vec<Symbol> {
    let ext = path.extension().and_then(|s| s.to_str()).unwrap_or("");
    // Cap per-file bytes read. A generated source file larger than this
    // is not worth mapping — its symbol-to-byte ratio is tiny and the
    // map budget is better spent on the dozens of files around it.
    const MAX_FILE_BYTES: u64 = 2 * 1024 * 1024;
    if let Ok(meta) = std::fs::metadata(path)
        && meta.len() > MAX_FILE_BYTES
    {
        return Vec::new();
    }
    let content = match std::fs::read_to_string(path) {
        Ok(c) => c,
        Err(_) => return Vec::new(),
    };
    match ext {
        "rs" => extract_rust(&content),
        "py" => extract_python(&content),
        "js" | "jsx" | "ts" | "tsx" => extract_js(&content),
        "go" => extract_go(&content),
        "rb" => extract_ruby(&content),
        "java" => extract_java(&content),
        "c" | "h" | "cc" | "cpp" | "hpp" | "cxx" => extract_c(&content),
        _ => Vec::new(),
    }
}

fn scan(content: &str, patterns: &[(&'static str, &Regex)]) -> Vec<Symbol> {
    let mut out = Vec::new();
    for (kind, re) in patterns {
        for cap in re.captures_iter(content) {
            if let Some(m) = cap.get(1) {
                let line = content[..m.start()].bytes().filter(|b| *b == b'\n').count() + 1;
                out.push(Symbol {
                    kind,
                    name: m.as_str().to_string(),
                    line,
                });
            }
        }
    }
    out.sort_by_key(|s| s.line);
    out.dedup_by(|a, b| a.name == b.name && a.kind == b.kind);
    out.truncate(200);
    out
}

fn extract_rust(content: &str) -> Vec<Symbol> {
    static FN: OnceLock<Regex> = OnceLock::new();
    static STRUCT: OnceLock<Regex> = OnceLock::new();
    static ENUM: OnceLock<Regex> = OnceLock::new();
    static TRAIT: OnceLock<Regex> = OnceLock::new();
    static MOD: OnceLock<Regex> = OnceLock::new();
    static CONST: OnceLock<Regex> = OnceLock::new();
    let f = FN.get_or_init(|| {
        Regex::new(r"(?m)^\s*(?:pub(?:\([^)]*\))?\s+)?(?:async\s+)?(?:unsafe\s+)?fn\s+([A-Za-z_][A-Za-z0-9_]*)").unwrap()
    });
    let s = STRUCT.get_or_init(|| {
        Regex::new(r"(?m)^\s*(?:pub(?:\([^)]*\))?\s+)?struct\s+([A-Za-z_][A-Za-z0-9_]*)").unwrap()
    });
    let e = ENUM.get_or_init(|| {
        Regex::new(r"(?m)^\s*(?:pub(?:\([^)]*\))?\s+)?enum\s+([A-Za-z_][A-Za-z0-9_]*)").unwrap()
    });
    let t = TRAIT.get_or_init(|| {
        Regex::new(r"(?m)^\s*(?:pub(?:\([^)]*\))?\s+)?trait\s+([A-Za-z_][A-Za-z0-9_]*)").unwrap()
    });
    let m = MOD.get_or_init(|| {
        Regex::new(r"(?m)^\s*(?:pub(?:\([^)]*\))?\s+)?mod\s+([A-Za-z_][A-Za-z0-9_]*)").unwrap()
    });
    let c = CONST.get_or_init(|| {
        Regex::new(r"(?m)^\s*(?:pub(?:\([^)]*\))?\s+)?const\s+([A-Za-z_][A-Za-z0-9_]*)").unwrap()
    });
    scan(
        content,
        &[
            ("fn", f),
            ("struct", s),
            ("enum", e),
            ("trait", t),
            ("mod", m),
            ("const", c),
        ],
    )
}

fn extract_python(content: &str) -> Vec<Symbol> {
    static DEF: OnceLock<Regex> = OnceLock::new();
    static CLASS: OnceLock<Regex> = OnceLock::new();
    let d = DEF.get_or_init(|| {
        Regex::new(r"(?m)^\s*(?:async\s+)?def\s+([A-Za-z_][A-Za-z0-9_]*)").unwrap()
    });
    let c = CLASS.get_or_init(|| Regex::new(r"(?m)^\s*class\s+([A-Za-z_][A-Za-z0-9_]*)").unwrap());
    scan(content, &[("def", d), ("class", c)])
}

fn extract_js(content: &str) -> Vec<Symbol> {
    static FN: OnceLock<Regex> = OnceLock::new();
    static CLASS: OnceLock<Regex> = OnceLock::new();
    static CONST: OnceLock<Regex> = OnceLock::new();
    let f = FN.get_or_init(|| {
        Regex::new(r"(?m)^\s*(?:export\s+)?(?:async\s+)?function\s+([A-Za-z_$][A-Za-z0-9_$]*)").unwrap()
    });
    let c = CLASS.get_or_init(|| {
        Regex::new(r"(?m)^\s*(?:export\s+)?class\s+([A-Za-z_$][A-Za-z0-9_$]*)").unwrap()
    });
    let k = CONST.get_or_init(|| {
        Regex::new(r"(?m)^\s*(?:export\s+)?(?:const|let|var)\s+([A-Za-z_$][A-Za-z0-9_$]*)\s*=").unwrap()
    });
    scan(content, &[("function", f), ("class", c), ("const", k)])
}

fn extract_go(content: &str) -> Vec<Symbol> {
    static FUNC: OnceLock<Regex> = OnceLock::new();
    static TYPE: OnceLock<Regex> = OnceLock::new();
    let f = FUNC.get_or_init(|| {
        Regex::new(r"(?m)^func\s+(?:\([^)]*\)\s+)?([A-Za-z_][A-Za-z0-9_]*)").unwrap()
    });
    let t = TYPE.get_or_init(|| Regex::new(r"(?m)^type\s+([A-Za-z_][A-Za-z0-9_]*)").unwrap());
    scan(content, &[("func", f), ("type", t)])
}

fn extract_ruby(content: &str) -> Vec<Symbol> {
    static DEF: OnceLock<Regex> = OnceLock::new();
    static CLASS: OnceLock<Regex> = OnceLock::new();
    static MODULE: OnceLock<Regex> = OnceLock::new();
    let d = DEF.get_or_init(|| Regex::new(r"(?m)^\s*def\s+([A-Za-z_][A-Za-z0-9_!?]*)").unwrap());
    let c = CLASS.get_or_init(|| Regex::new(r"(?m)^\s*class\s+([A-Za-z_][A-Za-z0-9_]*)").unwrap());
    let m = MODULE.get_or_init(|| Regex::new(r"(?m)^\s*module\s+([A-Za-z_][A-Za-z0-9_]*)").unwrap());
    scan(content, &[("def", d), ("class", c), ("module", m)])
}

fn extract_java(content: &str) -> Vec<Symbol> {
    static CLASS: OnceLock<Regex> = OnceLock::new();
    static IFACE: OnceLock<Regex> = OnceLock::new();
    let c = CLASS.get_or_init(|| {
        Regex::new(r"(?m)^\s*(?:public\s+)?class\s+([A-Za-z_][A-Za-z0-9_]*)").unwrap()
    });
    let i = IFACE.get_or_init(|| {
        Regex::new(r"(?m)^\s*(?:public\s+)?interface\s+([A-Za-z_][A-Za-z0-9_]*)").unwrap()
    });
    scan(content, &[("class", c), ("interface", i)])
}

fn extract_c(content: &str) -> Vec<Symbol> {
    static STRUCT: OnceLock<Regex> = OnceLock::new();
    let s = STRUCT.get_or_init(|| {
        Regex::new(r"(?m)^\s*(?:typedef\s+)?struct\s+([A-Za-z_][A-Za-z0-9_]*)").unwrap()
    });
    scan(content, &[("struct", s)])
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn extract_rust_finds_top_level_symbols() {
        let src = r#"
use std::io;

pub struct Engine {
    pub name: String,
}

enum State {
    Idle,
    Running,
}

pub fn run(name: &str) -> u32 {
    0
}

impl Engine {
    pub fn new() -> Self {
        Engine { name: String::new() }
    }
}
"#;
        let symbols = extract_rust(src);
        let names: Vec<&str> = symbols.iter().map(|s| s.name.as_str()).collect();
        assert!(names.contains(&"Engine"), "got: {:?}", names);
        assert!(names.contains(&"State"), "got: {:?}", names);
        assert!(names.contains(&"run"), "got: {:?}", names);
    }

    #[test]
    fn build_repo_map_walks_a_tempdir() {
        let tmp = TempDir::new().unwrap();
        std::fs::write(
            tmp.path().join("main.rs"),
            "fn main() {}\nstruct Foo;\n",
        )
        .unwrap();
        std::fs::create_dir_all(tmp.path().join("sub")).unwrap();
        std::fs::write(tmp.path().join("sub/lib.rs"), "pub fn helper() {}\n").unwrap();
        // Binary content should be ignored — no valid UTF-8, no symbols.
        std::fs::write(tmp.path().join("img.png"), [0x89, 0x50, 0x4E, 0x47, 0x00]).unwrap();

        let map = build_repo_map(tmp.path());
        assert!(map.file_count() >= 2, "expected main.rs and sub/lib.rs");
        let rendered = map.render(DEFAULT_MAP_CHARS);
        assert!(rendered.contains("main.rs"), "got: {rendered}");
        assert!(rendered.contains("fn main"), "got: {rendered}");
        assert!(rendered.contains("sub/lib.rs"), "got: {rendered}");
        assert!(rendered.contains("fn helper"), "got: {rendered}");
    }

    #[test]
    fn render_honors_budget() {
        let tmp = TempDir::new().unwrap();
        for i in 0..50 {
            std::fs::write(
                tmp.path().join(format!("f{i}.rs")),
                "pub fn a() {}\npub fn b() {}\npub fn c() {}\n",
            )
            .unwrap();
        }
        let map = build_repo_map(tmp.path());
        let rendered = map.render(200);
        assert!(
            rendered.len() < 400,
            "budget 200 should cap rendering; got {}",
            rendered.len()
        );
        assert!(
            rendered.contains("truncated") || rendered.lines().count() < 20,
            "expected truncation marker or short output: {rendered}"
        );
    }
}
