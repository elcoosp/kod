# kod-provider-openai

OpenAI-compatible LLM provider for KOD, built on `adk-model`.

Targets any OpenAI-compatible endpoint: OpenAI, Azure, local models
via vLLM or ollama, and OpenAI-compatible gateways. `adk-model`
handles the wire format; this crate adapts it to the KOD provider
trait.

## Part of KOD

Reached transitively through `kod-core-routing`.

See the [KOD workspace](https://github.com/elcoosp/kod).

## License

MIT
