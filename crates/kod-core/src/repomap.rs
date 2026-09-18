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
    /// Symbols per file, kept for callers that want them directly.
    pub entries: BTreeMap<PathBuf, Vec<Symbol>>,
    /// Cross-file references: for each file, the paths it imports
    /// (via `use`, `mod`, `import`, `#include`, `from ... import`,
    /// ...). Populated by `build_repo_map` when the language is
    /// recognized; empty otherwise.
    pub imports: BTreeMap<PathBuf, Vec<PathBuf>>,
    /// Per-file PageRank score computed from `imports`. Higher means
    /// more files depend on it. All files present in `entries` have
    /// an entry here (default 1.0 when they have no inbound or
    /// outbound references).
    pub rank: BTreeMap<PathBuf, f32>,
}

/// Default character budget for the rendered map. At the workspace's
/// 4-chars-per-token rule of thumb this is ~4k tokens, comfortable
/// inside an 8k-context model's prompt alongside everything else.
pub const DEFAULT_MAP_CHARS: usize = 16_000;

impl RepoMap {
    /// Render as a compact text block, one file per line, ordered by
    /// PageRank descending with a lexical tie-break. The budget goes
    /// to the files that matter, not to whichever happens to sort
    /// first alphabetically.
    pub fn render(&self, max_chars: usize) -> String {
        // Sort: rank desc, then path asc.
        let mut files: Vec<(&PathBuf, &Vec<Symbol>)> = self.entries.iter().collect();
        files.sort_by(|a, b| {
            let ra = self.rank.get(a.0).copied().unwrap_or(0.0);
            let rb = self.rank.get(b.0).copied().unwrap_or(0.0);
            rb.partial_cmp(&ra)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then_with(|| a.0.cmp(b.0))
        });

        let mut out = String::new();
        for (path, symbols) in files {
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
    let mut entries: BTreeMap<PathBuf, Vec<Symbol>> = BTreeMap::new();
    let mut raw_imports: BTreeMap<PathBuf, Vec<String>> = BTreeMap::new();
    let mut all_files: BTreeMap<PathBuf, PathBuf> = BTreeMap::new(); // rel -> abs

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
        all_files.insert(rel.clone(), path.to_path_buf());
        let (symbols, imports) = extract_symbols_and_imports(path);
        if !symbols.is_empty() {
            entries.insert(rel.clone(), symbols);
        }
        if !imports.is_empty() {
            raw_imports.insert(rel, imports);
        }
    }

    // Resolve import tokens ("crate::foo::bar", "./sibling", "foo.h")
    // into paths that exist in `all_files`. Resolution is fuzzy: we
    // try several candidates per token and take the first match.
    let mut imports: BTreeMap<PathBuf, Vec<PathBuf>> = BTreeMap::new();
    for (from, tokens) in &raw_imports {
        let mut targets: Vec<PathBuf> = Vec::new();
        for token in tokens {
            for candidate in resolve_import_candidates(from, token) {
                if all_files.contains_key(&candidate) {
                    if !targets.contains(&candidate) {
                        targets.push(candidate);
                    }
                    break;
                }
            }
        }
        if !targets.is_empty() {
            imports.insert(from.clone(), targets);
        }
    }

    let rank = compute_pagerank(&entries, &imports);

    RepoMap {
        entries,
        imports,
        rank,
    }
}

/// Resolve an import token into one or more candidate paths, tried in
/// order. Language-agnostic: each language produces tokens in its own
/// shape and the candidate list covers the common cases.
fn resolve_import_candidates(from: &Path, token: &str) -> Vec<PathBuf> {
    let mut out: Vec<PathBuf> = Vec::new();
    let from_dir = from.parent().unwrap_or(Path::new(""));

    // C family: "foo/bar.h" or "foo.h" — the token is a relative path.
    let token_no_quotes = token.trim_matches(|c| c == '"' || c == '<' || c == '>');
    if token_no_quotes.contains('.') || token_no_quotes.contains('/') {
        out.push(from_dir.join(token_no_quotes));
    }

    // Rust: "crate::a::b" or "crate::a::b::Item" — the module path
    // maps to `src/a/b.rs` or `src/a/b/mod.rs`.
    let cleaned = token
        .trim_start_matches("crate::")
        .trim_start_matches("self::")
        .trim_start_matches("super::");
    let parts: Vec<&str> = cleaned.split("::").collect();
    // Try progressive truncations: drop the last component (which is
    // usually the imported item, not the module).
    for drop in 0..parts.len().min(3) {
        let take = parts.len() - drop;
        if take == 0 {
            continue;
        }
        let path_parts: Vec<&str> = parts[..take].to_vec();
        let module = path_parts.join("/");
        // src/<module>.rs
        out.push(PathBuf::from(format!("src/{module}.rs")));
        // <from_dir>/<module>.rs — relative within the same tree
        out.push(from_dir.join(format!("{module}.rs")));
        // <module>/mod.rs
        out.push(PathBuf::from(format!("src/{module}/mod.rs")));
        out.push(from_dir.join(&module).join("mod.rs"));
    }

    // Python: "a.b.c" or "from a.b import c" — the token may be a
    // package path with dots or a relative "..pkg".
    if token.contains('.') && !token.contains('/') {
        let path = token.replace('.', "/");
        out.push(PathBuf::from(format!("{path}.py")));
        out.push(PathBuf::from(format!("{path}/__init__.py")));
        out.push(from_dir.join(format!("{path}.py")));
    }

    // JS/TS: "./sibling" or "../other/mod" — resolve against
    // from_dir; extensions tried by the caller's exists check.
    if token.starts_with("./") || token.starts_with("../") {
        let base = from_dir.join(token);
        out.push(base.with_extension("ts"));
        out.push(base.with_extension("tsx"));
        out.push(base.with_extension("js"));
        out.push(base.with_extension("jsx"));
        out.push(base.join("index.ts"));
        out.push(base.join("index.tsx"));
        out.push(base.join("index.js"));
    }

    out
}

/// PageRank-lite: rank scores proportional to incoming references,
/// iterated 20 times with damping 0.85. Files with no references at
/// all converge to a base score of `(1 - damping) / N`, giving the
/// lexical tie-break a chance to order them without rank noise.
fn compute_pagerank(
    entries: &BTreeMap<PathBuf, Vec<Symbol>>,
    imports: &BTreeMap<PathBuf, Vec<PathBuf>>,
) -> BTreeMap<PathBuf, f32> {
    const DAMPING: f32 = 0.85;
    const ITERATIONS: usize = 20;

    let n = entries.len().max(1) as f32;
    let base = (1.0 - DAMPING) / n;

    // Build the incoming-edges map: for each path, the set of files
    // that import it.
    let mut incoming: BTreeMap<PathBuf, Vec<PathBuf>> = BTreeMap::new();
    let mut out_degree: BTreeMap<PathBuf, f32> = BTreeMap::new();
    for (from, targets) in imports {
        let deg = targets.len() as f32;
        if deg == 0.0 {
            continue;
        }
        *out_degree.entry(from.clone()).or_insert(0.0) = deg;
        for t in targets {
            incoming.entry(t.clone()).or_default().push(from.clone());
        }
    }

    // Initialize to 1/N.
    let mut rank: BTreeMap<PathBuf, f32> = entries.keys().map(|k| (k.clone(), 1.0 / n)).collect();

    for _ in 0..ITERATIONS {
        let mut next: BTreeMap<PathBuf, f32> = entries.keys().map(|k| (k.clone(), base)).collect();
        for (path, sources) in &incoming {
            if !next.contains_key(path) {
                continue;
            }
            let contrib: f32 = sources
                .iter()
                .map(|src| {
                    let r = rank.get(src).copied().unwrap_or(0.0);
                    let deg = out_degree.get(src).copied().unwrap_or(1.0);
                    if deg > 0.0 { DAMPING * r / deg } else { 0.0 }
                })
                .sum();
            if let Some(slot) = next.get_mut(path) {
                *slot += contrib;
            }
        }
        rank = next;
    }

    rank
}

/// Extract (symbols, import tokens) from one file. The imports are
/// raw tokens from the file — the caller resolves them against the
/// set of files it knows about. Empty imports when the extension has
/// no import extractor.
fn extract_symbols_and_imports(path: &Path) -> (Vec<Symbol>, Vec<String>) {
    const MAX_FILE_BYTES: u64 = 2 * 1024 * 1024;
    if let Ok(meta) = std::fs::metadata(path)
        && meta.len() > MAX_FILE_BYTES
    {
        return (Vec::new(), Vec::new());
    }
    let content = match std::fs::read_to_string(path) {
        Ok(c) => c,
        Err(_) => return (Vec::new(), Vec::new()),
    };
    let ext = path.extension().and_then(|s| s.to_str()).unwrap_or("");
    let symbols = match ext {
        "rs" => extract_rust(&content),
        "py" => extract_python(&content),
        "js" | "jsx" | "ts" | "tsx" => extract_js(&content),
        "go" => extract_go(&content),
        "rb" => extract_ruby(&content),
        "java" => extract_java(&content),
        "c" | "h" | "cc" | "cpp" | "hpp" | "cxx" => extract_c(&content),
        _ => Vec::new(),
    };
    let imports = extract_imports(ext, &content);
    (symbols, imports)
}

/// Pull every import-like token from a source file. Deliberately
/// crude: one regex per common language shape, results deduplicated.
/// A real parser is a follow-up; today's goal is enough signal for
/// PageRank to prefer the hubs over alphabetically-first files.
fn extract_imports(ext: &str, content: &str) -> Vec<String> {
    use regex::Regex;
    // A small set of patterns keyed by language family.
    let patterns: &[&str] = match ext {
        "rs" => &[
            r"(?m)^\s*use\s+([a-zA-Z_][a-zA-Z0-9_:]*)\s*[;{]",
            r"(?m)^\s*(?:pub\s+)?mod\s+([a-zA-Z_][a-zA-Z0-9_]*)\s*;",
        ],
        "py" => &[
            r"(?m)^\s*from\s+([a-zA-Z_][a-zA-Z0-9_.]*)\s+import",
            r"(?m)^\s*import\s+([a-zA-Z_][a-zA-Z0-9_.]*)",
        ],
        "js" | "jsx" | "ts" | "tsx" => &[
            r#"(?m)^\s*import\s+(?:[^'"]+\s+from\s+)?['"]([^'"]+)['"]"#,
            r#"(?m)require\(\s*['"]([^'"]+)['"]\s*\)"#,
        ],
        "go" => &[
            r#"(?m)^\s*import\s+['"]([^'"]+)['"]"#,
            r#"(?m)^\s*import\s*\(\s*
(?:\s*['"]([^'"]+)['"]\s*
)*"#,
        ],
        "c" | "h" | "cc" | "cpp" | "hpp" | "cxx" => &[r#"(?m)^\s*#\s*include\s+["<]([^">]+)[">]"#],
        "rb" => &[r#"(?m)^\s*require(?:_relative)?\s+['"]([^'"]+)['"]"#],
        "java" => &[r"(?m)^\s*import\s+([a-zA-Z_][a-zA-Z0-9_.]*)\s*;"],
        _ => return Vec::new(),
    };

    let mut out: Vec<String> = Vec::new();
    for pat in patterns {
        if let Ok(re) = Regex::new(pat) {
            for cap in re.captures_iter(content) {
                if let Some(m) = cap.get(1) {
                    let s = m.as_str().trim().to_string();
                    if !s.is_empty() && !out.contains(&s) {
                        out.push(s);
                    }
                }
            }
        }
    }
    out
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
        Regex::new(r"(?m)^\s*(?:export\s+)?(?:async\s+)?function\s+([A-Za-z_$][A-Za-z0-9_$]*)")
            .unwrap()
    });
    let c = CLASS.get_or_init(|| {
        Regex::new(r"(?m)^\s*(?:export\s+)?class\s+([A-Za-z_$][A-Za-z0-9_$]*)").unwrap()
    });
    let k = CONST.get_or_init(|| {
        Regex::new(r"(?m)^\s*(?:export\s+)?(?:const|let|var)\s+([A-Za-z_$][A-Za-z0-9_$]*)\s*=")
            .unwrap()
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
    let m =
        MODULE.get_or_init(|| Regex::new(r"(?m)^\s*module\s+([A-Za-z_][A-Za-z0-9_]*)").unwrap());
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

    /// Regression target for D5-L4: the repomap sorts by PageRank, not
    /// alphabetically. A file imported by three others must appear
    /// before a file no one imports, even if the latter sorts first
    /// lexically.
    #[test]
    fn test_rank_favors_hubs() {
        let tmp = TempDir::new().unwrap();
        // Create a "hub" and three "spokes" that import it.
        std::fs::write(tmp.path().join("hub.rs"), "pub fn shared() {}\n").unwrap();
        std::fs::write(
            tmp.path().join("aaa.rs"),
            "use crate::hub;\npub fn a() {}\n",
        )
        .unwrap();
        std::fs::write(
            tmp.path().join("bbb.rs"),
            "use crate::hub;\npub fn b() {}\n",
        )
        .unwrap();
        std::fs::write(
            tmp.path().join("ccc.rs"),
            "use crate::hub;\npub fn c() {}\n",
        )
        .unwrap();
        // A lexical-first file that nothing imports. Its rank must be
        // lower than the hub's.
        std::fs::write(tmp.path().join("aaa_only.rs"), "pub fn alone() {}\n").unwrap();

        let map = build_repo_map(tmp.path());
        let hub_rank = map
            .rank
            .iter()
            .find(|(k, _)| k.ends_with("hub.rs"))
            .map(|(_, v)| *v)
            .unwrap_or(0.0);
        let alone_rank = map
            .rank
            .iter()
            .find(|(k, _)| k.ends_with("aaa_only.rs"))
            .map(|(_, v)| *v)
            .unwrap_or(0.0);
        assert!(
            hub_rank > alone_rank,
            "hub ({hub_rank}) should outrank an unimported file ({alone_rank})"
        );

        // And render puts the hub before the unimported file even
        // though alphabetical order would put `aaa_only.rs` first.
        let rendered = map.render(DEFAULT_MAP_CHARS);
        let hub_pos = rendered.find("hub.rs").unwrap_or(usize::MAX);
        let alone_pos = rendered.find("aaa_only.rs").unwrap_or(usize::MAX);
        assert!(
            hub_pos < alone_pos,
            "hub should render before unimported file: {rendered}"
        );
    }

    /// Budget goes to the hubs: with a tight cap, the hub is
    /// rendered and the low-rank files are dropped.
    #[test]
    fn test_render_budget_goes_to_hubs() {
        let tmp = TempDir::new().unwrap();
        // Hub imported by many spokes; each file is one line, so a
        // small cap fits only the first few lines.
        std::fs::write(tmp.path().join("hub.rs"), "pub fn shared() {}\n").unwrap();
        for i in 0..20 {
            std::fs::write(
                tmp.path().join(format!("spoke{i:02}.rs")),
                "use crate::hub;\npub fn s() {}\n",
            )
            .unwrap();
        }
        let map = build_repo_map(tmp.path());
        // Budget that fits roughly three lines.
        let rendered = map.render(200);
        assert!(
            rendered.contains("hub.rs"),
            "hub must be in the first lines under a tight budget: {rendered}"
        );
        assert!(
            rendered.contains("truncated"),
            "tight budget should report truncation: {rendered}"
        );
    }

    /// `extract_imports` finds Rust use/mod tokens.
    #[test]
    fn test_extract_imports_rust() {
        let content = "use crate::foo::bar;\npub mod baz;\n";
        let imports = extract_imports("rs", content);
        assert!(
            imports.iter().any(|s| s == "crate::foo::bar"),
            "got: {imports:?}"
        );
        assert!(imports.iter().any(|s| s == "baz"), "got: {imports:?}");
    }

    /// `extract_imports` finds C include tokens.
    #[test]
    fn test_extract_imports_c() {
        let content = "#include <stdio.h>\n#include \"local.h\"\n";
        let imports = extract_imports("c", content);
        assert!(imports.iter().any(|s| s == "stdio.h"), "got: {imports:?}");
        assert!(imports.iter().any(|s| s == "local.h"), "got: {imports:?}");
    }

    #[test]
    fn build_repo_map_walks_a_tempdir() {
        let tmp = TempDir::new().unwrap();
        std::fs::write(tmp.path().join("main.rs"), "fn main() {}\nstruct Foo;\n").unwrap();
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

#[cfg(test)]
mod coverage_language_extractors {
    //! Each language extractor is a regex list. A regression drops
    //! a symbol from the repository map, which then vanishes from
    //! every prompt — invisible until the model repeatedly asks
    //! "what does this function do" for a symbol it cannot see.
    //! These pin the top-level shape for each supported language.
    use super::*;

    #[test]
    fn rust_ignores_nested_fn_definitions() {
        // A nested `fn` inside a function body is an implementation
        // detail, not a top-level symbol. The regex anchors on
        // line start (with optional `pub`), so an indented inner
        // `fn` is not matched.
        let src = "pub fn outer() {\n    fn inner() {}\n}\n";
        let syms = extract_rust(src);
        let names: Vec<&str> = syms.iter().map(|s| s.name.as_str()).collect();
        assert!(names.contains(&"outer"));
        assert!(
            !names.contains(&"inner"),
            "nested fn leaked into top-level symbols: {names:?}",
        );
    }

    #[test]
    fn rust_pub_crate_is_recognised() {
        let src = "pub(crate) struct Foo;\npub(super) fn bar() {}\n";
        let syms = extract_rust(src);
        let names: Vec<&str> = syms.iter().map(|s| s.name.as_str()).collect();
        assert!(names.contains(&"Foo"));
        assert!(names.contains(&"bar"));
    }

    #[test]
    fn rust_async_and_unsafe_fns_are_recognised() {
        let src = "pub async fn a() {}\npub unsafe fn b() {}\n";
        let syms = extract_rust(src);
        let names: Vec<&str> = syms.iter().map(|s| s.name.as_str()).collect();
        assert!(names.contains(&"a"));
        assert!(names.contains(&"b"));
    }

    #[test]
    fn python_finds_defs_and_classes() {
        let src = "class Foo:\n    pass\n\ndef bar():\n    pass\n\nasync def baz():\n    pass\n";
        let syms = extract_python(src);
        let names: Vec<&str> = syms.iter().map(|s| s.name.as_str()).collect();
        assert!(names.contains(&"Foo"));
        assert!(names.contains(&"bar"));
        assert!(names.contains(&"baz"));
    }

    #[test]
    fn javascript_finds_functions_and_classes() {
        let src = "export function alpha() {}\nclass Beta {}\nconst gamma = () => {};\n";
        let syms = extract_js(src);
        let names: Vec<&str> = syms.iter().map(|s| s.name.as_str()).collect();
        assert!(names.contains(&"alpha"));
        assert!(names.contains(&"Beta"));
        assert!(names.contains(&"gamma"));
    }

    #[test]
    fn typescript_uses_the_same_extractor() {
        // TypeScript and JavaScript share the extractor; a change
        // that broke TS but not JS is impossible to see without
        // pinning the TS shape.
        let src = "export function foo(): number { return 1; }\n";
        let syms = extract_js(src);
        assert!(syms.iter().any(|s| s.name == "foo"));
    }

    #[test]
    fn go_finds_functions_with_receivers() {
        // A method on a type has a receiver in the signature:
        //   func (t *T) Name(...)
        // The extractor's regex allows the receiver and still
        // captures the method name.
        let src = "package p\nfunc Foo() {}\nfunc (t *T) Bar() {}\ntype Baz struct{}\n";
        let syms = extract_go(src);
        let names: Vec<&str> = syms.iter().map(|s| s.name.as_str()).collect();
        assert!(names.contains(&"Foo"));
        assert!(names.contains(&"Bar"));
        assert!(names.contains(&"Baz"));
    }

    #[test]
    fn ruby_finds_defs_classes_and_modules() {
        let src = "class A\n  def foo\n  end\nend\nmodule B\n  def bar\n  end\nend\n";
        let syms = extract_ruby(src);
        let names: Vec<&str> = syms.iter().map(|s| s.name.as_str()).collect();
        assert!(names.contains(&"A"));
        assert!(names.contains(&"B"));
        assert!(names.contains(&"foo"));
        assert!(names.contains(&"bar"));
    }

    #[test]
    fn java_finds_classes_and_interfaces() {
        let src = "public class A {}\npublic interface B {}\n";
        let syms = extract_java(src);
        let names: Vec<&str> = syms.iter().map(|s| s.name.as_str()).collect();
        assert!(names.contains(&"A"));
        assert!(names.contains(&"B"));
    }

    #[test]
    fn c_finds_structs() {
        let src = "typedef struct Foo { int x; } Foo;\nstruct Bar { int y; };\n";
        let syms = extract_c(src);
        let names: Vec<&str> = syms.iter().map(|s| s.name.as_str()).collect();
        assert!(names.contains(&"Foo"));
        assert!(names.contains(&"Bar"));
    }

    #[test]
    fn extractor_respects_the_max_file_size_cap() {
        // A 2 MB cap in `extract_symbols_and_imports` skips huge
        // files. The check is bounded by file size, not symbol
        // count; a change that removed the cap would make the map
        // build read a lockfile line by line.
        let tmp = tempfile::TempDir::new().unwrap();
        let big = tmp.path().join("big.rs");
        // 3 MB of `pub fn fN() {}` lines.
        let mut body = String::with_capacity(3 * 1024 * 1024);
        for i in 0..100_000 {
            body.push_str(&format!("pub fn f{i}() {{}}\n"));
            if body.len() > 3 * 1024 * 1024 {
                break;
            }
        }
        std::fs::write(&big, body).unwrap();
        // `build_repo_map` walks the directory; the file exceeds the
        // internal cap and should not appear.
        let map = build_repo_map(tmp.path());
        assert!(
            map.entries.is_empty(),
            "oversized file should have been skipped",
        );
    }
}
