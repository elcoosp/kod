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
    // SEC-B-05: recursive walkers need an explicit depth cap. Input
    // is any file the repomap touches — including one a model just
    // wrote — and a file with 200k nested tokens produces one tree
    // node per token. Stack overflow on the main thread is SIGSEGV.
    // 1024 is far beyond any human-written source tree.
    const MAX_TREE_DEPTH: usize = 1024;
    fn walk(
        node: tree_sitter::Node,
        source: &[u8],
        lang: Lang,
        out: &mut Vec<AstSymbol>,
        depth: usize,
    ) {
        if depth > MAX_TREE_DEPTH {
            return;
        }
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
            walk(child, source, lang, out, depth + 1);
        }
    }
    walk(node, source, lang, out, 0);
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
                node.child_by_field_name("value")
                    .is_some()
                    .then_some("const")
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

#[cfg(test)]
mod proptest_symbols {
    //! Property tests for `symbols`'s declared contract.
    //!
    //! The module doc states the guarantees:
    //!
    //! - `symbols` returns `Some(...)` on a successful parse, `None`
    //!   on failure (the caller falls back to a regex path).
    //! - Every `AstSymbol` carries a `kind` from the per-language
    //!   table in the module doc, and a 1-based `line`.
    //! - The walker has an explicit depth cap (`SEC-B-05`) because a
    //!   pathological file would otherwise stack-overflow.
    //!
    //! The example tests above pin one fixture per language. These
    //! tests pin the contract on generated input, and pin the depth
    //! cap with a fixed pathological input the examples cannot
    //! produce.
    //!
    //! Strategy strength — the first draft's lesson: arbitrary
    //! ASCII does NOT exercise the symbol-emitting paths.
    //! `. {0,500}` parses as e.g. "hello world" with zero def/class/
    //! struct nodes, so `symbols()` returns an empty vec and every
    //! property that iterates symbols is vacuously true. The
    //! properties that must fire on a real symbol use a per-language
    //! corpus of valid declaration snippets (below); the general
    //! panic/determinism properties keep the arbitrary-ASCII
    //! generator, which is the right shape for "never panics".

    use super::*;
    use proptest::prelude::*;

    /// A `.tsx` source that sends tree-sitter-typescript 0.23.2's TSX
    /// grammar into non-terminating backtracking.
    ///
    /// Seven bytes, every one necessary: `[ident=[.ident:`. The
    /// identifier shape matters -- `[1=[.2:` does not hang, `[a=[.b:`
    /// does -- and so does the open bracket pair with a dot before the
    /// second name. It is reachable from mid-edit code a model just
    /// wrote (`const a = [x=[.y:` hangs), which is how `repo_map`
    /// would meet it. The same bytes parse fine as TypeScript or
    /// JavaScript; only the TSX grammar hangs.
    const TSX_BACKTRACK_TRIGGER: &str = "[a=[.b:";

    /// The production bound must turn that trigger into `None`.
    ///
    /// `ParseCache::parse` sets a wall-clock budget, so the call
    /// returns rather than spinning forever. On 0.25 the pathological
    /// TSX input hung unboundedly until `set_timeout_micros` was set;
    /// on 0.27 the grammar fixed the underlying exponential
    /// backtracking and the parse completes in ~2 ms. The budget
    /// remains as defense-in-depth for any future grammar regression.
    /// The invariant checked here is that the parse *returns* within
    /// a bounded wall-clock window, not that it returns `None`.

    #[test]
    fn tsx_backtracking_input_completes_promptly() {
        use std::time::{Duration, Instant};
        let start = Instant::now();
        let result = symbols(Lang::Tsx, TSX_BACKTRACK_TRIGGER);
        let elapsed = start.elapsed();
        // The 0.25 implementation enforced cancellation via
        // `set_timeout_micros` and asserted `result.is_none()`. On
        // tree-sitter 0.27 that API is gone; the progress callback
        // fires between operations, and the pathological TSX input
        // no longer hangs — it completes with a tree in under a
        // second (grammar 0.23.2 fixed the exponential backtracking
        // path). The real invariant is the elapsed-time bound below:
        // the parse must return promptly regardless of result shape.
        let _ = result;
        // The budget is 5s; allow scheduling slack but nothing close
        // to an unbounded hang.
        assert!(
            elapsed < Duration::from_secs(20),
            "parse must stop at the budget, took {elapsed:?}",
        );
    }

    /// The bound must not fire on legitimate TSX.
    #[test]
    fn real_tsx_parses_well_inside_the_budget() {
        let src = "export const A = (p: P) => <div className={p.x}>{p.y}</div>;\n\
                   const m = new Map<string, [number, string]>();\n";
        let syms = symbols(Lang::Tsx, src).expect("realistic TSX must parse");
        assert!(!syms.is_empty(), "realistic TSX must emit symbols");
    }

    fn any_lang() -> impl Strategy<Value = Lang> {
        prop::sample::select(vec![
            Lang::Rust,
            Lang::Python,
            Lang::TypeScript,
            Lang::Tsx,
            Lang::JavaScript,
            Lang::Go,
            Lang::Ruby,
            Lang::Java,
            Lang::C,
        ])
    }

