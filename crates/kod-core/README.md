# kod-core

Core engine coordinating all KOD subsystems.

Every subsystem — routing, tools, memory, quality, state, telemetry —
wired into a single `KodEngine` that owns the streaming turn loop, the
tool dispatch loop, and the approval prompts. `kod-cli` and `kod-tui`
are thin clients over it.

## Part of KOD

The entry point for embedding KOD programmatically.

See the [KOD workspace](https://github.com/elcoosp/kod).

