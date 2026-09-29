# kod ↔ tab-bridge — Session Affinity, Memory Cadence & Tab Lifecycle Fix Plan

| | |
|---|---|
| **Date** | 2026-09-29 |
| **kod** | `github.com/elcoosp/kod` @ `956ae31` (main) |
| **tab-bridge** | `github.com/elcoosp/tab-bridge` @ `7a3af15` (main, package v1.2.46 — matches the runtime in your log) |
| **Evidence** | Your bridge/worker/console log from 2026-09-29 15:21:38–15:23:32Z (`--stateful --auto-create-tabs --managed-only --ttl=30m`) |
| **Citation tags** | `[tb]` = tab-bridge repo, `[kod]` = kod repo. Line numbers verified against the checked-out commits above. |

---

## 0. TL;DR

Your four observations are all real, and they trace to **six root causes** — four on the kod side, three on the tab-bridge side (one symptom has causes in both):

| # | Your observation | Root cause | Where | Fix (phase) |
|---|---|---|---|---|
| 1 | "warm creates a new session and next messages are sent in another tab — makes no sense" | kod's `prewarm` deliberately sends a **sessionless** 17k-char probe turn (`[kod] engine/mod.rs:2400-2404`); the bridge mints a fresh `anon-*` ephemeral session per sessionless request (`[tb] registry.ts:104`), which binds a *different* tab than your real conversation. The warmed tab is almost never the tab the next real turn uses, so the probe is pure noise on this backend | kod (+bridge) | WS-C (P1) |
| 2 | "is fact collection really working?" | **Yes, end-to-end** (extract → redb store → query-driven retrieval into later prompts). The `[]` reply you saw is a *different* extractor (sharpshooter decisions) correctly reporting "no decisions". What's missing is observability, so it's invisible | kod | WS-B (P1) + verification §7 |
| 3 | "every-turn memory fact collection is too much — should be configurable" | `RetentionCadence::every_n_turns` (default 5) is **dead code** — `is_due()` only checks `min_new_messages` (`[kod] retention.rs:188-190`), and any tool-using turn adds ≥4 messages, so extraction fires on effectively **every** turn. The sharpshooter decision extractor additionally fires every eligible turn. No config knobs exist for either | kod | WS-B (P1) |
| 4 | "we should reuse the same tab session, otherwise we get tab leakage" | (a) All kod background traffic is sessionless → new `anon-*` session per request; (b) the bridge **reuses dirty tabs without resetting the chat** (`[tb] engine.ts:296-298`) → cross-session context bleed; (c) TTL eviction sends a `RELEASE evict:<id>` the worker can never match (`[tb] bridge.ts:72` vs `background.js:788`) → tab **permanently occupied**; (d) released tabs are never closed, there is no `--max-tabs`, and `--warm-tabs` is a parsed-but-never-read stub | both | WS-A (P2), WS-D (P0), WS-E (P0/P3) |

**Recommended sequencing:** Phase 0 = two tiny bridge diffs (leak + bleed stop) → Phase 1 = kod config/cadence (kills the per-turn LLM tax and the warm noise) → Phase 2 = kod background session identity (one stable background session ⇒ one background tab) → Phase 3 = bridge pool hygiene (caps, idle close, warm tabs).

---

## 1. What your log actually shows

Reconstructed timeline (bridge audit log + worker console), annotated with the mechanism behind each line:

