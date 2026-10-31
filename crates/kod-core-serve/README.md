# kod-core-serve

Unix-socket daemon and ACP bridge for KOD.

Exposes a running engine to a local client over a Unix socket, and
translates ACP (Agent Client Protocol) frames to engine calls. Used by
the TUI and by the CLI in `serve` mode.

## Usage

```text
kod serve --socket ~/.kod/sock
```

Starts a long-lived daemon that speaks NDJSON over a Unix socket. The
TUI and `kod run --attach` connect to the same socket; each connection
is handled by [`serve_io`]. The ACP variant (`kod serve --acp`) speaks
LSP-style Content-Length framing over stdio for editor integration.

For embedding, `serve_io(engine, reader, writer, shutdown)` runs a
single connection over any `AsyncRead` / `AsyncWrite` pair — useful in
tests and for wiring the daemon into a non-Unix-socket transport.

## Part of KOD

Not part of the library surface.

See the [KOD workspace](https://github.com/elcoosp/kod).

## License

MIT
