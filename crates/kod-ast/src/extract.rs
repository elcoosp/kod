//! Delta §7.3: symbol extraction for the eight non-Rust languages,
//! via the parse cache.
//!
//! Returns the same `(kind, name, line)` tuples `repomap`'s regex
//! extractors produce, so the repo map downstream is unchanged. Each
//! language has a node-kind table; a shared walker emits a symbol for
//! every node whose kind is in the table and that carries a `name`
//! field.
//!
//! # Kind strings (must match the regex paths)
//!
//! | Language | kinds |
//! |---|---|
//! | Python | `def`, `class` |
//! | JS/TS | `function`, `class`, `const` |
//! | Go | `func`, `type` |
//! | Ruby | `def`, `class`, `module` |
//! | Java | `class`, `interface` |
//! | C | `struct` |
//!
//! # What is NOT extracted
//!
//! Only the node kinds above. A language's other declarations
//! (enums, traits, impls, consts beyond JS) are not in the repo map's
//! kind set and are not emitted, so the tree-sitter and regex paths
//! agree on the shape.

use crate::lang::Lang;
use crate::rust::AstSymbol;

/// Extract symbols for `lang` from `source`, or `None` when the parse
/// fails (the caller falls back to its regex path).
pub fn symbols(lang: Lang, source: &str) -> Option<Vec<AstSymbol>> {
    let tree = crate::parse_cache::global().parse(lang, source)?;
    let bytes = source.as_bytes();
    let mut out = Vec::new();
    collect(tree.root_node(), bytes, lang, &mut out);
    Some(out)
}

/// Walk the tree, emitting a symbol for every node the language's kind
/// table names, descending everywhere so nested items are found (the
/// regex scans are `^\s*`-anchored, so indented items count).
fn collect(node: tree_sitter::Node, source: &[u8], lang: Lang, out: &mut Vec<AstSymbol>) {
    if let Some(kind) = node_kind(lang, node)
        && let Some(name) = node_name(node, source, lang)
    {
        out.push(AstSymbol {
            kind,
            name,
            line: node.start_position().row + 1,
        });
    }
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        collect(child, source, lang, out);
    }
}

/// The repo-map kind string for `node`, or `None` if it is not a
/// symbol this language reports.
fn node_kind(lang: Lang, node: tree_sitter::Node) -> Option<&'static str> {
    let k = node.kind();
    match lang {
        Lang::Python => match k {
            "function_definition" => Some("def"),
            "class_definition" => Some("class"),
            _ => None,
        },
        Lang::JavaScript | Lang::TypeScript | Lang::Tsx => match k {
            "function_declaration" => Some("function"),
            "class_declaration" => Some("class"),
            // `const x = …` / `let x = …`: a declarator with a value.
            // `var x` is `variable_declaration`; both are captured by
            // the declarator node, which the regex `(?:const|let|var)`
            // also folds together.
            "variable_declarator" => {
                // Require a value, matching the regex's trailing `=`.
                node.child_by_field_name("value").is_some().then_some("const")
            }
            _ => None,
        },
        Lang::Go => match k {
            "function_declaration" | "method_declaration" => Some("func"),
            "type_spec" => Some("type"),
            _ => None,
        },
        Lang::Ruby => match k {
            "method" => Some("def"),
            "class" => Some("class"),
            "module" => Some("module"),
            _ => None,
        },
        Lang::Java => match k {
            "class_declaration" => Some("class"),
            "interface_declaration" => Some("interface"),
            _ => None,
        },
        Lang::C => match k {
            "struct_specifier" => Some("struct"),
            _ => None,
        },
        // Rust is handled by `crate::rust` (which applies the
        // test-attribute filter at the caller). Returning `None` here
        // keeps this table to the eight.
        Lang::Rust => None,
    }
}

/// The node's name, if it has one. Most kinds use a `name` field; C's
/// `struct_specifier` is anonymous when the struct has no tag, and a
/// missing field means "no symbol".
fn node_name(node: tree_sitter::Node, source: &[u8], _lang: Lang) -> Option<String> {
    let name_node = node.child_by_field_name("name")?;
    name_node.utf8_text(source).ok().map(|s| s.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn names(lang: Lang, src: &str) -> Vec<(&'static str, String)> {
        symbols(lang, src)
            .unwrap_or_default()
            .into_iter()
            .map(|s| (s.kind, s.name))
            .collect()
    }

    #[test]
    fn python_defs_and_classes() {
        let src = "def a():\n    pass\n\nclass B:\n    pass\n";
        let got = names(Lang::Python, src);
        assert!(got.contains(&("def", "a".to_string())), "{got:?}");
        assert!(got.contains(&("class", "B".to_string())), "{got:?}");
    }

    #[test]
    fn javascript_functions_classes_and_consts() {
        let src = "function a() {}\nclass B {}\nconst C = 1;\nlet D = 2;\n";
        let got = names(Lang::JavaScript, src);
        assert!(got.contains(&("function", "a".to_string())), "{got:?}");
        assert!(got.contains(&("class", "B".to_string())), "{got:?}");
        assert!(got.contains(&("const", "C".to_string())), "{got:?}");
        assert!(got.contains(&("const", "D".to_string())), "{got:?}");
    }

    #[test]
    fn typescript_interfaces_are_not_class() {
        // TS `interface` is not in the repo map's kind set (the regex
        // only looks for `class`), so the AST path must not emit it.
        let src = "interface I {}\nclass C {}\n";
        let got = names(Lang::TypeScript, src);
        assert!(!got.iter().any(|(k, _)| *k == "interface"), "{got:?}");
        assert!(got.contains(&("class", "C".to_string())), "{got:?}");
    }

    #[test]
    fn go_funcs_methods_and_types() {
        let src = "package m\nfunc A() {}\nfunc (r R) B() {}\ntype T struct{}\n";
        let got = names(Lang::Go, src);
        assert!(got.contains(&("func", "A".to_string())), "{got:?}");
        assert!(got.contains(&("func", "B".to_string())), "{got:?}");
        assert!(got.contains(&("type", "T".to_string())), "{got:?}");
    }

    #[test]
    fn ruby_defs_classes_and_modules() {
        let src = "def a\nend\nclass B\nend\nmodule M\nend\n";
        let got = names(Lang::Ruby, src);
        assert!(got.contains(&("def", "a".to_string())), "{got:?}");
        assert!(got.contains(&("class", "B".to_string())), "{got:?}");
        assert!(got.contains(&("module", "M".to_string())), "{got:?}");
    }

    #[test]
    fn java_classes_and_interfaces() {
        let src = "public class C {}\ninterface I {}\n";
        let got = names(Lang::Java, src);
        assert!(got.contains(&("class", "C".to_string())), "{got:?}");
        assert!(got.contains(&("interface", "I".to_string())), "{got:?}");
    }

    #[test]
    fn c_structs() {
        let src = "struct S { int x; };\ntypedef struct T { int y; } T;\n";
        let got = names(Lang::C, src);
        assert!(got.contains(&("struct", "S".to_string())), "{got:?}");
        assert!(got.contains(&("struct", "T".to_string())), "{got:?}");
    }

    #[test]
    fn line_numbers_are_one_based() {
        let src = "# comment\n\ndef f():\n    pass\n";
        let s = symbols(Lang::Python, src).unwrap();
        let f = s.iter().find(|s| s.name == "f").unwrap();
        assert_eq!(f.line, 3);
    }
}
