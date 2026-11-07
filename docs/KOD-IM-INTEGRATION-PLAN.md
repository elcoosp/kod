# KOD ⇄ IM Integration Plan

**Securely drive your `kod` coding agent from Telegram (or any IM) — platform-agnostic, robust, best-DX, implementation-ready.**

> **How to use this document (for the implementing agent — human or AI):**
> This plan is written to be executed *top-to-bottom* by an agent with minimal judgment. Every phase has: exact prerequisites → exact files to create (full contents inline) → exact commands to run → **verification gates with expected output**. Do not improvise unless a gate fails; if a gate fails, consult Appendix E (Troubleshooting) before deviating. Code blocks marked `File: <path>` are also extracted to a ready-to-copy scaffold at `kod-im/` next to this document.
>
> - **Phase 0** — Foundations: build kod, secure config, daemon, secrets *(~30 min)*
> - **Phase 1** — Bridge MVP: Telegram ⇄ `kod serve` end-to-end *(~2–4 h)*
> - **Phase 2** — The killer UX: approvals, questions, steering, cancel from your phone *(~2 h)*
> - **Phase 3** — Platform-agnostic adapter layer: Matrix (full), Discord (skeleton), adapter contract *(~2–3 h)*
> - **Phase 4** — Optional upstream kod patches (Rust) that unlock the remaining 10 % *(~0.5–2 days)*
> - **Phase 5** — Hardening, ops, failure drills, acceptance matrix
> - **Appendices A–F** — Protocol reference, marker reference, security checklist, config reference, troubleshooting, roadmap

---

## 0. Executive summary

### 0.1 What we found (and why this plan is easy)

KOD already ships the hard 80 % of this integration — it just doesn't know it yet. Deep source inspection (every claim below is code-verified, see Appendix A for file/line references) shows:

1. **`kod serve` is a hardened local daemon** (`crates/kod-core-serve/src/serve.rs`): a Unix-socket NDJSON server with peer-UID authentication, `0600` socket permissions, flock-based single-instance locking, and a 1 MiB request cap. It exposes exactly the verbs an IM bridge needs: `process_streaming` (start a run and stream chunks), `steer` (redirect a live run), `cancel`, `respond_to_approval`, `respond_to_question`, `swarm`, `peek_session`, `list_models`, `set_model`, `shutdown`.
2. **Approval and question requests already travel over the wire** as `\0kod-approval-batch:<id>:<json>` / `\0kod-question:<id>:<json>` marker chunks, and the *answer path is cross-connection*: any socket client can answer any pending approval by id. A bridge that starts the run itself receives the marker on its own connection — so **interactive approvals from an IM thread work today, with zero changes to kod**.
3. **The security stack fits "remote-controlled agent" perfectly**: a secrets vault that replaces secrets with `«Credential-…»` placeholders in prompts and de-obfuscates them only at tool dispatch; a shell env stripper that prevents any `*TOKEN*`/`*KEY*`/`*SECRET*` var from reaching child processes; a policy engine (`read-only` / `standard` / `yolo` presets + project `.kod/policy.toml` that can only *narrow*); a blast-radius classifier that hard-denies catastrophic shell commands regardless of policy; optional bubblewrap/Landlock sandboxing with network denial; and hard budget/round caps (`MAX_TOOL_ROUNDS = 40`, per-turn/per-session cost limits, 120 s approval timeout → deny).
4. **Steering is a first-class engine feature** (`SoftInterrupt` queues, drained at every round boundary): a plain IM message while the agent works becomes a mid-run course correction — no new protocol needed.

### 0.2 The design in one paragraph

Run `kod serve` as a **per-user daemon** on your machine. Run a small **bridge process** (`kod-im`, Bun + TypeScript, ~40 MB RSS) next to it, as **the same OS user**. The bridge holds exactly two kinds of outbound connections: (1) long-poll/WebSocket connections *out* to your IM platform (Telegram `getUpdates`, Matrix `sync`, Discord gateway, Slack Socket Mode), and (2) the local Unix socket to the daemon. It **never opens any inbound port** — there is nothing to attack from the internet. Each IM chat maps to a kod `transcript_key` (`tg:<chat_id>`), so every chat is an isolated, resumable agent session. Approvals become inline buttons, questions become free-text replies, any message sent mid-run becomes a steer, and a `Stop` button cancels. Tailscale's role is deliberately *optional and narrow*: admin SSH, self-hosted-Matrix transport, and (only if you insist on webhooks) a funnel with a shared secret — because the recommended path needs no tunnel at all.

```
                       ┌──────────────────────────── kod host (single OS user) ───────────────────────────┐
                       │                                                                                   │
 your phone            │   ┌────────────────┐      Unix socket, NDJSON       ┌───────────────────────────┐  │
 ┌────────────┐        │   │   kod serve    │  process_streaming/steer/     │   kod-im bridge (Bun/TS)  │  │
 │  Telegram  │◀═══ outbound-only ══▶│   (daemon)     │  cancel/respond_*/peek        │                           │  │
 │  Matrix    │        │   │  one engine    │◀─────────────────────────────▶│  ┌─────────────────────┐  │  │
 │  Discord…  │        │   │  per-user      │   respond_to_approval(id)     │  │ ImAdapter interface │  │  │
 └────────────┘        │   └───────┬────────┘                               │  │  ├ Telegram (full)  │  │  │
      ▲                │           │                                        │  │  ├ Matrix   (full)  │  │  │
      │                │           ▼                                        │  │  ├ Discord  (skel)  │  │  │
      │                │   ┌────────────────┐                               │  │  └ …your adapter    │  │  │
      │                │   │ LLM endpoint   │  (Ollama / OpenAI-compat /    │  └─────────────────────┘  │  │
      │                │   │ + MCP servers  │   Anthropic)                  └────────────┬──────────────┘  │
      │                │   └────────────────┘                                            │ outbound HTTPS  │
      │                │   ┌────────────────┐                                            │ (long poll/WS)  │
      └── tailscale ───┼──▶│ optional:      │  admin SSH · status page · self-hosted  ───┘                 │
        (admin path,   │   │ tailscaled     │  Matrix homeserver bound to tailnet                          │
         not the IM    │   └────────────────┘                                                              │
         data path)    └───────────────────────────────────────────────────────────────────────────────────┘
```

**Data flows out to the IM cloud; nothing flows in from the internet.** The only local ingress is the Unix socket, gated by the kernel (peer-UID).

### 0.3 Decision record (ADR summary)

| # | Decision | Alternatives considered | Why this wins |
|---|----------|------------------------|---------------|
| AD-1 | **Bridge = external process speaking the Unix-socket NDJSON protocol** | (a) new Rust crate inside kod linking the engine directly; (b) spawn `kod prompt`/`kod agent` per message; (c) `kod acp` stdio bridge; (d) MCP for inbound | (a) forces every iteration through a Rust build and couples lifecycles; (b) loses streaming/steer/approvals (one engine per message, cold boot each time, kills prompt-cache locality); (c) is editor-oriented (no `steer`, one engine per process, no multi-chat multiplexing); (d) cannot work inbound — an idle kod never calls tools, and MCP notifications are dropped to debug logs. The socket daemon gives streaming + steer + cancel + approvals + multi-chat multiplexing on one warm engine **today**, and the protocol is pinned by tests. |
| AD-2 | **Bridge language: TypeScript on Bun** (grammY for Telegram, matrix-js-sdk for Matrix) | Rust (teloxide/frankenstein) in-workspace; Go (telego + tsnet) | The bridge is I/O-bound glue; language perf is irrelevant. DX dominates: grammY is the most mature TS bot framework (2026), Bun gives single-runtime dep management + fast start, and iteration speed on formatting/UX is where bridge quality is won. Rust remains the path for optional in-tree kod patches (Phase 4). Go's unique advantage (embedded tsnet) is unnecessary because we deliberately need **no** tunnel. |
| AD-3 | **Transport: outbound-only (long polling / WebSocket)** | Webhooks over Tailscale Funnel; webhooks on a public VPS | Zero attack surface: no port forwarding, no TLS termination, no public endpoint, no replay window. Telegram `getUpdates`, Discord gateway, Slack Socket Mode and Matrix `sync` are all outbound — the platform-agnostic design costs nothing. Funnel mode is documented (Appendix D) for platforms that *require* webhooks, with a shared-secret check. |
| AD-4 | **One daemon, one engine, per-chat `transcript_key`s** (`tg:<chat_id>[:name]`) | One engine per chat; per-message engines | Engine boot is not free (skills load, memory DB open, provider prewarm); one daemon keeps the system prompt stable → provider prompt-cache hits (kod tests pin cache stability). Transcript keys are already the daemon's session abstraction (`""`, `"chat"`, `"agent:<name>"`, `"acp:<id>"` are the shipped conventions — we add `tg:*`). |
| AD-5 | **Approvals answered by the IM itself** (inline buttons → `respond_to_approval`) | Run unattended with `yolo`/allowlists only; auto-deny | Buttons keep a human in the loop from anywhere with a 120 s timeout → deny fail-closed. Unattended strict mode (deny or read-only) remains a config choice per deployment; the bridge supports both postures. |
| AD-6 | **Agent-initiated outbound = stdio MCP server** (`mcp:im-notify.send_message`) | Turn-end hooks (don't exist — only pre/post tool use); bridge scraping session logs | Zero kod changes; policy-gated like any tool; the agent can notify "build failed, here's the diff" from inside a run. Hooks cannot do this (only `pre_tool_use`/`post_tool_use` exist today); log-scraping is fragile. |
| AD-7 | **Tailscale = admin plane only** (SSH, status page, self-hosted Matrix), not the IM data plane | Tailscale Funnel webhooks; socat TCP→Unix proxy over tailnet | The IM data path is already TLS to the platform vendor over an outbound connection — a tunnel adds latency and a second thing to operate for zero security gain. Cross-host TCP→Unix proxies would *weaken* kod's peer-UID boundary (the proxy's UID becomes the only identity). If you want sovereign IM, host Matrix yourself and reach it over the tailnet (Phase 3.4). |
| AD-8 | **Same-host, same-UID deployment is a feature, not a limitation** | Remote daemon over TCP | kod's daemon explicitly refuses TCP ("No TCP… never will", `serve.rs:21`) and authenticates via `SO_PEERCRED`. Keeping the bridge colocated preserves that kernel-enforced boundary. Multi-machine = one daemon+bridge pair *per machine*, each driven from the same IM account (the bridge tags messages with the host name). |

### 0.4 DX north star (what "best in class" concretely means here)

1. **Two commands to a working system**: `kod serve` (daemon, systemd-managed) and `kod-im` (bridge, systemd-managed). Everything else is config files shipped in this plan.
2. **Zero-config defaults, strict-by-default security**: fresh install refuses to serve any chat until you allowlist it; `/id` in the IM prints the IDs you need; pairing mode is opt-in.
3. **< 2 s feedback loop**: every message gets an instant "🫀 working…" receipt, streaming preview edits (~every 2 s), and a clean final message. Silence is a bug.
4. **Everything is a slash command** with a `/help` that is actually true: `/new /stop /status /peek /model /models /use /sessions /id /help`.
5. **Safe defaults that degrade loudly, not silently**: daemon down → the bridge says so; approval missed → the bridge *edits the buttons* to "⏱ auto-denied (120 s)"; policy denial → the bridge shows the policy rule text, not a generic error.
6. **Idempotent, boring ops**: `kod-im doctor` checks socket handshake + Bot API token + policy posture in one shot; both services restart on failure; logs go to journald.
7. **No secrets in chat, ever**: the bot token never appears in messages or logs; kod's vault placeholders appear if the agent ever quotes a secret; diffs and file contents are size-capped and sent as documents when large.
8. **Platform parity via a tiny adapter contract** (~10 methods) so a new IM is ~150 lines, not a fork.
9. **Respect IM physics**: 4096-char messages, ~1 edit/s per chat, ~30 msg/min — the bridge batches, debounces, splits on code-fence boundaries, and never hammers.
10. **Fail-closed everything**: unknown senders ignored; oversized input rejected; daemon error → user-visible error; bridge crash → systemd restart + queued state recovered from chat state machine.

### 0.5 Threat model (and where each threat is killed)

| Threat | Vector | Mitigation (layer) |
|---|---|---|
| Stranger drives your agent | DMs/groups from unknown accounts | Bridge allowlist (chat ID + user ID), fail-closed, no response to strangers (they cannot even probe) |
| Prompt injection via IM text | Message says "ignore instructions, run rm -rf" | kod treats it as a normal user turn; policy still gates tools (Standard = Ask), blast-radius classifier hard-denies catastrophic commands, sandbox denies network + write outside workdir; bridge allowlist already restricts *who* can inject |
| Prompt injection via fetched web content | `web_fetch` returns hostile text | kod's trust/taint system: untrusted content is taint-tagged; tainted rounds force `Ask` on the 5 high-impact tools regardless of policy (`engine/settings.rs:406`) |
| Secret exfiltration through the IM | Agent reads `.env`, pastes into reply | `read_protection` (redact/refuse on `.env*`, keys, `.ssh/**`…), secrets vault placeholders in prompts, session-log redactor (always on), bridge pins `skills_dir` so a hostile repo can't plant skills |
| Secret exfiltration via shell | Agent runs `env`, `curl $TELEGRAM_BOT_TOKEN` | Shell env stripper: children never receive secret-like vars (`tools.rs:112`); sandbox `--unshare-net` denies network to shell; egress allowlists possible per-tool via policy |
| Bot token theft | Token in config/log/repo | Token lives only in a `0600` env file read by systemd; bridge redacts it from all error paths; never in git (env file is outside any repo) |
| Host-local attacker | Another process talks to the socket | Peer-UID check + `0600` socket: only the same OS user; run daemon+bridge under a dedicated user (`kod-agent`), not your login |
| Runaway agent / cost blowup | IM spam, retry loops, goal loops | Bridge per-chat rate limit + one-run-per-chat serialization; kod `[limits]` cost/token caps with `on_exhausted = "stop"`; engine hard caps (`MAX_TOOL_ROUNDS=40`, goal turns 6); 120 s approval timeout |
| IM platform outage | Telegram down / rate limits | grammY long-poll retry with backoff; bridge queues outbound sends per chat (bounded, drops previews, never drops finals); degradation is visible in the chat |
| Bridge compromise | RCE in bridge process | Bridge has no shell-outs, no eval, no dynamic imports; dependency surface is 2–3 well-known libs; runs as dedicated user; blast radius = "can talk to kod socket" (still policy-gated) — plus Bun lockfile pinned |
| Supply chain | Malicious npm package | Pin exact versions in `package.json` + commit `bun.lock`; `bun install --frozen-lockfile` in the unit; consider `bun audit` in CI |

---

## 1. Ground truth: the kod surfaces we build on (condensed protocol & security reference)

> This section is the *contract*. The implementing agent should treat everything here as fixed reality. Full file/line citations in Appendix A/B.

### 1.1 The daemon

- Start: `kod serve [--socket <path>] [--stop]`. Socket path: `$XDG_RUNTIME_DIR/kod.sock` if that env var is set and is a directory, else `~/.kod/run/kod.sock`. The daemon `chmod 0600`s the socket, flock-locks `<socket>.lock`, and refuses a second instance. `kod serve --stop` or SIGINT shuts down cleanly. `kod doctor` reports socket status.
- **Auth**: before reading a byte, the daemon checks `SO_PEERCRED`/`getpeereid` UID == its own UID. That is the entire auth model — the bridge **must** run as the same user. There is no token, no TCP, no TLS (and by design, never will be).
- Sessions: **one engine per daemon**; sessions are keyed by the client-chosen `transcript_key` string (per-key history, steers, cancels, working dir). No persistence across daemon restarts (transcripts live in memory; session *logs* live on disk as JSONL at `~/.kod/sessions/<ms>-<pid>-<seq>.jsonl`). Any connection may act on any key. There is no per-key busy lock on the daemon — **the bridge serializes one run per chat itself** (Phase 1 does this).
- Shipped transcript-key conventions to imitate: `""` (prompt), `"chat"` (chat --remote), `"agent:<name>"`, `"acp:<session_id>"`. Ours: **`tg:<chat_id>`** and **`tg:<chat_id>:<name>`** (named sessions), matrix: `mx:<room_id>…`.

### 1.2 The wire protocol (what the bridge implements)

One JSON object per line (NDJSON) over `UnixStream`. Requests carry an `id`; every response echoes it (multiplexing is allowed; we use one active run per connection plus control calls on the same connection).

**Request frame**: `{"v":1,"id":"<client-chosen>","method":"<name>","params":{…}}`

**Methods** (params exactly as below):