| Time (UTC) | Event | What it actually is |
|---|---|---|
| 15:21:58 | `turn.plan session-ba29a9e2… SEED empty-chain` → tab **1069586344** created | Your main conversation's first request. kod sends its transcript session id as the OpenAI `user` field (`[kod] engine/mod.rs:10975` → `provider.rs:326-339`); the bridge resolves it as the session key (`[tb] http.ts:218-227`). Affinity works: all 7 main turns stay on tab …344. |
| 15:22:15 | `turn.plan anon-f1f4204a38feca7e SEED historyLen:1` → tab **1069586345** created | **Memory fact extraction**, fired 2 ms after your main turn finished (`[kod] engine/mod.rs:9331-9337` → `maybe_extract_continuously`). kod sent it via the legacy `generate()` path, which carries no session id (`[kod] provider.rs:189-204`), so the bridge minted a fresh ephemeral `anon-<16hex>` (`[tb] registry.ts:104`, `bridge.ts:119-122`). The fact-JSON reply (`[{"type":"fact",…}]`, 2062 chars) **was stored** (see §2 RC3). |
| 15:22:26 | `anon-eebb0365ea18143b` → reuses tab …345, reply `[]` | **Sharpshooter decision extraction** (`[kod] engine/mod.rs:13110+`): "return decisions from the user prompt, or `[]` if none" (`[kod] sharpshooter.rs:222-247`). The `[]` is a *correct* empty result, not a failure. |
| 15:22:30 | `anon-ac64a0c2f762baf1` → tab …345, 17 222 chars, reply "Warm acknowledged…" | **`KodEngine::prewarm`** (`[kod] engine/mod.rs:2340-2415`), fired when you typed the first character of your next message (`[kod] kod-tui/src/main_loop.rs:5895-5909`). It re-sends the cacheable system-prompt head plus a literal `"warm"` user message with `max_tokens: 1` and `session_id: None` — again a fresh `anon-*`. The tab submission alone took **58 s** (`submitted in 58285ms`); kod gave up waiting after 5 s (`timeout(Duration::from_secs(5), …)`, `:2410-2414`) — but the bridge turn kept running, occupying tab …345 for over a minute. |
| 15:22:57 | `anon-065edeb4539142fa` → tab **1069586346** created | Second fact extraction (after main turn 7). Tab …345 was still busy with the abandoned-but-running warm turn, so the worker found no free managed tab and auto-created another one (`[tb] background.js:442-452`). **This is the tab ratchet in one line.** |
| 15:23:32 | warm turn finally completes | 62 s of tab occupancy for a response kod discarded after 5 s. |

**Direct answers:**

