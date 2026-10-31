# kod-core-tools

Adapter layer wrapping KOD's built-in tools for the engine.

Bridges the tool implementations in `kod-tools` and the engine in
`kod-core`: tool discovery, argument validation, permission checks,
and pre-dispatch risk classification all live here.

## Usage

This crate is an adapter layer: the tool types in `lsp_tools`,
`jfind`, `prewalk`, `speculation`, and `tool_loop_guard` are consumed
by `kod-core`'s engine loop, not by end users directly. Most callers
reach them through the engine; a crate that wants to embed the tools
without the full engine can call them directly.

The `Lsp*Tool` types (`lsp_diagnostics`, `lsp_definition`,
`lsp_references`, `lsp_hover`) each wrap a shared `LspManager` and
answer one question about a file plus a cursor position — reaching
the language server through `kod-lsp` under the hood.

`jfind::search` runs a two-stage retrieval over a repo root: lexical
candidates first, then a judge scoring pass. `speculation::*` provides
the file-read primitives the engine uses to speculatively read while
an edit is in flight, with `Evidence` re-validation before the read
is trusted.

## Part of KOD

Internal adapter; end users reach it through `kod-core`.

See the [KOD workspace](https://github.com/elcoosp/kod).

## License

MIT