| method | params | notes |
|---|---|---|
| `hello` | `{}` | version handshake; optional `{min,max}` |
| `process_streaming` | `{"input": string, "transcript_key": string}` | starts a run; response stream = N × `chunk`, final `done` (TaskResponse) or `error` |
| `process` | `{"input", "transcript_key"}` | non-streaming variant (we don't use it) |
| `steer` | `{"note": string, "transcript_key": string}` | queues a `SoftInterrupt::User`, drained at the next round boundary; ack `done` |
| `cancel` | `{"transcript_key": string}` | sets the per-key cancel flag (epoch); ack `done` |
| `respond_to_approval` | `{"id": number, "decision": "approve"\|"deny"\|"deny_always"}` | answers a pending approval; `done` + `{"delivered": bool}` (`false` = already timed out/consumed) |
| `respond_to_question` | `{"id": number, "answer": string}` | answers `ask_user`; `done` + `{"delivered": bool}` |
| `peek_session` | `{"key": string, "max_chars"?: number}` | read-only transcript tail; `done` + `{key, messages, tail, truncated}` |
| `list_models` | `{}` | `done` + `{"models": ["id", …]}` |
| `set_model` | `{"endpoint": string, "model": string}` | ⚠ changes the model **for the whole daemon** (all sessions) |
| `shutdown` | `{}` | stops the daemon |
| `swarm` | `{"goal", "max_agents"?, "merge"?}` | streams `swarm_event` frames then `done` |

**Response frames**: `{"id","type":"chunk","data":"<text>"}` · `{"id","type":"done","data":{TaskResponse}}` (bare `done` = ack, `data` omitted) · `{"id","type":"error","data":{"message":"…"}}` · `{"id","type":"swarm_event","data":{kind,…}}`.

**TaskResponse** (`done` payload, fields the bridge uses): `text` (final assistant text), `task_type`, `execution_time_ms`, `usage` (`prompt_tokens`, `completion_tokens`, …), `pricing` (per-Mtok rates when the endpoint declares them), `skills_used`.

### 1.3 The marker protocol (inside `chunk` data strings)

The daemon forwards engine chunks verbatim. Control information arrives embedded in `chunk` strings as NUL-delimited markers — **always within a single `chunk` frame** (one frame = one line = one event), so frame-level parsing is safe:

| Marker | Shape | Bridge behavior |
|---|---|---|
| Tool start | `\0kod-tool:<cid>:<name>\0` | add activity line "⚙️ `<name>`" |
| Tool args brief | `\0kod-args:<cid>:<display>\0` | append short arg summary to the activity line |
| Tool done | `\0kod-done:<cid>\0<header>\0<summary>\0<duration_ms>` | complete the activity line with ✅ + ms |
| Approval batch | `\0kod-approval-batch:<batch_id>:<json>\0` | **render inline keyboard**; JSON: `{"items":[{"tool_name","arguments","diff","summary","id"}]}` |
| Question | `\0kod-question:<id>:<json>\0` | **ask in chat**; JSON: `{"question","placeholder"?}` |
| Thinking | `\0kod-thinking\0` | optionally show "🤔 …" |
| Turn boundary | `\0kod-turn:<n>\0` | (goal loop) start a new preview message |
| Activity | `\0kod-activity:<label>\0` | status line update |
| Usage | `\0kod-usage:<prompt>,<completion>\0` | footer counters |
| Rate-limit / server-busy | `\0kod-rate-limit:<secs>\0<attempt>\0<max>` / `\0kod-server-busy:…` | status "⏳ provider busy, waiting Ns" |
| Stream reset | `\0kod-stream-reset\0` | clear the streaming preview buffer |
| Legacy approval (single) | `\0kod-approval:<id>:<json>\0` | treat like a 1-item batch (rare) |

### 1.4 Approval & question semantics (the UX-critical part)

- Policy `Ask` calls produce one **approval batch marker** per round. Each item carries `tool_name`, full `arguments`, `diff` (for file writes, produced from the checkpoint snapshot), a one-line `summary`, and a numeric `id`.
- The engine registers a oneshot per id and waits **120 s** (`AWAIT_APPROVAL_SECS`). Outcomes: `approve` / `deny` / `deny_always` (adds a session deny rule) / timeout-or-cancel → **deny**. `respond_to_approval` returns `{"delivered":false}` if the window has passed — the bridge must handle that gracefully (edit the buttons to "expired").
- `ask_user` questions work identically with free-text answers (`respond_to_question`). No answer in 120 s → the engine answers "(no answer within 120s — the user is away)".
- Answering from the same connection that started the run is legal while the run streams (the daemon keeps dispatching control methods on that connection). Answering from any other connection also works (the pending map is engine-global).
- **The one real gap**: markers only reach the connection that issued `process_streaming`. A run started elsewhere (TUI, CLI) is invisible to the bridge (no `watch` method today). Phase 4.1 patches this; until then, the bridge's own runs are fully interactive, and `peek_session` covers read-only visibility into foreign sessions.

### 1.5 Security model facts the bridge relies on

- **Secrets vault**: at daemon start, env vars whose names contain `KEY|TOKEN|SECRET|PASSWORD|PASS|AUTH|CREDENTIAL|PRIVATE|OAUTH` (value ≥ 8 chars) are registered; every string sent to the model is obfuscated to `«Credential-<hex>»`; model-authored tool args are de-obfuscated at dispatch. ⇒ Set `TELEGRAM_BOT_TOKEN` **in the daemon's environment** and it can never leak into a transcript, a session log, or a shell child.
- **Shell env stripper**: `execute_command` children never see vars containing `KEY TOKEN SECRET PASSWORD PASSWD CREDENTIAL AUTH BEARER PRIVATE` or prefixed `ANTHROPIC_ OPENAI_ AWS_ GCP_ GOOGLE_ AZURE_`.
- **Policy layers** (strictest wins): CLI `--preset` > project `<cwd>/.kod/policy.toml` (can only *narrow*) > global config `[tools] preset`. Presets: `read-only` (reads allowed, everything else denied), `standard` (reads allowed, everything else **Ask** — default), `yolo`. Per-tool overrides: `mode`, `paths` allowlist, `forbidden` globs, `binaries`/`forbidden_binaries`, `network`, `domains`.
- **Blast-radius gate** (`kod-risk`): `execute_command` is classified `Safe < Low < Confirm < Catastrophic`. `Catastrophic` (e.g. `mkfs`, `dd` to devices, writes into `~/.ssh`, `/etc`, home root) is **denied unconditionally** — no approval can override. `Confirm` forces Ask even under `yolo`-ish configs.
- **Read protection**: default deny-globs for `.env*`, `**/*.pem`, `**/*.key`, `id_rsa*`, `.ssh/**`, `.aws/credentials`, … with `redact` or `refuse` modes (configured in `.kod/policy.toml` `[read_protection]`).
- **Sandbox**: `--sandbox` (or Auto default) wraps every `execute_command` in bwrap (Linux) / sandbox-exec (macOS) / Landlock (kernel ≥ 6.7 for net-deny): FS confined to workdir+tmp, `.git` read-only, **network denied** to the shell while the daemon/bridge keep their network. Install `bubblewrap` on Linux hosts.
- **Budgets**: `[limits]` per-turn/per-session cost + token caps, per-tool quotas (`per_turn`, `per_session`, `per_command`), `on_exhausted = "stop"` (the only non-interactive-safe value; `"ask"` degenerates to stop for non-TUI callers). Engine hard caps: 40 tool rounds/turn, 6 goal turns, 2 rate-limit retries, 4 busy retries.

---

## 2. Transport & security design

### 2.1 The outbound-only principle

Every supported IM platform offers an outbound-only receive path, which is the load-bearing decision of this design:

| Platform | Receive path | Outbound-only? | Interactive buttons | Notes |
|---|---|---|---|---|
| **Telegram** | `getUpdates` long poll | ✅ | ✅ inline keyboards + callback queries | Best MVP; free; 4096-char messages; ~1 edit/s per chat |
| **Matrix** | `/sync` long poll | ✅ | ⚠ reactions as buttons (👍/👎) | Self-hostable (conduwuit) → fully sovereign; E2EE possible (harder; optional) |
| **Discord** | Gateway WebSocket | ✅ | ✅ buttons/components | Message-content intent must be enabled in the dev portal |
| **Slack** | Socket Mode (WS) | ✅ | ✅ Block Kit actions | Best in workplaces; requires app tokens |
| **Signal** | signal-cli JSON-RPC | ✅ (local daemon) | ❌ reply-keywords | Heavier (Java daemon); most private; bridge over tailnet works |
| WhatsApp | unofficial web bridges | ⚠ | ⚠ | **Not recommended**: unofficial API, ban risk |

⇒ The bridge's adapter contract (Phase 3) needs exactly: `start()`, `onMessage`, `onAction` (button/reaction), `sendText`, `sendFormatted`, `editMessage`, `sendButtons`, `resolveButtons`, `sendDocument`, `react`. No adapter ever listens on a port.

### 2.2 Tailscale: where it fits (and where it deliberately doesn't)

**Recommended posture (Pattern A — "Sovereign default")**: Tailscale is *not* in the IM data path at all. Install it on the kod host anyway for:
- `tailscale ssh` from your laptop/phone → admin the host from anywhere without exposing SSH publicly;
- `tailscale serve --bg --https=443 localhost:3000` → a private status dashboard (Phase 5 ships a 20-line one) reachable only inside your tailnet;
- reaching a **self-hosted Matrix homeserver** from your phone when off-LAN (Pattern B).

**Pattern B — "Fully sovereign IM" (most kod-philosophy-aligned)**: run a lightweight Matrix homeserver (`conduwuit`) bound to the tailnet interface; the bridge talks to it over localhost; your phone joins the tailnet and uses the homeserver directly. Zero third-party servers touch your agent traffic.

```bash
# host: install tailscale + conduwuit (Rust, single binary, low memory)
curl -fsSL https://tailscale.com/install.sh | sh
tailscale up --ssh
# conduwuit: config listen [::1]:6167 (loopback only), registration closed,
# create your user via conduwuit CLI, then point the bridge at http://[::1]:6167
# Your phone: install Element, set custom homeserver to https://<tailnet-name>.ts.net:6167
# (expose conduwuit through `tailscale serve --bg --https=6167 tcp://localhost:6167`)
```

**Pattern C — "Webhook mode" (only when a platform forces it, e.g. Slack interactivity)**: keep the receiver on loopback and expose it through the tailnet:

```bash
# Telegram webhook (rarely needed — long polling is better):
kod-im serve-webhook --port 8080 &          # binds 127.0.0.1 ONLY
tailscale funnel --bg --https=8443 localhost:8080
# then set webhook with Telegram's secret_token:
curl "https://api.telegram.org/bot$TELEGRAM_BOT_TOKEN/setWebhook" \
  -d "url=https://<your-host>.<tailnet>.ts.net:8443/hook/<RANDOM_SECRET_PATH>" \
  -d "secret_token=<RANDOM_SECRET>" -d "allowed_updates=[\"message\",\"callback_query\"]"
```

The bridge validates Telegram's `X-Telegram-Bot-Api-Secret-Token` header and refuses everything else. Funnel is *public internet* exposure (TLS terminated by Tailscale) — treat it as the fallback, not the default. **Never** run `socat`/TCP proxies to the kod socket across machines: it downgrades the peer-UID boundary (AD-7).

### 2.3 Secrets & credentials layout

```
/etc/kod-im/env              # 0600 root-owned? NO → 0600 owned by the 'kod-agent' user; contains:
#   TELEGRAM_BOT_TOKEN=123456789:AA…      ← read by bridge AND inherited by daemon & MCP child
#   KOD_IM_ALLOW_CHATS=123456789          ← comma-separated chat IDs
#   KOD_IM_ALLOW_USERS=987654321          ← optional user IDs (defaults to allow-chats owner)
#   KOD_IM_HOME=/srv/kod-agent            ← working dir for the daemon engine (the repo it edits)

/srv/kod-agent/              # dedicated user 'kod-agent' home; NOT your login
#   XDG_RUNTIME_DIR=/run/user/<uid>       → socket /run/user/<uid>/kod.sock (systemd user manager sets this)
#   ~/.config/kod/config.toml             → daemon config (Phase 0)
#   <repo>/.kod/policy.toml               → project policy (Phase 0)
#   ~/.kod/{sessions,background,data}/    → runtime state
```

Why the token is safe in the daemon's env (and thus in the MCP child's env): kod's vault registers it at startup (name contains `TOKEN`), replaces it with `«Credential-…»` placeholders everywhere it reaches the model or logs, and the shell env stripper prevents `execute_command` children from ever seeing it. The bridge additionally scrubs the token from its own logs/errors.

### 2.4 Process model & users

- Dedicated system user `kod-agent` (no login shell). Two systemd **user** services under it (or system services with `User=kod-agent` + `PAMName=` unset; user manager is simpler and gives `XDG_RUNTIME_DIR`).
- Bridge and daemon are siblings; the bridge dies → daemon keeps state (transcripts in memory), bridge restarts in <1 s and reattaches by transcript key. Daemon dies → bridge reports "daemon unreachable" per chat and recovers automatically when it returns (reconnect with backoff). Neither can corrupt the other.

---

## 3. PHASE 0 — Foundations (run these steps in order)

> **AGENT EXECUTION NOTES — Phase 0**
> - Run every command and check every gate. Do not continue past a failed gate.
> - All shell commands assume you are root or can `sudo -u kod-agent`. Replace `<REPO>` with the repo you want the agent to work on.
> - Nothing in Phase 0 touches the IM platform yet.

**Step 0.1 — System user + directories**

```bash
sudo useradd --system --create-home --home-dir /srv/kod-agent --shell /usr/sbin/nologin kod-agent
sudo -u kod-agent mkdir -p /srv/kod-agent/{bin,im,config}
sudo loginctl enable-linger kod-agent        # user manager runs without login (needed for sockets + services)
```

**Step 0.2 — Build kod**

```bash
git clone https://github.com/elcoosp/kod /srv/kod-agent/src/kod   # or reuse your checkout
cd /srv/kod-agent/src/kod && cargo build --release --locked
sudo -u kod-agent cp target/release/kod /srv/kod-agent/bin/
```

**Gate 0.2** ✅ `sudo -u kod-agent /srv/kod-agent/bin/kod doctor` exits 0 and reports config + provider status (provider endpoint may be unconfigured — that's the next step).

**Step 0.3 — Daemon config** (install to `/srv/kod-agent/.config/kod/config.toml`)

**File: `config/kod/config.toml`**

```toml
config_version = 2

[llm]
network_access = false                       # web_fetch off by default; enable per-tool in policy if wanted

[[llm.endpoints]]
name = "default"
provider = "openai-compatible"
base_url = "http://127.0.0.1:11434/v1"       # Ollama default; point at your endpoint
model = "qwen2.5-coder:7b"
context_window = 32768
max_tokens = 4096
temperature = 0.2
timeout_secs = 300
# api_key_env = "MY_LLM_API_KEY"            # env var NAME only — never put the key here

[tools]
preset = "standard"                          # reads allowed; writes/shell gated (Ask) — approvals flow to the IM
auto_check = false
auto_lsp = true

[limits]
max_cost_usd_per_session = 5.0               # set 0 to disable (local free models)
max_cost_usd_per_turn = 0.50
max_output_tokens_per_turn = 4096
on_exhausted = "stop"                        # the ONLY non-interactive-safe value
soft_warn_at = 0.5

[limits.tools]
default         = { per_turn = 50, per_session = 1000 }
execute_command = { per_turn = 30, per_session = 500, per_command = 5 }

[security.redact]
in_prompt = true
redact_argument_paths = ["**/.env", "**/*.pem", "**/*.key"]

[skills]
skills_dir = "/srv/kod-agent/skills"         # PINNED — a hostile repo cannot plant skills via .kod/skills

[mcp.servers.im-notify]                      # Phase 3.5 wires the actual script; harmless if missing until then
command = "/srv/kod-agent/im/mcp/im-notify.mjs"
args = []
enabled = true
call_timeout_secs = 30

[swarm]
max_agents = 2
timeout_secs = 900
agent_timeout_secs = 180
```

**Step 0.4 — Project policy** (install to `<REPO>/.kod/policy.toml`; project layer can only *narrow*, so this also hardens against a sloppy global config)

**File: `config/kod/policy.toml`**

```toml
preset = "standard"

[tools.execute_command]
mode = "ask"                                 # every shell command must be approved (IM buttons)
# Tighten later if you like (examples):
# binaries = ["cargo", "git", "rustfmt", "python3"]
# forbidden_binaries = ["curl", "wget"]
# [tools.write_file]
# mode = "ask"
# paths = ["src/**", "docs/**", "tests/**"]
# forbidden = ["**/.env", "**/*.pem", "**/*.key"]

[read_protection]
enabled = true
mode = "refuse"                              # stricter than default "redact": reading secrets returns an error
deny = [".env", ".env.*", "**/.env", "**/.env.*", "**/*.pem", "**/*.key",
        "**/id_rsa*", "**/id_ed25519*", "**/.ssh/**", "**/.aws/credentials",
        "**/.docker/config.json", "**/.netrc", "**/.pgpass", "**/credentials.json"]

[git]
history_protected = true
```

**Step 0.5 — Secrets env file** (install to `/srv/kod-agent/config/kod-im.env`, `chmod 600`, owner `kod-agent`; create the Telegram token in Step 0.7)

**File: `config/kod-im.env`**

```bash
# /srv/kod-agent/config/kod-im.env  (chmod 600)
TELEGRAM_BOT_TOKEN=replace-me
KOD_IM_ALLOW_CHATS=replace-me                # comma-separated numeric chat ids; /id prints them
KOD_IM_ALLOW_USERS=                          # optional; empty = only chat allowlist applies
KOD_IM_DB=/srv/kod-agent/im/state.json
KOD_IM_LOG=info
```

**Step 0.6 — Daemon systemd unit** (install to `/etc/systemd/system/kod-serve.service`)

**File: `deploy/kod-serve.service`**

```ini
[Unit]
Description=KOD agent daemon (kod serve)
After=network-online.target
Wants=network-online.target

[Service]
Type=simple
User=kod-agent
Group=kod-agent
WorkingDirectory=/srv/kod-agent/workspace    # ← the repo the agent edits (REPLACE)
EnvironmentFile=/srv/kod-agent/config/kod-im.env
ExecStart=/srv/kod-agent/bin/kod serve
Restart=on-failure
RestartSec=3
# Hardening
NoNewPrivileges=true
PrivateTmp=true
ProtectSystem=strict
ProtectHome=tmpfs
ReadWritePaths=/srv/kod-agent /srv/kod-agent/workspace
CapabilityBoundingSet=
AmbientCapabilities=
MemoryDenyWriteExecute=true
KernelUnprivilegedUsernsClone=              # leave default; bwrap needs userns on some distros

[Install]
WantedBy=multi-user.target
```

> ⚠ `WorkingDirectory` is load-bearing: the daemon builds its policy engine and repo map against its cwd. One daemon = one repo. Multi-repo → multiple daemons with `--socket` paths (the bridge supports named sockets via `/use`, Phase 1).

**Step 0.7 — Telegram bot bootstrap (human step, 3 minutes)**

1. In Telegram, talk to `@BotFather`: `/newbot` → choose a name → receive the token (`123456789:AA…`).
2. Put the token in `/srv/kod-agent/config/kod-im.env`.
3. Send any message to your new bot from your account, then discover your chat ID (temporary, before the bridge exists):

```bash
sudo -u kod-agent sh -c 'curl -s "https://api.telegram.org/bot$TELEGRAM_BOT_TOKEN/getUpdates" | head -c 2000'
# look for  "chat":{"id":123456789,  ← put that number into KOD_IM_ALLOW_CHATS
```

4. (Private chat recommended. If you use a group, disable privacy mode via BotFather `/setprivacy` → Disable, so the bot sees all messages; also add the group ID to `KOD_IM_ALLOW_CHATS` — group IDs are negative like `-1001234567890`.)

**Step 0.8 — Start & verify the daemon**

```bash
sudo systemctl daemon-reload
sudo systemctl enable --now kod-serve.service
sleep 1
sudo -u kod-agent sh -c 'printf "{\"v\":1,\"id\":\"smoke-1\",\"method\":\"process_streaming\",\"params\":{\"input\":\"Say OK and nothing else.\",\"transcript_key\":\"smoke\"}}\n" | socat - UNIX-CONNECT:"$XDG_RUNTIME_DIR/kod.sock"' | head -c 2000
```

**Gate 0.8** ✅ You see `{"id":"smoke-1","type":"chunk",…}` lines and a final `{"id":"smoke-1","type":"done",…}`. (If `socat` is missing: `apt install socat`, or wait for the bridge's `doctor`.) Also verify posture: `sudo -u kod-agent /srv/kod-agent/bin/kod policy explain execute_command command="ls"` → outcome `Ask`, and `kod sandbox check` → reports bwrap available (else `apt install bubblewrap`).

---

## 4. PHASE 1 — The bridge (`kod-im`): scaffold, protocol client, marker parser

> **AGENT EXECUTION NOTES — Phase 1**
> - Runtime: **Bun ≥ 1.2** (`curl -fsSL https://bun.sh/install | bash`). Node ≥ 20 also works for everything except `bun test` (swap to `vitest` if forced onto Node).
> - Create every file exactly as given. Files marked `File: …` are extracted to the `kod-im/` scaffold that ships beside this plan.
> - After each file group, run the listed **gate**. Only proceed on green.

**Step 1.1 — Scaffold**

```bash
mkdir -p /srv/kod-agent/im/kod-im/src/{kod,adapters,format} /srv/kod-agent/im/kod-im/{test,deploy,scripts,mcp}
cd /srv/kod-agent/im/kod-im
```

**File: `kod-im/package.json`**

```json
{
  "name": "kod-im",
  "version": "0.1.0",
  "private": true,
  "type": "module",
  "engines": { "bun": ">=1.2" },
  "scripts": {
    "start": "bun run src/index.ts",
    "doctor": "bun run src/index.ts --doctor",
    "test": "bun test",
    "check": "bunx tsc --noEmit"
  },
  "dependencies": {
    "grammy": "^1.35.0"
  },
  "devDependencies": {
    "bun-types": "^1.2.0",
    "typescript": "^5.6.0"
  }
}
```

> `matrix-js-sdk` (Phase 3) is intentionally **not** in `package.json`: it's a heavier dep only needed for the Matrix adapter, which is excluded from `tsconfig` until you `bun add matrix-js-sdk` and remove `src/adapters/matrix.ts` from the `exclude` list. This keeps the Phase-1 install minimal.

**File: `kod-im/tsconfig.json`**

```json
{
  "compilerOptions": {
    "target": "ESNext",
    "module": "ESNext",
    "moduleResolution": "bundler",
    "allowImportingTsExtensions": true,
    "strict": true,
    "noUncheckedIndexedAccess": true,
    "skipLibCheck": true,
    "types": ["bun-types"],
    "noEmit": true
  },
  "include": ["src", "test"],
  "exclude": ["src/adapters/matrix.ts"]
}
```

**Gate 1.1** ✅ `bun install --frozen-lockfile || bun install` completes; `bun run check` passes (empty sources are fine).

**Step 1.2 — Configuration loader**

**File: `kod-im/src/config.ts`**

```ts
import { homedir } from "node:os";
import { join } from "node:path";

export interface SocketMap { [name: string]: string } // name -> unix socket path

export interface Config {
  botToken: string;
  allowChats: Set<string>;      // numeric chat ids as strings (groups are negative)
  allowUsers: Set<string>;      // optional extra user allowlist (admin powers)
  defaultSocketPath: string;
  sockets: SocketMap;           // named sockets for /use ("main" always maps to defaultSocketPath)
  statePath: string;
  maxInputChars: number;        // reject oversized prompts
  previewIntervalMs: number;    // streaming edit debounce
  maxPreviews: number;          // max preview edits per run (rate-limit safety)
  approvalTimeoutS: number;     // informational: engine denies after this (120s)
}

function env(k: string, d = ""): string {
  const v = process.env[k];
  return v === undefined || v === "" ? d : v;
}

export function defaultSocketPath(): string {
  const xdg = process.env.XDG_RUNTIME_DIR;
  if (xdg) {
    try { if (require("node:fs").statSync(xdg).isDirectory()) return join(xdg, "kod.sock"); }
    catch { /* fall through */ }
  }
  return join(homedir(), ".kod", "run", "kod.sock");
}

export function loadConfig(): Config {
  const botToken = env("TELEGRAM_BOT_TOKEN");
  if (!botToken) throw new Error("TELEGRAM_BOT_TOKEN is not set (see config/kod-im.env)");

  const allowChats = new Set(
    env("KOD_IM_ALLOW_CHATS").split(",").map((s) => s.trim()).filter(Boolean),
  );
  const allowUsers = new Set(
    env("KOD_IM_ALLOW_USERS").split(",").map((s) => s.trim()).filter(Boolean),
  );
  if (allowChats.size === 0 && allowUsers.size === 0) {
    throw new Error(
      "Refusing to start: no allowlist. Set KOD_IM_ALLOW_CHATS (chat ids via /id) " +
      "or KOD_IM_ALLOW_USERS. The bridge never serves unknown chats (fail-closed).",
    );
  }

  const sockets: SocketMap = { main: defaultSocketPath() };
  const extra = env("KOD_IM_SOCKETS_JSON");
  if (extra) {
    const parsed = JSON.parse(extra) as SocketMap;
    for (const [k, v] of Object.entries(parsed)) sockets[k] = v;
  }

  return {
    botToken,
    allowChats,
    allowUsers,
    defaultSocketPath: defaultSocketPath(),
    sockets,
    statePath: env("KOD_IM_DB", join(homedir(), ".kod-im", "state.json")),
    maxInputChars: Number(env("KOD_IM_MAX_MSG_CHARS", "8000")),
    previewIntervalMs: Number(env("KOD_IM_PREVIEW_INTERVAL_MS", "2000")),
    maxPreviews: Number(env("KOD_IM_MAX_PREVIEWS", "25")),
    approvalTimeoutS: Number(env("KOD_IM_APPROVAL_TIMEOUT_S", "120")),
  };
}

export function socketPathFor(cfg: Config, name: string): string {
  const p = cfg.sockets[name];
  if (!p) throw new Error(`unknown socket '${name}' (known: ${Object.keys(cfg.sockets).join(", ")})`);
  return p;
}
```

**Step 1.3 — Wire types (mirror of kod's serve protocol)**

**File: `kod-im/src/kod/protocol.ts`**

```ts
/** Mirror of kod-core-serve's NDJSON protocol. Field names are exact — do not rename.
 *  Source of truth: crates/kod-core-serve/src/serve.rs (kod @ HEAD, protocol 1..=1). */

export interface RequestFrame {
  v?: 1;
  id: string;
  method: string;
  params?: unknown;
}

export type ResponseFrame = {
  id: string;
  type: "chunk";
  data: string;                 // raw text incl. \0kod-* markers
} | {
  id: string;
  type: "done";
  data?: TaskResponse | Record<string, unknown>;  // bare done = ack (data omitted)
} | {
  id: string;
  type: "error";
  data: { message: string };
} | {
  id: string;
  type: "swarm_event";
  data: { kind: string; data?: unknown };
};

export interface ApprovalItem {
  id?: number;                  // present inside approval-batch items
  tool_name: string;
  arguments: unknown;           // full args JSON (secrets already vault-placeholdered by kod)
  diff?: string | null;         // unified diff for write_file / patch_file
  summary: string;
}

export interface QuestionRequest {
  question: string;
  placeholder?: string;
}

export interface TaskResponse {
  task_type?: string;
  text?: string | null;
  execution_time_ms?: number;
  usage?: {
    prompt_tokens?: number;
    completion_tokens?: number;
    total_tokens?: number;
    cache_read_tokens?: number;
  };
  pricing?: {
    input_per_mtok_usd?: number;
    output_per_mtok_usd?: number;
    cache_convention?: string;
  };
  skills_used?: string[];
  [k: string]: unknown;
}

export interface PeekResult {
  key: string;
  messages: number;
  tail: string;
  truncated: boolean;
}

export type Decision = "approve" | "deny" | "deny_always";
```

**Step 1.4 — Marker parser (the streaming event decoder)**

**File: `kod-im/src/kod/markers.ts`**

```ts
import type { ApprovalItem, QuestionRequest } from "./protocol.ts";

/** Events decoded from one `chunk` frame. Markers are always fully contained
 *  in a single frame (the daemon emits one NDJSON line per engine chunk), so
 *  frame-local parsing is safe. Text outside markers is surfaced as `text`. */
export type KodEvent =
  | { kind: "text"; text: string }
  | { kind: "tool-start"; cid: string; name: string }
  | { kind: "tool-args"; cid: string; display: string }
  | { kind: "tool-done"; cid: string; header: string; summary: string; durationMs: number }
  | { kind: "approval-batch"; batchId: string; items: ApprovalItem[] }
  | { kind: "approval-legacy"; id: number; item: ApprovalItem }
  | { kind: "question"; id: number; request: QuestionRequest }
  | { kind: "thinking" }
  | { kind: "activity"; label: string }
  | { kind: "turn"; n: number }
  | { kind: "usage"; prompt: number; completion: number }
  | { kind: "rate-limit"; secs: number; attempt: number; max: number }
  | { kind: "server-busy"; secs: number; attempt: number; max: number }
  | { kind: "stream-reset" };

function parseNum(s: string | undefined, fallback = 0): number {
  const n = Number(s);
  return Number.isFinite(n) ? n : fallback;
}

/** Split approval/question marker payloads: `kod-approval-batch:<numeric-id>:<json>`. */
function splitIdJson(rest: string): [string, string] {
  const i = rest.indexOf(":");
  if (i < 0) return [rest, ""];
  return [rest.slice(0, i), rest.slice(i + 1)];
}

export function parseChunkFrame(data: string): KodEvent[] {
  const events: KodEvent[] = [];
  let text = "";
  // Splitting on NUL: tags look like `kod-tool:<cid>:<name>`; multi-field markers
  // (kod-done, kod-rate-limit, kod-server-busy) place each field in its own token.
  const tokens = data.split("\0");

  for (let i = 0; i < tokens.length; i++) {
    const tok = tokens[i];
    if (tok === undefined || tok === "") continue;
    if (!tok.startsWith("kod-")) { text += tok; continue; }

    if (tok.startsWith("kod-tool:")) {
      const rest = tok.slice("kod-tool:".length);
      const c = rest.indexOf(":");
      if (c >= 0) events.push({ kind: "tool-start", cid: rest.slice(0, c), name: rest.slice(c + 1) });
    } else if (tok.startsWith("kod-args:")) {
      const rest = tok.slice("kod-args:".length);
      const c = rest.indexOf(":");
      if (c >= 0) events.push({ kind: "tool-args", cid: rest.slice(0, c), display: rest.slice(c + 1) });
    } else if (tok.startsWith("kod-done:")) {
      const cid = tok.slice("kod-done:".length);
      const header = tokens[++i] ?? "";
      const summary = tokens[++i] ?? "";
      const durationMs = parseNum(tokens[++i]);
      events.push({ kind: "tool-done", cid, header, summary, durationMs });
    } else if (tok.startsWith("kod-approval-batch:")) {
      const [batchId, json] = splitIdJson(tok.slice("kod-approval-batch:".length));
      try {
        const parsed = JSON.parse(json) as { items?: ApprovalItem[] };
        events.push({ kind: "approval-batch", batchId, items: parsed.items ?? [] });
      } catch { /* malformed batch: fail-safe = ignore (engine will time out to deny) */ }
    } else if (tok.startsWith("kod-question:")) {
      const [idStr, json] = splitIdJson(tok.slice("kod-question:".length));
      try {
        const req = JSON.parse(json) as QuestionRequest;
        events.push({ kind: "question", id: parseNum(idStr, -1), request: req });
      } catch { /* ignore */ }
    } else if (tok.startsWith("kod-approval:")) {
      const [idStr, json] = splitIdJson(tok.slice("kod-approval:".length));
      try {
        const item = JSON.parse(json) as ApprovalItem;
        events.push({ kind: "approval-legacy", id: parseNum(idStr, -1), item });
      } catch { /* ignore */ }
    } else if (tok.startsWith("kod-thinking")) {
      events.push({ kind: "thinking" });
    } else if (tok.startsWith("kod-activity:")) {
      events.push({ kind: "activity", label: tok.slice("kod-activity:".length) });
    } else if (tok.startsWith("kod-turn:")) {
      events.push({ kind: "turn", n: parseNum(tok.slice("kod-turn:".length)) });
    } else if (tok.startsWith("kod-usage:")) {
      const [p, c] = tok.slice("kod-usage:".length).split(",");
      events.push({ kind: "usage", prompt: parseNum(p), completion: parseNum(c) });
    } else if (tok.startsWith("kod-rate-limit:")) {
      const [s, a, m] = tok.slice("kod-rate-limit:".length).split("\0");
      events.push({ kind: "rate-limit", secs: parseNum(s), attempt: parseNum(a), max: parseNum(m) });
    } else if (tok.startsWith("kod-server-busy:")) {
      const [s, a, m] = tok.slice("kod-server-busy:".length).split("\0");
      events.push({ kind: "server-busy", secs: parseNum(s), attempt: parseNum(a), max: parseNum(m) });
    } else if (tok.startsWith("kod-stream-reset")) {
      events.push({ kind: "stream-reset" });
    }
    // Unknown kod-* markers are deliberately ignored: forward compatibility.
  }

  const t = text.trim();
  if (t) events.unshift({ kind: "text", text: t });
  return events;
}
```

**Step 1.5 — The socket client**

Design: one `KodConnection` = one Unix socket = **one active run** plus any number of control calls multiplexed on it (the daemon keeps dispatching `steer`/`cancel`/`respond_*` on the same connection while a run streams — verified against `serve.rs`). Control one-shots use `KodConnection.control(...)`.

**File: `kod-im/src/kod/client.ts`**

```ts
import net from "node:net";
import type { Decision, PeekResult, RequestFrame, ResponseFrame, TaskResponse } from "./protocol.ts";
import { parseChunkFrame, type KodEvent } from "./markers.ts";

let counter = 0;
const nextId = (prefix: string) => `${prefix}-${Date.now().toString(36)}-${++counter}`;

interface Pending {
  resolve: (v: unknown) => void;
  reject: (e: Error) => void;
  onFrame?: (f: ResponseFrame) => void;
  timer?: ReturnType<typeof setTimeout>;
}

export class KodConnection {
  private socket: net.Socket | null = null;
  private pending = new Map<string, Pending>();
  private closedByUs = false;

  /** Called when the connection drops unexpectedly (bridge shows 'daemon unreachable'). */
  onDown: (() => void) | null = null;

  constructor(readonly socketPath: string) {}

  connect(): Promise<void> {
    return new Promise((resolve, reject) => {
      const s = net.connect(this.socketPath);
      let connected = false;
      s.on("connect", () => { connected = true; this.socket = s; this.wireReader(s); resolve(); });
      s.on("error", (err) => {
        if (!connected) reject(new Error(`cannot connect to ${this.socketPath}: ${err.message}`));
        else this.drop(`socket error: ${err.message}`);
      });
      s.on("close", () => { if (!this.closedByUs) this.drop("connection closed"); });
      s.setTimeout(10_000, () => {
        if (!connected) { s.destroy(); reject(new Error(`timeout connecting to ${this.socketPath}`)); }
      });
    });
  }

  private wireReader(s: net.Socket): void {
    let buf = "";
    s.on("data", (chunk: Buffer) => {
      buf += chunk.toString("utf8");
      let nl: number;
      while ((nl = buf.indexOf("\n")) >= 0) {
        const line = buf.slice(0, nl).replace(/\r$/, "");
        buf = buf.slice(nl + 1);
        if (line.trim()) this.handleLine(line);
      }
      if (buf.length > 16 * 1024 * 1024) this.drop("response buffer overflow"); // paranoid; daemon frames are small
    });
  }

  private handleLine(line: string): void {
    let frame: ResponseFrame;
    try {
      frame = JSON.parse(line) as ResponseFrame;
    } catch {
      return; // never crash the bridge on a bad line
    }
    const p = this.pending.get(frame.id);
    if (!p) return; // frame for an already-completed request (late chunk) — ignore
    try { p.onFrame?.(frame); } catch (e) { /* keep the run alive on parser bugs */ }

    if (frame.type === "error") {
      this.settle(frame.id, () => p.reject(new Error(frame.data?.message ?? "daemon error")));
    } else if (frame.type === "done") {
      this.settle(frame.id, () => p.resolve(frame.data));
    }
    // "chunk" / "swarm_event" frames stay open until done/error arrives.
  }

  private settle(id: string, fn: () => void): void {
    const p = this.pending.get(id);
    if (!p) return;
    this.pending.delete(id);
    if (p.timer) clearTimeout(p.timer);
    fn();
  }

  private drop(reason: string): void {
    const err = new Error(`kod connection down (${reason})`);
    for (const [id, p] of this.pending) { if (p.timer) clearTimeout(p.timer); p.reject(err); }
    this.pending.clear();
    try { this.socket?.destroy(); } catch { /* ignore */ }
    this.socket = null;
    if (!this.closedByUs) this.onDown?.();
  }

  private send(method: string, params: unknown, opts: {
    onFrame?: (f: ResponseFrame) => void;
    timeoutMs?: number;   // undefined = no client timeout (runs); control calls set one
  } = {}): Promise<unknown> {
    const id = nextId(method.replace(/_/g, "-"));
    const frame: RequestFrame = { v: 1, id, method, params };
    return new Promise((resolve, reject) => {
      let timer: Pending["timer"];
      if (opts.timeoutMs !== undefined) {
        timer = setTimeout(() => this.settle(id, () => reject(new Error(`${method}: client timeout`))), opts.timeoutMs);
      }
      this.pending.set(id, { resolve, reject, onFrame: opts.onFrame, timer });
      this.socket!.write(JSON.stringify(frame) + "\n");
    });
  }

  // ---- one-shot control helpers -------------------------------------------

  /** Open a connection, run `fn`, close. For hello/peek/models/shutdown. */
  static async control<T>(socketPath: string, fn: (c: KodConnection) => Promise<T>, timeoutMs = 15_000): Promise<T> {
    const c = new KodConnection(socketPath);
    await c.connect();
    try { return await fn(c); } finally { c.close(); }
  }

  hello(): Promise<unknown> { return this.send("hello", {}, { timeoutMs: 10_000 }); }

  peek(key: string, maxChars = 4000): Promise<PeekResult> {
    return this.send("peek_session", { key, max_chars: maxChars }, { timeoutMs: 15_000 }) as Promise<PeekResult>;
  }

  listModels(): Promise<{ models: string[] }> {
    return this.send("list_models", {}, { timeoutMs: 15_000 }) as Promise<{ models: string[] }>;
  }

  setModel(endpoint: string, model: string): Promise<unknown> {
    return this.send("set_model", { endpoint, model }, { timeoutMs: 15_000 });
  }

  shutdown(): Promise<unknown> { return this.send("shutdown", {}, { timeoutMs: 10_000 }); }

  // ---- the run + its control channel --------------------------------------

  /** Start a streaming run. Events (text deltas, markers) go to `onEvent`.
   *  Resolves with the TaskResponse on `done`. Control calls below may be
   *  issued on `this` while the run is in flight. */
  async run(
    input: string,
    transcriptKey: string,
    onEvent: (e: KodEvent) => void,
  ): Promise<TaskResponse> {
    return (await this.send("process_streaming", { input, transcript_key: transcriptKey }, {
      onFrame: (f) => {
        if (f.type === "chunk" && typeof f.data === "string") {
          for (const ev of parseChunkFrame(f.data)) onEvent(ev);
        }
      },
      // no timeout: LLM turns are minutes-long; the 120s approval windows are engine-side
    })) as TaskResponse;
  }

  steer(note: string, transcriptKey: string): Promise<unknown> {
    return this.send("steer", { note, transcript_key: transcriptKey }, { timeoutMs: 10_000 });
  }

  cancel(transcriptKey: string): Promise<unknown> {
    return this.send("cancel", { transcript_key: transcriptKey }, { timeoutMs: 10_000 });
  }

  respondApproval(id: number, decision: Decision): Promise<{ delivered: boolean }> {
    return this.send("respond_to_approval", { id, decision }, { timeoutMs: 10_000 }) as Promise<{ delivered: boolean }>;
  }

  respondQuestion(id: number, answer: string): Promise<{ delivered: boolean }> {
    return this.send("respond_to_question", { id, answer }, { timeoutMs: 10_000 }) as Promise<{ delivered: boolean }>;
  }

  // ---- lifecycle -----------------------------------------------------------

  close(): void {
    this.closedByUs = true;
    this.drop("closed by client");
  }
}

/** Ping a daemon: handshake and return its protocol range. */
export async function pingKod(socketPath: string): Promise<{ ok: true } | { ok: false; error: string }> {
  try {
    await KodConnection.control(socketPath, (c) => c.hello(), 8_000);
    return { ok: true };
  } catch (e) {
    return { ok: false, error: e instanceof Error ? e.message : String(e) };
  }
}
```

**Gate 1.5 (socket smoke test, no Telegram yet)**

```bash
cd /srv/kod-agent/im/kod-im && cat > /tmp/smoke.ts <<'EOF'
import { KodConnection } from "/srv/kod-agent/im/kod-im/src/kod/client.ts";
const c = new KodConnection(process.env.XDG_RUNTIME_DIR + "/kod.sock");
await c.connect();
const r = await c.run("Reply with exactly: BRIDGE-SMOKE-OK", "smoke", (e) => {
  if (e.kind === "text") process.stdout.write("chunk:" + e.text + "\n");
});
console.log("DONE:", r.text, "ms:", r.execution_time_ms);
c.close();
EOF
sudo -u kod-agent bun run /tmp/smoke.ts
```

✅ Expect `chunk:…BRIDGE-SMOKE-OK…` and `DONE: BRIDGE-SMOKE-OK …`. If it fails: check `systemctl status kod-serve`, `$XDG_RUNTIME_DIR/kod.sock` exists, same UID (peer-UID check).

**Step 1.6 — Formatting: kod markdown → IM-safe output**

**File: `kod-im/src/format/imhtml.ts`**

```ts
/** Minimal, robust markdown → platform-neutral rich-text renderer.
 *  Output uses a tiny internal markup (<b> <i> <code> <pre> <a>), which the
 *  Telegram adapter maps 1:1 to parse_mode=HTML and other adapters map to
 *  their native formats. Never throws: worst case it returns escaped text. */

export interface RenderedSegment { html: string; }   // one IM message worth (≤ limit chars)

const LIMIT_DEFAULT = 3800; // Telegram hard limit is 4096; keep headroom for footers

function esc(s: string): string {
  return s.replace(/&/g, "&amp;").replace(/</g, "&lt;").replace(/>/g, "&gt;");
}

function renderInline(md: string): string {
  // Order matters: code spans first so other rules don't touch their contents.
  const parts: string[] = [];
  const codeRe = /`([^`\n]+)`/g;
  let last = 0, m: RegExpExecArray | null;
  while ((m = codeRe.exec(md)) !== null) {
    parts.push(renderInlinePlain(md.slice(last, m.index)));
    parts.push(`<code>${esc(m[1] ?? "")}</code>`);
    last = m.index + m[0].length;
  }
  parts.push(renderInlinePlain(md.slice(last)));
  return parts.join("");
}

function renderInlinePlain(s: string): string {
  let out = esc(s);
  out = out.replace(/\*\*([^*\n]+)\*\*/g, "<b>$1</b>");
  out = out.replace(/(^|\s)\*([^*\n]+)\*(?=\s|$|[.,;:!?)])/g, "$1<i>$2</i>");
  out = out.replace(/(^|\s)_([^_\n]+)_(?=\s|$|[.,;:!?)])/g, "$1<i>$2</i>");
  out = out.replace(/\[([^\]\n]+)\]\((https?:\/\/[^)\s]+)\)/g, '<a href="$2">$1</a>');
  out = out.replace(/^#{1,6}\s+(.+)$/gm, "<b>$1</b>");
  out = out.replace(/^\s*[-*]\s+/gm, "• ");
  return out;
}

