//! Rust symbol extraction via the parse cache.
//!
//! Returns the same `(kind, name, line)` shape `repomap`'s regex
//! extractor produces, so a caller can swap one for the other. The
//! kinds are the same strings: `fn`, `struct`, `enum`, `trait`,
//! `mod`, `const`.
//!
//! The test-attribute filter (dropping `#[test]` fns) is *not* applied
//! here — it is a repo-map policy, and the caller applies it with the
//! same `has_test_attribute` it uses for the regex path. Keeping the
//! filter in one place is what makes the two paths comparable.

use crate::lang::Lang;

/// One symbol, matching `repomap::Symbol`'s shape.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AstSymbol {
    pub kind: &'static str,
    pub name: String,
    /// 1-based line of the item's start.
    pub line: usize,
}

/// Extract symbols from `source` as Rust, or `None` when the parse
/// fails (the caller falls back to its regex path).
pub fn rust_symbols(source: &str) -> Option<Vec<AstSymbol>> {
    let tree = crate::parse_cache::global().parse(Lang::Rust, source)?;
    let bytes = source.as_bytes();
    let mut out = Vec::new();
    collect(tree.root_node(), bytes, &mut out);
    Some(out)
}

/// Walk the tree, emitting a symbol for every item node that carries
/// a name. Descends everywhere, so a nested `fn` inside a `mod` body
/// is found — matching the regex extractor's `^\s*`-anchored scan.
fn collect(node: tree_sitter::Node, source: &[u8], out: &mut Vec<AstSymbol>) {
    if let Some(kind) = item_kind(node.kind())
        && let Some(name_node) = node.child_by_field_name("name")
        && let Ok(name) = name_node.utf8_text(source)
    {
        out.push(AstSymbol {
            kind,
            name: name.to_string(),
            line: node.start_position().row + 1,
        });
    }
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        collect(child, source, out);
    }
}

/// Map a tree-sitter node kind to the repo map's kind string, or
/// `None` for a node that is not a symbol.
fn item_kind(kind: &str) -> Option<&'static str> {
    match kind {
        "function_item" => Some("fn"),
        "struct_item" => Some("struct"),
        "enum_item" => Some("enum"),
        "trait_item" => Some("trait"),
        "mod_item" => Some("mod"),
        "const_item" => Some("const"),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn kinds(syms: &[AstSymbol]) -> Vec<(&'static str, &str)> {
        syms.iter().map(|s| (s.kind, s.name.as_str())).collect()
    }

    #[test]
    fn finds_the_core_item_kinds() {
        let src = "\
pub fn a() {}
struct B;
enum C { X }
trait D {}
mod e {}
const F: u32 = 1;
";
        let syms = rust_symbols(src).expect("parses");
        assert_eq!(
            kinds(&syms),
            vec![
                ("fn", "a"),
                ("struct", "B"),
                ("enum", "C"),
                ("trait", "D"),
                ("mod", "e"),
                ("const", "F"),
            ],
        );
    }

    #[test]
    fn line_numbers_are_one_based_and_accurate() {
        let src = "// header\n\nfn at_three() {}\n";
        let syms = rust_symbols(src).unwrap();
        assert_eq!(syms.len(), 1);
        assert_eq!(syms[0].line, 3, "fn is on line 3");
    }

    #[test]
    fn finds_nested_items() {
        // The regex scan is `^\s*`-anchored, so an indented item inside
        // a module body is found. Tree-sitter must match that.
        let src = "mod m {\n    pub fn inner() {}\n}\n";
        let syms = rust_symbols(src).unwrap();
        let names: Vec<&str> = syms.iter().map(|s| s.name.as_str()).collect();
        assert!(names.contains(&"m"), "outer mod: {names:?}");
        assert!(names.contains(&"inner"), "inner fn: {names:?}");
    }

    #[test]
    fn does_not_emit_impl_or_type_items() {
        // Those are not in the repo map's kind set; emitting them
        // would make the tree-sitter and regex paths disagree.
        let src = "struct S;\nimpl S { fn m(&self) {} }\ntype T = u32;\n";
        let syms = rust_symbols(src).unwrap();
        let ks: Vec<&str> = syms.iter().map(|s| s.kind).collect();
        assert!(!ks.contains(&"impl"), "no impl: {ks:?}");
        // The method inside `impl` *is* a fn (regex finds it too).
        assert!(syms.iter().any(|s| s.name == "m" && s.kind == "fn"));
    }

    #[test]
    fn empty_source_is_no_symbols() {
        assert_eq!(rust_symbols("").unwrap(), Vec::new());
    }

    #[test]
    fn a_broken_file_still_yields_the_items_it_can() {
        // tree-sitter is error-tolerant: a missing brace does not stop
        // it finding the items before the break.
        let src = "fn a() {}\nstruct B { \n";
        let syms = rust_symbols(src).unwrap();
        let names: Vec<&str> = syms.iter().map(|s| s.name.as_str()).collect();
        assert!(names.contains(&"a"), "got: {names:?}");
    }
}
