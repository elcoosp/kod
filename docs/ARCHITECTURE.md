# KOD Architecture

_Last verified 2026-10-28 at `6754937e`. 26 crates. When this file and
the workspace disagree, the workspace is right — update this file._

## Overview

KOD is a high-performance AI coding agent harness built as a multi-crate
Cargo workspace. The system is modular, extensible, and performant by
construction: a thin interface layer over a split core engine, with
cross-cutting concerns (AST parsing, risk classification, output
minimization, schema dialects, telemetry, statistics) in their own
crates so a consumer pays only for what it links.

## High-Level Architecture

```
┌──────────────────────────────────────────────────────────────┐
│  Interface         kod-cli, kod-tui                           │
├──────────────────────────────────────────────────────────────┤
│  Core engine       kod-core (router, engine, swarm runner),   │
│                    kod-core-state, kod-core-routing,          │
│                    kod-core-quality, kod-core-tools,          │
│                    kod-core-serve                             │
├──────────────────────────────────────────────────────────────┤
│  Agents & tools    kod-swarm, kod-tools                       │
├──────────────────────────────────────────────────────────────┤
│  Skills & memory   kod-skills, kod-memory                     │
├──────────────────────────────────────────────────────────────┤
│  LLM providers     kod-provider, kod-provider-openai,         │
│                    kod-provider-anthropic                     │
├──────────────────────────────────────────────────────────────┤
│  Protocol clients  kod-lsp, kod-mcp                           │
├──────────────────────────────────────────────────────────────┤
│  Foundation        kod-types, kod-error, kod-config,          │
│                    kod-ast, kod-minimize, kod-risk,           │
│                    kod-schema-dialect, kod-stats,             │
│                    kod-telemetry                              │
└──────────────────────────────────────────────────────────────┘
```

## Crate Structure

Twenty-six crates in seven layers. A crate may depend on anything below
it and nothing above it; the layer order is the dependency order.

### Foundation Layer

#### kod-types
The workspace vocabulary — every type that crosses a crate boundary
lives here:
- Strongly-typed IDs (`AgentId`, `MessageId`, `SkillId`, …)
- Message types (`ChatMessage`, `AgentMessage`)
- Skill, memory, and tool definitions
- **Redaction primitives** (`redact`, `secret_placeholder`,
  `secret_sources`) — the single source of truth for what may leave
  the process; every prompt path and persistence path routes through
  them
- String and terminal helpers (`strutil`, `term`), trust levels, aliases

#### kod-error
- `KodError`: one error enum for the workspace
- Recovery detection (recoverable vs terminal)
- `Result<T>` alias re-exported everywhere

#### kod-config
- TOML configuration with environment overrides and defaults
- Modules: `config`, `instructions`, `jev`, `limits`, `llm`, `mcp`,
  `memory`, `policy`, `profiles`, `retry`, `skills`, `swarm`
- The **policy engine** (`policy`): presets (read-only / standard /
  yolo), per-tool overrides, session deny rules — surfaced by
  `kod policy show` and trimmed by `kod policy forget <n>`, which is
  what makes accumulated "never" rules removable without a restart
- Depends on `kod-risk` so a command's blast-radius class is available
  at policy evaluation

#### kod-ast
- tree-sitter parsing for the nine bundled grammars
- Process-global LRU `parse_cache` keyed on
  `(xxh3(source), source.len(), Lang)` with a byte-for-byte check on hit
