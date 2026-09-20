# KOD config reference

Every block KOD reads from `~/.kod/config.toml`. Blocks not listed
here are documented in their own modules; this file covers the
production-hardening additions and the blocks that changed since
v0.1.0.

The config is read once at startup. Two exceptions — `/jev tune set`
and `/budget raise` — mutate the running engine in place; the first
also persists to disk so the change survives a restart.

---

## `[limits]` — cost and token caps

Refuses a round whose estimated cost would exceed a cap; ends a turn
that would exceed a per-turn cap. All caps are opt-in: `0` disables
that cap.

| Key | Type | Default | Notes |
|---|---|---|---|
| `max_cost_usd_per_session` | float | `0.0` | Total session spend. `/budget` shows the running total. |
| `max_cost_usd_per_turn` | float | `0.0` | Per-turn spend. Reset at the start of every `process_*` call. |
| `max_input_tokens_per_turn` | int | `0` | Pre-flight estimate; the round is refused when the projection exceeds. |
| `max_output_tokens_per_turn` | int | `0` | Bounds a chatty model. |
| `on_exhausted` | `"ask"` \| `"stop"` \| `"continue"` | `"ask"` | `ask` prompts in the TUI; `stop` ends the turn; `continue` is audit mode. |
| `soft_warn_at` | float in `[0, 1]` | `0.5` | Warn once when spend crosses this fraction of a cap. `0` disables. |

### `[limits.tools]`

Per-tool quotas. `per_command` only applies to `execute_command` and
catches the retry-the-same-broken-thing loop.

```toml
[limits.tools.grep]
per_turn = 20
per_session = 200
per_command = 0     # unused for grep

[limits.tools.execute_command]
per_turn = 30
per_session = 500
per_command = 5
```

A `[limits.tools.default]` entry applies to any tool not otherwise
listed. A quota with every field at `0` is treated as absent, so
listing a tool without values is the same as not listing it.

---

## `[jev]` — TypeSafe AI integration

Optional. Disabled by default. Enabling requires `TYPESAFE_API_KEY`
in the environment or `api_key` in the block.

| Key | Type | Default | Notes |
|---|---|---|---|
| `enabled` | bool | `false` | Master switch. |
| `api_key` | string | unset | Overrides `TYPESAFE_API_KEY`. |
| `base_url` | string | SDK default | `https://api.typesafe.ai` or a gateway. |
| `model` | string | `jev-latest` | Model alias. |
| `cache_ttl_secs` | int | `300` | Decision cache TTL. `0` disables the cache. |
| `timeout_ms` | int | `800` | Per-request timeout. Floored at 50 ms. |
| `fail_open` | bool | `true` | On Jev error, fall through to the heuristic. |
| `redact_paths` | bool | `false` | Hash absolute paths before sending state. |
| `reasoning_timeout_secs` | int | `20` | TUI reasoning filter safety valve. `0` disables — trust the classifier absolutely. |

### `[jev.thresholds]`

Every boolean decision compares a probability against one of these.
Values are in `[0, 1]`; `JevThresholds::clamp` bounds them at load
time.

| Key | Default | Used by |
|---|---|---|
| `task_classify_min` | `0.6` | Task classification refinement |
| `tool_filter_min` | `0.7` | Tool inventory filtering |
| `early_termination_min` | `0.9` | Streaming early-termination |
| `auto_approve_min` | `0.95` | Confidence-gated auto-approval |
| `memory_filter_min` | `0.7` | Memory filter; also grep ranking and diff hunk triage |
| `ambiguity_min` | `0.85` | Ambiguity pre-detection |

### `[jev.round_routing]`

Maps round kinds to endpoint names declared under `[llm]`.

```toml
[jev.round_routing]
planning = "cloud-anthropic"
tool_execution = "local-ollama"
synthesis = "cloud-anthropic"
summary = "local-ollama"
```

Empty by default — no per-round routing.

---

## `[policy.read_protection]` — secret paths