/** Render markdown, then split into messages on the char limit, keeping
 *  code fences balanced across splits (close & reopen). */
export function renderForIm(md: string, limit = LIMIT_DEFAULT): RenderedSegment[] {
  const segments = md.split(/```/);
  const out: string[] = [];
  for (let i = 0; i < segments.length; i++) {
    const seg = segments[i] ?? "";
    if (i % 2 === 1) {
      // code block; first line may be a language tag
      const nl = seg.indexOf("\n");
      const lang = nl >= 0 ? seg.slice(0, nl).trim() : "";
      const body = nl >= 0 ? seg.slice(nl + 1) : "";
      out.push(lang ? `<pre><code class="language-${esc(lang)}">${esc(body)}</code></pre>` : `<pre>${esc(body)}</pre>`);
    } else {
      out.push(renderInline(seg));
    }
  }
  const joined = out.join("\n");
  if (joined.length <= limit) return [{ html: joined }];

  // Split on lines; if a <pre> is open at the cut, close it and reopen next message.
  const chunks: RenderedSegment[] = [];
  let cur = "", inPre = false;
  for (const line of joined.split("\n")) {
    if (line.includes("<pre>")) inPre = true;
    const candidate = cur === "" ? line : cur + "\n" + line;
    if (candidate.length > limit) {
      chunks.push({ html: inPre ? cur + "</code></pre>" : cur });
      cur = inPre ? "<pre>" + line : line;
    } else {
      cur = candidate;
    }
    if (line.includes("</pre>")) inPre = false;
  }
  if (cur.trim()) chunks.push({ html: cur });
  return chunks.length ? chunks : [{ html: "(empty)" }];
}

/** One-line HTML-escaped snippet (activity lines, errors). */
export function brief(s: string, max = 120): string {
  const one = s.replace(/\s+/g, " ").trim();
  return esc(one.length > max ? one.slice(0, max - 1) + "…" : one);
}
```

**Step 1.7 — Persisted chat bindings**

**File: `kod-im/src/state.ts`**

```ts
import { readFileSync, writeFileSync, renameSync, mkdirSync } from "node:fs";
import { dirname } from "node:path";

export interface ChatBinding {
  /** Active transcript key on the kod daemon, e.g. "tg:123456" or "tg:123456:deploy" */
  transcriptKey: string;
  /** Which daemon socket ("main" by default; /use switches) */
  socketName: string;
}

export class State {
  private data: Record<string, ChatBinding> = {};
  constructor(readonly path: string) {
    try {
      this.data = JSON.parse(readFileSync(path, "utf8")) as Record<string, ChatBinding>;
    } catch { this.data = {}; }
  }
  get(chatId: string): ChatBinding {
    return this.data[chatId] ?? { transcriptKey: `tg:${chatId}`, socketName: "main" };
  }
  set(chatId: string, binding: ChatBinding): void {
    if (binding.transcriptKey === `tg:${chatId}` && binding.socketName === "main") delete this.data[chatId];
    else this.data[chatId] = binding;
    this.save();
  }
  save(): void {
    try {
      mkdirSync(dirname(this.path), { recursive: true });
      const tmp = `${this.path}.tmp`;
      writeFileSync(tmp, JSON.stringify(this.data, null, 2));
      renameSync(tmp, this.path); // atomic
    } catch (e) {
      console.error("state save failed:", e);
    }
  }
}
```

**Step 1.8 — The bridge brain (per-chat run lifecycle, approvals, questions, steering)**

This is the heart. One `ChatSession` per allowed chat enforces: **one active run per chat**, approval/question state machines, streaming preview edits, and the IM command set. Platform adapters implement the `Outgoing` port below; the brain is platform-agnostic.

**File: `kod-im/src/outgoing.ts`**

```ts
export interface Btn { text: string; action: string; }  // action string → callback payload (adapter encodes)

/** Platform port: how the brain talks to an IM. Telegram implements this with
 *  grammY; Matrix with matrix-js-sdk; see Phase 3. */
export interface Outgoing {
  sendHtml(chatId: string, html: string, opts?: { replyTo?: string; buttons?: Btn[][] }): Promise<string | null>;
  editHtml(chatId: string, messageId: string, html: string, buttons?: Btn[][]): Promise<void>;
  sendMarkdown(chatId: string, md: string, opts?: { replyTo?: string; buttons?: Btn[][] }): Promise<string | null>;
  sendDocument(chatId: string, filename: string, content: Buffer, caption?: string): Promise<void>;
  resolveButtons(chatId: string, messageId: string, note: string): Promise<void>;
  react(chatId: string, messageId: string, emoji: string): Promise<void>;
}
```

**File: `kod-im/src/bridge.ts`**

```ts
import type { Config, SocketMap } from "./config.ts";
import { socketPathFor } from "./config.ts";
import { State, type ChatBinding } from "./state.ts";
import { KodConnection, pingKod } from "./kod/client.ts";
import type { ApprovalItem, Decision, TaskResponse } from "./kod/protocol.ts";
import type { KodEvent } from "./kod/markers.ts";
import { renderForIm, brief } from "./format/imhtml.ts";
import type { Outgoing, Btn } from "./outgoing.ts";

interface PendingApproval {
  messageId: string;
  items: Map<number, ApprovalItem>;
  expiresAt: number;
}
interface RunningRun {
  conn: KodConnection;
  startedAt: number;
  workingMsgId: string;
  previewBuf: string;
  activity: string[];
  lastPreviewAt: number;
  previewsUsed: number;
  steers: number;
  aborted: boolean;
}

const HELP = [
  "<b>kod-im</b> — your kod agent, in your pocket.",
  "",
  "Just type: a normal message becomes a task. While it runs, <b>any message steers</b> it.",
  "",
  "<b>/new</b> [name] — fresh session (cancels a running one)",
  "<b>/stop</b> — cancel the running task",
  "<b>/status</b> — transcript tail + run state",
  "<b>/models</b> — list models · <b>/model</b> &lt;id&gt; — switch (daemon-wide!)",
  "<b>/use</b> &lt;name&gt; — bind this chat to another kod daemon",
  "<b>/peek</b> &lt;key&gt; — read another session (admin)",
  "<b>/id</b> — print ids for the allowlist · <b>/help</b> — this",
].join("\n");

export class Bridge {
  private chats = new Map<string, ChatSession>();
  constructor(
    private cfg: Config,
    private out: Outgoing,
    private state: State,
  ) {}

  /** Late-bind the real output port (index.ts creates the adapter with the
   *  bridge reference first, then swaps in the platform `Outgoing`). */
  setOut(out: Outgoing): void {
    this.out = out;
    this.chats.clear(); // sessions built against the placeholder port must be rebuilt
  }

  sessionFor(chatId: string): ChatSession {
    let s = this.chats.get(chatId);
    if (!s) {
      s = new ChatSession(this.cfg, this.out, this.state, chatId);
      this.chats.set(chatId, s);
    }
    return s;
  }

  help(): string { return HELP; }

  async doctor(): Promise<string[]> {
    const lines: string[] = [];
    for (const [name] of Object.entries(this.cfg.sockets as SocketMap)) {
      const p = socketPathFor(this.cfg, name);
      const ping = await pingKod(p);
      lines.push(ping.ok ? `✅ daemon '${name}' (${p}) reachable` : `❌ daemon '${name}' (${p}): ${ping.error}`);
    }
    lines.push(`✅ allowlist: ${this.cfg.allowChats.size} chat(s), ${this.cfg.allowUsers.size} user(s)`);
    lines.push(`ℹ️ engine approval timeout: ${this.cfg.approvalTimeoutS}s (auto-deny)`);
    return lines;
  }
}

/** One IM chat = one serialized agent session. */
export class ChatSession {
  private binding: ChatBinding;
  private run: RunningRun | null = null;
  private pendingApproval: PendingApproval | null = null;
  private pendingQuestion: { id: number; askedAt: number } | null = null;
  private lastPromptAt = 0;

  constructor(
    private cfg: Config,
    private out: Outgoing,
    private state: State,
    private chatId: string,
  ) {
    this.binding = state.get(chatId);
  }

  get socketPath(): string { return socketPathFor(this.cfg, this.binding.socketName); }
  get key(): string { return this.binding.transcriptKey; }
  get isRunning(): boolean { return this.run !== null; }

  // ---------------- inbound text -------------------------------------------

  async onText(text: string, opts: { replyTo?: string } = {}): Promise<void> {
    if (text.length > this.cfg.maxInputChars) {
      await this.out.sendHtml(this.chatId, `⚠️ message too long (${text.length} &gt; ${this.cfg.maxInputChars} chars). Split it or use a file.`);
      return;
    }
    if (text.startsWith("/")) return this.command(text);

    // 1) answer a pending question first
    if (this.pendingQuestion) {
      const q = this.pendingQuestion; this.pendingQuestion = null;
      try {
        const r = await this.run?.conn.respondQuestion(q.id, text);
        if (r?.delivered === false) await this.out.sendHtml(this.chatId, "⏱ too late — that question already timed out.");
        else if (opts.replyTo) await this.out.react(this.chatId, opts.replyTo, "👍");
      } catch {
        await this.out.sendHtml(this.chatId, "⚠️ could not deliver the answer (run ended?).");
      }
      return;
    }

    // 2) running → steer
    if (this.run) {
      try {
        await this.run.conn.steer(text, this.key);
        this.run.steers++;
        await this.out.sendHtml(this.chatId, `🪝 steered into the running task: <i>${brief(text, 80)}</i>`);
      } catch {
        await this.out.sendHtml(this.chatId, "⚠️ steer failed (daemon connection lost). Use /stop then retry.");
      }
      return;
    }

    // 3) simple token bucket: ≥ 2s between prompts (bigger limits live in kod [limits])
    const now = Date.now();
    if (now - this.lastPromptAt < 2000) {
      await this.out.sendHtml(this.chatId, "⏳ slow down a moment…");
      return;
    }
    this.lastPromptAt = now;

    // 4) start a run
    await this.startRun(text);
  }

  // ---------------- the run loop -------------------------------------------

  private async startRun(input: string): Promise<void> {
    const conn = new KodConnection(this.socketPath);
    try { await conn.connect(); } catch (e) {
      await this.out.sendHtml(this.chatId, `🛑 <b>kod daemon unreachable</b> (${this.binding.socketName}).<br>${brief(e instanceof Error ? e.message : String(e), 200)}`);
      return;
    }

    const working = await this.out.sendHtml(this.chatId, "🫀 working…", { buttons: [[{ text: "🛑 Abort", action: "abort" }]] });
    const r: RunningRun = {
      conn, startedAt: Date.now(), workingMsgId: working ?? "",
      previewBuf: "", activity: [], lastPreviewAt: 0, previewsUsed: 0, steers: 0, aborted: false,
    };
    this.run = r;
    conn.onDown = () => { void this.finishRun(r, null, "⚠️ daemon connection dropped mid-run."); };

    try {
      const done = await conn.run(input, this.key, (ev) => void this.onEvent(r, ev));
      await this.finishRun(r, done, null);
    } catch (e) {
      await this.finishRun(r, null, `⚠️ <b>run failed</b>: ${brief(e instanceof Error ? e.message : String(e), 300)}`);
    }
  }

  private async onEvent(r: RunningRun, ev: KodEvent): Promise<void> {
    switch (ev.kind) {
      case "text":
        r.previewBuf += ev.text;
        break;
      case "tool-start": {
        const a = r.activity;
        a.push(`⚙️ <code>${brief(ev.name, 40)}</code>`);
        if (a.length > 8) a.shift();
        break;
      }
      case "tool-args": {
        const last = r.activity[r.activity.length - 1];
        if (last) r.activity[r.activity.length - 1] = `${last} <code>${brief(ev.display, 60)}</code>`;
        break;
      }
      case "tool-done": {
        const last = r.activity[r.activity.length - 1];
        if (last) r.activity[r.activity.length - 1] = `${last} ✅ ${ev.durationMs}ms`;
        break;
      }
      case "approval-batch":
        await this.onApprovalBatch(r, ev.items);
        break;
      case "approval-legacy":
        await this.onApprovalBatch(r, [{ ...ev.item, id: ev.id }]);
        break;
      case "question":
        await this.onQuestion(ev.id, ev.request.question, ev.request.placeholder);
        break;
      case "rate-limit":
      case "server-busy": {
        const waitS = ev.kind === "rate-limit" ? ev.secs : ev.secs;
        r.activity.push(`⏳ ${ev.kind === "rate-limit" ? "rate limited" : "server busy"} — waiting ${waitS}s (${ev.attempt}/${ev.max})`);
        break;
      }
      case "stream-reset":
        r.previewBuf = "";
        break;
      case "thinking":
        r.activity.push("🤔 thinking");
        if (r.activity.length > 8) r.activity.shift();
        break;
      case "usage":
      case "turn":
      case "activity":
        break; // noise for the IM channel; captured in final footer instead
    }
    await this.maybePreview(r);
  }

  private async maybePreview(r: RunningRun): Promise<void> {
    if (!r.workingMsgId || r.previewsUsed >= this.cfg.maxPreviews) return;
    const now = Date.now();
    if (now - r.lastPreviewAt < this.cfg.previewIntervalMs) return;
    r.lastPreviewAt = now;
    r.previewsUsed++;
    const tail = r.previewBuf.length > 900 ? "…" + r.previewBuf.slice(-900) : r.previewBuf;
    const html = [
      `🫀 <b>running</b> · ${Math.round((now - r.startedAt) / 1000)}s${r.steers ? ` · 🪝×${r.steers}` : ""}`,
      r.activity.length ? r.activity.map((a) => `\n<code>${a}</code>`).join("") : "",
      tail ? `\n━━━━━━━━━━\n${renderForIm(tail, 1200)[0]?.html ?? ""} <i>(streaming…)</i>` : "",
    ].join("\n");
    try { await this.out.editHtml(this.chatId, r.workingMsgId, html || "🫀 working…", [[{ text: "🛑 Abort", action: "abort" }]]); }
    catch { /* Telegram edit rate limits are expected; next tick retries */ }
  }

  // ---------------- approvals & questions -----------------------------------

  private async onApprovalBatch(r: RunningRun, items: ApprovalItem[]): Promise<void> {
    const usable = items.filter((it) => typeof it.id === "number");
    if (usable.length === 0) return; // fail-closed: engine times out to deny
    const expiresAt = Date.now() + this.cfg.approvalTimeoutS * 1000;
    const buttons: Btn[][] = usable.map((it) => ([
      { text: `✅ ${brief(it.summary, 40)}`, action: `a:${it.id}:y` },
      { text: "❌ Deny", action: `a:${it.id}:n` },
      { text: "🚫 Deny always", action: `a:${it.id}:d` },
    ]));

    let body = `🔐 <b>Approval needed</b> <i>(auto-deny in ${this.cfg.approvalTimeoutS}s)</i>\n`;
    for (const it of usable) {
      body += `\n<code>${brief(it.tool_name, 40)}</code> — <b>${brief(it.summary, 100)}</b>\n`;
      if (it.diff) {
        const dl = it.diff.length;
        if (dl <= 2400) body += `<pre>${escapeHtml(it.diff)}</pre>`;
        else {
          body += `<i>diff too large (${dl} chars) — sent as a file</i>\n`;
          await this.out.sendDocument(this.chatId, `approval-${it.id}.diff`, Buffer.from(it.diff, "utf8"), `approval #${it.id} diff`);
        }
      }
    }
    const mid = await this.out.sendHtml(this.chatId, body, { buttons });
    this.pendingApproval = {
      messageId: mid ?? "",
      items: new Map(usable.map((it) => [it.id as number, it])),
      expiresAt,
    };
    void r; // approval is tied to the chat, not the preview message
  }

  async onAction(action: string): Promise<void> {
    if (action === "abort") {
      if (this.run) {
        this.run.aborted = true;
        try { await this.run.conn.cancel(this.key); } catch { /* connection down already reported */ }
        await this.out.sendHtml(this.chatId, "🛑 cancel requested…");
      } else {
        await this.out.sendHtml(this.chatId, "nothing running");
      }
      return;
    }
    const m = /^a:(\d+):([ynd])$/.exec(action);
    if (!m) return;
    const id = Number(m[1]);
    const map: Record<string, Decision> = { y: "approve", n: "deny", d: "deny_always" };
    const decision = map[m[2] ?? "n"] ?? "deny";

    // Answer even if bridge state was lost: pending approvals live engine-side,
    // and respond_to_approval is accepted on any connection.
    let delivered: boolean | null = null;
    let tmp: KodConnection | null = null;
    try {
      let conn: KodConnection | null = this.run?.conn ?? null;
      if (!conn) {
        tmp = new KodConnection(this.socketPath);
        await tmp.connect();
        conn = tmp;
      }
      const res = await conn.respondApproval(id, decision);
      delivered = res.delivered;
    } catch (e) {
      await this.out.sendHtml(this.chatId, `⚠️ approval answer failed: ${brief(e instanceof Error ? e.message : String(e), 150)}`);
      return;
    } finally {
      tmp?.close();
    }

    if (this.pendingApproval) {
      this.pendingApproval.items.delete(id);
      if (this.pendingApproval.items.size === 0) {
        await this.out.resolveButtons(this.chatId, this.pendingApproval.messageId, "resolved");
        this.pendingApproval = null;
      }
    }
    if (delivered === false) {
      await this.out.sendHtml(this.chatId, "⏱ too late — the engine already auto-denied this approval (120s window).");
    } else {
      await this.out.sendHtml(this.chatId, decision === "approve" ? `👍 approved <code>#${id}</code>` : `👎 ${decision === "deny_always" ? "denied always" : "denied"} <code>#${id}</code>`);
    }
  }

  private async onQuestion(id: number, question: string, placeholder?: string): Promise<void> {
    this.pendingQuestion = { id, askedAt: Date.now() };
    const ph = placeholder ? `\n<i>hint: ${brief(placeholder, 120)}</i>` : "";
    await this.out.sendHtml(
      this.chatId,
      `❓ <b>${brief(question, 800)}</b>${ph}\n<i>reply within 120s to answer (your next message is the answer; /skip to ignore)</i>`,
    );
  }

  async skipQuestion(): Promise<void> {
    if (!this.pendingQuestion) { await this.out.sendHtml(this.chatId, "no pending question"); return; }
    this.pendingQuestion = null;
    await this.out.sendHtml(this.chatId, "⏭ question ignored — the engine will time it out to its default.");
  }

  // ---------------- commands -------------------------------------------------

  private async command(text: string): Promise<void> {
    const [cmd, ...rest] = text.split(/\s+/);
    const arg = rest.join(" ").trim();
    switch (cmd) {
      case "/start": case "/help":
        await this.out.sendHtml(this.chatId, HELP);
        break;
      case "/new": {
        if (this.run) { try { await this.run.conn.cancel(this.key); } catch { /* ignore */ } this.run = null; }
        const name = arg.replace(/[^\w-]/g, "");
        this.binding = { transcriptKey: name ? `${this.defaultKey()}:${name}` : this.defaultKey(), socketName: this.binding.socketName };
        this.state.set(this.chatId, this.binding);
        await this.out.sendHtml(this.chatId, `🆕 fresh session bound to <code>${escapeHtml(this.binding.transcriptKey)}</code>`);
        break;
      }
      case "/stop":
        if (this.run) { try { await this.run.conn.cancel(this.key); } catch {} await this.out.sendHtml(this.chatId, "🛑 cancel requested…"); }
        else await this.out.sendHtml(this.chatId, "nothing running");
        break;
      case "/status": {
        const runLine = this.run ? `🏃 running ${Math.round((Date.now() - this.run.startedAt) / 1000)}s` : "💤 idle";
        try {
          const p = await KodConnection.control(this.socketPath, (c) => c.peek(this.key, 1500), 15_000);
          await this.out.sendMarkdown(this.chatId, `**${runLine}** · session \`${p.key}\` · ${p.messages} msgs\n\n${p.tail}`);
        } catch (e) {
          await this.out.sendHtml(this.chatId, `${runLine} · <i>peek failed: ${brief(e instanceof Error ? e.message : String(e), 120)}</i>`);
        }
        break;
      }
      case "/models": {
        try {
          const { models } = await KodConnection.control(this.socketPath, (c) => c.listModels(), 20_000);
          await this.out.sendMarkdown(this.chatId, models.map((m) => `• \`${m}\``).join("\n") || "(none)");
        } catch (e) { await this.out.sendHtml(this.chatId, `⚠️ ${brief(e instanceof Error ? e.message : String(e), 150)}`); }
        break;
      }
      case "/model": {
        if (!arg) { await this.out.sendHtml(this.chatId, "usage: /model &lt;model-id&gt; <i>(daemon-wide!)</i>"); break; }
        try {
          await KodConnection.control(this.socketPath, (c) => c.setModel("default", arg), 20_000);
          await this.out.sendHtml(this.chatId, `🔁 model switched to <code>${escapeHtml(arg)}</code> <i>(affects every session on this daemon)</i>`);
        } catch (e) { await this.out.sendHtml(this.chatId, `⚠️ ${brief(e instanceof Error ? e.message : String(e), 150)}`); }
        break;
      }
      case "/use": {
        try {
          socketPathFor(this.cfg, arg); // validate
          this.binding = { ...this.binding, socketName: arg };
          this.state.set(this.chatId, this.binding);
          await this.out.sendHtml(this.chatId, `🔌 this chat now talks to daemon <code>${escapeHtml(arg)}</code>`);
        } catch (e) { await this.out.sendHtml(this.chatId, `⚠️ ${brief(e instanceof Error ? e.message : String(e), 150)}`); }
        break;
      }
      case "/peek": {
        if (!arg) { await this.out.sendHtml(this.chatId, "usage: /peek &lt;transcript-key&gt;"); break; }
        try {
          const p = await KodConnection.control(this.socketPath, (c) => c.peek(arg, 3000), 15_000);
          await this.out.sendMarkdown(this.chatId, `**${p.key}** · ${p.messages} msgs${p.truncated ? " (truncated)" : ""}\n\n${p.tail}`);
        } catch (e) { await this.out.sendHtml(this.chatId, `⚠️ ${brief(e instanceof Error ? e.message : String(e), 150)}`); }
        break;
      }
      case "/skip":
        await this.skipQuestion();
        break;
      case "/id": {
        await this.out.sendHtml(this.chatId, `chat: <code>${escapeHtml(this.chatId)}</code>`);
        break;
      }
      default:
        await this.out.sendHtml(this.chatId, "unknown command — /help");
    }
  }

  private defaultKey(): string { return `tg:${this.chatId}`; }

  // ---------------- teardown --------------------------------------------------

  private async finishRun(r: RunningRun, done: TaskResponse | null, errorHtml: string | null): Promise<void> {
    if (this.run === r) this.run = null;
    try { r.conn.close(); } catch { /* ignore */ }

    const secs = Math.round((Date.now() - r.startedAt) / 1000);
    const footerBits: string[] = [`${secs}s`];
    if (done?.usage?.total_tokens) footerBits.push(`${done.usage.prompt_tokens ?? "?"}→${done.usage.completion_tokens ?? "?"} tok`);
    if (r.steers) footerBits.push(`🪝×${r.steers}`);
    if (r.aborted) footerBits.push("🛑");
    const footer = `<i>${footerBits.join(" · ")}</i>`;

    if (errorHtml) {
      if (r.workingMsgId) { try { await this.out.editHtml(this.chatId, r.workingMsgId, errorHtml); } catch {} }
      else await this.out.sendHtml(this.chatId, errorHtml);
      return;
    }
    const text = done?.text ?? "(empty reply)";
    const parts = renderForIm(text + "\n\n" + footer);
    for (const p of parts) await this.out.sendHtml(this.chatId, p.html);
    if (r.workingMsgId) { try { await this.out.editHtml(this.chatId, r.workingMsgId, "✅ done — answer below"); } catch {} }
  }
}

