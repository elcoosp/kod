# kod-provider

LLM provider abstraction layer for KOD.

The provider contract: the `LlmProvider` trait, streaming and
non-streaming response types, tool definitions, and the concurrency
and retry policy every provider shares. Implementing a new provider
means implementing `LlmProvider` and reusing the concurrency primitive
in `kod-provider::concurrency`.

## Part of KOD

Depended on by every provider crate and by `kod-core`; direct use is
for implementing a new provider.

See the [KOD workspace](https://github.com/elcoosp/kod).

## License

MIT
