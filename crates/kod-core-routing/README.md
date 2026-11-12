# kod-core-routing

Task routing and provider selection policy for KOD.

Decides which model to use for which task, applies per-task retry
policy, and follows a fallback chain when a provider fails or
rate-limits. The rules come from config; the policy is uniform across
providers.

## Part of KOD

Reached transitively through `kod-core`.

See the [KOD workspace](https://github.com/elcoosp/kod).