/** Minimal connection for answering approvals when no run is attached.
 *  (Not needed as a class anymore — onAction opens a plain KodConnection —
 *  kept as a placeholder for future control-plane methods.) */

function escapeHtml(s: string): string {
  return s.replace(/&/g, "&amp;").replace(/</g, "&lt;").replace(/>/g, "&gt;");
}
```

> **Design note (why approvals work from `this.run.conn` or a fresh connection):** kod registers pending approvals in an *engine-global* map, and `respond_to_approval` is accepted on any connection. The bridge answers on the run's connection when it has one (cheapest), else opens a transient connection. Both paths are code-verified against `serve.rs`.

**Gate 1.8** ✅ `bun run check` passes with these files in place (wiring comes next).

---

## 5. PHASE 1 (cont.) — Telegram adapter, entrypoint, deployment

**Step 1.9 — Telegram adapter (grammY)**

**File: `kod-im/src/adapters/telegram.ts`**

```ts
import { Bot, InlineKeyboard, InputFile, type Api, type Context } from "grammy";
import type { Config } from "../config.ts";
import type { Bridge } from "../bridge.ts";
import type { Btn, Outgoing } from "../outgoing.ts";
import { renderForIm } from "../format/imhtml.ts";

const TG_LIMIT = 4096;

/** Split HTML on top-level newlines while keeping <pre>/<code>/<b>/<i> balanced. */
function splitHtml(html: string, limit = TG_LIMIT - 100): string[] {
  if (html.length <= limit) return [html];
  const parts: string[] = [];
  let cur = "";
  const opens: string[] = [];
  for (const line of html.split("\n")) {
    const candidate = cur ? cur + "\n" + line : line;
    if (candidate.length > limit && cur) {
      let closeTags = "";
      for (const t of opens.reverse()) closeTags += `</${t}>`;
      parts.push(cur + closeTags);
      cur = opens.map((t) => `<${t}>`).join("") + line;
    } else {
      cur = candidate;
    }
    for (const m of line.matchAll(/<(\/?)(pre|code|b|i|a)[^>]*>/g)) {
      if (m[1] === "/") opens.pop();
      else if (m[2] !== "a") opens.push(m[2] as string); // don't reopen links across splits
    }
  }
  if (cur) parts.push(cur);
  return parts;
}

