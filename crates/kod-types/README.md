# kod-types

Shared type definitions for the KOD workspace — a terminal-native AI
coding agent.

This crate is the common vocabulary: tool identifiers and permission
scopes, model descriptors, transcript keys, and the secret-vault
primitives that keep credentials out of prompts.

## Part of KOD

Nearly every other crate in the workspace depends on this one; end
users reach it through `kod-core` or `kod-provider`.

See the [KOD workspace](https://github.com/elcoosp/kod) for the CLI,
the engine, and the documentation site.

## License

MIT
