# kod-schema-dialect

JSON Schema dialect adaptation for KOD providers.

Adapts a JSON Schema to the subset each LLM provider accepts: renames
forbidden constructs, drops unsupported keywords, and remembers
per-endpoint rejections so retries drop only the offending piece
rather than the whole schema.

## Part of KOD

Reached transitively through `kod-core`.

See the [KOD workspace](https://github.com/elcoosp/kod).

## License

MIT
