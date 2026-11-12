# kod-config

Configuration loading and validation for the KOD workspace.

Loads config files, environment variables, and CLI overrides, merges
them in a defined precedence order, and produces a single validated
`Config` that the engine consumes. Invalid combinations fail at load
time with a diagnostic naming the offending field.

## Part of KOD

Most users configure KOD through the CLI, which calls this crate.
Direct use is for embeddings that need to read the same config.

See the [KOD workspace](https://github.com/elcoosp/kod).