How `read_file` treats known-secret paths.

| Key | Type | Default | Notes |
|---|---|---|---|
| `enabled` | bool | `true` | `false` disables every rule below. |
| `mode` | `"redact"` \| `"refuse"` \| `"allow"` | `"redact"` | `redact` replaces matches in the content; `refuse` returns an error; `allow` passes through. |
| `deny` | array of globs | see below | A pattern with `/` matches the full path; without, the basename at any depth. |

Default `deny` list:

```
.env
.env.*
**/.env
**/.env.*
**/*.pem
**/*.key
**/id_rsa*
**/id_ed25519*
**/id_ecdsa*
**/.aws/credentials
**/.aws/config
**/.ssh/**
**/.docker/config.json
**/.netrc
**/.pgpass
**/credentials.json
**/service-account*.json
```

The list is replaced, not appended, when set. Copy the defaults into
your config if you want to keep them and add to them.

---

## `[security]` — reserved

Placeholder for future injection-defense configuration. No runtime
effect today — the trust boundary markers and taint tracking are on
by default and not configurable. See `crates/kod-types/src/trust.rs`.

---

## Interaction notes

- **`[limits]` and `[jev]` are independent.** Run Jev without cost
  caps, or cost caps without Jev.
- **`[policy.read_protection]` composes with the policy engine.** A
  `read_file` on a protected path is redacted/refused *before* the
  policy gate runs, so the policy gate sees `read_file` calls only
  for paths that passed the protection.
- **`[jev] fail_open = true`** (the default) means every Jev-aware
  call site still works when Jev is down. Set `false` only if you
  want Jev errors to propagate — usually a mistake in production.
- **Cost caps use micro-USD integer arithmetic internally.** A cap
  of `5.0` means exactly $5.00, not a float rounding of it. The
  budget display in the status bar shows four decimals because
  individual calls routinely cost fractions of a cent.

---

## Where to find the code

| Block | Source |
|---|---|
| `[limits]` | `crates/kod-config/src/limits.rs` |
| `[jev]` | `crates/kod-config/src/jev.rs` |
| `[policy.read_protection]` | `crates/kod-config/src/policy.rs` |
| Cost tracker runtime | `crates/kod-core/src/cost.rs` |
| Tool quota runtime | `crates/kod-core/src/tool_quota.rs` |
| Jev client wrapper | `crates/kod-core/src/jev.rs` |
| Trust levels | `crates/kod-types/src/trust.rs` |
| Secret redaction | `crates/kod-types/src/redact.rs` |
| Session log variants | `crates/kod-core/src/session_log.rs` |
| Turn traces | `crates/kod-core/src/trace.rs` |
| Plan artifact | `crates/kod-core/src/plan.rs` |
| Decisions log | `crates/kod-core/src/decisions.rs` |

---

## A complete example

```toml
[limits]
max_cost_usd_per_session = 5.0
max_cost_usd_per_turn = 0.50
soft_warn_at = 0.5
on_exhausted = "ask"

[limits.tools]
default = { per_turn = 50, per_session = 1000 }
grep = { per_turn = 20, per_session = 200 }
execute_command = { per_turn = 30, per_session = 500, per_command = 5 }

[jev]
enabled = true
model = "jev-latest"
cache_ttl_secs = 300
timeout_ms = 800
fail_open = true
reasoning_timeout_secs = 20

[jev.thresholds]
task_classify_min = 0.6
tool_filter_min = 0.7
early_termination_min = 0.9
auto_approve_min = 0.95
memory_filter_min = 0.7
ambiguity_min = 0.85

[jev.round_routing]
planning = "cloud"
synthesis = "cloud"
tool_execution = "local"
summary = "local"

[policy.read_protection]
enabled = true
mode = "redact"
deny = [
    ".env", ".env.*",
    "**/*.pem", "**/*.key",
    "**/id_rsa*", "**/id_ed25519*",
    "**/.aws/credentials", "**/.ssh/**",
]
```
