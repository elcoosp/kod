# kod-ast

Tree-sitter parsing with a process-global parse cache for KOD.

Builds the repo-map understanding the KOD engine uses to answer
questions like where is X defined and who calls Y. Wraps `tree-sitter`
plus a per-language grammar set (Rust, Python, TypeScript, JavaScript,
Go, Ruby, Java, C) behind a single parse service and caches parse
trees for the lifetime of the process.

## Part of KOD

Reached transitively through `kod-core` and `kod-core-quality`.

See the [KOD workspace](https://github.com/elcoosp/kod).

## License

MIT