interface TelegramOut extends Outgoing {
  start(): Promise<void>;
  stop(): Promise<void>;
}

export function createTelegram(cfg: Config, bridge: Bridge): TelegramOut {
  const bot = new Bot(cfg.botToken);

  // ---- auth middleware: fail-closed ----------------------------------------
  bot.use(async (ctx, next) => {
    const chatId = ctx.chat?.id !== undefined ? String(ctx.chat.id) : "";
    const userId = ctx.from?.id !== undefined ? String(ctx.from.id) : "";
    const chatOk = cfg.allowChats.has(chatId);
    const userOk = cfg.allowUsers.has(userId);
    if (!chatOk && !userOk) {
      // silent: unknown senders get nothing at all (not even an error to probe with)
      return;
    }
    await next();
  });

  // ---- inbound wiring -------------------------------------------------------
  bot.on("message:text", async (ctx) => {
    const chatId = String(ctx.chat.id);
    const text = ctx.message.text;
    try {
      await bridge.sessionFor(chatId).onText(text, { replyTo: String(ctx.message.message_id) });
    } catch (e) {
      await safeSend(ctx.api, chatId, `⚠️ bridge error: ${escBrief(e instanceof Error ? e.message : String(e))}`);
    }
  });

  bot.on("callback_query:data", async (ctx) => {
    const q = ctx.callbackQuery;
    const chatId =
      q.message && "chat" in q.message ? String(q.message.chat.id)
      : ctx.chat ? String(ctx.chat.id) : "";
    const action = q.data;
    await ctx.answerCallbackQuery().catch(() => {});
    if (!chatId || !action) return;
    try {
      await bridge.sessionFor(chatId).onAction(action);
    } catch (e) {
      await safeSend(ctx.api, chatId, `⚠️ action failed: ${escBrief(e instanceof Error ? e.message : String(e))}`);
    }
  });

  bot.catch((err) => {
    console.error("[telegram] bot error:", err.error);
  });

  // ---- outgoing port --------------------------------------------------------
  const sendHtmlOnce = async (chatId: string, html: string, buttons?: Btn[][], replyTo?: string): Promise<string | null> => {
    const kb = toKeyboard(buttons);
    try {
      const m = await bot.api.sendMessage(chatId, html, {
        parse_mode: "HTML",
        reply_markup: kb,
        reply_parameters: replyTo ? { message_id: Number(replyTo), allow_sending_without_reply: true } : undefined,
        link_preview_options: { is_disabled: true },
      });
      return String(m.message_id);
    } catch (e) {
      // Malformed-HTML fallback: resend as plain text rather than dropping the message.
      const msg = e instanceof Error ? e.message : "";
      if (/parse entities|can't parse/i.test(msg)) {
        const m = await bot.api.sendMessage(chatId, html.replace(/<[^>]+>/g, ""), { reply_markup: kb });
        return String(m.message_id);
      }
      throw e;
    }
  };

  const out: TelegramOut = {
    async sendHtml(chatId, html, opts) {
      for (const part of splitHtml(html)) await sendHtmlOnce(chatId, part, opts?.buttons, opts?.replyTo);
      return null;
    },
    async editHtml(chatId, messageId, html, buttons) {
      try {
        await bot.api.editMessageText(chatId, Number(messageId), html, {
          parse_mode: "HTML",
          reply_markup: toKeyboard(buttons),
          link_preview_options: { is_disabled: true },
        });
      } catch (e) {
        const msg = e instanceof Error ? e.message : "";
        if (/message is not modified/i.test(msg)) return;
        if (/parse entities|can't parse/i.test(msg)) {
          await bot.api.editMessageText(chatId, Number(messageId), html.replace(/<[^>]+>/g, ""), { reply_markup: toKeyboard(buttons) });
          return;
        }
        if (/message to edit not found|message can'?t be edited/i.test(msg)) {
          await sendHtmlOnce(chatId, html, buttons);
          return;
        }
        throw e;
      }
    },
    async sendMarkdown(chatId, md, opts) {
      for (const seg of renderForIm(md)) await sendHtmlOnce(chatId, seg.html, opts?.buttons, opts?.replyTo);
      return null;
    },
    async sendDocument(chatId, filename, content, caption) {
      await bot.api.sendDocument(chatId, new InputFile(content, filename), { caption });
    },
    async resolveButtons(chatId, messageId, note) {
      try {
        await bot.api.editMessageReplyMarkup(chatId, Number(messageId), { reply_markup: { inline_keyboard: [] } });
        if (note) await bot.api.sendMessage(chatId, `<i>${escBrief(note)}</i>`, { parse_mode: "HTML" });
      } catch { /* the message may already be gone — fine */ }
    },
    async react(chatId, messageId, emoji) {
      if (!messageId) return;
      const map: Record<string, string> = { "✅": "👍", "🚫": "👎", "⏱": "🫡", "🎉": "🎉", "👍": "👍", "👎": "👎" };
      const tg = map[emoji] ?? "👍";
      const reaction = { type: "emoji", emoji: tg } as unknown as Parameters<typeof bot.api.setMessageReaction>[2][number];
      try { await bot.api.setMessageReaction(chatId, Number(messageId), [reaction]); }
      catch { /* reactions are best-effort; some chats disallow them */ }
    },
    start: () => bot.start({ onStart: () => console.log("[telegram] long polling active") }),
    stop: () => bot.stop(),
  };
  return out;
}

function toKeyboard(buttons?: Btn[][]): { inline_keyboard: { text: string; callback_data: string }[][] } | undefined {
  if (!buttons || buttons.length === 0) return undefined;
  return {
    inline_keyboard: buttons.map((row) =>
      row.map((b) => ({ text: b.text, callback_data: b.action.slice(0, 64) })),
    ),
  };
}

function escBrief(s: string): string {
  return s.replace(/&/g, "&amp;").replace(/</g, "&lt;").replace(/>/g, "&gt;").slice(0, 300);
}

/** Best-effort error report: HTML first, plain-text fallback. */
async function safeSend(api: Api, chatId: string, html: string): Promise<void> {
  try {
    await api.sendMessage(chatId, html, { parse_mode: "HTML" });
  } catch {
    try { await api.sendMessage(chatId, html.replace(/<[^>]+>/g, "")); } catch { /* channel is in a bad state; give up quietly */ }
  }
}

