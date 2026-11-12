# kod-telemetry

OTLP `GenAI` telemetry for KOD.

Emits traces and metrics per request, tool call, and turn, using the
OpenTelemetry `GenAI` semantic conventions. Wired behind a trait so a
session without an exporter pays no cost.

## Part of KOD

Reached transitively through `kod-core`.

See the [KOD workspace](https://github.com/elcoosp/kod).

