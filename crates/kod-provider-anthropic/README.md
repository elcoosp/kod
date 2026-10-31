# kod-provider-anthropic

Anthropic Messages API provider for KOD.

Streaming, non-streaming, native compaction, and the wire module that
preserves prompt-caching blocks the Anthropic client would otherwise
flatten. Two wire paths: the modern path that builds the request body
locally, and the legacy text path via `adk-model`.

## Part of KOD

Reached transitively through `kod-core-routing`.

See the [KOD workspace](https://github.com/elcoosp/kod).

## License

MIT