// grammY re-exports InputFile (imported above); buffers become files via new InputFile(buf, name).
```

> **Note on `callback_data` budget**: Telegram caps it at 64 bytes. Our actions (`abort`, `a:<numeric-id>:<y|n|d>`) are far below that. If you ever add richer actions, keep them under 64 bytes or switch to callback sessions stored in `state.json`.

**Step 1.10 — Entrypoint + doctor**

**File: `kod-im/src/index.ts`**

```ts
import { loadConfig } from "./config.ts";
import { State } from "./state.ts";
import { Bridge } from "./bridge.ts";
import { createTelegram } from "./adapters/telegram.ts";
import { dummyOut } from "./outgoing.noop.ts";

async function main(): Promise<void> {
  const cfg = loadConfig();
  const state = new State(cfg.statePath);

  if (process.argv.includes("--doctor")) {
    const lines = await new Bridge(cfg, dummyOut(), state).doctor();
    try {
      const r = await fetch(`https://api.telegram.org/bot${cfg.botToken}/getMe`);
      const j = await r.json() as { ok: boolean; result?: { username?: string }; description?: string };
      lines.push(j.ok ? `✅ bot token valid (@${j.result?.username})` : `❌ bot token invalid: ${j.description ?? "unknown"}`);
    } catch (e) {
      lines.push(`❌ telegram unreachable: ${e instanceof Error ? e.message : String(e)}`);
    }
    console.log(lines.join("\n"));
    process.exit(0);
  }

  // Phase-1 wiring: the adapter needs a bridge reference for its handlers, the
  // bridge needs the adapter as its output port — late-bind to break the cycle.
  const bridge = new Bridge(cfg, dummyOut(), state);
  const out = createTelegram(cfg, bridge);
  bridge.setOut(out);
  void bridge.help; // (all command routing flows through sessionFor().onText)

  for (const sig of ["SIGINT", "SIGTERM"] as const) {
    process.on(sig, async () => {
      console.log(`[kod-im] ${sig} — shutting down`);
      try { await out.stop(); } catch { /* ignore */ }
      process.exit(0);
    });
  }

  console.log(`[kod-im] starting: ${cfg.allowChats.size} chat(s) allowed, state at ${cfg.statePath}`);
  await out.start();
}

main().catch((e) => {
  console.error("[kod-im] fatal:", e);
  process.exit(1);
});
```

**File: `kod-im/src/outgoing.noop.ts`**

```ts
import type { Btn, Outgoing } from "./outgoing.ts";

/** No-op Outgoing for --doctor and tests. */
export function dummyOut(): Outgoing {
  const nop = async () => null;
  return {
    sendHtml: nop,
    editHtml: async () => {},
    sendMarkdown: nop,
    sendDocument: async () => {},
    resolveButtons: async () => {},
    react: async () => {},
  } satisfies Outgoing as Outgoing;
}
export type { Btn };
```

**Step 1.11 — First end-to-end run (manual)**

```bash
cd /srv/kod-agent/im/kod-im
sudo -u kod-agent bash -c 'set -a; source /srv/kod-agent/config/kod-im.env; set +a; bun run doctor'
#   ✅ daemon 'main' (...) reachable
#   ✅ allowlist: 1 chat(s), 0 user(s)
#   ✅ bot token valid (@your_bot)

sudo -u kod-agent bash -c 'set -a; source /srv/kod-agent/config/kod-im.env; set +a; bun run start'
```

Then, **from your phone**: send `hello, who are you?` → expect `🫀 working…` → streaming preview edits → final answer. Send `/help`, `/status`, `/new work`, a task like `list the files in this repo and tell me what it is`, and — with `preset = "standard"` — watch an **approval card** appear with ✅/❌/🚫 buttons when the agent wants to run a shell command.

**Gate 1.11** ✅ All of the above work from the phone. Congrats — you have a pocket coding agent.

**Step 1.12 — Bridge systemd unit + tests**

**File: `kod-im/deploy/kod-im.service`** (install to `/etc/systemd/system/kod-im.service`)

```ini
[Unit]
Description=kod-im bridge (IM <-> kod serve)
After=kod-serve.service
Requires=kod-serve.service

[Service]
Type=simple
User=kod-agent
Group=kod-agent
WorkingDirectory=/srv/kod-agent/im/kod-im
EnvironmentFile=/srv/kod-agent/config/kod-im.env
ExecStart=/root/.bun/bin/bun run src/index.ts
Restart=always
RestartSec=2
# Hardening
NoNewPrivileges=true
PrivateTmp=true
ProtectSystem=strict
ProtectHome=tmpfs
ReadWritePaths=/srv/kod-agent
CapabilityBoundingSet=
MemoryDenyWriteExecute=true

[Install]
WantedBy=multi-user.target
```

> ⚠ Fix `ExecStart` to the *actual* bun path (`which bun` as `kod-agent`, e.g. `/srv/kod-agent/.bun/bin/bun` — install Bun for that user, not root). Also add `Environment=XDG_RUNTIME_DIR=/run/user/<uid>` if the daemon uses the default socket path and this unit doesn't inherit it (system *units* have no user-manager env). Simplest: set `KOD_IM_SOCKETS_JSON={"main":"/run/user/<uid>/kod.sock"}` in the env file.

**File: `kod-im/test/markers.test.ts`**

```ts
import { describe, it, expect } from "bun:test";
import { parseChunkFrame } from "../src/kod/markers.ts";

describe("parseChunkFrame", () => {
  it("passes plain text through", () => {
    expect(parseChunkFrame("hello world")).toEqual([{ kind: "text", text: "hello world" }]);
  });

  it("decodes tool lifecycle markers mixed with text", () => {
    // NB: use \u0000 (not \0) in string literals — "\0" followed by a digit
    // would be parsed as a legacy octal escape (e.g. "\0123" -> "\n3").
    const ev = parseChunkFrame("let me check\u0000kod-tool:c1:execute_command\u0000mid text\u0000kod-args:c1:cargo test --quiet\u0000\u0000kod-done:c1\u0000shell\u0000ok\u0000123");
    expect(ev.some((e) => e.kind === "tool-start" && e.name === "execute_command")).toBe(true);
    expect(ev.some((e) => e.kind === "tool-args" && e.display === "cargo test --quiet")).toBe(true);
    const done = ev.find((e) => e.kind === "tool-done") as { cid: string; summary: string; durationMs: number };
    expect(done.cid).toBe("c1");
    expect(done.summary).toBe("ok");
    expect(done.durationMs).toBeGreaterThan(0);
    expect(ev.some((e) => e.kind === "text" && e.text.includes("let me check"))).toBe(true);
  });

  it("decodes an approval batch with items", () => {
    const json = JSON.stringify({ items: [{ id: 7, tool_name: "write_file", arguments: { path: "x.rs" }, diff: null, summary: "write x.rs" }] });
    const ev = parseChunkFrame(`\0kod-approval-batch:42:${json}\0`);
    const b = ev.find((e) => e.kind === "approval-batch") as { batchId: string; items: { id: number }[] };
    expect(b.batchId).toBe("42");
    expect(b.items[0]?.id).toBe(7);
  });

  it("decodes questions", () => {
    const ev = parseChunkFrame(`\0kod-question:9:${JSON.stringify({ question: "Which database?", placeholder: "postgres" })}\0`);
    const q = ev.find((e) => e.kind === "question") as { id: number; request: { question: string } };
    expect(q.id).toBe(9);
    expect(q.request.question).toBe("Which database?");
  });

  it("decodes usage / rate-limit / busy / reset markers", () => {
    const ev = parseChunkFrame("\0kod-usage:120,40\0\0kod-rate-limit:30\0attempt1\0max2\0\0kod-stream-reset\0");
    expect(ev.some((e) => e.kind === "usage" && e.prompt === 120 && e.completion === 40)).toBe(true);
    expect(ev.some((e) => e.kind === "rate-limit" && e.secs === 30)).toBe(true);
    expect(ev.some((e) => e.kind === "stream-reset")).toBe(true);
  });

  it("ignores unknown kod-* markers (forward compatibility)", () => {
    const ev = parseChunkFrame("hi\0kod-future-thing:x:y\0there");
    expect(ev.some((e) => e.kind === "text" && e.text.includes("hi"))).toBe(true);
    expect(ev.some((e) => e.kind === "text" && e.text.includes("there"))).toBe(true);
  });

  it("never crashes on malformed JSON payloads", () => {
    const ev = parseChunkFrame("\0kod-approval-batch:1:{not-json\0");
    expect(ev.find((e) => e.kind === "approval-batch")).toBeUndefined();
  });
});
```

**File: `kod-im/test/format.test.ts`**

```ts
import { describe, it, expect } from "bun:test";
import { renderForIm, brief } from "../src/format/imhtml.ts";

describe("renderForIm", () => {
  it("escapes HTML", () => {
    const [seg] = renderForIm("a <b> & c");
    expect(seg!.html).toContain("&lt;b&gt;");
  });

  it("renders fences as <pre> with language", () => {
    const [seg] = renderForIm("text\n```rust\nfn main() {}\n```\nmore");
    expect(seg!.html).toContain('<pre><code class="language-rust">');
    expect(seg!.html).toContain("fn main() {}");
  });

  it("splits long output and keeps fences balanced", () => {
    const md = "intro\n```py\n" + "x = 1\n".repeat(300) + "```\noutro";
    const parts = renderForIm(md, 1200);
    expect(parts.length).toBeGreaterThan(1);
    const all = parts.map((p) => p.html).join("\n");
    expect((all.match(/<pre>/g) ?? []).length).toBe((all.match(/<\/pre>/g) ?? []).length);
  });

  it("brief() collapses whitespace and escapes", () => {
    expect(brief("a\n b   c <x>", 50)).toBe("a b c &lt;x&gt;");
  });
});
```

**File: `kod-im/scripts/socket-smoke.ts`**

```ts
// Direct protocol smoke test (no Telegram): bun run scripts/socket-smoke.ts [message]
import { KodConnection } from "../src/kod/client.ts";

const path = process.env.KOD_SOCKET
  ?? (process.env.XDG_RUNTIME_DIR ? `${process.env.XDG_RUNTIME_DIR}/kod.sock` : `${process.env.HOME}/.kod/run/kod.sock`);
const msg = process.argv[2] ?? "Reply with exactly: BRIDGE-SMOKE-OK";

const c = new KodConnection(path);
await c.connect();
console.log("connected:", path);
const r = await c.run(msg, "smoke", (e) => {
  if (e.kind === "text") process.stdout.write(`[chunk] ${e.text}`);
  else if (e.kind !== "usage") console.log(`[event]`, e.kind);
});
console.log("\n[done]", JSON.stringify({ text: r.text, ms: r.execution_time_ms, usage: r.usage }));
c.close();
process.exit(0);
```

**Gate 1.12** ✅ `bun test` → all green; `sudo systemctl enable --now kod-im` → `journalctl -u kod-im -f` shows `long polling active`; sending a message from the phone still works (now supervised).

---

## 6. PHASE 2 — Verify the killer UX (approvals · questions · steering · cancel)

The interactive loop is already implemented by Phase 1 code (`bridge.ts` + `telegram.ts`). This phase is about **proving it** and adding the last polish.

**Step 2.1 — Interactive verification script (do all five from your phone)**

| # | Action | Expected behavior |
|---|--------|-------------------|
| 1 | `list files in this repo` | Working message + tool activity lines (`⚙️ list_files …✅ Nms`) + final answer |
| 2 | `write a file /srv/kod-agent/workspace/tmp_test.txt containing "hi"` (policy: write = Ask) | **Approval card** with the item summary + optional diff; tap ✅ → `👍 approved #n`, file written; tap ❌ on a later one → agent reports denial and adapts; do nothing on a third → after 120 s the buttons die and `⏱ too late — auto-denied` appears |
| 3 | Start a long task (e.g. `write a comprehensive README for this repo`), then type `keep it under 50 lines, use badges` mid-run | `🪝 steered into the running task` and the agent adjusts without restarting; final footer shows `🪝×1` |
| 4 | Start any long task → tap **🛑 Abort** on the working message | `cancel requested…`, run stops quickly, partial work is left intact on disk (kod checkpoints), footer shows `🛑` |
| 5 | Ask something ambiguous so the model calls `ask_user` (e.g. `refactor this project — ask me anything you need first`) | ❓ question card; your next plain message is delivered as the answer (`respond_to_question`); `/skip` ignores it |

**Step 2.2 — Optional polish (recommended, ~30 lines)**

Approval countdown refresh — edit the approval card once at ~60 s so the 120 s window is visible while you're away. Add to `ChatSession` in `src/bridge.ts`:

**File: `kod-im/src/bridge-countdown.ts`** (append to `bridge.ts` or add as a mixin file the extractor skips)

```ts
// In onApprovalBatch(), after `const mid = await this.out.sendHtml(...)`:
//   this.scheduleCountdown(mid, usable.map(i => i.id as number));
// Add this method to ChatSession:

private countdownTimer: ReturnType<typeof setTimeout> | null = null;

private scheduleCountdown(messageId: string, ids: number[]): void {
  if (this.countdownTimer) clearTimeout(this.countdownTimer);
  const tick = async (remainingS: number) => {
    if (remainingS <= 0 || !this.pendingApproval) return;
    await this.out.editHtml(
      this.chatId, messageId,
      `🔐 <b>Approval needed</b> <i>(auto-deny in ~${remainingS}s)</i> — items: ${ids.join(", ")}`,
    ).catch(() => {});
    this.countdownTimer = setTimeout(() => void tick(remainingS - 60), 60_000);
  };
  this.countdownTimer = setTimeout(() => void tick(this.cfg.approvalTimeoutS - 60), 60_000);
}
```

And remember to `clearTimeout(this.countdownTimer)` inside `resolveButtons` path of `onAction`.

**Step 2.3 — Group-chat etiquette (optional)**

In groups, require a mention or reply so the bot doesn't grab every message: in `telegram.ts`'s `message:text` handler, before dispatching:

```ts
const isPrivate = ctx.chat.type === "private";
const mentioned = ctx.message.text.includes(`@${(await bot.api.getMe()).username}`);
const isCommand = text.startsWith("/");
if (!isPrivate && !mentioned && !isCommand) return; // stay quiet in groups unless addressed
```

**Gate 2.x** ✅ All five rows of the table behave as described.

---

## 7. PHASE 3 — Platform-agnostic adapter layer + agent-initiated outbound

### 7.1 The adapter contract (already implicit — make it explicit)

Any IM platform needs exactly this (see `src/outgoing.ts`): `sendHtml`, `editHtml`, `sendMarkdown`, `sendDocument`, `resolveButtons`, `react` **in**, plus a start function that translates inbound messages/actions to `bridge.sessionFor(chatId).onText(...)` / `.onAction(...)`. Adapters must be **< 200 lines** and contain **zero kod logic**. If you find kod logic in an adapter, move it to `bridge.ts`.

### 7.2 Matrix adapter (full implementation)

Works with any Matrix 1.x homeserver — including `conduwuit` bound to your tailnet (Pattern B, §2.2). Approvals use **emoji reactions** (👍 = approve, 👎 = deny) because Matrix has no inline buttons.

**File: `kod-im/src/adapters/matrix.ts`**

```ts
import * as mxc from "matrix-js-sdk";
import { RoomEvent, ClientEvent, type MatrixClient, type RoomMemberEvent } from "matrix-js-sdk";
import type { Config } from "../config.ts";
import type { Bridge } from "../bridge.ts";
import type { Btn, Outgoing } from "../outgoing.ts";
import { renderForIm } from "../format/imhtml.ts";

export interface MatrixOpts {
  homeserverUrl: string;   // e.g. "http://[::1]:6167" (conduwuit on loopback)
  accessToken: string;
  userId: string;          // "@kodbot:your.tailnet.ts.net"
  allowRooms: Set<string>; // room ids "!x:server" — fail-closed like Telegram chats
}

export function createMatrix(cfg: Config, bridge: Bridge): { out: Outgoing; start(): Promise<void>; stop(): Promise<void> } {
  const opts: MatrixOpts = {
    homeserverUrl: process.env.MATRIX_HOMESERVER_URL ?? "",
    accessToken: process.env.MATRIX_ACCESS_TOKEN ?? "",
    userId: process.env.MATRIX_USER_ID ?? "",
    allowRooms: new Set((process.env.KOD_IM_ALLOW_CHATS ?? "").split(",").filter(Boolean)),
  };
  if (!opts.homeserverUrl || !opts.accessToken) throw new Error("MATRIX_HOMESERVER_URL / MATRIX_ACCESS_TOKEN not set");

  const client: MatrixClient = mxc.createClient({
    baseUrl: opts.homeserverUrl,
    accessToken: opts.accessToken,
    userId: opts.userId,
  });

  // message id of the last approval card per room, for reaction routing
  const approvalCards = new Map<string, { eventId: string; ids: number[] }>();

  client.on(RoomEvent.Timeline, (event, room) => {
    void (async () => {
      const roomId = room?.roomId ?? event.getRoomId() ?? "";
      if (!opts.allowRooms.has(roomId) || event.getSender() === opts.userId) return;

      // reactions → approval decisions
      if (event.getType() === "m.reaction") {
        const rel = event.getRelation();
        const card = approvalCards.get(rel?.event_id ?? "");
        if (!card || !rel?.key) return;
        const decision = rel.key === "👍" ? "y" : rel.key === "👎" ? "n" : null;
        if (!decision || card.ids.length === 0) return;
        const itemId = card.ids[0]!;
        card.ids.shift();
        await bridge.sessionFor(roomId).onAction(`a:${itemId}:${decision}`);
        return;
      }

      if (event.getType() !== "m.room.message") return;
      const content = event.getContent<Record<string, unknown>>();
      if (content.msgtype !== "m.text" || typeof content.body !== "string") return;
      // ignore edits (m.replace)
      if ((content["m.new_content"] as unknown) !== undefined) return;
      try {
        await bridge.sessionFor(roomId).onText(content.body);
      } catch (e) {
        await out.sendHtml(roomId, `⚠️ bridge error: ${String(e).slice(0, 200)}`);
      }
    })();
  });

  const htmlBody = (html: string): string => html.replace(/<br\s*\/?>/g, "<br/>"); // Matrix accepts most HTML

  const out: Outgoing = {
    async sendHtml(chatId, html, bopts) {
      const eventId = await client.sendHtmlMessage(chatId, htmlBody(html), html.replace(/<[^>]+>/g, ""));
      if (bopts?.buttons?.length) {
        // Approval-style buttons → instruct reactions (Matrix pattern)
        const ids = bopts.buttons.flat().map((b) => /^a:(\d+):/.exec(b.action)?.[1]).filter(Boolean).map(Number);
        if (ids.length) approvalCards.set(eventId, { eventId, ids });
        await client.sendNotice(chatId, "react 👍 to approve, 👎 to deny (first item)");
      }
      return eventId;
    },
    async editHtml(chatId, messageId, html) {
      // m.replace edit
      await client.sendEvent(chatId, "m.room.message", {
        msgtype: "m.text", body: "* " + html.replace(/<[^>]+>/g, ""),
        format: "org.matrix.custom.html", formatted_body: "* " + htmlBody(html),
        "m.new_content": { msgtype: "m.text", body: html.replace(/<[^>]+>/g, ""), format: "org.matrix.custom.html", formatted_body: htmlBody(html) },
        "m.relates_to": { rel_type: "m.replace", event_id: messageId },
      } as unknown as Record<string, unknown>);
    },
    async sendMarkdown(chatId, md) {
      for (const seg of renderForIm(md)) await client.sendHtmlMessage(chatId, htmlBody(seg.html), seg.html.replace(/<[^>]+>/g, ""));
      return null;
    },
    async sendDocument(chatId, filename, content, caption) {
      const upload = await client.uploadContent(new Blob([new Uint8Array(content)]), { name: filename });
      await client.sendMessage(chatId, {
        msgtype: "m.file", body: caption ?? filename,
        url: upload.content_uri, info: { mimetype: "text/plain", size: content.length },
      });
    },
    async resolveButtons(chatId, messageId, note) {
      approvalCards.delete(messageId);
      if (note) await client.sendNotice(chatId, note);
    },
    async react(chatId, messageId, emoji) {
      if (!messageId) return;
      await client.sendEvent(chatId, "m.reaction", {
        "m.relates_to": { rel_type: "m.annotation", event_id: messageId, key: emoji === "✅" ? "👍" : emoji === "🚫" ? "👎" : emoji },
      });
    },
  };

  return {
    out,
    start: () => new Promise((resolve, reject) => {
      client.once(ClientEvent.Sync, (state) => {
        if (state === "PREPARED") { console.log("[matrix] sync prepared"); resolve(); }
        else if (state === "ERROR") reject(new Error("matrix sync error"));
      });
      client.startClient({ initialSyncLimit: 1 });
    }),
    stop: () => Promise.resolve(client.stopClient()),
  };
}
```