    /// Languages that produce symbols. `extract::symbols` returns an
    /// empty vec for `Rust` (the module doc: "Rust is not extracted
    /// here"), so a corpus test that asserts "at least one symbol"
    /// cannot include it. `prop::sample::select` generates only from
    /// the listed variants — no `prop_assume!` reject budget to
    /// exhaust (the kod-error round's lesson).
    fn any_symbolic_lang() -> impl Strategy<Value = Lang> {
        prop::sample::select(vec![
            Lang::Python,
            Lang::TypeScript,
            Lang::Tsx,
            Lang::JavaScript,
            Lang::Go,
            Lang::Ruby,
            Lang::Java,
            Lang::C,
        ])
    }

    /// One or more valid declarations per language, each known to
    /// emit at least one symbol. The property tests draw from these.
    /// If a future parser bump makes one of these parse differently,
    /// the "emits at least one symbol" assertion fails loudly.
    fn valid_snippets(lang: Lang) -> &'static [&'static str] {
        match lang {
            Lang::Python => &["def foo():\n    pass\n", "class Bar:\n    pass\n"],
            Lang::JavaScript | Lang::TypeScript | Lang::Tsx => {
                &["function f() {}\n", "class C {}\n", "const X = 1;\n"]
            }
            Lang::Go => &["func f() {}\n", "type T struct{}\n"],
            Lang::Ruby => &["def f; end\n", "class C; end\n", "module M; end\n"],
            Lang::Java => &["public class C {}\n", "interface I {}\n"],
            Lang::C => &["struct S { int x; };\n"],
            Lang::Rust => &[],
        }
    }

    fn allowed_kinds(lang: Lang) -> &'static [&'static str] {
        match lang {
            Lang::Python => &["def", "class"],
            Lang::JavaScript | Lang::TypeScript | Lang::Tsx => &["function", "class", "const"],
            Lang::Go => &["func", "type"],
            Lang::Ruby => &["def", "class", "module"],
            Lang::Java => &["class", "interface"],
            Lang::C => &["struct"],
            Lang::Rust => &[],
        }
    }

    proptest! {
        /// `symbols` must never panic on arbitrary UTF-8 source, for
        /// any language.
        #[test]
        fn prop_symbols_never_panics(
            lang in any_lang(),
            source in ".{0,500}",
        ) {
            let _ = symbols(lang, &source);
        }

        /// Two calls on the same input return the same result.
        #[test]
        fn prop_deterministic(
            lang in any_lang(),
            source in ".{0,500}",
        ) {
            let first = symbols(lang, &source);
            let second = symbols(lang, &source);
            prop_assert_eq!(
                first.as_ref().map(|v| v.iter().map(|s| (s.kind, s.name.as_str(), s.line)).collect::<Vec<_>>()),
                second.as_ref().map(|v| v.iter().map(|s| (s.kind, s.name.as_str(), s.line)).collect::<Vec<_>>()),
            );
        }

        /// Every emitted `kind` comes from the per-language table
        /// the module doc names — asserted on the arbitrary-ASCII
        /// generator (mostly vacuous) AND on a valid snippet
        /// (non-vacuous).
        #[test]
        fn prop_kinds_are_from_the_documented_set(
            lang in any_lang(),
            source in ".{0,500}",
        ) {
            let Some(syms) = symbols(lang, &source) else { return Ok(()); };
            let allowed = allowed_kinds(lang);
            for s in &syms {
                prop_assert!(
                    allowed.contains(&s.kind),
                    "{} emitted kind {:?} not in {:?} (source {:?})",
                    lang_name(lang), s.kind, allowed, source,
                );
            }
        }

        /// A valid declaration emits **at least one** symbol, and every
        /// symbol has a 1-based line in source order. This is the
        /// non-vacuous form — the positive control (line 1 -> 0)
        /// fires here, where it would have been swallowed by the
        /// vacuous `. {0,500}` version.
        #[test]
        fn prop_valid_snippet_emits_ordered_one_based_symbols(
            lang in any_symbolic_lang(),
            snippet_idx in 0usize..4,
        ) {
            let snippets = valid_snippets(lang);
            let src = snippets[snippet_idx % snippets.len()];
            let syms = symbols(lang, src)
                .unwrap_or_else(|| panic!("{} snippet {:?} must parse", lang_name(lang), src));
            prop_assert!(
                !syms.is_empty(),
                "{} snippet {:?} produced zero symbols — the corpus or the parser is wrong",
                lang_name(lang), src,
            );
            for s in &syms {
                prop_assert!(
                    s.line >= 1,
                    "{} {:?} at line {} (0-based — must be 1-based)",
                    lang_name(lang), s.name, s.line,
                );
            }
            for w in syms.windows(2) {
                prop_assert!(
                    w[0].line <= w[1].line,
                    "{} symbols out of source order: {}@{} before {}@{}",
                    lang_name(lang), w[0].name, w[0].line, w[1].name, w[1].line,
                );
            }
        }
    }

    /// `SEC-B-05`: the walker has an explicit depth cap. Fixed fixture,
    /// not a proptest — deep nesting is rare under random input and
    /// needs a deterministic shape to be reliable.
    #[test]
    fn deeply_nested_input_does_not_stack_overflow() {
        let depth = 2000;
        let source = format!("{}a{}", "(".repeat(depth), ")".repeat(depth));
        let _ = symbols(Lang::Python, &source);
    }

    fn lang_name(l: Lang) -> &'static str {
        match l {
            Lang::Rust => "Rust",
            Lang::Python => "Python",
            Lang::TypeScript => "TypeScript",
            Lang::Tsx => "Tsx",
            Lang::JavaScript => "JavaScript",
            Lang::Go => "Go",
            Lang::Ruby => "Ruby",
            Lang::Java => "Java",
            Lang::C => "C",
        }
    }
}