1. *"warm creates a new session and the next messages go to another tab"* — Correct observation, wrong culprit direction: it isn't the warm turn *stealing* your conversation's tab; it's that warm (and every background request) runs under a **throwaway session** that necessarily lands on *some other* tab. Your real conversation was never at risk of receiving the warm probe (that's by design — see `[kod] engine/mod.rs:2400-2404`), but on a stateful tab backend the design defeats its own purpose: the warmed tab is *not* the tab your next real turn uses, because your main session keeps its own tab (…344) the whole time. The probe's TTFT benefit would only ever apply to a session's **very first** turn.
2. *"is fact collection really working?"* — Yes. Both fact batches in your log came back well-formed, were content-hash-deduped and stored as episodic memories in the redb long-term store with `metadata.session_id` bookkeeping (`[kod] engine/mod.rs:13076`, `manager.rs:479-580`), and retrieval *does* inject them into later prompt builds via hybrid search (`[kod] router.rs:1132-1164`, `manager.rs:760-770`). The visibility problem: nothing in the TUI/bridge log tells you this happened. (Verification recipe in §7.)
3. *"every turn is too much"* — Agreed, and it's a bug, not a design choice: the documented cadence ("every 5 turns") is dead code (`[kod] retention.rs:176-191`), and the sharpshooter adds a second per-turn request. In your log that's **2 extra LLM turns per main turn** (plus warm on demand).
4. *"tab leakage"* — Three distinct mechanisms, all confirmed: pool growth to peak concurrent sessions (your 3-tab incident), the TTL-eviction release mismatch (permanent occupancy — didn't fire in this short log, but is guaranteed on any session idle >30 m), and zero tab retirement (released tabs are never closed).

---

## 2. Root causes (with receipts)

### RC1 — kod background traffic carries no session identity → fresh `anon-*` per request
- `[kod] provider.rs:189-204` — `text_request()` (the legacy `generate(&str)` path used by **all** background features) builds a single-user-message `LlmRequest` and never sets any session field.
- `[kod] request.rs:162-169` — `CompletionRequest.session_id` exists, but is stamped at exactly **one** site: `build_grounded_request` (`[kod] engine/mod.rs:10975`), i.e. main turns only.
- `[tb] http.ts:218-227` — no `x-session-id` header / `user` field / `metadata.session_id` ⇒ session key `null`.
- `[tb] bridge.ts:119-122` + `registry.ts:102-105` — `null` ⇒ `createEphemeral()` mints `anon-${randomId(8)}` (16 hex — exactly the ids in your log). The row is deleted and its tab released at end of turn (`[tb] bridge.ts:187-193`), so per-request it "recycles" — but every request still pays a full bind/SEED cycle, may force a new tab when a previous background turn is still streaming (your 15:22:57 event), and lands on *whatever tab is free*, making background chat history scatter across tabs.

### RC2 — prewarm is a heavyweight, un-cancellable LLM turn on tab backends
- Payload: the full cacheable prompt head (~17 k chars in your log) + `"warm"` user message (`[kod] engine/mod.rs:2362-2382`).
- kod abandons it after 5 s (`:2410-2414`), but nothing cancels the server side: the bridge turn runs to completion (58 s submit + stream), holding a generation-gate slot and a tab the whole time.
- `max_tokens: 1` is unenforceable on a browser tab — hence the 237-char "Warm acknowledged. I don't have any tool…" reply.
- Net effect on a tab backend: pure cost, no benefit (RC1 analysis), plus it is the single biggest driver of concurrent background load that forces new tab creation.

### RC3 — extraction cadence: `every_n_turns` is dead, sharpshooter is per-turn
- `[kod] retention.rs:185-191` — `is_due(new_messages)` checks only `new_messages >= min_new_messages` (4). `every_n_turns: 5` is never consulted (tests pin the struct, nothing calls the field).
- A tool-using main turn appends user + assistant + tool messages ≥ 4 almost immediately ⇒ extraction runs after effectively every main turn (`[kod] engine/mod.rs:13045` uses `RetentionCadence::default()`).
- `maybe_extract_decisions` (`[kod] engine/mod.rs:13110+`) fires after every turn whose last user prompt is eligible (≥16 chars, non-slash-command; `[kod] sharpshooter.rs` `prompt_is_eligible`). No cadence counter, no config gate.
- Also note: both hooks reload `KodConfig::load_default()` from disk per call (`:13017`) — fine functionally, but the knobs they *should* read don't exist yet.

### RC4 — bridge reuses dirty tabs without resetting the chat (context bleed)
- `[tb] engine.ts:296-298` — `if (plan.plan === "SEED") { if (row.chain.length > 0 || row.pendingReset) await resetOrThrow(...) }`. A brand-new/ephemeral row has an empty chain, so **no reset** — the tab still contains the previous session's conversation, and the seed prompt is appended into it.
- Your log ran this path repeatedly: tab …345 hosted fact-extraction session → sharpshooter session → warm session back-to-back, each seeing the previous one's content in the tab (harmless here because each SEED prompt is self-contained, but it is unbounded context pollution and a correctness hazard for any client whose prompt assumes a clean chat).
- The reset machinery exists and works (`[tb] engine.ts:299-302` for `RESET_RESEED`; injector new-chat click/shortcut/navigation `[tb] injector.js:2523-2564`) — it's just not triggered on this path.

### RC5 — TTL eviction can never free its tab (hard leak)
- `[tb] bridge.ts:70-73` — `onEvict` → `this.pool.release(\`evict:${sessionId}\`, 3_000)`.
- `[tb] background.js:787-789` — `handleRelease` does `sessionTab.get(m.sessionId)` with the **raw** id; BIND stored the raw id (`[tb] pool.ts:186`). The `evict:`-prefixed lookup always misses ⇒ `st.sessionId` stays set ⇒ `allocateTab` skips the tab forever (`[tb] background.js:426-434`).
- Compounding: if the expired session id is later re-used by the client, the registry creates a fresh row, the worker's stale mapping gets overwritten by a new BIND, and the old tab is orphaned permanently — two leaks from one expiry.

### RC6 — no tab retirement; `--warm-tabs` is a stub
- RELEASE (`[tb] background.js:787-798`) only marks the tab `ready`/unoccupied — the tab (and its whole conversation) stays open indefinitely.
- Tabs are closed only on 3-strike submit failures (`[tb] background.js:1090-1104`) or manual close. There is no `--max-tabs`, no idle reaper.
- `--warm-tabs` is parsed (`[tb] config.ts:142-147`), shipped in HELLO config (`pool.ts:131`), stored (`background.js:55`) — and **never read anywhere** (verified: the only reference in `extension/` is the declaration). `warm_tabs:0` in your log is just a config echo.

---

## 3. kod-side fixes

### WS-A — One stable background session per engine (Phase 2)

**Goal:** every background LLM request carries a stable per-engine session key (`bg-<uuid>`), so the bridge keeps exactly **one** background chat session (⇒ one background tab, bound once, reused — dirty-SEED reset from WS-D keeps it correct across TTL expiry).

**Design (minimal blast radius — no changes to the 12 call sites' signatures):**

1. Mint once per engine, next to the existing session id (`[kod] engine/mod.rs:3821`):
   ```rust
   background_session_id: kod_types::SessionId::new(),  // rendered "bg-<uuid>"
   ```
   (`define_id!` supports custom prefixes — `[kod] kod-types/src/ids.rs:66`.)
2. Stamp at the provider layer, not per call site:
   - `OpenAICompatProvider` gains `default_session: Option<String>` + `.with_default_session(id)`.
   - `text_request()` (`provider.rs:189-204`) merges it into `config.extensions["openai"]["user"]` exactly like `request_from_completion` already does for explicit ids (`provider.rs:326-339`) — reuse that helper.
   - `request_from_completion` falls back to `default_session` when `req.session_id` is `None` (covers prewarm and `JudgmentClient`, which already use `complete()` with `session_id: None` — `[kod] judgment.rs:585-603`).
   - Main turns are unaffected: they always set `session_id: Some(...)` (`engine/mod.rs:10975`), which takes precedence.
3. Opt-in gate so normal (non-tab) providers are untouched: new `EndpointConfig` flag
   ```toml
   [llm.endpoints.<name>]
   tab_bridge = true        # default false
   ```
   (`[kod] kod-config/src/llm.rs:390-460`). `provider_setup` (`[kod] kod-core/src/provider_setup.rs:46-119`) applies `with_default_session` only when the flag is set. This is the same flag WS-C keys off.
4. **Serialization + 409 tolerance (important — naive version breaks):** the bridge rejects same-session overlap with `409 session_busy` (`[tb] bridge.ts:130-138`) and never queues it. Today's background calls are mostly sequential (extraction → sharpshooter are awaited in order), but prewarm-abandoned turns can still hold the session (62 s in your log). So:
   - Wrap background provider calls in a per-engine `tokio::sync::Mutex` (or a small semaphore) so kod itself never overlaps them;
   - Treat `409 session_busy` as retryable with short backoff (e.g. 2 attempts, 2 s / 8 s) for background-class requests only — main turns keep today's fail-fast semantics.

**Audit list of everything that becomes attributable** (all currently sessionless — full inventory with line numbers in Appendix A): memory extraction, sharpshooter extraction + consolidation, compaction summaries, background-shell summaries, auto plan, turn summaries, swarm decompose/brief/merge, Jev judges, unexpected-stop diagnosis, prewarm.

**Tests:** (a) `text_request` stamps `user` from `default_session` when flag set, and not otherwise (mirror `session_id_travels_as_openai_user_extension`, `provider.rs:1316-1335`); (b) explicit `req.session_id` wins over default; (c) 409-backoff unit test.

### WS-B — Extraction cadence becomes real and configurable (Phase 1)

**Goal:** "every turn" becomes a policy choice, defaulting to the cadence the code always claimed to have.

1. **Fix the dead field.** `RetentionCadence::is_due` (`[kod] retention.rs:185-191`) gains a turn count:
   ```rust
   pub fn is_due(&self, new_messages: usize, new_user_turns: usize) -> bool {
       new_user_turns >= self.every_n_turns && new_messages >= self.min_new_messages
   }
   ```
   The retention cursor (`[kod] engine/mod.rs:13013-13091`) already tracks the tail; add a user-turn counter alongside the message count.
2. **Config surface** (`[kod] kod-config/src/memory.rs:55-106`):
   ```toml
   [memory]
   extraction_mode          = "continuous"  # "off" | "continuous" | "shutdown"
   extraction_every_n_turns = 5             # was dead; now enforced
   extraction_min_messages  = 4
   decisions_enabled        = true          # sharpshooter gate
   decisions_every_n_turns  = 1             # its own cadence counter
   ```
   Wire-up points: `maybe_extract_continuously` (`engine/mod.rs:13045` — build the cadence from config instead of `RetentionCadence::default()`), `maybe_extract_decisions` (`:13110+` — add a per-transcript decisions cursor mirroring the retention cursor), shutdown extraction stays behind `extract_on_shutdown` (`:13461`, default false) and `extraction_mode = "shutdown"`.
3. **Make it visible (answers "is it really working?"):** emit one `tracing::info!` per extraction with `stored = n, deduped = m, transcript = key`; surface the same numbers in the session log next to the existing `MemoryWrite` entries; and document `kod memory list` (`[kod] kod-cli/src/commands/memory.rs` — the CLI already exists) as the inspection command in the TUI's `/memory` help text.
4. **Default judgment:** with `every_n_turns = 5` enforced, your log's 2 extractions in 7 main turns would become 1, and the sharpshooter stays per-turn but is now individually switchable. Users who want the old intensity set `extraction_every_n_turns = 1`.

**Tests:** cadence boundary (4 msgs/4 turns ≠ due; 5th user turn due); `extraction_mode = "off"` fully silent; decisions cursor counts only eligible prompts; config parsing round-trip.

### WS-C — Prewarm policy for tab backends (Phase 1)

**Goal:** stop paying 17 k chars + ~60 s tab occupancy for a probe whose benefit doesn't apply.

1. **Auto-off on tab providers:** new top-level knob
   ```toml
   [llm]
   prewarm = "auto"   # "auto" | "on" | "off"
   ```
   `"auto"` (default) ⇒ prewarm enabled, **except** when the active endpoint has `tab_bridge = true` (WS-A's flag) ⇒ off. Enforcement point: the TUI trigger (`[kod] kod-tui/src/main_loop.rs:5895-5909`) checks before spawning; `engine.prewarm` keeps a hard early-return for the same condition (defense in depth).
2. **If a user explicitly forces `prewarm = "on"` with `tab_bridge = true`:** the probe is stamped with the background session id (falls through `request_from_completion`'s new `default_session` fallback, WS-A step 2), so it warms the *one persistent background tab* instead of minting a new session each keystroke — and it's serialized by the background mutex so it can't force a second tab.
3. **Fix the abandonment illusion (comment fix + optional enhancement):** the comment at `[kod] engine/mod.rs:2400-2404` ("the bridge releases ephemeral tabs after the turn, so the warmed tab is recycled… the real turn") is wrong for ongoing sessions — the real turn has its own bound tab. Correct the comment; and note near the 5 s timeout that on a bridge backend the turn continues server-side regardless.
4. *(Optional, bridge-side, Phase 3)*: honor `metadata: {"tab_bridge": "ping"}` as a no-model-turn liveness probe (PING the bound tab, return a canned completion) so a future "prewarm = probe" mode costs ~0. Not required for the fix; flagged as a clean extension point in `runTurn`'s plan selection.

---

## 4. tab-bridge-side fixes

### WS-D — Never SEED into a dirty tab (Phase 0 — correctness)

**Goal:** the bleed mechanism (`[tb] engine.ts:296-298`) is closed regardless of who reuses the tab — ephemeral churn, TTL rebind, service-worker restart, or a second client.

1. **Worker-side dirtiness tracking** (`[tb] extension/background.js`):
   - `st.dirty = true` when a tab completes a turn (SEND→TURN_DONE path) — the worker already sees every turn;
   - cleared on successful RESET completion and on fresh tab creation;
   - **unknown ⇒ dirty** (set `dirty = true` when re-registering a managed tab after MV3 service-worker restart or injector reconnect — the worker can't know what the page shows).
2. **BOUND observation carries it** (`[tb] pool.ts` observation payload + `protocol.ts`): `BOUND { sessionId, tabId, dirty }`.
3. **Engine reset condition** (`[tb] engine.ts:296-298`):
   ```ts
   if (plan.plan === "SEED") {
     if (row.chain.length > 0 || row.pendingReset || tab.dirty) await resetOrThrow(req.adapter, tab);
   ```
   (`RESET_RESEED` already always resets; `INJECT_*` semantics unchanged — they only run on a verified chain.)
4. **Escape hatch flag** `--reset-on-seed=auto|always|never` (default `auto` = the dirty-driven behavior above; `always` ≈ per-SEED reset for paranoid setups; `never` restores today's behavior).

**Why this is safe:** a fresh tab reports `dirty = false` → zero extra resets for the common path; a reused tab pays one new-chat click (~1 s, already implemented with fallbacks) instead of silently appending into someone else's conversation.

**Tests:** bind-dirty→SEED triggers reset; fresh tab→SEED doesn't; rebind after MV3 restart (unknown) resets; `never` flag preserves current behavior.

### WS-E — Tab pool hygiene (Phase 0 hotfix + Phase 3 hardening)

**E1. Fix the eviction release (Phase 0, one line):** `[tb] bridge.ts:72` → `this.pool.release(sessionId, 3_000)` (drop the `evict:` prefix). For cross-version safety, `handleRelease` (`[tb] background.js:787`) additionally strips a leading `evict:` if present. *This is the single highest-value diff in the plan: without it, every TTL-expired session permanently occupupies its tab and `--ttl=30m` actively manufactures leaks.*

**E2. `--max-tabs <n>` (Phase 3, default `4`, `0 = unbounded`):** in `allocateTab` (`[tb] background.js:421-452`), when creation is indicated and `managedTabs.size >= max`: close the least-recently-released dirty tab first (LRU among `ready && !sessionId`), then create. If nothing is closable → `BIND_FAILED no-tab-available` (bridge already waits out the bind deadline and surfaces a clean 503/429 — `[tb] pool.ts:190-195`).

**E3. `--tab-idle-close <dur>` (Phase 3, default `15m`, `0 = never`):** worker-side sweep (use `chrome.alarms` — MV3 service workers die otherwise) closing tabs that are `ready && !sessionId` and idle beyond the threshold. Keeps steady-state tab count ≈ peak concurrent sessions, not lifetime-unique sessions.

**E4. Implement `--warm-tabs` or delete it (Phase 3):** recommend implementing (it's spec'd as milestone M4, `[tb] docs/SPEC.md:387,414`): on `HELLO_OK`, if `poolConfig.warmTabs > 0`, pre-create up to N managed tabs (loop `ensurePoolWindow` + create + wait-ready). Alternative: remove the flag so the usage text stops promising it. Today it silently does nothing either way.

**E5. Ephemeral traffic never grows the pool (Phase 3):** if the request is ephemeral *and* carries `metadata.tab_bridge_class = "background"`, bind with a `noCreate` hint: `allocateTab` skips step 3 (creation) and returns `BIND_FAILED no-tab-available` immediately → the bridge maps it to `429` with a short `Retry-After`. With WS-A in kod this path should never trigger; it's the seatbelt that guarantees background load can never ratchet the tab count even if a future kod regression re-anonymizes background traffic.

**E6. *(kod-side companion, optional Phase 3)*:** on engine shutdown, best-effort `DELETE /v1/sessions/<main>` and `DELETE /v1/sessions/<bg>` (endpoint already exists, `[tb] http.ts:453-464`, and releases the tab with the raw id) so a TUI exit frees both tabs immediately instead of waiting for TTL.

---

## 5. Sequencing, effort & risk

| Phase | Repo | Scope | Effort | Risk |
|---|---|---|---|---|
| **P0 — stop the bleeding** | tab-bridge | E1 (evict-release fix) + WS-D (dirty-SEED reset, `--reset-on-seed`) | ~2 small diffs, both mechanically testable | Low — reset path is battle-tested (`RESET_RESEED`); `auto` keeps fresh-tab behavior identical |
| **P1 — kill the per-turn tax** | kod | WS-B (cadence fix + config) + WS-C (prewarm auto-off + `tab_bridge` flag) | Medium: 1 logic fix, ~6 config keys, 2 gate points | Low — defaults move from "de facto every turn" to documented every-5-turns; opt-outs preserved |
| **P2 — one background session** | kod | WS-A (default_session stamping + bg mutex + 409 backoff) | Medium: provider-layer change + engine field + wiring | Medium — Mitigations: opt-in via `tab_bridge = true`; explicit `session_id` precedence keeps main turns byte-identical; 409 backoff covers overlap |
| **P3 — pool hygiene & polish** | tab-bridge (+kod E6) | E2–E6, optional prewarm ping | Medium | Low-Medium — each flag independently defaultable to today's behavior (`--max-tabs 0`, `--tab-idle-close 0`) |

**Deliberate non-goals:** no change to the SEED/INJECT/RESET chain scheme (`CHAIN_SCHEME 3`), no per-tab concurrency change (the global gate at 2 matches DeepSeek's "Another message is being generated" limit — `[tb] config.ts:22-26`), no client-visible API break (new flags are optional, new metadata keys optional).

---

## 6. Proposed config surface (summary)

**kod `config.toml` (new/changed keys):**
```toml
[llm]
prewarm = "auto"                 # auto | on | off   (auto = off when active endpoint is tab_bridge)

[llm.endpoints.<name>]
tab_bridge = true                # default false — enables bg-session stamping + prewarm auto-off

[memory]
extraction_mode          = "continuous"   # off | continuous | shutdown
extraction_every_n_turns = 5              # becomes real (was dead code)
extraction_min_messages  = 4
decisions_enabled        = true
decisions_every_n_turns  = 1
```

**tab-bridge `serve` (new flags, defaults preserve-or-improve):**
```text
--reset-on-seed=auto        # auto | always | never   (default auto — closes the bleed)
--max-tabs=4                # 0 = unbounded           (P3)
--tab-idle-close=15m        # 0 = never               (P3)
--warm-tabs=<0..8>          # becomes real            (P3)
```

---

## 7. Post-fix verification (rerun your exact scenario)

With `tab_bridge = true`, defaults elsewhere, a fresh bridge and one kod conversation exercising ~7 tool-using turns:

| Check | Before (your log) | After |
|---|---|---|
| Tab creations for the scenario | 3 (…344 main, …345 first-anon, …346 second-anon) | **2** (main + background), created once each |
| `turn.plan SEED empty-chain` count | 6 (1 main + 5 background) | 2 (1 main + 1 background, at first use) |
| Background session ids | 4 distinct `anon-*` | 1 stable `bg-*` for extraction/sharpshooter/warm-if-enabled |
| Fact extractions in 7 turns | 2 (de facto every turn) | 1 (`every_n_turns = 5`) |
| Sharpshooter calls | per eligible turn | unchanged count, but gated by `decisions_enabled` |
| Warm turn | 17 k chars, new session, 62 s tab occupancy, response discarded | none (`prewarm = "auto"` + `tab_bridge = true`) |
| Tab left occupied after TTL expiry of an idle session | forever (evict-prefix bug) | released within the 3 s release window |
| Rebound tab content | previous session's chat still present | reset (new chat) before first SEED |

**Manual fact-collection audit** (`is it really working?`): run `kod memory list` after a session — expect the `auto-fact`/`auto-preference`/`auto-decision`/`auto-pattern` entries from `extract.rs:44-53` with timestamps matching the bridge's `turn.emitted` fact-JSON replies; the new `stored/deduped` log lines give the same numbers inline.

---

## Appendix A — kod sessionless request inventory (everything RC1 makes anonymous today)

All of these become `bg-<uuid>`-stamped under WS-A without signature changes:

| # | Call site | Purpose | Path |
|---|---|---|---|
| 1 | `[kod] kod-memory/src/extract.rs:160` (via `engine/mod.rs:13062`, `:13314`) | memory fact extraction | `generate()` |
| 2 | `engine/mod.rs:13156` | sharpshooter decision extraction | `generate()` |
| 3 | `engine/mod.rs:13255` | sharpshooter consolidation (`.kod/decisions/*.md`) | `generate()` |
| 4 | `compaction_dispatcher.rs:932` | compaction summarization | `generate()` |
| 5 | `engine/mod.rs:2234` (`spawn_compaction_summary`) | background compaction summary | `generate()` |
| 6 | `engine/mod.rs:5508` | background-shell job summary | `generate()` |
| 7 | `engine/mod.rs:3976` | auto plan generation | `generate()` |
| 8 | `engine/mod.rs:8879` | fallback turn summary | `generate()` |
| 9 | `engine/mod.rs:10657` (`stream_summary`) | streamed summary | `generate()` |
| 10 | `kod-swarm/src/swarm_runner.rs:1871, 1957, 2066` | swarm decompose / subtask brief / merge | `generate()` |
| 11 | `engine/mod.rs:2375-2405` | prewarm probe (`session_id: None` explicit) | `complete()` |
| 12 | `kod-provider/src/judgment.rs:585-603` (`JudgmentClient.ask`) | all Jev judges + unexpected-stop diagnosis | `complete()` |

Session-carrying today (no change): main turns (`engine/mod.rs:9753-9761` → `:10975`) and swarm agents' main turns (per-agent ids via `session_id_for_holder`, `engine/mod.rs:14018-14025`).

## Appendix B — Your log's non-issues (no action needed)

- **`WebAssembly.instantiateStreaming … MIME type` (×8)** — the DeepSeek page's own WASM loader; unrelated to bridge turns.
- **`Datadog Browser SDK loaded more than once`** — site-side analytics, noise.
- **`submitted in 58285ms` / `first fragment 61491ms`** — real signal, but of RC2 (the 17 k-char warm probe on a paste-mode=file tab), not of a bridge bug.
- **`SEND … was sent 30s ago with no FRAGMENT`** — the worker's stall heuristic firing correctly on the slow warm turn.
- **`submit unverified by DOM (background tab?)`** — expected for background tabs; the SSE hook confirms delivery.

## Appendix C — Version notes

- Plan written against **kod @ `956ae31`** and **tab-bridge @ `7a3af15`** (package version 1.2.46, matching the `extVersion: 1.2.46` in your log). The one commit past the 1.2.45 release tag (`fix(injector): DOM continuation after trusted Continue click`) does not touch session/tab lifecycle, so all line references above apply to your running build.
- The kod-side doc comment at `engine/mod.rs:2400-2404` and the bridge-side comment at `bridge.ts:189-190` ("instead of leaking one tab per sessionless request") both encode assumptions the other repo doesn't guarantee — WS-A/WS-C/WS-D replace them with mechanisms, not comments.