- Its own crate because each grammar compiles a C parser — a consumer
  that does not need AST parsing (the CLI's doctor path, a test binary)
  does not pay for nine `cc` invocations

#### kod-minimize
- Rewrites large shell output into a shorter form that keeps the
  informative content (`git status`, `git log`, `cargo check` failures)
- Public surface: `Minimizer::with_builtins`,
  `minimize(command, raw, exit_code) -> Minimized`,
  `Minimized::was_rewritten`
- The raw output survives as an artifact — the model gets the short
  form, the human can still ask for the original
- Exists because byte-boundary truncation (`cap_rendered_result`,
  `MAX_CMD_OUTPUT_BYTES`) cuts the part the model very often needed

#### kod-risk
- Command blast-radius classification by effect, not by command name
- `RiskLevel` plus `GateOutcome { Allow, Reflect { prompt }, Deny {
  reason } }` — an unrecognized wrapper, an unresolvable `$VAR`, or a
  glob over a protected directory produces `Reflect`, not `Allow`. The
  failure mode of a too-clever classifier is a deleted filesystem; the
  failure mode of a too-cautious one is an extra justification prompt
- Justifications are checked (`Justification::is_substantive`):
  all-whitespace and one-word affirmations are rejected
- Defense in depth alongside the OS sandbox and the policy engine

#### kod-schema-dialect
- Makes one JSON Schema acceptable to every provider
- `sanitize(schema, spec) -> (Value, Vec<AppliedTransform>)` plus a
  rejection classifier and a per-endpoint memory of what was refused
- Three layers: prevention, recovery, memory — the cost of a provider
  quirk is one wasted round trip ever, not one per request
- Exists because providers disagree on `const`, `oneOf`, nested `anyOf`

#### kod-stats
- Pure functions over data the engine already has
- `behavioral` (quality signals from user messages — "no, I meant…" is
  the signal), `request` (per-request analytics and aggregates),
  `commit`, `commit_mapreduce`, `if_bench`

#### kod-telemetry
- OTLP/GenAI telemetry exported as OTLP/HTTP JSON to
  `OTEL_EXPORTER_OTLP_ENDPOINT`
- `Telemetry::disabled()` / `from_env()`; `record_turn` / `record_tool`
  — fire-and-forget POST, no global singleton
- Hand-rolled rather than `opentelemetry-rust`: the crate wants
  env-gated lazy export and flush at turn boundaries, not a periodic-
  exporter task whose shutdown ordering fights the engine's

### Protocol Clients Layer

#### kod-lsp
- Minimal Language Server Protocol client: JSON-RPC over a server's
  stdio with `Content-Length` framing
- `initialize`, `didOpen`/`didChange`, `publishDiagnostics`,
  `definition`, `references`, `hover`
- One server per language, spawned lazily, killed on engine shutdown
- Every other server-to-client request is answered `{"result": null}`
  so the server does not block waiting on capabilities kod does not
  implement

#### kod-mcp
- Minimal Model Context Protocol client: newline-delimited JSON-RPC 2.0
  over a spawned server's stdio
- Client role only: `initialize`, `tools/list`, `tools/call`.
  Resources, prompts, sampling, and roots are not implemented — the
  four tools kod needs from a plugin are listable and callable
- Tools surface on the engine registry as `mcp:<server>.<tool>`
- One background reader task per `McpClient`; lifetime of the client is
  the lifetime of the child process

### LLM Provider Layer

#### kod-provider
- The `LlmProvider` trait every backend implements
- Shared machinery: `catalog`, `concurrency`, `image_budget`,
  `judgment`, `native_compaction`, `registry`, `replay`, `request`,
  `retry`, `retry_safety`, `stream_guard`, `ttsr`, `types`
- `CompletionRequest` lives here; the eventual AD-01 migration moves
  the engine from rendered text to this typed body
- A provider contract suite gated behind the `testkit` feature

#### kod-provider-openai
- One code path for every OpenAI-spec chat-completions endpoint
  (Ollama `/v1`, LM Studio, MLX Omni Serve, vLLM, OpenAI), backed by
  `adk-model`
- `base_url` accepts a server root or a full API root — a missing `/v1`
  suffix is added automatically so existing Ollama-style configs keep
  working

#### kod-provider-anthropic
- Anthropic Messages API provider with a local `wire` module for
  `complete`, streaming, and native compaction
- The wire module exists because `adk-model`'s Anthropic client takes
  a single `system: String` and flattens a multi-segment system prompt
  before the wire call, which is fatal to prompt caching:
  `cache_control: {"type": "ephemeral"}` is a marker on a *block*
  inside a system array, not on a top-level string
- `system_blocks(prompt)` places `cache_control` on the **last
  cacheable** segment — the breakpoint sits at the end of the invariant
  prefix (Identity + RepoMap), and everything after it stays uncached,
  as intended
- `generate` / `generate_with_tools` keep the legacy `adk-model` text
  path and carry no cache hints

### Skills & Memory Layer

#### kod-skills
- Markdown skill system: YAML front matter and markdown parsing,
  loader with caching, pattern and lexical matcher, filesystem watcher
  for hot reload
- Two lists reach prompt construction: the lexical match and the
  Jev-augmented refined list. A semantic-only skill must reach the
  prompt through the refined list — the lexical matcher alone will not
  find it

#### kod-memory
- Multi-layer memory: short-term (in-process, capacity-bounded),
  long-term (redb), episodic (`VectorIndex`, brute-force in-process)
- `manager` unifies the backends; `context` builds the prompt section
- Supporting modules: `embedding`, `extract`, `fusion`, `hygiene`,
  `mental_models`, `retention`, `retrieval`, `sharpshooter`,
  `stopwords`, `tier`
- Redaction is unconditional at write time

### Tool & Agent Layer

#### kod-tools
- The `Tool` trait, `ToolRegistry`, `ToolContext`, and the executor
- Built-in filesystem and shell tools
- Depends on `kod-minimize` so a large tool result can be minimized
  before it reaches the model
- `ToolContext` currently carries 21 public fields; grouping into
  sub-structs is tracked separately because it touches every tool and
  every test fixture

#### kod-swarm
- Multi-agent lifecycle, registry, direct messaging, shared workspace,
  and task orchestration
- Depends on `kod-memory`, `kod-tools`, and `kod-provider`
- The engine-side adapter lives in `kod-core::swarm_runner`; the TUI's
  `dispatch_swarm_with` runs `preflight::sample()` before an overnight
  run

### Core Engine Layer

Five of these are extractions from `kod-core` (Oct 2026). The parent
re-exports the public items so existing paths keep resolving; a caller
that wants the smaller surface imports the child directly.

#### kod-core-state
Persistent state, journals, ledgers, and accounting. Leaf of the layer
— depends only on `kod-types`, `kod-error`, `kod-config`, `kod-provider`,
`kod-tools`. Modules: `budget`, `cache_journal`, `cache_ledger`,
`cache_tracker`, `checkpoint`, `citations`, `commit_lock`,
`context_gauge`, `cost`, `decisions`, `goals`, `plan`, `presence`,
`state`, `trace`, `trace_writer`. The `presence` module owns the
`~/.kod/active_pids/` markers: `mark`, `unmark`, `list`, and the
process-alive probe that distinguishes a live holder from a stale one.
`kod-core` re-exports `CostTracker`, `DecisionLog`, `Goal`, `Plan`,
`EngineState`, `TraceWriter`, and `read_traces` from here.

#### kod-core-routing
Provider construction and retry policy — *not* the prompt builder.
Modules: `config`, `context`, `context_engine`, `provider_setup`,
`retry_strategy`. Depends on `kod-core-state` plus every provider crate
and `kod-memory`. The `TaskRouter` lives in `kod-core`, not here; the
routing crate is the piece that turns a config into a live provider
handle and a retry strategy.

#### kod-core-quality
Codebase analysis and quality gates: `auto_thinking`, `preflight`,
`prune`, `repomap`, `shake`, `snapcompact`, `transcript_coherence`,
`worktree`, `worktree_isolation_ownership`. `preflight::sample()`
returns a `PreflightSample` with `pressure_reasons(cores)` for low RAM,
swapping, load above core count, and battery; it is called from
`kod-tui::main_loop::dispatch_swarm_with` under `spawn_blocking` (the
sampler shells out on macOS), and it warns rather than refuses.
`snapcompact::rasterize_to_png` renders text as a PNG — the design
motivation is the same as `kod-minimize`'s: a byte-boundary cut loses
information a rewritten representation keeps.

#### kod-core-tools
Adapter layer between the built-in tools and the engine: `jfind`,
`lsp_tools`, `prewalk`, `speculation`, `tool_loop_guard`. Depends on
`kod-tools`, `kod-lsp`, `kod-mcp`, `kod-swarm`, `kod-skills`,
`kod-risk`, `kod-core-state`, `kod-core-routing`, `kod-core-quality`.

#### kod-core
The engine proper. Modules: `async_delivery`, `background`,
`compaction`, `compaction_dispatcher`, `doctor`, `engine`, `fixture`,
`hooks`, `jev`, `output_spool`, `pause_gate`, `router`,
`run_collector`, `swarm_adapters`, `swarm_runner`, `tool_quota`,
`unexpected_stop`.

- `router`: `TaskRouter` and the six `build_prompt*` variants
  (`build_prompt`, `_with_context`, `_with_budget`,
  `_with_budget_and_skills`, `build_prompt_plan`,
  `build_prompt_plan_with_skills`). The variants are delegations on one
  builder; collapsing them into a `PromptRequest` struct is tracked
  separately because it touches the byte-stable prompt contract.
- `PromptPlan` (`router.rs:222`): `system: Vec<SystemSegment>` +
  `messages_text`, with `render_text()` reproducing the pre-plan prompt
  byte for byte. The engine still calls `build_prompt_with_budget`
  directly; `build_prompt_plan` calls it internally and splits the
  result, so the two paths cannot drift. This is the AD-01 migration
  target, still pending.
- `doctor` lives here, not in `kod-cli`, because both `kod-cli` and
  `kod-tui` need it and a direct dependency between the two for it
  would cycle.

#### kod-core-serve
External clients for a running engine: `serve` (Unix-socket daemon,
NDJSON framing, peer-UID check), `acp` (stdio bridge, LSP-style
`Content-Length` framing), `transport` (shared framing). Depends on
`kod-core` and `kod-core-state`. The CLI imports from here directly —
there are no re-exports in `kod-core` for these.

### Interface Layer

#### kod-tui
- Ratatui terminal UI: `app`, `event`, `main_loop`, `components`,
  `markdown`, `highlight`, `theme`, `keybindings`, `clipboard`, `ui`
- `main_loop::run` writes the presence marker
  (`kod_core_state::presence::mark(root, Interactive, session)`) at
  start and stores the returned path on the loop; `restore_terminal`
  releases terminal ownership first (so a subscriber never writes while
  the shell expects the terminal), then unmarks
- `dispatch_swarm_with` (`main_loop.rs:1994`) samples the machine via
  `preflight::sample` before an overnight swarm run

#### kod-cli
- clap command structure (`commands/mod.rs`) and handlers, plus
  `logging`
- Re-exports `kod_core::doctor` (see the `kod-core` entry for why it
  does not live here)
- The `Command` enum is the ground truth for subcommands. Notable
  variants and their actions:
  - `Chat` / `Swarm` / `Agent` — each takes `--remote` and `--socket`
    to attach to a running `kod serve` daemon
  - `Sessions { Show | Clear | Export | Import | Latest | Count | Revive | Live }`
    — `Live` reads the `~/.kod/active_pids/` markers via
    `presence::list` and reports a marker whose process is gone as
    `stale` rather than hiding it (the crashed-session signal is what
    the transcript-revive path exists for)
  - `Trace { List { limit, path } | … }` — reads `turns.jsonl`
  - `Policy { Show | Forget(?) | … }` — `Show` prints the effective
    policy with per-rule provenance; the forget arm drops one
    accumulated session-deny rule by index, which is the only way to
    remove one without a restart

## Data Flow

1. User input arrives through `kod-cli` or `kod-tui`
2. `kod-core::router::TaskRouter` classifies the task
   (`build_prompt*` family) and assembles the prompt
3. Context is drawn from `kod-memory` and `kod-skills`; both prompt
   paths pass through `kod-types::redact` — the scaffolding appended
   after the initial redaction (plan, decision log, swarm blackboard)
   is re-redacted, which is a class of defect this codebase has paid
   for
4. `kod-provider-*` performs the generation (streaming or not).
   `kod-provider-anthropic::wire::system_blocks` places `cache_control`
   on the last cacheable system segment
5. Tool calls dispatch through `kod-tools` under the policy gate, the
   per-tool quota (keyed on the *outer* call name, so sub-calls in
   `batch` do not escape), and `kod-risk`'s blast-radius classifier,
   which may escalate a shell command to `Reflect` or `Deny`
6. Tool results feed the next round — large results are minimized by
   `kod-minimize` first, then capped if still too long
7. The response returns to the interface; session state is journaled
   by `kod-core-state`

## Design Principles

1. **Modularity**: one responsibility per crate. The `kod-core-*` split
   exists so a consumer of routing does not link the quality tools.
2. **Performance**: parse caching, prompt-cache-aware wire bodies,
   fidelity-aware context budgeting.
3. **Extensibility**: trait-based providers and tools; MCP for external
   tools.
4. **Type safety**: strongly-typed IDs prevent mixing.
5. **Error handling**: one `KodError` with recovery classification.
6. **Security is a path property**: a guard applied on one path and not
   another is the defect class this codebase has paid for most often.
   Redaction, policy, and quota must be applied at every site that
   reaches the model or the disk — not at the sites that were obvious
   when the guard was written.
7. **Testing**: unit, property, and characterization tests. The
   cacheable prompt prefix is asserted byte-for-byte by
   `characterization_prompts` and `golden_prefix`; any refactor that
   touches prompt construction runs those at every step.

## Performance Considerations

- **Async I/O**: tokio throughout
- **Caching**: parse cache (`kod-ast`), fidelity cache
  (`kod-core-state`), skill cache (`kod-skills`), memoized prompt
  redaction (`kod-types`)
- **Prompt cache alignment**: the byte-stable cacheable prefix is a
  contract; `system_blocks` puts the `cache_control` breakpoint at its
  end
- **Allocation discipline**: the skill inventory renders from names,
  not cloned bodies; large tool results are minimized rather than
  truncated at a byte boundary
- **Connection reuse**: one HTTP client per provider

## Delivered since this document was first written

Cumulative — these all post-date the doc and were missing from it:

- **Core split**: `kod-core-state`, `kod-core-routing`,
  `kod-core-quality`, `kod-core-tools`, `kod-core-serve` extracted from
  `kod-core` (Oct 4–5)
- **`kod-ast`**: tree-sitter grammars with a process-global parse
  cache (Sep 30)
- **ACP bridge** (`kod-core-serve::acp`): stdio with LSP-style framing,
  alongside the NDJSON Unix-socket daemon
- **Preflight** (`kod-core-quality::preflight`): machine sampling before
  an overnight swarm run, warned not enforced
- **Presence** (`kod-core-state::presence`): `~/.kod/active_pids/`
  markers plus `kod sessions live`, with a `stale` state for a marker
  whose process is gone
- **LSP** (`kod-lsp`): diagnostics, definition, references, hover
- **MCP client** (`kod-mcp`): external tools under `mcp:<server>.<tool>`
- **Sandbox backends**: bwrap on Linux, sandbox-exec on macOS, Landlock
  on Linux ≥ 5.13
- **Unix-socket daemon** (`kod serve`): NDJSON, peer-UID check,
  `--remote` on `kod prompt`, `kod chat`, `kod agent`
- **Policy engine** (`kod-config::policy`): presets, per-tool overrides,
  session deny rules, `kod policy show` and `kod policy forget <n>`

Note: `kod-minimize`, `kod-risk`, `kod-schema-dialect`, `kod-stats`,
`kod-telemetry`, and `kod-swarm` all pre-date this document — they were
missing from it, not delivered after it. They belong to the crate list
above, not to this section.

## Still on the roadmap

- **Debugger integration**: Debug Adapter Protocol for stepping. No DAP
  client exists in the tree at HEAD.
- **Vector search**: external vector databases. `kod-memory`'s
  `VectorIndex` is brute-force and in-process.
- **Plugin system**: dynamic loading beyond MCP.
- **Prompt-plan migration (AD-01)**: the engine still calls
  `build_prompt_with_budget` directly at the `LlmProvider` boundary.
  `PromptPlan` exists and is exercised by tests, and `build_prompt_plan`
  reproduces the current prompt byte for byte, but the engine has not
  been switched. The remaining step is that switch — and it will
  collapse the six `build_prompt*` variants into one `PromptRequest`
  builder — only with `characterization_prompts` and `golden_prefix`
  green at every step, because that prefix is a byte-stable contract.
