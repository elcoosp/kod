# kod-provider-anthropic

Anthropic Messages API provider for KOD.

Streaming, non-streaming, native compaction, and the wire module that
preserves prompt-caching blocks the Anthropic client would otherwise
flatten. Two wire paths: the modern path that builds the request body
locally, and the legacy text path via `adk-model`.

## Usage

The provider is constructed with a config builder and passed to the
router; most users never call it directly.

```text
kod config set providers.anthropic.api_key "$ANTHROPIC_API_KEY"
```

Set the key once; the engine's routing layer instantiates an
`AnthropicProvider` for any turn whose route targets Anthropic. Direct
use (for embeddings) is via the `LlmProvider` trait — see
`kod-provider` for the contract.

For request-body construction (used by tooling and the wire tests),
`wire::build_messages_body` accepts a `CompletionRequest` and emits
the exact JSON Anthropic expects, including the prompt-caching
`cache_control` blocks that a thin wrapper would flatten.

## Part of KOD

Reached transitively through `kod-core-routing`.

See the [KOD workspace](https://github.com/elcoosp/kod).

## License

MIT
