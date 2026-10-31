# kod-lsp

Minimal Language Server Protocol client for KOD.

Speaks LSP over stdio and exposes only the subset the engine needs:
definitions, references, and diagnostics. Designed to be cheap to
spawn and tear down per query rather than to run a persistent server.

## Part of KOD

Reached transitively through `kod-core-tools`.

See the [KOD workspace](https://github.com/elcoosp/kod).

## License

MIT