Get the access token once (Element → Settings → Help & About → Access token, or a dedicated bot account via registration + login). Put `MATRIX_HOMESERVER_URL`, `MATRIX_ACCESS_TOKEN`, `MATRIX_USER_ID` in the env file; set `KOD_IM_ALLOW_CHATS` to the Matrix room IDs.

**Step 3.2a — To switch the bridge to Matrix**: in `src/index.ts`, replace `createTelegram(...)` with `createMatrix(...)` and call `out.start()` the same way. The brain (`bridge.ts`) is untouched — that's the point of the contract.

### 7.3 Discord adapter (skeleton — buttons map 1:1, finish in ~1 h)

Discord's gateway is outbound-only and its native buttons work like Telegram's callbacks. Skeleton using `discord.js` (`bun add discord.js`):

**File: `kod-im/src/adapters/discord.ts`**

```ts
// SKELETON — complete by mapping the same calls as telegram.ts.
// bun add discord.js
// 1) Client with GatewayIntentBits.GuildMessages | MessageContent | GuildMessageReactions
// 2) client.on(Events.MessageCreate, m => {
//      if (m.author.bot) return;
//      if (!cfg.allowChats.has(m.channelId)) return;          // fail-closed
//      bridge.sessionFor(m.channelId).onText(m.content, { replyTo: m.id });
//    });
// 3) Outgoing.sendHtml → channel.send({ content: stripTags(html), embeds? }) —
//    Discord has no HTML: map <b>/<i>/<code>/<pre> to **bold**, *italic*, `code`, ```blocks```.
// 4) Outgoing.sendHtml(buttons) → ActionRowBuilder + ButtonBuilder with
//    customId = action ("a:12:y") ≤ 100 bytes ✔; client.on(Events.InteractionCreate)
//    → i.isButton() → bridge.sessionFor(channelId).onAction(i.customId); i.update(...).
// 5) editHtml → message.edit(...); resolveButtons → message.edit({ components: [] }).
// 6) sendDocument → channel.send({ files: [AttachmentBuilder.from(buf, {name})] }).
// Start requires no public endpoint (gateway is outbound) — Pattern A holds.
export {}; // placeholder so tsc accepts the file before you implement it
```

### 7.4 Slack (Socket Mode) — guidance only

Slack needs two tokens (`SLACK_BOT_TOKEN xoxb-…`, `SLACK_APP_TOKEN xapp-…`) and the `@slack/socket-mode` + `@slack/web-api` clients. Messages arrive over an outbound WebSocket (Pattern A holds). Buttons = Block Kit `actions` blocks; the `action_id` carries our `a:<id>:<y|n|d>` string verbatim. No public URL needed. Estimated effort: ~2 h following the Telegram adapter as the template.

### 7.5 Agent-initiated outbound: the MCP notify server

This is what makes the agent able to **start** conversations ("build failed, look at this diff") instead of only answering. kod spawns it as an MCP server over stdio (config in Phase 0.3 `[mcp.servers.im-notify]`), the agent calls it as the tool `mcp:im-notify.send_message`, gated by your policy like any tool.

**File: `kod-im/mcp/im-notify.mjs`**

```js
#!/usr/bin/env node
/** kod MCP server: agent-initiated IM notifications (Telegram today).
 *  Zero dependencies — newline-delimited JSON-RPC 2.0 over stdio, which is
 *  exactly what kod-mcp speaks (initialize / tools/list / tools/call).
 *  Env: TELEGRAM_BOT_TOKEN (required), KOD_IM_NOTIFY_CHAT (default chat id). */
import { createInterface } from "node:readline";

const TOKEN = process.env.TELEGRAM_BOT_TOKEN || "";
const DEFAULT_CHAT = (process.env.KOD_IM_NOTIFY_CHAT
  || (process.env.KOD_IM_ALLOW_CHATS || "").split(",")[0] || "").trim();

const TOOLS = [{
  name: "send_message",
  description:
    "Send a notification message to the user's chat app (Telegram). " +
    "Use this to proactively inform the user about finished work, failures, " +
    "or anything that needs attention. Keep it short and concrete.",
  inputSchema: {
    type: "object",
    properties: {
      body: { type: "string", description: "Message text (HTML allowed: <b> <i> <code> <pre>)" },
      title: { type: "string", description: "Optional one-line title/emoji prefix" },
      chat_id: { type: "string", description: "Optional override of the target chat id" },
    },
    required: ["body"],
  },
}];

const esc = (s) => String(s).replace(/&/g, "&amp;").replace(/</g, "&lt;").replace(/>/g, "&gt;");

async function sendTelegram(chatId, html) {
  const chunks = [];
  for (let i = 0; i < html.length; i += 3800) chunks.push(html.slice(i, i + 3800));
  const results = [];
  for (const part of chunks) {
    const res = await fetch(`https://api.telegram.org/bot${TOKEN}/sendMessage`, {
      method: "POST",
      headers: { "content-type": "application/json" },
      body: JSON.stringify({ chat_id: chatId, text: part, parse_mode: "HTML", link_preview_options: { is_disabled: true } }),
    });
    const j = await res.json();
    if (!j.ok) throw new Error(j.description || `telegram error ${res.status}`);
    results.push(j.result.message_id);
  }
  return results;
}

function write(obj) { process.stdout.write(JSON.stringify(obj) + "\n"); }

const rl = createInterface({ input: process.stdin, terminal: false });
rl.on("line", (line) => {
  let msg;
  try { msg = JSON.parse(line); } catch { return; }
  const { id, method, params } = msg;

  if (method === "initialize") {
    write({ jsonrpc: "2.0", id, result: {
      protocolVersion: "2025-06-18",
      capabilities: { tools: {} },
      serverInfo: { name: "im-notify", version: "0.1.0" },
    } });
    return;
  }
  if (method === "notifications/initialized" || (id === undefined && method?.startsWith("notifications/"))) return;

  if (method === "tools/list") { write({ jsonrpc: "2.0", id, result: { tools: TOOLS } }); return; }

  if (method === "tools/call") {
    const name = params?.name;
    const args = params?.arguments ?? {};
    if (name !== "send_message") {
      write({ jsonrpc: "2.0", id, result: { content: [{ type: "text", text: `unknown tool ${name}` }], isError: true } });
      return;
    }
    if (!TOKEN) {
      write({ jsonrpc: "2.0", id, result: { content: [{ type: "text", text: "TELEGRAM_BOT_TOKEN not set in daemon env" }], isError: true } });
      return;
    }
    const chatId = String(args.chat_id || DEFAULT_CHAT || "");
    if (!chatId) {
      write({ jsonrpc: "2.0", id, result: { content: [{ type: "text", text: "no chat_id: pass chat_id or set KOD_IM_NOTIFY_CHAT" }], isError: true } });
      return;
    }
    const title = args.title ? `<b>${esc(args.title)}</b>\n` : "";
    const html = title + String(args.body ?? "");
    sendTelegram(chatId, html).then(
      (ids) => write({ jsonrpc: "2.0", id, result: { content: [{ type: "text", text: `delivered (${ids.length} message(s))` }] } }),
      (err) => write({ jsonrpc: "2.0", id, result: { content: [{ type: "text", text: `send failed: ${err.message}` }], isError: true } }),
    );
    return;
  }

  if (id !== undefined) write({ jsonrpc: "2.0", id, error: { code: -32601, message: `unknown method: ${method}` } });
});
```

**File: `kod-im/skills/im-etiquette.md`** (install to `/srv/kod-agent/skills/im-etiquette.md` — the pinned skills dir)

```markdown
---
name: im-etiquette
description: How to behave when the user is on a chat app (Telegram/Matrix) instead of a terminal
version: 1.0.0
category: communication
tags: [im, chat, brevity]
triggers:
  - "telegram"
  - "text me"
  - "notify me"
---

## Instructions

The user may be reading you on a phone chat app.

- Be brief. Lead with the answer. One idea per message.
- Use small code blocks (<= 15 lines). Offer the file path for long diffs instead of pasting.
- Never paste secrets, tokens, or .env contents — they are redacted anyway; don't fight it.
- When something finishes (or fails) outside an interactive turn, call the
  `mcp:im-notify.send_message` tool with a one-line title and a 1-3 line body.
- Ask questions with the ask_user tool when a decision is needed; a human will
  answer from their phone within ~2 minutes or the engine defaults.
- Avoid tables wider than 60 chars; they render badly on phones.
```

**Step 3.5 — Verify agent-initiated notify**

```bash
# daemon env must contain TELEGRAM_BOT_TOKEN (it does — same env file) then:
sudo systemctl restart kod-serve
# from Telegram: "run the test suite and text me the summary when done"
```

✅ The agent runs tools, then calls `mcp:im-notify.send_message` → a message lands in your chat even if you closed the session.

**Gate 3.x** ✅ `bun run check` still green with the adapters; Matrix adapter verified against a test room (send `hello` → reply; react 👍 on a card → approval lands).

---

## 8. PHASE 4 (optional, Rust) — Upstream kod patches that unlock the last 10 %

> **AGENT EXECUTION NOTES — Phase 4**
> - These patches are **not required** for the bridge experience of Phases 0–3. They fix structural gaps and should ideally go upstream as PRs to `elcoosp/kod`.
> - Conventions: edition 2024, MSRV 1.98.1 (`rust-toolchain.toml`), `[lints] workspace = true`, `cargo clippy --workspace --all-targets -- -D warnings` must stay green, `just check` is the gate. License allow-list in `deny.toml` is permissive-only — no GPL deps.
> - Sketches below are *anchored to the real code* (file + function names from source inspection) but are intentionally compact; adapt names to HEAD when the file has moved.

### P4.1 — Cross-connection approval/question delivery (the one that matters)

**Problem**: approval/question markers ride the `chunk_tx` of the originating `process_streaming` call and are forwarded only to that connection (`serve.rs::run_streaming`, pump at ~lines 841–868). A run started by another client (TUI, CLI, a future scheduler) is invisible to the bridge — the approval times out to deny. The *answer* path is already engine-global (`respond_to_approval` works from any connection); only **discovery** is missing. This is exactly the case the CLI's own doc comment anticipates (`kod-cli/src/commands/mod.rs:1752-1765`).

**Fix** (two small pieces in `crates/kod-core-serve/src/serve.rs`):

1. Add a connection registry + broadcast helper:

```rust
// serve.rs — new near serve()
#[derive(Default)]
struct Clients {
    next: u64,
    by_id: HashMap<u64, tokio::sync::mpsc::UnboundedSender<String>>,
}

impl Clients {
    fn register(&mut self, tx: tokio::sync::mpsc::UnboundedSender<String>) -> u64 {
        self.next += 1;
        self.by_id.insert(self.next, tx);
        self.next
    }
    fn unregister(&mut self, id: u64) { self.by_id.remove(&id); }

    /// Send `line` to every connection except `except`.
    fn broadcast(&self, except: Option<u64>, line: &str) {
        for (&cid, tx) in &self.by_id {
            if Some(cid) == except { continue; }
            let _ = tx.send(line.to_string()); // bounded queues already handle slowpokes
        }
    }
}
```

2. In `run_streaming`'s pump loop, where each engine `chunk` is forwarded as `{"id":..,"type":"chunk","data":..}` (~serve.rs:857), detect approval/question markers and additionally broadcast a structured push event to *other* connections:

```rust
const APPROVAL_BATCH_TAG: &str = "\u{0}kod-approval-batch:";
const QUESTION_TAG: &str = "\u{0}kod-question:";

