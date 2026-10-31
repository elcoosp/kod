# kod-minimize

Shell output minimizer for KOD.

Compresses long shell output before it enters the conversation
context: drops boilerplate, keeps the head and tail of long streams,
and preserves error lines. Reduces token cost without hiding failures
the model needs to see.

## Part of KOD

Reached transitively through `kod-tools` and `kod-core-tools`.

See the [KOD workspace](https://github.com/elcoosp/kod).

## License

MIT
