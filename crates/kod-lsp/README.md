# kod-lsp

Minimal Language Server Protocol client for KOD.

Speaks LSP over stdio and exposes only the subset the engine needs:
definitions, references, and diagnostics. Designed to be cheap to
spawn and tear down per query rather than to run a persistent server.

## Usage

```rust
use kod_lsp::language_id_for;
use std::path::Path;

assert_eq!(language_id_for(Path::new("foo.rs")), "rust");
assert_eq!(language_id_for(Path::new("bar.py")), "python");
```

`language_id_for` maps a file extension to the LSP language identifier
the server expects. It is the one piece of this crate that is pure: no
process, no I/O.

Full usage — spawning a server, sending `initialize`, waiting for
`publishDiagnostics` — is driven by `LspClient` and is typically
reached through `kod-core-tools`' `lsp_diagnostics` tool rather than
called directly. The trait surface is `LspClient::new(program,
workspace_root)`, `::initialize().await`, and
`::collect_diagnostics().await`.

## Part of KOD

Reached transitively through `kod-core-tools`.

See the [KOD workspace](https://github.com/elcoosp/kod).

## License

MIT