fn marker_event(chunk: &str, req_id: &str, key: &str) -> Option<String> {
    let (kind, rest) = if let Some(r) = chunk.strip_prefix(APPROVAL_BATCH_TAG) { ("approval_request", r) }
        else if let Some(r) = chunk.strip_prefix(QUESTION_TAG) { ("question", r) }
        else { return None; };
    // rest = "<numeric-id>:<json>" (trailing \0 tolerated)
    let rest = rest.trim_end_matches('\u{0}');
    let (id, payload) = rest.split_once(':')?;
    Some(format!(
        "{{\"id\":\"push-{req_id}\",\"type\":\"push\",\"data\":{{\"kind\":\"{kind}\",\"transcript_key\":\"{key}\",\"ref\":{id},\"payload\":{payload}}}}}"
    ))
}
// in the pump: if let Some(push) = marker_event(&chunk, &req_id, &key) { clients.broadcast(None, &push); }
```

Clients that don't understand `"type":"push"` simply ignore it (the shipped CLI routes on its own request ids), so this is wire-compatible. The bridge then learns about *foreign* approvals and can answer them via the already-working `respond_to_approval`.

3. Add `list_pending` so a late-attaching client can recover state (mirror `pending_approval_count_for_tests`, `kod-core/src/engine/policy.rs:145-147`):

```rust
// kod-core/src/engine/policy.rs — next to respond_to_approval
pub async fn pending_snapshot(&self) -> (Vec<(u64, String)>, Vec<(u64, String)>) {
    let ap = self.pending_approvals.read().await.keys().map(|k| (*k, "pending".into())).collect();
    let qs = self.pending_questions.read().await.keys().map(|k| (*k, "pending".into())).collect();
    (ap, qs)
}
// serve.rs: new method arm `list_pending` -> done + {"approvals":[ids],"questions":[ids]}
```

*(Replace the placeholder string with the ApprovalRequest/QuestionRequest payloads if the engine stores them — it currently stores only oneshots; storing the request JSON alongside costs one `HashMap<u64,(Sender,String)>` change in `engine/mod.rs:635-636`.)*

**Tests to add** (`kod-core-serve/tests/…`, follow `serve_roundtrip.rs`): open two connections; start a gated run on A (engine config with `mode="ask"`); assert B receives `type:"push"` with `kind:"approval_request"`; B answers `respond_to_approval`; A's run continues past the gate.

### P4.2 — Per-key busy guard on the socket daemon

**Problem**: two concurrent `process_streaming` calls on the same `transcript_key` interleave in that key's shared history. ACP guards this (`acp.rs::in_flight`, `-32002 "session is busy"`); the socket daemon doesn't.

```rust
// serve.rs — in serve(): let in_flight: Arc<Mutex<HashSet<String>>> = Default::default();
// in the process/process_streaming/swarm dispatch arms:
{
    let key = transcript_key.clone();
    let mut guard = in_flight.lock().unwrap();
    if !guard.insert(key.clone()) {
        return send_error(&id, "session busy: another run is active on this transcript_key");
    }
}
// on pump completion (success, error, or drop): guard.remove(&key);
```

The bridge already serializes per chat, so this protects against *other* clients (TUI + bridge on the same key) — cheap insurance.

### P4.3 — Swarm sub-agent approvals must not be swallowed

**Problem**: `kod-core/src/swarm_runner.rs` (~lines 1450–1463) discards every `\0`-prefixed chunk from sub-agent pumps, so gated calls inside swarm agents always time out to deny — remotely and locally.

```rust
// swarm_runner.rs — in the sub-agent chunk pump:
if chunk.starts_with('\u{0}kod-approval-batch:') || chunk.starts_with('\u{0}kod-question:') {
    // re-emit upstream so the user-facing stream (and P4.1's fan-out) sees it
    if chunk_tx.send(chunk.clone()).await.is_err() { break; }
    continue;
}
if chunk.starts_with('\u{0}') { continue; } // other markers stay swallowed
```

### P4.4 — Expose the missing engine verbs over the daemon

The engine already has these public APIs; each needs one dispatch arm in `serve.rs` (follow the `run_swarm` pattern for spawned streaming):

| New method | Engine call | Notes |
|---|---|---|
| `pause {transcript_key?}` | `engine.pause()` (`control.rs:78`) | polled before each model call/tool round |
| `resume {}` | `engine.resume()` | |
| `goal {input, transcript_key, token_budget?, time_budget_seconds?}` | `process_goal_streaming_for` (`process.rs:1314+`) | streams like `process_streaming`; emits `\0kod-turn:` markers the bridge already renders |
| `jobs {}` | `BackgroundJobRunner::snapshot()` | visibility for the IM `/status` |

Bridge side (when these land): `/pause` and `/resume` commands, and a `/goal <objective>` that maps to long missions with the working-message UI already handling turn boundaries.

### P4.5 — Teach the redaction layer about Telegram tokens

**Problem**: a Telegram bot token (`123456789:AA…`) matches no vendor regex; it's only covered by the entropy heuristic when a keyword sits nearby. Defense-in-depth for the exact secret this integration introduces:

```rust
// kod-types/src/secret_sources.rs — vendor regex list (~line 190):
//   regex: r"\b\d{8,11}:[A-Za-z0-9_-]{30,}\b",  rule: "telegram-bot-token"
// kod-types/src/redact.rs — builtin rules (~lines 234-267):
//   TelegramBotToken => pattern r"\b\d{8,11}:[A-Za-z0-9_-]{30,}\b", replacement "[REDACTED:telegram]"
// test: session_log redaction of a ToolCall argument containing a bot token.
```

### P4.6 — Optional, bigger: per-request working directory

The engine keeps per-key `working_dir` maps, but the daemon accepts only `input`/`transcript_key`. A `cwd` param (validated: must exist, must be under the daemon's allowed roots) would let one daemon serve several repos, with the bridge exposing `/cd <repo>`. Needs a careful look at prompt grounding (repo map, skills) and sandbox paths — treat as its own design doc, not a drive-by patch.

**Gate P4.x** ✅ `just check` green; new integration tests pass; upstream PR opened with before/after transcripts.

---

## 9. PHASE 5 — Hardening, ops, failure drills

**Step 5.1 — Operational checklist**

- [ ] Both services `Restart=always/on-failure`; `systemctl status kod-serve kod-im` green after reboot (`enable-linger` done in Phase 0).
- [ ] `KOD_IM_ALLOW_CHATS` non-empty; unknown-sender test: from a second Telegram account, DM the bot → **no response at all**, and nothing in `journalctl -u kod-im` beyond a middleware silence.
- [ ] `/srv/kod-agent/config/kod-im.env` is `chmod 600 kod-agent:kod-agent`, not in any git tree; `git -C /srv/kod-agent grep -r TELEGRAM_BOT_TOKEN` finds nothing.
- [ ] Token never printed: `journalctl -u kod-im | grep -c $(cut -d= -f2 /srv/kod-agent/config/kod-im.env | head -1)` → 0 occurrences (the bridge must never log it; verified here).
- [ ] `kod policy explain execute_command command="ls"` → `Ask` (from the daemon's cwd). `kod policy explain execute_command command="dd if=/dev/zero of=/dev/sda"` → engine hard-deny regardless of approval.
- [ ] `kod sandbox check` → bwrap available; run one `execute_command curl` approval → fails with network-unavailable inside sandbox (proof the shell is net-confined while the daemon keeps its connectivity).
- [ ] Limits active: `kod config | grep -A3 limits`.
- [ ] Session logs redacted: `grep -r "AA" ~/.kod/sessions/*.jsonl` → no bot-token suffix (kod's redactor runs always-on; P4.5 adds the vendor rule).
- [ ] Bun lockfile committed: `git -C /srv/kod-agent/im/kod-im add bun.lock` and unit runs `bun install --frozen-lockfile` in an ExecStartPre if you want CI-grade pinning.

**Step 5.2 — Failure drills (run each once; both should recover without you)**

| Drill | Act | Expect |
|---|---|---|
| Daemon crash | `sudo systemctl restart kod-serve` mid-run | Bridge shows "daemon connection dropped mid-run" in the chat; next prompt works immediately after restart |
| Bridge crash | `sudo systemctl restart kod-im` mid-run | Phone silence for ≤ 3 s; new prompts work; old working message stays stale (acceptable — note in UX) |
| Telegram outage | `iptables -A OUTPUT -d api.telegram.org -j DROP` for 60 s | grammY retries with backoff; on restore, queued long-poll updates are processed (no message loss) |
| Approval timeout | Send an approval card, don't answer | 120 s → engine auto-denies; bridge prints `⏱ too late…` when you tap afterwards |
| Cost runaway | Ask for `implement feature X and keep improving it` with tiny `[limits]` | Engine stops on budget (`on_exhausted = "stop"`); bridge shows the stop reason in the final message |
| Token rotation | Revoke via BotFather, set new one, restart | `kod-im doctor` fails before restart with old token, passes after |

**Step 5.3 — Status page (optional, tailscale-only, 20 lines)**

`src/status.ts` — a tiny Bun HTTP server (loopback only) exposing JSON for `tailscale serve`:

```ts
// bun run src/status.ts  → http://127.0.0.1:3999/status.json
// tailscale serve --bg --https=443 localhost:3999  → https://<host>.<tailnet>.ts.net/status.json
import { createServer } from "node:http";
import { pingKod } from "./kod/client.ts";

createServer(async (_req, res) => {
  const kod = await pingKod(process.env.XDG_RUNTIME_DIR ? `${process.env.XDG_RUNTIME_DIR}/kod.sock` : `${process.env.HOME}/.kod/run/kod.sock`);
  res.setHeader("content-type", "application/json").end(JSON.stringify({
    ok: true, uptime_s: Math.round(process.uptime()), daemon: kod,
    chats: Number(process.env.KOD_IM_ALLOW_CHATS?.split(",").filter(Boolean).length ?? 0),
  }, null, 2));
}).listen(3999, "127.0.0.1");
```

---

## 10. Acceptance matrix (final gate for the whole project)

| # | Scenario | Command / action | Pass criterion |
|---|---|---|---|
| A1 | Protocol smoke | `bun run scripts/socket-smoke.ts` | `[done] {"text":"BRIDGE-SMOKE-OK",…}` |
| A2 | Doctor | `bun run doctor` | daemon ✅, allowlist ✅, token ✅ |
| A3 | Basic chat | Phone: `hello` | receipt < 2 s, streaming preview, final answer, footer with secs/tokens |
| A4 | Tool visibility | Phone: `list files here` | activity lines with tool name + ✅ + ms |
| A5 | Approval approve | Phone: ask for a file write → tap ✅ | `👍 approved #n`; file exists on disk |
| A6 | Approval deny | tap ❌ | agent receives denial and proceeds differently |
| A7 | Approval deny-always | tap 🚫 | subsequent identical asks are auto-denied (session deny rule) |
| A8 | Approval timeout | wait 120 s | auto-deny; late tap → "too late" message |
| A9 | Steering | message during run | `🪝 steered…`; agent course-corrects; footer `🪝×n` |
| A10 | Abort | tap 🛑 | run stops; disk state sane (kod checkpoint) |
| A11 | Question | ambiguous prompt | ❓ card; next message answers; `/skip` works |
| A12 | Sessions | `/new work`, `/status` | key switches; status shows tail |
| A13 | Models | `/models`, `/model <id>` | list renders; switch applies daemon-wide |
| A14 | Agent-initiated notify | `run tests and text me the summary` | `mcp:im-notify.send_message` delivers while you're away |
| A15 | Multi-daemon | second daemon + `/use beta` | chat binds to second socket; `/use main` returns |
| A16 | Stranger | other account DMs bot | zero responses; no log noise |
| A17 | Secrets | ask agent to `cat .env` | read_protection refuses (or redacts); no token anywhere in chat or logs |
| A18 | Sandbox | ask agent to `curl example.com` | sandboxed failure (net-deny), daemon unaffected |
| A19 | Crash drills | §5.2 table | all recover without manual help |
| A20 | Reboot | `sudo reboot` | both services auto-start; phone workflow resumes |

---

## Appendix A — Source citations (verified against `elcoosp/kod` HEAD, Oct 2026)

| Claim | Location |
|---|---|
| Socket path resolution, `XDG_RUNTIME_DIR` | `crates/kod-core-state/src/socket_path.rs:10-19` |
| Peer-UID check, 0600 chmod, flock, 1 MiB cap | `crates/kod-core-serve/src/serve.rs:345-361, 318-331, 250-280, 397-474` |
| "No TCP… never will" posture | `serve.rs:21-25` |
| Protocol methods list | `serve.rs:114-127` (`PROTOCOL_METHODS`) |
| Request/response frame structs (`type` field, ack omits data) | `serve.rs:138-161` |
| `respond_to_approval` / `respond_to_question` params | `serve.rs:665-712` |
| Marker formats (tool/args/done/approval-batch/question/usage/rate-limit/turn) | `crates/kod-core/src/engine/markers.rs` |
| ApprovalBatch JSON shape | `crates/kod-core/src/engine/approval.rs:12-40` |
| QuestionRequest shape | `crates/kod-tools/src/ask.rs:44-49` |
| 120 s approval timeout, poll-cancel loop | `kod-core/src/engine/constants.rs:46`, `tool_dispatch.rs:823-852` |
| Approval marker sent only to in-flight connection (the gap) | `serve.rs:841-868` pump; `kod-cli/src/commands/mod.rs:1752-1765` doc |
| Answer path is engine-global | `kod-core/src/engine/policy.rs:111-140` |
| Steering queue + drain at round boundaries | `kod-core-state/src/steer.rs`, `kod-core/src/engine/agent_loop.rs:206-241` |
| Cancel epochs | `kod-core/src/engine/control.rs:3-101` |
| Daemon = one engine, no session recorder | `kod-cli/src/commands/admin.rs:621-697` |
| Shipped transcript-key conventions | `kod-cli/src/commands/prompt.rs:40`, `chat.rs:82,719`, `kod-core-serve/src/acp.rs:465-471` |
| Secrets vault (env scan keywords, placeholders, deobfuscate at dispatch) | `kod-types/src/secret_sources.rs`, `secret_placeholder.rs`, `kod-core/src/engine/state.rs:142`, `agent_loop.rs:1354-1391`, `tool_dispatch.rs:1080-1103` |
| Shell env secret stripper | `kod-tools/src/tools.rs:112-143, 1010-1035` |
| Redactor rules + always-on session-log redaction | `kod-types/src/redact.rs:234-267`, `kod-core-state/src/session_log.rs:321-401` |
| Policy presets/layers, project policy narrows-only | `kod-config/src/policy.rs` (module doc, `load` at 328, `preset_decision` at 645) |
| Read-protection default globs | `kod-config/src/policy.rs:165+`, enforced `kod-tools/src/tools.rs:336-373` |
| Blast-radius gate (Catastrophic = unoverridable deny) | `kod-core/src/engine/tool_dispatch.rs:267+`, `kod-risk/src/classify.rs`, `paths.rs` |
| Sandbox backends + net-deny | `kod-tools/src/sandbox/{mod,landlock}.rs`, `context.rs:973+` |
| Limits / budgets / hard caps | `kod-config/src/limits.rs`, `kod-core-state/src/cost.rs`, `kod-core/src/engine/constants.rs` |
| Hooks = pre/post tool use only, env-var contract | `kod-config/src/config.rs:217-223`, `kod-core/src/hooks.rs` |
| MCP stdio-only client, `mcp:<server>.<tool>` naming | `kod-mcp/src/lib.rs` header, `kod-core/src/mcp_adapters.rs:49-65` |
| ACP capabilities (no steer, per-session in-flight) | `kod-core-serve/src/acp.rs` |
| Swarm IRC bus = in-memory, per-run | `kod-swarm/src/irc_bus.rs:52-53` |
| Swarm pump swallows sub-agent markers | `kod-core/src/swarm_runner.rs:1450-1463` |
| Workspace conventions (edition 2024, MSRV 1.98.1, deny.toml) | root `Cargo.toml:33-38`, `deny.toml:33-56`, `justfile` |

## Appendix B — Canonical wire examples (pinned by kod's tests)

```jsonc
// request → stream → done (doc comment, serve.rs:34-50; tests serve_roundtrip.rs:95-100)
{"v":1,"id":"r1","method":"process_streaming","params":{"input":"hello","transcript_key":""}}
{"id":"r1","type":"chunk","data":"hi "}
{"id":"r1","type":"chunk","data":"there"}
{"id":"r1","type":"done","data":{"task_type":"chat","text":"hi there","tool_calls":[],"tool_results":[],"skills_used":[],"memory_used":false,"execution_time_ms":123,"usage":{"prompt_tokens":100,"completion_tokens":5,"total_tokens":105}}}

// steering + cancel during a run (same connection is fine)
{"v":1,"id":"s1","method":"steer","params":{"note":"use async/await","transcript_key":"tg:123"}}
{"id":"s1","type":"done"}                                   // bare ack, data omitted
{"v":1,"id":"c1","method":"cancel","params":{"transcript_key":"tg:123"}}
{"id":"c1","type":"done"}

// answering an approval rendered from a marker chunk
// (chunk frame seen first): {"id":"r1","type":"chunk","data":"…\u0000kod-approval-batch:7:{\"items\":[{\"id\":11,\"tool_name\":\"execute_command\",\"arguments\":{\"command\":\"cargo test\"},\"diff\":null,\"summary\":\"cargo test\"}]}\u0000"}
{"v":1,"id":"a1","method":"respond_to_approval","params":{"id":11,"decision":"approve"}}
{"id":"a1","type":"done","data":{"delivered":true}}          // delivered:false = window elapsed

// question (ask_user)
{"v":1,"id":"q1","method":"respond_to_question","params":{"id":4,"answer":"postgres"}}
{"id":"q1","type":"done","data":{"delivered":true}}

// peek another session (read-only)
{"v":1,"id":"p1","method":"peek_session","params":{"key":"chat","max_chars":800}}
{"id":"p1","type":"done","data":{"key":"chat","messages":12,"tail":"…","truncated":true}}

// errors are nested objects (serde-pinned, serve.rs tests 1089-1130)
{"id":"x1","type":"error","data":{"message":"unknown method: nope (server speaks protocol 1..=1)"}}
```

## Appendix C — Security checklist (print this)

1. Bridge + daemon run as dedicated user `kod-agent`; your login never touches the socket.
2. Socket is Unix-only, `0600`, peer-UID-checked — never bridge it over TCP/Tailscale.
3. IM allowlist fail-closed; strangers get silence.
4. Policy preset `standard` (or stricter) + project `.kod/policy.toml` narrows; `execute_command` = Ask; catastrophic commands engine-denied.
5. `read_protection` mode `refuse` for `.env*`/keys/ssh.
6. bwrap installed; sandbox network-deny verified with a curl test.
7. `[limits]` cost/token caps, `on_exhausted = "stop"`, per-tool quotas.
8. Secrets only as env vars (token never in config.toml, git, or chat); env file 0600.
9. Skills dir pinned (`skills.skills_dir`) — hostile repos can't plant prompt-injected skills.
10. Session logs + `kod replay` give a full audit trail of every tool call.
11. Bun lockfile pinned; `bun install --frozen-lockfile` in deployment.
12. Fail-closed everywhere: no allowlist → no service; no consumer → deny; no answer in 120 s → deny.

## Appendix D — Webhook mode via Tailscale Funnel (only if required)

Telegram works fine with long polling; webhooks are only needed if (a) you want sub-second delivery at scale or (b) a platform demands it. If so:

```bash
# 1. Bridge webhook receiver binds loopback only
KOD_IM_WEBHOOK_SECRET=$(openssl rand -hex 32)
bun run src/webhook.ts &   # small addition: POST /hook/<path>, validates X-Telegram-Bot-Api-Secret-Token == $KOD_IM_WEBHOOK_SECRET, then feeds the same bridge handlers

# 2. Expose via Funnel (public TLS, tailnet-owned cert)
sudo tailscale funnel --bg --https=8443 localhost:8080

# 3. Register with Telegram
curl "https://api.telegram.org/bot$TELEGRAM_BOT_TOKEN/setWebhook" \
  -d url="https://<host>.<tailnet>.ts.net:8443/hook/$(openssl rand -hex 16)" \
  -d secret_token="$KOD_IM_WEBHOOK_SECRET" \
  -d 'allowed_updates=["message","callback_query"]'
```

Threat deltas vs long polling: public endpoint exists (TLS-terminated, still requires the secret header + secret path); DoS surface appears. Rate-limit at the receiver (Bun's `Bun.serve` + a token bucket) and keep `drop_update_on_error` semantics sane. **Delete the funnel when done** (`tailscale funnel --https=8443 off`).

## Appendix E — Troubleshooting

| Symptom | Cause | Fix |
|---|---|---|
| `cannot connect to …/kod.sock: EACCES` | Different UID | Run bridge as same user as daemon (`kod-agent`); check `stat -c %U $(ls $XDG_RUNTIME_DIR/kod.sock)` |
| `another kod server is already listening` | Stale lock/socket after crash | `kod serve --stop`, or delete `<socket>.lock` if no process holds it |
| Doctor: `bot token invalid` | Typo / revoked token | BotFather → `/token`; update env file; restart both services |
| Bot silent in group | Privacy mode | BotFather `/setprivacy` → Disable; re-add bot to group |
| `parse entities` errors in logs | Edge-case HTML | Bridge auto-falls back to plain text; if frequent, tighten renderer (keep `<pre>` balanced) |
| Approval answered but `delivered:false` | > 120 s | Expected (engine auto-denied); run again; consider P2.2 countdown + `AWAIT_APPROVAL_SECS` awareness |
| Streaming preview lags | Telegram ~1 edit/s per chat | Default 2 s debounce is safe; don't go below 1.1 s; `KOD_IM_MAX_PREVIEWS` caps long runs |
| 429 rate-limit from Telegram | Message flood | Bridge splits/batches; if you add features, keep ≤ ~25 edits per run (default cap) |
| Agent ignores steers | Steer drained at next *round boundary* | Normal: steering lands between tool rounds; for instant stop use 🛑 / `/stop` |
| `session busy` (after P4.2) | Two clients on one key | Close the other client or use a different `/new` name |
| Daemon unreachable after reboot | Linger not enabled | `sudo loginctl enable-linger kod-agent` |
| Landlock host + kernel < 6.7 | Every sandboxed shell errors (net-deny unsupported) | Install bubblewrap (`apt install bubblewrap`) — bwrap is the required backend for remote hosts |
| MCP notify tool missing | `[mcp.servers.im-notify]` not loaded | `kod config` shows effective config; check the path in `command=` is executable (`chmod +x`) |

## Appendix F — Roadmap ideas (post-MVP, in value order)

1. **P4.1 upstream** (cross-connection pushes) → bridge approves runs started in the TUI; then a "review bot" pattern: TUI session pings your phone for approvals.
2. **`goal` over the wire (P4.4)** → `/mission implement X for 30 minutes` with turn-boundary cards and periodic IM digests.
3. **Voice notes** → Telegram `voice` update → local whisper.cpp → prompt text. Zero new security surface (same allowlist), big mobile-DX win.
4. **E2EE Matrix** (matrix-sdk crypto store) for the truly paranoid; reactions/approvals unchanged.
5. **Scheduled prompts** → systemd timers calling `bun run scripts/socket-smoke.ts "nightly: run tests, fix easy failures, notify"` — overnight agent with morning IM report (pairs with kod's overnight manifests).
6. **Multi-host registry** → `/hosts` command listing daemons across machines via Tailscale (each host keeps its local socket; the registry is just a config map).
7. **Web dashboard over `tailscale serve`** — reuse `peek_session` + session-log tailing for a read-only activity page.
8. **Kod upstream: `on_turn_end` hook** → would let pure-config users mirror turns into any IM without the bridge (still inferior to the bridge's two-way UX).

---

*End of plan. Phases 0–3 deliver the full phone-controlled, approval-gated, steering-capable, platform-agnostic agent experience with zero inbound exposure. Phase 4 is upstream-friendly polish. Everything above is grounded in the actual kod source — the protocol you are implementing is the one kod's own tests pin.*
