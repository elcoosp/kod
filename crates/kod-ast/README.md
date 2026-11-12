# kod-ast

Tree-sitter parsing with a process-global parse cache for KOD.

Builds the repo-map understanding the KOD engine uses to answer
questions like where is X defined and who calls Y. Wraps `tree-sitter`
plus a per-language grammar set (Rust, Python, TypeScript, JavaScript,
Go, Ruby, Java, C) behind a single parse service and caches parse
trees for the lifetime of the process.

## Usage

```rust
use kod_ast::rust::rust_symbols;

let syms = rust_symbols("pub fn main() {}").expect("parses");
assert_eq!(syms[0].kind, "fn");
assert_eq!(syms[0].name, "main");
```

`rust_symbols(source)` parses `source` with the tree-sitter Rust
grammar and returns the symbols it found — `fn`, `struct`, `enum`,
`trait`, `mod`, `const` — each with its 1-based start line. The parse
tree is cached process-wide by `parse_cache::global()`, keyed on
`(language, source hash, source length)` with a byte-for-byte check
on a hash hit. A second call with the same source reuses the tree.

A `None` return means the parse failed; callers fall back to their
regex path rather than refusing to answer.

## Part of KOD

Reached transitively through `kod-core` and `kod-core-quality`.

See the [KOD workspace](https://github.com/elcoosp/kod).

