# kod-error

Error types and diagnostic context for the KOD workspace.

The workspace-wide `KodError` enum: one variant per failure mode the
engine can produce, plus helpers for surfacing context to the user.
The engine carries `KodError` through every fallible boundary so a
failure in a tool call, a provider request, or a state write surfaces
with the same shape.

## Part of KOD

Depended on by every other crate; end users reach it through
`kod-core`.

See the [KOD workspace](https://github.com/elcoosp/kod).

## License

MIT
