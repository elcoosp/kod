# Borrowing from oh-my-pi: the delta brainstorm for kod

*Companion to `Kod_Harness_Engineering_Review.pdf` (PDF observations → P0–P9) and `kod_borrow_from_jcode.md` (jcode → B1–B12). This document assumes those changes are already implemented and answers one question: **what does oh-my-pi (can1357/oh-my-pi, "omp") do that is still worth stealing?***

All dump references are `D<line>` into `can1357-oh-my-pi-8a5edab282632443.txt`. kod references are `crate/file.rs` from kod's own dump.

---

## 0. TL;DR — top 12 deltas on top of P0–P9 + B1–B12

| # | Borrow | What it gives kod | Delta vs. already-planned | Effort | Lands in |
|---|--------|-------------------|---------------------------|--------|----------|
| 1 | **Cache-coherent transcript editing** (digest/version protocol + longest-stable-prefix sync) | In-place history edits (prune, rewrap, image-strip) stop nuking the provider KV cache | NEW — P0/B-borrows cover prefix + breakpoints, not *transcript-tail* mutation coherence | M | `kod-core` engine + `kod-provider` |
| 2 | **Supersede pruning + shake** (mechanical, no-LLM context reduction) | Fills the "nothing between do-nothing and FIFO-drop" gap without a summarizer | NEW — jcode ladder summarizes; this elides | M | `kod-core/context.rs` |
| 3 | **Cache-warm-suffix guard** | Never mutate a tool result whose suffix is still warm cached prefix (cacheWrite premium > savings) | NEW — the piece PDF-P0 missed | S | same |
| 4 | **Provider-anchored token accounting** | Context size from provider's own last settled `usage` + tokenize only the tail; O(turn) not O(transcript) | UPGRADE of P0 cache accounting | S | `kod-core/state.rs` |
| 5 | **Ordered multi-method compaction dispatcher** with fall-through + **speculation lead band** | Compaction becomes a preference list (remote → shake → handoff → soft) with fail-over, pre-computed below threshold | UPGRADE of jcode 3-tier ladder | S/M | `kod-core/engine.rs` |
| 6 | **Shell output minimizer** (declarative TOML pipelines) | `git log`/pytest/cargo output rewritten into information-preserving digests instead of chopped at 16 KiB | NEW — kod truncates; omp reduces | M | new `kod-minimize` |
| 7 | **`xd://` lazy tool mounting** | Progressive disclosure as a *protocol* via `read xd://<tool>` instead of a bespoke `tool_search` tool | UPGRADE of PDF-P3 | S/M | `kod-tools/registry.rs` |
| 8 | **Speculative read execution** (admit → evidence → validate → commit) | Hide read latency behind model generation; commit only when digests prove TOCTOU-clean | NEW | M (read slice) | `kod-core` + `kod-tools` |
| 9 | **Thinking-loop guard + replay-safe stream retry** | Kill degenerate reasoning loops mid-stream; retry only while nothing meaningful was emitted | NEW — kod has Jev gate, no runaway detector | M | `kod-provider` |
| 10 | **Reversible keyed secret placeholders** | Model edits files containing secrets without ever seeing them; deobfuscated at execution | NEW — closes the tool-output→provider→edit-args hole PDF-P7 half-covers | M | `kod-types::redact` + tools |
| 11 | **Advisor emission guard + delta-split feed** | The policy layer cross-model review (P6) needs or it floods the transcript (real incident: 309 advise calls) | UPGRADE of P6 | M | `kod-swarm` |
| 12 | **WorkPool + park/revive agent lifecycle** | Swarm members become reusable, addressable, parkable workers with per-batch output schemas | UPGRADE of B1 swarm | M | `kod-swarm` |

Everything else (≈40 more items) is in the sections below, ordered by subsystem.

---

## 1. What oh-my-pi is, and why it matters for kod

oh-my-pi is Stencil Labs' fork/extension of badlogic's *pi* coding agent: a Bun/TypeScript monorepo (`packages/agent` = host-agnostic agent core, `packages/ai` = provider stack, `packages/coding-agent` = the product) with a large Rust native layer behind napi-rs (`crates/pi-shell` — an in-process bash interpreter with uutils coreutils compiled as builtins, `pi-edit`, `pi-ast`, `pi-diff`, `pi-iso`, `pi-vcs`, `pi-natives`). Version 18.x, roughly 6,000 files in the dump, with an unusually disciplined bench culture (`bench/` pins perf invariants like stable-prefix build cost, streaming throughput, session-tree navigation).

The philosophy differs from both kod and jcode in three ways that matter:

1. **The transcript is a cache.** The entire context pipeline is built around one invariant: bytes already sent to the provider are never re-serialized differently. Where kod guards the *system prefix* (PromptPlan, golden-prefix tests) and jcode guards *breakpoints*, omp guards **every message** — with a digest/version cache-coherence protocol that makes in-place history editing safe (see §2).
2. **Reduction before summarization.** Most context shrinking is mechanical: supersede pruning of stale tool results, surgical elision of heavy text blocks ("shake"), bitmap-frame imaging for vision models. An LLM summarizer is the *last* rung, not the first.
3. **Everything is observable and adversarially tested.** Compactions carry no-reduction guards; persistence has crash invariants; speculative execution validates against on-disk digests before commit; the minimizer falls back to raw output on anything ambiguous.

 kod takeaway: kod's cache discipline is real but covers a narrow window (Identity + RepoMap). omp shows what the same discipline looks like when applied to the whole transcript, the tool surface, and even model-stream output.

**Caveats inherited from the dump:** several core files (`packages/agent/src/agent.ts`, `agent-loop.ts`, `speculative-execution.ts`, `packages/snapcompact/src/snapcompact.ts`, the minimizer `engine.rs`, coding-agent `agent-session.ts`) are referenced but absent from the dump. Their behavior was reconstructed from tests, benches, call sites, and the modules that do exist. Where confidence is lower, the text says so.

---

## 2. C1 — Cache-coherent transcript editing (the crown jewel)

### 2.1 The problem kod has today

kod's golden-prefix tests pin the *system* prefix. The transcript tail is a different story: `compact_history_for` drops oldest turns, tool results are capped at 16 KiB, memory blocks re-render per turn — and none of these mutations carry any notion of "which bytes did the provider already cache." After any mutation, the next request re-bills the whole transcript (on Anthropic without transcript breakpoints — P0 fixes the breakpoints; nothing yet fixes the *coherence* of mutated content).

### 2.2 How omp does it (D140737–141194, D143773–143866)

Three contracts, enforced by tests and benches:

**StablePrefix.** `{systemPrompt[], tools[]}` is frozen once per turn-set and served byte-identical. Each tool gets an identity key `name\0desc\0strict\0wireName\0intent-fn-id\0params-objId\0customFormat-objId\0examples-objId` — this catches settings-driven schema getters that return *new objects with equal content* under stable references. Fingerprint = 32-bit `(h<<5)-h+c` over the JSON of prompt + tool fields (D140783).

**AppendOnlyLog.** Messages only grow. `syncMessages(normalized)` each turn handles exactly three cases:
- **append** — the normal turn: reuse everything;
- **compaction** — the array shrank: clear + replay (this is the one sanctioned wholesale rewrite);
- **in-place rewrite** (prune/shake/image-strip re-render): find the **longest byte-stable prefix** via per-message digests, truncate the log to it, re-append only the diverged tail. The doc comment cites issue #3406: without this, llama.cpp re-prefilled ~40k tokens every turn when any extension touched one message (D141077–141083).

**Digest/version cache coherence.** Every message carries a lazily-computed FNV-1a-32 digest over *every serialized field* (role, content, providerPayload, toolCalls incl. snake_case aliases, toolCallId, toolName, isError, id), memoized in a `WeakMap<message, {version, digest}>` (D141170–141193). A version tag (`kEstimateVersion` symbol) lives on the message itself; **owners of in-place mutation (prune, shake, image-strip) MUST call `invalidateMessageCache`**, which bumps the version and pokes cross-package invalidators (the convert memo, the tokenizer memo). A **settle gate** (D143845) only caches estimates for assistants with `usage.totalTokens > 0` and stopReason ∉ {aborted, error} — streaming assistants are mutated in place, so caching them would freeze mid-stream counts.

Bench culture: `stable-prefix.bench.ts` asserts steady-state build is µs with `changed = 0`; `llm-assembly.bench.ts` pins convert/estimate ≥10× speedup and append-growth O(suffix) (D140678, D344979).

### 2.3 Design for kod

```rust
// kod-core/src/context.rs (new module: transcript_coherence.rs)

/// FNV-1a-32 over every field that reaches the wire for one message.
/// Mirrors omp's digest: missing a field = silent cache corruption.
pub struct MessageDigest(u32);

impl MessageDigest {
    pub fn of(msg: &ChatMessage, wire: &WireShape) -> MessageDigest {
        let mut h: u32 = 2166136261;
        let mut feed = |b: &[u8]| { for &byte in b { h = (h ^ byte as u32).wrapping_mul(16777619); } };
        feed(msg.role.as_str().as_bytes());
        for block in &msg.content { feed(block.wire_bytes(wire)); }
        if let Some(tc) = &msg.tool_calls {
            for c in tc { // canonical order: name, id, args-json (canonicalized)
                feed(c.name.as_bytes()); feed(c.id.as_bytes());
                feed(canon_json(&c.arguments).as_bytes());
            }
        }
        if let Some(r) = &msg.tool_result {
            feed(r.call_id.as_bytes()); feed(r.is_error.then_some(1u8).as_slice());
            feed(r.content_wire_bytes(wire));
        }
        MessageDigest(h)
    }
}

/// Version tag stored ON the message. Mutation owners MUST bump it.
/// kod already has a precedent: RepoMapCache invalidation discipline.
#[derive(Default)]
pub struct CoherenceVersion(u64);

pub trait TranscriptMutator {
    /// Contract: after ANY in-place edit (prune, rewrap, image strip,
    /// decision re-render), call this. Debug builds verify via digest diff.
    fn invalidate_message_cache(&mut self, msg: &mut ChatMessage);
}

/// The omp syncMessages equivalent: run after every mutation, before
/// the next build_grounded_request.
pub enum SyncOutcome {
    Append,                       // nothing to do: cache stays warm
    Compaction,                   // clear + replay (full re-bill, sanctioned)
    InPlaceRewrite { stable_prefix: usize }, // re-bill only the tail
}

pub fn sync_messages(log: &mut Vec<ChatMessage>, digests: &DigestMemo) -> SyncOutcome {
    if log.len() < digests.recorded_len() { return SyncOutcome::Compaction; }
    let mut stable = 0;
    for (i, msg) in log.iter().enumerate() {
        if Some(MessageDigest::of(msg, WireShape::current())) == digests.get(i) { stable = i + 1; } else { break; }
    }
    // longest byte-stable prefix = `stable`; provider cache survives to there
    SyncOutcome::InPlaceRewrite { stable_prefix: stable }
}
```

Integration points in kod:
- `engine.rs::build_grounded_request` already splits at `"## Volatile suffix"`; add the digest memo next to `strip_conversation_tail`.
- `compact_history_for` stays, but the *surviving* tail gets digest-verified so `record_turn_for` appends are O(suffix).
- Wire the digest memo invalidation into the same places kod already invalidates RepoMapCache.

**Test to port:** "prune one 8 KiB tool result in the middle of a 40-turn transcript → next request differs from the previous in exactly [mutated turn, tail]" — a byte-level golden test in the `golden_prefix.rs` style, but for messages.

### 2.4 Provider-anchored transcript token accounting (D145157–145273)

omp answers "how big is the context?" from the provider's own last settled usage, then tokenizes only the tail:

```
estimate = anchor.usage_context_tokens            // provider-charged: system+tools+messages
         + Σ count_message(tail after anchor)     // local estimate of what's new
```

Anchor = newest assistant message with stopReason ∉ {aborted, error} and usable usage, *after the last compaction boundary* (older usage describes prompts no longer sent). Callers must branch: anchored totals include system+tools (the provider charged them); unanchored sums are message-only.

For kod: once P0 adds `cache_read`/`cache_creation` to `TokenUsage`, `calculate_context_tokens(usage) = prompt + cache_read + cache_creation` becomes exact. This turns the compaction trigger (§4) and `/debug tokens` into O(one turn) work instead of O(transcript) char arithmetic — and replaces `budget.rs`'s 4-chars-per-token heuristic for the *history* section (keep the heuristic only for never-sent candidate content).

```rust
// kod-core/src/state.rs
pub struct ContextGauge {
    anchor: Option<(usize /*msg idx*/, u64 /*context tokens from usage*/)>,
}

impl ContextGauge {
    pub fn observe_settled(&mut self, idx: usize, usage: &TokenUsage) {
        if usage.prompt > 0 { self.anchor = Some((idx, usage.prompt + usage.cache_read + usage.cache_creation)); }
    }
    pub fn estimate(&self, tail: impl Fn() -> u64) -> u64 {
        self.anchor.map(|(_, base)| base + tail()).unwrap_or_else(|| /* message-only sum */ 0)
    }
}
```

---

## 3. C2 — Mechanical context reduction: supersede pruning, shake, and the cache-warm guard

kod's gap: between "do nothing" and "FIFO drop" there is nothing. jcode's ladder (B1) summarizes with an LLM. omp fills the same gap **without a single LLM call**, using three mechanical passes — and adds a fourth insight (the cache-warm guard) that none of the prior plans had.

### 3.1 Supersede pruning (pruning.ts D144159–144604, tool-protection.ts D145086–145152)

Newest-first walk over tool results:

| Rule | Value | Rationale |
|------|-------|-----------|
| `protectTokens` | **40,000** | never mutate the most recent window (it *is* the working set) |
| `minimumSavings` | **20,000** | don't churn the cache for pocket change |
| `MIN_PRUNE_TOKENS` | **50** | placeholder ≈8 tokens; blanking a 30-token result *grows* context |
| Supersede key | read path, selector stripped | a later read of the same file blanks the older result with `[Superseded by a newer read of this file]` |
| Key hierarchy | `K` supersedes `K+"\0…"` | a bare read supersedes ranged reads of the same file |
| Incremental gate | suffix ≤ **8,000** tokens | only prune when the all-messages suffix is cheap to re-cache |
| Idle flush | **30 min** | must exceed provider cache retention (Anthropic long = 1 h); when the cache is provably cold, flush all pending candidates |
| USELESS flag | tool-declared | uninformative results (`[Uneventful result elided]`) bypass the protect window |

Protection matchers receive `{toolResult, toolCall}` and keep `skill://` and `artifact://` recovery reads — eliding an artifact-recovery read would mint another artifact (infinite loop).

### 3.2 Shake (shake.ts D144605–145081)

Surgical elision of heavy *text* regions: tool-result text plus large fenced/XML blocks in user/assistant messages (≥ `fenceMinTokens` **400**), conservative scan (unterminated fence ⇒ ineligible; XML suppressed inside fences; overlap = containment). Dropped content offloads to `artifact://` docs so nothing is lost. Presets:

| Preset | protectTokens | minSavings | fence |
|--------|---------------|------------|-------|
| default | 16k | 4k | 400 |
| aggressive (`/shake`) | 4k | 0 | 400 |
| rescue | 0 | 0 | 400 (+artifact protection — a rescue that can't drop its blocker isn't a rescue) |

Placeholder cost ≈ 16 tokens; highest-start-first application; `keepBoundaryId` (compaction boundary) entries never touched.

### 3.3 The cache-warm-suffix guard (D144203) — the piece everyone else missed

Before mutating any tool result, compute its **all-messages suffix token total**. If the suffix is still inside the provider's cached window, mutation forces a full **cacheWrite premium** (1.25× on 5-min TTL, 2× on 1 h) that can exceed the savings.omp's guard: skip mutating any result whose suffix exceeds `cacheWarmSuffixTokens`; leave those to compaction/shake, which rebuild the cache anyway and amortize the write.

```rust
// kod-core/src/context.rs (new: prune.rs)

pub struct PruneConfig {
    pub protect_tokens: u64,        // 40_000
    pub minimum_savings: u64,       // 20_000
    pub min_prune_tokens: u64,      // 50
    pub cache_warm_suffix_tokens: u64, // provider cache window (5m: ~window since last send)
    pub idle_flush: Duration,       // 30 min
}

pub enum PruneAction { Blank { placeholder: &'static str }, Skip, DeferToIdleFlush }

pub fn plan_prune(
    transcript: &Transcript,
    supersede_index: &SupercedeIndex,   // built from read_file results: path -> newest call_id chain
    cfg: &PruneConfig,
    cache_ctx: &CacheContext,           // from P0's CacheLedger: what the provider currently holds
) -> Vec<(MessageId, PruneAction)> {
    let mut out = vec![];
    for result in transcript.tool_results_newest_first() {
        if result.tokens < cfg.min_prune_tokens { continue; }
        if transcript.suffix_tokens_after(result.msg_id) <= cfg.protect_tokens { continue; }
        if transcript.suffix_tokens_after(result.msg_id) > cfg.cache_warm_suffix_tokens
            && cache_ctx.prefix_is_warm() { continue; }          // §3.3: leave warm bytes alone
        if result.meta.useless {
            out.push((result.msg_id, PruneAction::Blank { placeholder: "[Uneventful result elided]" }));
            continue;                                            // bypasses protect window
        }
        if supersede_index.is_superseded(result.msg_id) {
            out.push((result.msg_id, PruneAction::Blank {
                placeholder: "[Superseded by a newer read of this file]" }));
        }
    }
    // savings gate + idle-flush deferral (cold-cache flush when now - last_send > idle_flush)
    out
}
```

`SupercedeIndex`: `HashMap<CanonicalPath, Vec<(call_id, selector)>>` built incrementally by `kod-tools::read_file` (kod already records every read in the transcript; this is one extra index). kod's `decisions.rs` precedent shows exactly where the hook goes.

**Port order:** useless flag (S) → supersede index (S) → protect/savings gates (S) → cache-warm guard (S, needs P0's CacheLedger) → shake (M, text-region scanner with artifact offload).

---

## 4. C3 — Compaction: dispatcher, speculation lead, admission control, native lane

jcode's B1 gives kod a 3-tier ladder. omp generalizes it into a **preference list with fail-over** and adds three engineering guarantees.

### 4.1 Ordered multi-method dispatcher (compaction-methods.ts D491049–491186, tests D626223–626452)

Default order: `[remote, snapcompact, handoff, shake, soft]` —
- `remote`: provider-native compaction (§4.4);
- `snapcompact`: bitmap-frame imaging, needs `model.input ∋ image` (§4.5);
- `handoff`: one-shot handoff document *as* the compaction summary (1 call);
- `shake`: §3.2, no LLM;
- `soft`: local summarize with a compaction model (1 call).

Each method has an availability check; the maintenance loop **falls through on failure or unavailability** with a user notice. `/compact [method] [focus]` maps to manual override. Threshold: post-turn maintenance when prompt tokens exceed `resolveThresholdTokens(window, settings)`; `reserve = max(15%·window, 16_384)`; `keepRecentTokens` default 20k.

```rust
// kod-core/src/engine.rs — replaces the hardcoded tiers from jcode B1
pub trait CompactionMethod {
    fn name(&self) -> &'static str;
    fn available(&self, ctx: &SessionCtx) -> bool;
    fn run(&self, ctx: &mut SessionCtx, plan: &CompactionPlan) -> Result<CompactionOutcome, CompactionError>;
}

pub struct CompactionDispatcher { methods: Vec<Box<dyn CompactionMethod>> } // preference order

impl CompactionDispatcher {
    pub fn compact(&self, ctx: &mut SessionCtx, focus: Option<&str>) -> CompactionOutcome {
        for m in &self.methods {
            if !m.available(ctx) { continue; }
            match m.run(ctx, &self.plan(ctx)) {
                Ok(o) => return o,
                Err(e) => ctx.notify(format!("compaction method `{}` failed ({}); falling through", m.name(), e)),
            }
        }
        CompactionOutcome::Failed  // context-full handling stays
    }
}
```

### 4.2 Speculation lead band (speculation-lead.ts D502593–502622)

A background summarizer fires inside `[threshold − lead, threshold)` with `lead = clamp(0.125 × threshold, 8_192, 32_000)` — scales with window, floor prevents tiny-window churn, cap bounds how much the armed summary misses before apply. Local/instant methods skip speculation. `runIdleCompaction` at idle.kod mapping: this is a one-knob upgrade to jcode's background-compact; the status gauge renders the marker (kod-tui already renders plan/cost gauges).

### 4.3 Budget projection + no-reduction guard (tests D626453–626828, D627521–627702)

Admission control for *any* compaction kod adopts:
1. **Projection before persist:** `countTokens(summary + edges) + Σ frame/item cost + nonMessage overhead + keptRecent ≤ window − reserve`, with worst-case edges `ceil(2×edgeCap/4) + 2000` and a 4k summary-template reserve.
2. **No-reduction guard:** reject pre-persist if projected local context isn't smaller than pre-compaction, recomputed **locally with the same tokenizer** — never trust provider-reported `tokensBefore` (imported sessions report 0).
3. Opaque reasoning bytes (`thinkingSignature`, `redactedThinking`) excluded symmetrically from both sides of the comparison.

```rust
// kod-core/src/budget.rs (addition)
pub struct CompactionAdmission { pub projected: u64, pub current: u64, pub window: u64, pub reserve: u64 }

impl CompactionAdmission {
    pub fn admits(&self) -> bool {
        self.projected < self.current                       // no-reduction guard (local tokens only)
            && self.projected <= self.window - self.reserve // budget projection
    }
}
```

### 4.4 Anthropic native compaction lane (compaction/anthropic.ts D142836–143145)

`compact-2026-01-12` beta: the **API** summarizes the prompt from the already-cached prefix (same system/tools/messages + edit, `pause_after_compaction`), returning a `compaction` block whose `encrypted_content` replays verbatim on Anthropic (the API drops everything before it) while the plain text doubles as the summary for every other provider. Constants: `ANTHROPIC_COMPACTION_MIN_TRIGGER_TOKENS 50_000` (API floor), min-context lane 55k; the retained-tail scope counts *wire* messages (consecutive tool results collapse; trailing assistant prefill pad) so the summary covers only the dropped region and never duplicates the tail.Port: `kod-provider/anthropic` (beta header + block replay) + the dispatcher above. Bonus freebie from the same file: **thinking-signature protobuf decode → embedded serving-model id** (field 6) detects gateway model substitution (anthropic-signature.ts D172285+) — a free audit for kod's multi-endpoint routing.

### 4.5 Snapcompact: bitmap frames (experimental, D501893–502473, D978919+)

1568×1568 PNGs of text rendered in bitmap fonts; a 6×10 frame holds ~40,716 chars billed at a flat `FRAME_TOKEN_ESTIMATE = 5024` (~2:1 win on dense text, better on sparser). Two modes:
- **Full compaction:** discarded history rasterized into frames attached to the compaction entry; re-attached on every context rebuild; survives resume.
- **Inline imaging** (the practical first step): per-request transform swaps (i) old tool results ≥ `MIN_TOOL_RESULT_TOKENS 3000` and (ii) system-prompt sections for frames; `SAVINGS_MARGIN 0.9`; **oldest-first for cache-stable bytes but always skipping the last (freshest) tool result**; error results stay text; budget = provider image count; render cache keyed `toolCallId + hash(text)`; lazy rasterization via blob broker (§7.5).

Guard lattice (all from regression tests): budget-sized `maxFrames`; no-reduction guard; frame dead-end rescue (rebuild trailing archive at `0.8×threshold` band); glyph preflight (unrenderable PUA → method disabled, fall through); legacy crash guards.

Verdict for kod: **experimental, L effort** (Rust rasterization: embedded BDF font + `image` crate, ~600 lines). The *inline tool-result imaging* variant is the interesting first step for vision-capable kod sessions; the QA-recall harness they built (SQuAD over the corpus, `metaharness/adapters/snapcompact.py` D923691) is the right way to pick font/variant per provider. Park behind a feature flag.

---

## 5. T1 — Shell output minimizer: reduce, don't chop

### 5.1 The gap

kod caps tool results at 16 KiB (`STRUCTURED_TOOL_MSG_CAP`) and `cap_rendered_result` chops. omp **rewrites** command output into information-preserving digests before it reaches the model: `git log` → short-hash + subject lines; `git status` → `branch / staged 2, unstaged 1, untracked 1 / file list`; pytest → failures + summary; cargo → error slices. Raw output is preserved as an artifact (`artifact://<id>`) so the human or agent can still fetch it. Kill-switch + `only/except` config.

### 5.2 Mechanism (minimizer.rs D115684–115842, config.rs D116637–117095, plan.rs D118722–119296, pipeline.rs D118015–118722, defs D119807–122837)

1. **Command classification by a real parser** (brush-parser): `CommandPlan::{Single, Piped, Chain, Compound, Unsupported}`. **Pipes are opaque** — rewriting piped output is a correctness bug (the filter can't know what the *final* stage consumed). `&&`/`;` chains are segmented per-`ChainSegment` with a re-parse-shape guard; here-docs excluded.
2. **Filter dispatch** via `detect.rs`: per-program global-flag skip tables (git/cargo/npm/pip/aws/uv/npx/yarn/…), then first match in the registry.
3. Two filter families:
   - **10-stage declarative TOML pipelines** (76 built-ins ship as TOML: apt, df, du, gcc, jq, just, make, npx, rustc, shellcheck, ssh, systemctl, terraform, uv, xcodebuild…): `strip_ansi → replace → match_output(short-circuit, with unless) → strip/keep_lines(RegexSet) → replace_after → truncate_lines_at → head/tail_lines → max_lines → preserve_if_empty → on_empty`, gated by `only_on_exit/except_on_exit`. User TOML supported (`schema_version=1`, inline `[[tests]]` runnable in CI).
   - **Native Rust filters** for cargo/rust, pytest/go/cpp/dotnet/node/bun/gh/glab.
4. Output: `MinimizerOutput{text, filter, original_text → artifact, input_bytes, output_bytes}`. Trust gate: settings loaded only if `xxh64(settings_path)` matches the recorded hash; `max_capture_bytes` default **4 MiB**.

### 5.3 Design for kod

```rust
// kod-minimize/src/lib.rs (new crate, ~2k lines for engine + plan + detect)
// kod already ships fixture-driven tests (check.rs style) — the minimizer
// ports cleanly with `.raw` vs `.min` fixture pairs.

pub struct Minimizer { registry: FilterRegistry, cfg: MinimizerConfig }

impl Minimizer {
    /// Called from kod-tools::execute_command AFTER capture, BEFORE the
    /// 16 KiB wire cap. Full original persists as an artifact either way.
    pub fn minimize(&self, cmd: &str, raw: &Capture) -> Minimized {
        match plan::classify(cmd) {                    // brush-parser in omp; kod can start
            CommandPlan::Piped => return Minimized::raw(raw),   // pipe-opacity invariant
            CommandPlan::Single(prog) | CommandPlan::Chain(_, segs @ ..) =>
                self.run_filters(prog, segs, raw),
            CommandPlan::Unsupported => Minimized::raw(raw),
        }
    }
}

// Start with the TOML pipeline engine + 5 defs (git status, git log,
// cargo check/test, pytest, npm test) + fixture tests; skip native filters.
```

Fixture-style tests port directly (omp ships `.raw`/`.min` pairs; D131210–132604). Target: new `kod-minimize` crate, invoked from `kod-tools::execute_command`, original → `artifact://` (see §7.5's internal-URL layer for the handle).

---

## 6. T2 — `xd://` lazy tool mounting: progressive disclosure as a protocol

### 6.1 Why this beats the planned `tool_search` tool

PDF-P3 planned a `tool_search` tool (deferred schemas, snippet → full schema on demand). omp implements the same idea with **zero extra tools**: tools marked `loadMode: "discoverable"` are *removed from the tools array* and mounted as virtual devices behind the always-present `read`/`write` tools:

- `read xd://` → mounted tool listing;
- `read xd://<tool>` → docs + full JSON schema;
- `write xd://<tool>` → execute (content = JSON args).

Validation returns the schema on mismatch, so a malformed call self-corrects without a round trip (xdev.ts D545941–546404; essential-tools.ts D531939–531993 pins the demotion guard so a UI re-register can't silently demote essential tools; code-mode.ts D489433–489559 is the Codex-specific keep-set flavor — skip that).

### 6.2 Design for kod

```rust
// kod-tools/src/registry.rs (additions)
pub enum LoadMode { Essential, Discoverable }

impl ToolDef { pub fn load_mode(&self) -> LoadMode { /* default Essential */ } }

// engine.rs tools-array assembly (build_grounded_request):
// - Essential tools -> native tools array (unchanged, cache-stable, sorted)
// - Discoverable    -> omitted; read/write gain an xd:// route

// kod-tools/src/tools/read.rs:
//   path starts_with "xd://" -> list tools or return docs+schema for <tool>
// kod-tools/src/tools/write.rs:
//   path starts_with "xd://" -> parse content as JSON args, validate against
//   the schema, execute via registry, return result with schema-on-error
```

Cache note: the discoverable set is *stable per session* (same as kod's sorted inventory), so removing them shrinks the prefix once, not per turn. This composes with kod's existing Jev category filter (which mutates the tools array — P0's hysteresis still applies).

---

## 7. T3–T6 — Tool-surface upgrades

### 7.1 Hashline edit mode + tag-aware read (kills stale-`old_str` by construction)

pi-edit's hashline mode: `read` emits `N:line`-numbered output and records a snapshot; the edit format addresses lines, not strings: header `[path#TAG]` (TAG = 4-hex content hash of the file *as the model last saw it*), hunks `PUT 600.=600:`, `PUT <N:`, `PUT >$:`, `CUT s.=e`, `REM`, `MV`, with `+` payload rows and `@` clipboard registers. Edit rejects **stale tags** and **unseen anchor lines**; result header returns the *new* tag so chains work. Read side: multi-range selectors `path:1-10,20-30`, `:raw`, `:-N` tails, elision footer teaching the selector syntax ("[…812ln elided; re-read needed ranges with path:40-90]"), range padding `LEADING=1 / TRAILING=3`, `READ_CHUNK_SIZE 8KiB`, tree-sitter block-context elision (format.rs D49280–49411, read-format.ts D540796–541376, muse-hashline bench D345222+ pins sub-2 ms apply).

For kod: a new `edit` tool alongside `patch_file`. The tag store is `HashMap<canonical_path, (content_hash, seen_lines: BitSet)>` in session state; parser ≈1.5k lines incl. recovery. Keep `patch_file` for string-replace muscle memory; hashline for the failure-prone bulk.

```rust
// kod-tools/src/edit_hashline.rs (new)
pub struct EditStore { snapshots: HashMap<PathBuf, Snapshot> }
pub struct Snapshot { pub tag: [u8;2], pub text: String, pub seen: BitSet } // 4-hex tag

impl EditStore {
    pub fn record_snapshot(&mut self, path: &Path, text: &str, seen: BitSet) -> [u8;2];
    pub fn apply(&mut self, path: &Path, tag: [u8;2], ops: &[Hunk]) -> Result<AppliedEdit, EditError> {
        let snap = self.snapshots.get(path).ok_or(EditError::NeverRead)?;
        if snap.tag != tag { return Err(EditError::StaleTag { expected: snap.tag, got: tag }); }
        for h in ops { if !snap.seen.contains_range(h.anchor_range()) { return Err(EditError::UnseenAnchor); } }
        // all-or-nothing staging (pi-edit ModeEngine contract), then disk write
    }
}
```

### 7.2 Edit/write LSP write-through with deferred late diagnostics

kod has `kod-lsp` + an `lsp_diagnostics` tool (poll-on-demand). omp's writethrough: write → `didOpen/didChange` → **inline wait 500 ms** for fresh diagnostics (version echo accepted immediately; non-echoing servers like tsserver settle after a 250 ms quiet window) → keep fetching up to **12 s** and inject late via a deferred queue with version guards + ledger dedup. Defaults: `diagnosticsOnEdit = false` (edit stays sub-2 ms), `diagnosticsOnWrite = true`. Batch mode flushes on the **last write of a call** (`BATCH 400 ms`) (writethrough.ts D455984–456615, diagnostics.ts D452759–453361, deferred D452629–452702).

kod port (M): `kod-lsp` clients expose `notify_did_change_wait(version, quiet_window)`; `kod-core` gains a `DeferredDiagnostics` queue drained at next turn start (this plugs straight into YieldQueue, §10.4).

### 7.3 pi-ast parse_cache (S, directly powers P8)

Process-global LRU of parsed `tree_sitter::Tree`s: `Key{xxh64(src, seed), len, lang}`, **hit requires byte-for-byte equality** with the retained source (collision ⇒ re-parse, never a wrong tree); `MAX_ENTRIES=12`, `MAX_TOTAL_SOURCE_BYTES 4 MiB`; linear-scan LRU (12 slots — cheaper than an intrusive list); `Tree` is `Send`-not-`Sync` ⇒ entries behind `Mutex`, lock held only for probe+compare+`ts_tree_copy` refcount bump (parse_cache.rs D11177–11616). ~450 dependency-light lines. Lands next to `repomap.rs` or a new `kod-ast`; also makes hashline's parse-regression check free.

### 7.4 jfind: judge-model cascade for semantic code search (M)

"Semantic grep": `{query: plain-language, grep_keywords: []}` → files + line ranges with probabilities, via a 3-wave cascade using a cheap judge model: (1) native lexical scan (IDF-weighted file ranking, 30 s timeout, 4 MiB cap); (2) filename judge over top **128** candidates in batches of **64**; (3) read **20** files, cut **24 × 8 KiB** windows, judge **384-byte verbatim sketch cards** (≤**48**/request, 18k state budget) at **PARALLEL=16**; (4) verify complete passages of sketches ≥ **CUTOFF 0.45**. File is a hit at **THRESHOLD 0.2**; sketches are routing signals only — reported heat always comes from a complete passage (cascade.ts D563127–563490).

kod port: orchestration over kod's existing grep walker + a `judge` role (§12.2). Adjacent to P0's meta-attention but query-time instead of transcript-time.

### 7.5 blob-broker + internal URLs: stable handles instead of base64

Two omp systems compose:
- **blob-broker** (broker.ts D359108–359523): outgoing images get stable, content-hash-keyed, **multi-use** URLs (single-use tokens are disqualifying — Anthropic silently forgets images unless a resent turn is byte-identical; OpenAI issues two GETs per image; any provider may refetch on cache miss). 128-bit random token per blob; **fail toward inline base64** on every failure; savings journal per project. For kod: in-process hash-keyed store (S) — tunnels/uploaders optional (M).
- **internal-urls** (types.ts D446377–446620, router.ts D445380–445547): a `scheme://` router giving the model path-free handles — `memory://`, `skill://`, `artifact://`, `agent://`, `conflict://` — through the same read/write tools as files. `ProtocolHandler{scheme, immutable, resolve(url, ResolveContext), write?, complete?}`; **`ResolveContext` binds resolution to the caller** (session, cwd), not process-global first-match; unknown schemes fall back to MCP resources. This is the URI-space layer kod's planned MCP bridge socket wants, and it's where minimizer originals (`artifact://`), hashline snapshots, and background-job outputs (`agent://`) all live.

### 7.6 Hub tool: one surface for peers + jobs + processes (UPGRADE of B1 swarm surface)

Single tool, `op` enum: messaging (`send` w/ reply-await, `inbox`, `wait[from]`, `list`), jobs (`wait`, `cancel`, `jobs`), process supervision (`start/ps/logs/stop/restart/describe`, stdin `send`, signal) (hub/index.ts D561336–561845). Key contracts:
- **Unified `wait` blocks until the FIRST of**: matching peer message, watched job settling, wait window elapsing, or steering interrupt. "Job results always deliver themselves — wait exists for when the agent has nothing else to do."
- `start` carries **readiness conditions** (log regex or port probe, timeout 30 s), restart policy `no|on-failure|always`, `persist`/`detached`; per-op approval tiers (`send`-to-process = exec because it's a stdin write).
- One op-dispatched tool beats many tiny tools: fewer schema tokens, better cache stability.

kod port (M): `kod-tools` op-enum tool over `kod-swarm` + a process supervisor with readiness probes; complements the FileTouch bus from B1.

### 7.7 Fast-wins bundle (each S)

1. **`useless` tool-result flag** + structured truncation meta (`direction`, `nextOffset` continuation, `artifactId`, limits auto-appended at the message boundary) — feeds §3 pruning (tool-result.ts D545296–545405).
2. **Non-interactive env for every subprocess** (D426239–426320): `PAGER/GIT_PAGER/MANPAGER/...=cat`, `LESS=FRX`, `AWS_PAGER=""`, `TERM=dumb`, `NO_COLOR=1`, `PYTHONUNBUFFERED=1`, `GIT_EDITOR=VISUAL=EDITOR=true`, `GIT_TERMINAL_PROMPT=0`, `SSH_ASKPASS=false`, `CI=true`, `AGENT=1`, `npm_config_yes=true`, `npm_config_update_notifier=false`, `npm_config_fund=false`, `npm_config_audit=false`, `npm_config_progress=false`, `PNPM_DISABLE_SELF_UPDATE_CHECK=true`. One static map in `kod-tools::execute_command`.
3. **Bash interceptor** (D529189–529343): regex rules that *block* `grep/cat/find` bash calls with a redirect message + `suggestedTool` — teaches routing without executing. Natural fit for kod's hooks.
4. **Conflict-resolution loop** (D530956–531697): `read` scans strict column-0 `<<<<<<<` blocks (10 MiB cap), assigns stable ids, model resolves via `write conflict://<id>`.
5. **Inline sloppy-edit recovery** (D493336–493394): on a clean stop turn with zero tool calls, lift `*** Begin Patch`-shaped payloads out of assistant text into a synthetic `edit` tool call (full validation/approval still applies). One hook in kod's turn loop.
6. **MCP tool cache** (D463293–463398): cache `tools/list` keyed by SHA-256(stable-JSON config) + TTL 30 d — startups never spawn servers just to list. `kod-mcp` upgrade.
7. **Walker scan cache + ranked collection** (D138196–138296, D85208–85537): global fs-scan cache with `invalidate_all()` + per-write invalidation hooks; `collect_ranked(mtime_desc, limit)`; startup scan also discovers `AGENTS.md` (depth 1–4, cap 200; `MAX_ENTRIES 100_000`; excluded dirs `{node_modules,.git,.next,dist,build,target,.venv,.cache,.turbo,.parcel-cache,coverage}`). Upgrades kod's grep/repomap walker.
8. **Tool-choice queue** (D503961–504271): generator-based forced-`tool_choice` directives with `requeue|drop|drop_sequence` on abort, plus **non-forcing pending invokers** resolved through ordinary tool calls (cache-preserving alternative to hard `tool_choice`).

---

## 8. T7 — The in-process shell (strategic, L)

`pi-shell` embeds **brush-core** (a vendored Rust bash interpreter) plus **uutils coreutils compiled as builtins**, executing against a `Host` abstraction instead of `std::env`. What breaks without fork/exec is precisely what kod wants:

- **Zero spawn latency** for `ls/cat/grep/sed/jq/sort/xargs/diff`;
- **Session-persistent cwd/env/jobs** — `cd x && make` then later calls see `x` (kod's per-call spawn resets cwd every time);
- **Real job control** — `nohup ./server &`, `jobs`, `bg/fg/wait/kill %1` via brush's job table + process builtins (`ps/pgrep/pkill/pidwait/timeout/nohup/sleep/top`) "so a long-lived embedded shell can inspect and control its own children without forking";
- Output streams through `flume` channels (pipeline stages concurrent; regression tests assert first-chunk-before-exit and no pipe-buffer deadlock); `CancelToken{deadline, AtomicU8 reason, Notify}` + heartbeat for cooperative abort; a uutils panic = failed command, not a dead agent (lib.rs D115659+, cancel.rs D115489–115659, napi shell.rs D82155–82869, factory.rs D22788–23149).

**The critical caveat:** pi-shell has *no sandbox layer*. kod's bwrap/Landlock/Seatbelt must keep wrapping the fork/exec parts (real binaries), and the **builtin-withholding mechanism** maps 1:1 onto kod's `forbidden_binaries` policy: `utility_builtins()` is kept out of defaults precisely because they shadow real binaries — "the embedding shell may withhold the destructive ones — rm, mv, ln" (factory.rs D22974–22979).

Incremental port path: keep `execute_command` (spawned, sandboxed) as default; add a **session shell** profile behind policy — `kod-shell` crate wrapping brush-shell (Apache/MIT) with a `Host` trait wired to kod's sandbox/policy state. kod being pure Rust means the whole napi-boundary machinery (TSF, drain, panic-scope recovery) simply doesn't exist in the port.

---

## 9. L1 — Loop hardening

### 9.1 Oneshot retry kit with hint extraction (UPGRADE of jcode retry kit) — S

For non-loop completions (summaries, titles, judges, compaction): 3 attempts, base 500 ms doubling, ceiling 8 s, max 30 s, 75–100% jitter. Retryable = transient status | UsageLimit; **NOT retryable: ContextOverflow** ("a fixed prompt that doesn't fit fails identically — retry burns the deadline"), ContentBlocked, PayloadRejected, deterministic parse-500s. Hints: `retry-after-ms` / `retry-after` (s or HTTP-date) / `x-ratelimit-reset(-ms)` (epoch-s vs epoch-ms vs delta heuristics) — take the **MAX** of all candidates; **text hint extraction** from error messages ("try again in ~5m") with per-provider timezone for naive stamps; over-cap hint → surface error instead of parking (oneshot-retry.ts D156554–156800, retry-after.ts D208544–208678; Anthropic transport: `x-should-retry` header override, **retry-after > 60 s cap → decline retry, surface original error**, D171497–171744).

```rust
// kod-provider/src/retry.rs (additions)
pub struct RetryHints { pub delay: Option<Duration>, pub cap_declined: bool }
pub fn extract_retry_hints(status: Option<u16>, headers: &HeaderMap, body: &str) -> RetryHints {
    // MAX over: retry-after-ms, retry-after (s|HTTP-date), x-ratelimit-reset (s|ms|delta),
    // text-scan of body ("try again in ~5m"), suffix hint "retry-after-ms=N"
    // cap_declined = any hint > 60s  => caller surfaces the original error
    // kod's existing Retry-After handling joins here; jcode's capped Retry-After folds in.
}
```

### 9.2 Replay-safe stream retry + empty-completion retry — M

Retry transient failures **only while nothing meaningful was emitted**: buffer pre-output events; first `text_delta/thinking_delta/toolcall_delta(non-empty)` **commits** the attempt; `toolcall_start/end` alone never commit (stream dying before args → discard markers, retry). Empty completion retry: stop + no visible content + `usage.output ≤ 1` → ≤ **2** retries @500 ms·2ⁿ (empty-completion-retry.ts D204685–204886).

### 9.3 Thinking-loop guard (NEW — kod has Jev gate, no runaway detector) — M

Four detectors over the 4096-char streaming tail, scanned every 128 new chars (thinking-loop.ts D208995–209539):
1. **Exact suffix cycles** — Z-array over the reversed tail; cycle ≤1024 chars; short (≤60 chars) needs 4 repeats ≥180 chars, long needs 3 repeats ≥1024;
2. **Near-duplicate paragraphs** — normalize (lowercase, strip headings/bold-titles/code backticks), word-trigram Jaccard ≥ **0.8**, cluster ≥ **4** in window 16, warm-up ≥ 8 segments;
3. **Progress-lexicon stall** — segment novelty ≤ **0.2** vs 8-segment vocabulary window, no **new** concrete anchor (code spans/paths/identifiers regex), run ≥ **8**;
4. **Gemini header runaway** — ≥ **36** consecutive `##`/`**bold**` summary titles.

On hit: abort upstream, emit terminal error tagged `ThinkingLoop` + "stream stall" so retry classifiers treat it as transient; ≤ 3 guarded re-samples; visible text latches the thinking detector off. Exact cycles apply to all models; semantics gated to gemini/deepseek/xai classes. Calibrated against 536k real blocks (max legit run 7; hardest negative 3/13.5k).

```rust
// kod-provider/src/stream_guard.rs (new)
pub enum StallVerdict { Clean, Loop { detector: &'static str } }
pub struct StreamGuard { tail: RingBuf<u8, 4096>, /* ... per-detector state ... */ }
impl StreamGuard { pub fn feed(&mut self, delta: &[u8]) -> StallVerdict { /* stride 128 */ } }
// engine run_streaming_loop: on StallVerdict::Loop -> cancel stream, classify as transient,
// retry within the existing TurnFailure taxonomy (SameEndpoint::LowerTemp is a good fit).
```

### 9.4 Unexpected-stop classifier + tool-call loop redirect — S

Candidate = stopReason "stop", ≥1 text or **signed** thinking block (reasoning models trap answers in thinking + signature), no toolCall → one yes/no judge question with bulleted measured examples ("wording measured on lfm2-1.2b/qwen2.5-1.5b: prose criteria cost ~3 points recall"), threshold **0.5**. Loop guard: hash of sorted canonicalized tool calls (keys sorted, intent stripped) across consecutive turns; at threshold inject structured corrective `{toolName, count, argumentsSummary ≤400, resultSummary ≤200}` (unexpected-stop-classifier.ts D505018–505106, tool-call-loop-guard.ts D209578–209696).

### 9.5 Judgment framework (substrate for 9.4, auto-thinking, review) — S

`Questions` (choice/score/yes-no) rendered into **one byte-stable system prompt** (prompt-cache hits) + XML-field state as user message; N questions batch into one completion, one `id: keyword` line each; parse = earliest whole-word label (longer label wins tie: `xhigh` beats `high`), one-hot probabilities; format-correction retry with a strict `submit_judgment` tool + forced toolChoice; `JUDGMENT_CHAT_MAX_TOKENS 4096` (must exceed Anthropic's 1024 min thinking budget even when reasoning "disabled"), temp 0, disableReasoning (judgment/text.ts D170837–171147, chat.ts D170728–170822).

```rust
// kod-judgment (new module in kod-core or crate)
pub enum Question { Choice { criteria: Vec<(String, String)> }, YesNo { rubric: (String, String) }, Score { levels: Vec<String> } }
pub struct JudgmentClient { chain: RoleChain /* judge role w/ fallback */ }
impl JudgmentClient {
    pub async fn ask(&self, state: &str, qs: &[Question]) -> Result<Answers, JudgeError> {
        // system = PREAMBLE + question defs (byte-stable -> provider prompt cache)
        // user   = XML state + answer cue; parse `id: label` lines; 2 format retries
    }
}
```

### 9.6 Auto-thinking: per-prompt effort classification — S/M

`thinking: auto` — a judge picks this turn's reasoning effort: one ChoiceQuestion over the preprocessed user message; judge = `judge` role chain or a **local sub-2B model answering a coarser 3-bucket question** (`trivial|moderate|hard` → low|high|xhigh; "3-class is more reliable than 4-way ordinal on sub-2B"). Ladder criteria per level (low: "rename, typo, one-line edit…"; max requires xhigh **plus** no-repro/irreversible/live-cutover). Tie-break: "choose the lower one". Ceiling: auto defaults to one tier below top; Max only via explicit override; clamp to model's supported efforts (classifier.ts D355252–355389). kod's keyword classifier stays as the offline fallback; this is the LLM-judge version.

### 9.7 Declarative retry-fallback chains (UPGRADE of NextEndpoint) — M

Config `retry.fallbackChains: Record<role | "provider/model[effort]" | "provider/*" | "provider/prefix/*", string[]>` resolved by specificity: exact model+effort → normalized-effort → suffixless base → **longest id-prefixed wildcard** (`openrouter/google/*` beats `openrouter/*`) → provider wildcard → hinted role → role default. Wildcard *entries* transform ids (`google/*` keeps id; `prefix/*` re-prefixes bare id; aggregator→direct de-vendoring `openrouter/google/x` → `google-vertex/x`). Candidates are only entries **after** the current selector (never re-try the failed model), with optional wrapAround and transitive expansion (`tiny:[B]+B:[C]` reaches C). Attribution `ServingModel{selector, modelIdentity, thinkingLevel, isFallback}`; revert policy `cooldown-expiry | never` (retry-fallback-chains.ts D495025–495612). Adds effort-aware matching + wildcards to kod's `resolve_chain_for_task`.

### 9.8 Pause gate (pairs with jcode InterruptSignal epochs) — S

Process-global gate polled by every loop at exactly two boundaries (before each model call, before each tool call); in-flight streams/tools run to completion; queued steer/followUp deliver after resume; a run's own AbortSignal unwinds only that run — **abort parks, never releases the gate**; re-engage-safe wait loop (pause.ts D141279–141391). kod: a `tokio_util::sync::CancellationToken` global + checks in `run_streaming_loop`/`run_tool_calls`.

### 9.9 Provider concurrency narrow bracket — S

Per-provider `Semaphore` wrapped around **only the streaming HTTP request**, released when the stream finishes producing — not held across the agent's lifetime (holding it deadlocks any spawn tree wider than the cap: parents wait on children that wait on parents' slots — issue #3749). Semaphore must support abortable `acquire` and **in-place `resize`** (replacing orphans in-flight slots and overshoots the cap); `≤0 = unbounded` (provider-concurrency.ts D523079–523184).

### 9.10 Provider image budget + undecodable-image degrade — S

Two wire defenses: (1) drop **oldest transient** images (user/developer/toolResult; never assistant) to fit the provider's per-request image cap; (2) degrade **undecodable** inline images to text — one corrupt image otherwise rejects the entire request and wedges the session. Checks all 3 hiding places (generic content, native `providerPayload` items replayed verbatim, `providerMetadata.screenshot`); decode verdicts cached in LRU **512** keyed `mime:len:hash` (provider-image-budget.ts D494289–494632).

### 9.11 Misc

- **Run collector** (D141849–142485): `AgentRunSummary` per turn — per-stopReason counts, tool status ok/error/skipped/blocked/timeout/aborted, per-tool counters, cost.unavailableReasons, coverage toolsAvailable/Invoked/Unused. S; feeds `kod-tui /stats`.
- **Session entry `reset_boundary`** (D143661–143669): durable `/clear` marker; context rebuilds start after it; history kept on disk for export. UPGRADE of jcode session persistence. S.
- **Native tokenizers via the napi layer** — kod equivalent: `tokenizers`/`tiktoken-rs` crates for provider-true estimates, plus `IMAGE_TOKEN_ESTIMATE 1200` and a `checkTokenBudget` free-fit probe (bytes ≤ budget ⇒ fits; bound overshoots ~4×) (tokenizer.ts D142512–142833). Replaces `budget.rs` char arithmetic where it matters. S/M.

---

## 10. S1 — Speculative read execution (admit → evidence → validate → commit)

While the model streams a tool call, speculatively execute **provably read-only** work; commit only if reality matches. This hides read latency behind generation (bench: a 60 ms read hidden behind a 40 ms provider tail; D345604–345758). kod should port the **narrow** version — streaming-read shadowing for `read_file` — and skip the eval-cell AST machinery (§10.4) until kod grows code-exec cells.

### 10.1 Host policy (fail-closed) — host.ts D512567–512871

Authorize only when: enabled ∧ effect is a single `local_read` ∧ no extension lifecycle handlers ∧ approval auto-allow ∧ `resolve(cwd, args.path) == resource.path` (lexical bind) ∧ target not ipynb/svg/video/convertible. With a `beforeToolCall` hook installed, execution defers until after the hook gate (`deferBeforeToolCall: true`); hook-free sessions start immediately for full overlap. `maxInFlight` live setting.

### 10.2 Evidence + validation (TOCTOU gates)

`captureEvidence` pre-execution: image/binary/PDF/SQLite sniff, lstat identity, **sha256 of raw bytes + sha256 of normalized (BOM-strip, LF) text**. At commit: consumed evidence matches, re-authorize, re-resolve target (**symlink-swap veto**), **fresh digest == captured digest** (post-execution mutation veto). Single-use evidence. Dependency DAG: dependents admitted only after parents **commit**; a vetoed parent discards dependents that *never executed* (tests D153099–153474, 745722–746500).

```rust
// kod-core/src/speculation.rs (new) + kod-tools read speculation policy
pub struct Evidence { dev: u64, ino: u64, mtime_ms: u64, size: u64,
                      digest_raw: [u8;32], digest_normalized: [u8;32] }

pub enum SpecReadOutcome { Committed(ToolResult), Discarded(Reason) }

pub struct SpecCoordinator { inflight: HashMap<CandidateId, SpecHandle>, max_in_flight: usize }

impl SpecCoordinator {
    /// Called by the streaming loop when a read_file call's args are complete-enough.
    pub fn admit(&mut self, cand: Candidate) -> &SpecHandle { /* evidence captured pre-exec */ }
    /// When the model's final args land: reconcileFinalCalls drops candidates whose
    /// finalized args changed; commit() re-validates digests or discards.
    pub fn reconcile(&mut self, final_calls: &[(CallId, ToolArgs)]) { /* ... */ }
}
```

Same-branch synergy: the evidence tuple `(dev, ino, mtimeMs, size, digest)` is exactly what the jcode-B1 FileTouch conflict bus wants for validating stale-touch notifications.

### 10.3 What NOT to port (yet)

The **streaming shadow-plan IR** (eval cells: incremental JSON arg decoder → `ShadowPlan{operations, controls, barrier}` → `ShadowClaimStore` one-shot claims keyed `(siteId, dynamicPath, name, fingerprint, occurrence)`; D424287–425518) is brilliant but presupposes kod has an eval/code-cell tool. Note it, skip it.

### 10.4 Speculative compaction lead

Covered in §4.2 — same family, cheaper to adopt, do it first.

---

## 11. M1 — Multi-agent, async, and lifecycle

### 11.1 WorkPool: keep-alive swarm workers (UPGRADE of B1 swarm) — M

A pool of persistent subagents consuming many small items instead of spawn-per-task. Items `{id: name#seq, status}`; dispatch: **least-loaded-*idle* by `contextTokens/contextWindow` ratio** → spawn while `agents < limit` → round-robin among `running` (queue onto the busy agent's next turn). Per turn the worker processes its **whole queue as one batch**, rendered from a template, with a **per-batch output schema** — one required key per item id — so the worker yields per-item results in a single structured response (`buildWorkPoolOutputSchema`). `freshAgents` mode = spawn-per-item. Stranded queued items re-dispatch; yield-contract failure → tombstone release (worker can't be reused with a stale schema) (workpool.ts D524495–525163).

kod mapping: `kod-swarm`'s TaskCoordinator dispatches whole subtasks to fresh agents; WorkPool adds the item-queue + batch-turn layer on top — ideal for kod's reviewer/optimizer role workloads.

### 11.2 Park/revive agent lifecycle — M

Finished subagents stay addressable: idle → (TTL) → **parked** (session disposed, ref + transcript file kept) → revived on message/hub focus, even **cold across process restarts**. `AgentRef{id, kind: main|sub|advisor, status, session, sessionFile, activity, history{resolvedModel, metrics, branchName, patchPath}, lifecycle{responseAt, acceptedAt, terminalAt}}`; tombstone sidecar on explicit kill; park/ensureLive coalesced per id and **bound to the exact AgentRef** (stale finalizers can't clobber a newer same-id ref). Cold revive peeks persisted `session_init` to rebuild tools/systemPrompt, derives task depth by walking the parent chain, refuses if isolated or cwd gone (agent-registry.ts D479035+, persisted-revive.ts D522791–523049).

### 11.3 IrcBus messaging semantics (UPGRADE of B1 DM/broadcast) — M

Send never blocks; replies are real recipient turns. Per-agent mailboxes (`MAILBOX_CAP 100`) + waiters; delivery receipt ∈ {`injected` (waiter/aside), `woken` (idle wake turn), `revived` (park revival)}; buffered **only** on failed live hand-off (no double delivery). `send await:true` distinguishes `TargetStopped` from plain timeout (`DEFAULT_IRC_TIMEOUT_MS 120_000`). Mid-turn busy → **non-interrupting aside** at next step boundary; idle subagent → monitored wake turn whose output is relayed to sender; if a real reply turn is impossible → ephemeral side-channel auto-reply. Bridge queues: interrupts > asides; **deferredWakes parked while a pooled yield contract owns the worker**; session-transition snapshots merge new arrivals ahead-of, not over (bus.ts D447606–448094). kod's hub mpsc (100-msg history cap) already has the bones; these are the semantics to adopt.

### 11.4 AsyncJobManager + owner-routed delivery + auto-background (UPGRADE of PDF-P6) — M

Typed background jobs `{type, id, ownerId, queued→running, promise, signal, reportProgress}`; per-agent **delivery sink** → owner's YieldQueue → idle flush → batched `async-result` message (inline cap **12,000** chars, preview **4,000**, payloads point at `agent://` artifacts); delivery entries carry an **epoch** — session transitions drop stale-transcript entries even after job-id reuse. Auto-background for bash: foreground-wait threshold default **60,000 ms** clamped to `timeout − 1000`; a steering signal ⇒ background immediately; `maxJobs 100` (async-job-delivery.ts D487612–487748, auto-background.ts D355164–355243).

### 11.5 YieldQueue: the typed aside pipeline — M

Generic per-kind queue that injects background events into the turn loop as asides: `register<P>(kind, {isStale?, build, skipIdleFlush?})`; two flush modes (**streaming** inject mid-run vs **idle** batched follow-up turn); **`drainLazy()` returns thunks** so the loop decides *at injection time* whether a batch is still worth delivering (freshness by construction); settlement via commit/discard symbol props; `cancelIdleFlushScheduling()` for cancelled host tasks (yield-queue.ts D505111–505404). This is the backbone behind async-result delivery, **late LSP diagnostics** (§7.2), and **advisor cards** (§11.8). kod: one struct in `kod-core`, drained from `run_streaming_loop` between rounds.

### 11.6 Goals runtime: budgeted objective with continuation — M

One active objective per session with token + wall-clock budget: `Goal{id, objective, status: active|budget-limited|paused|complete|dropped, tokenBudget?, tokensUsed, timeUsedSeconds}`; delta accounting `Δinput + ΔcacheWrite + Δoutput` (cacheRead excluded — reused prefix; **cacheWrite counted** because 1 h-cache rotation can write 100K+ tokens); flush per tool completion + wall clock; crossing budget → status `budget-limited` + **one** steer message (deduped); user interrupt → auto-pause persisted; agent_end with active goal → hidden continuation message; completion → final budget report (goals/runtime.ts D437357–437884). Natural sibling for kod's `plan.rs` + `cost.rs`.

### 11.7 TodoTracker nudge machinery (UPGRADE of B12 semantic todos) — M

Don't just track todos — *schedule the agent back onto them*: **eager prelude** (first turn: hidden prelude + optional forced `tool_choice=todo`; skips when prompt ends `?`/`!`, plan mode, existing todos); **mid-run nudge**: after **12** successful mutating tools without a todo touch, inject hidden reconcile nudge, max **2** per cycle; **completion reminder**: agent stops with incomplete todos → `<system-reminder>` + `scheduleAgentContinue`, with a `reminderAwaitingProgress` latch, skip if the assistant's last line is a question or async wakes are pending (todo-tracker.ts D503511–503916).

### 11.8 Advisor emission guard + delta-split feed (the missing half of PDF-P6) — M

kod plans cross-model review; omp shows the review *output* side must be policed or it floods the transcript — their real incident: one advisor made **309 advise calls / 92 unique notes** (114× "Stop."). Watcher agents configured in `WATCHDOG.yml` call `advise(note, severity ∈ nit|concern|blocker)`; notes render as `<advisory severity="…" guidance="weigh, don't blindly obey">`. The **AdvisorEmissionGuard** (admission order, D354128–354456):
1. empty drop;
2. **noise list** of ~37 normalized content-free phrases (`stop`, `done`, `no issues`, `lgtm`, `on track`… — NFKC + punctuation-fold);
3. **rank-aware dedupe**: normalized key → highest admitted severity rank; equal/lower re-raise suppressed, strictly higher = real escalation; FIFO history cap **4096**;
4. **per-update budget 4** non-blockers (max 32): suppressed notes never consume budget; when full, a strictly-higher-rank note **displaces the lowest-rank pending slot**; routed notes can't be displaced; **blockers exempt**.

Delivery routing: `nit` → non-interrupting aside; `concern`/`blocker` → steer (interrupt); preserve as visible card when primary is idle/terminal/post-interrupt; post-interrupt cooldown = half-open turn fence (blocker exempt). Advisor loop gets its own tool-loop guard.

**The delta-split cache trick** (D354020–354127): render the transcript delta as **one user message per source message** so provider prefix caches hit incrementally (measured cache_read 11066 → 11091 → 11112 growing, vs pinned at 11066 for a single ever-growing message); heading on first chunk, WIP marker on last so flips never change the stable prefix; message identity = hash of *rendered fields only*.

```rust
// kod-swarm/src/advisor.rs (new)
pub struct EmissionGuard { seen: HashMap<String, Severity>, history: VecDeque<String>, budget: Budget }
impl EmissionGuard {
    pub fn admit(&mut self, note: &Advice) -> Admission { /* noise filter -> dedupe -> budget */ }
}
pub enum Delivery { Aside, Steer, Card }
pub fn route(note: &Advice, primary_state: PrimaryState) -> Delivery { /* nit/concern/blocker */ }
// Feed rendering: one message per source message (delta-split), identity = hash(rendered)
```

### 11.9 Cleanse loop: file-sticky streaming dispatch (UPGRADE of swarm_runner) — S/M

Multi-agent lint-repair where **two workers never edit the same file**: `pending: Map<fileKey, diag[]>`, `owned: Map<fileKey, OwnerEntry{worker, held, sending, released}>`, follow-up sends single-flight with requeue on failure/release, `takeBatch` budget = `totalWeight / maxAgents` (default 32) so one free slot can't swallow the backlog, drain loop, one `verify()` pass decides clean|stalled (cleanse/loop.ts D369789–370057). Fixes kod's known swarm interleaving caveat (engine-global transcript) for the repair-fleet use case.

### 11.10 Plan-mode hardening kit (UPGRADE of plan.rs) — M

(i) **plan model role**: entering plan mode switches to the `plan`-role model, restores on exit; reassignment applies at next turn boundary; switch **deferred while streaming** (`resolvePlanModelTransition` pure decision). (ii) **plan read compaction protection**: matcher keeps `read` results for the plan file + reference path (suffix selectors handled) intact through prune/shake. (iii) **plan handoff**: approved plan injected into subagent contexts (skipped *during* plan mode — drafts must not leak as approved). (iv) plan-mode subagent clamp: tools = read/grep/glob/web_search, no spawns/isolation. (v) **autosave** approved plans with `O_EXCL` claim loop (`MAX_AUTOSAVE_CANDIDATES 1000`, stem ≤ 32 chars) (plan-mode/* D477928–478422).

### 11.11 Session worktree `/wt` + isolation-ownership GC (UPGRADE of Isolation::Worktree) — M

(i) `/wt` relocates the session into a clone-first worktree **carrying uncommitted changes**, branch `wt/<yyyymmdd-hhmmss>`, optional `cleanSource`. (ii) **isolation-ownership markers**: `.omp-isolation-owner.json {pid, id, startToken}` where startToken = `/proc/<pid>/stat` field 22 — GC treats `ESRCH`-only as dead and **rejects recycled pids** by token mismatch. (iii) retained-backend sidecar routes overlayfs/projfs/btrfs workspaces to native teardown instead of `rm -r` (plain `rm` on a mount destroys the preserved layer) (session-worktree.ts D501622–501738, isolation-ownership.ts D520593–520765). Directly hardens kod's swarm worktree merge + cleanup path.

### 11.12 Prewalk: one-way model handoff mid-session (UPGRADE of P7 routing) — M

Armed with a target model, prewalk injects a hidden "deep-plan nudge"; at turn end, once the todo gate is open **and the first workspace-mutating action fires** (edit/write, or write/exec-tier device dispatch — read-only calls don't trigger), it waits for persistence, **scrubs the nudge messages from history** (invalidateMessageCache + splice), switches the ephemeral model, and steers a hidden checklist. `plan-yolo` variant: plan mode + write tool, on plan approval autosaves and switches to the implementation model (prewalk.ts D493862–494288). This is CacheLedger-compatible: the handoff happens once, at a mutation boundary, where the cache is dirty anyway.

---

## 12. Mem1 — Memory upgrades

### 12.1 Per-memory-type Weibull forgetting curves (UPGRADE of half-life decay) — S

`decay = exp(-((age_hours / eta) ** k))` — the shape parameter `k` is the whole point: `k<1` = heavy-tailed (durable knowledge decays slowly forever), `k>1` = rapid early decay then flat. Their table (weibull.ts D940027–940156): `profile {k .3, η 8760}`, `relationship {.35, 8760}`, `preference {.4, 4380}`, `entity {.5, 4380}`, `learning {.7, 1440}`, `fact {.8, 720}`, `artifact {.75, 2160}`, `pattern {.6, 1680}`, `project {.85, 1080}`, `context {.85, 360}`, `observation/instruction {.9, 480}`, `goal {.9, 720}`, `decision {1.0, 336}`, `commitment {1.0, 240}`, `error/issue {1.1, 336}`, `event {1.2, 168}`, `request {1.5, 72}`, `general {1.0, 168}`. A "preference" at 6 months still scores ~0.75; a "request" is dead in 3 days. kod: add `k: f32` next to the existing half-life; the type comes from kod's existing memory-type enum.

### 12.2 Veracity consolidation: supersede/contradict with Bayesian confidence — M

The concrete algorithm behind the planned "supersede/contradicts": SPO fact content id = `cf_ + sha256(len-prefixed NFC(s|p|o))[:24]`. Re-mention: `conf += (1 − conf) · weight · 0.3` (saturating update); new fact base = `weight · 0.5`. Veracity weights: `stated 1.0, inferred 0.7, imported 0.6, unknown 0.8, tool 0.5` (D939544). Same subject+predicate, different object → `conflicts` row (contradiction). Consolidation pass: facts with `mention_count > 2` auto-resolve contradictions by confidence (loser gets `superseded_by = winner`); manual resolution refuses re-resolution. `getContaminated(limit, minImportance)` surfaces high-importance facts with veracity ∈ {inferred, false, unknown} for review. Plugs into kod's hourly consolidation loop in redb.

### 12.3 Sharpshooter: friction-gated decision memory — M

Durable project decisions extracted **per prompt** by a smol model (fallback role `smol`, Effort.Low, 2048 tokens, toolChoice required): `record_deltas` tool emits `{kind ∈ architecture_decision|product_decision|style_decision|constraint|rejected_approach|correction, statement (timeless normative, no task state/paths), rejectedAlternative?, rationale?, source, evidence, friction{corrective, regression, subtle}}`. **Admission gate: `evidence` must be an exact substring of the current user prompt, verified host-side — hallucinated decisions are dropped.** Per committed user prompt (skip `< 16` chars or `/`-prefixed); deltas queue as JSON files; consolidation every **5 min** under a file lock → LLM (Effort.Medium) rewrites `architecture.md / product.md / style.md` with a **hard 120-line ceiling per file**; consumed deltas deleted. Injection: "These are friction-earned decisions; follow them unless the user overrides" (sharpshooter/* D505405–506672). Upgrades kod's `decisions.rs` from append-log to curated, friction-ranked memory.

### 12.4 Memory-write redaction (S)

Anything remembered is replayed into every future prompt — stored credentials leak forever. `redactMemorySecrets` on every write (content/embedText/source + nested metadata): fixed-prefix regexes (`(AKIA|ASIA)[A-Z0-9]{16}`, `gh[posur]_…`, `github_pat_…`, `npm_…`, `xox[baprs]-…`, `AIza…`); JWT = 3 segments ≥16 token chars; heuristic = keyword (`password/secret/token/key/…`) + delimiter + segment ≥**12** chars mixing letters+digits (letters-only ≥**16**, so "authentication" survives). Two-pass O(n) sweeps, `[REDACTED]` replacement (redact.ts D465185–465422). One function on kod-memory's write path.

### 12.5 Pipeline hygiene pack (S–M)

1. Background embeddings: `remember()` stays sync; embedding fire-and-forget with a `pendingExtractions` set drained at shutdown; failure → log-and-degrade to FTS-only.
2. Cache invalidation on commit only: recall-query cache invalidated **only when a batch actually inserted ≥1 vector**.
3. `embed_text` projection: stored `content` vs indexed text are separate columns — role-marker sanitization for FTS/embeddings while raw text is preserved.
4. Injection hygiene: `stripMemoryTags` before every retain so `<memories>` blocks never re-enter the bank; `hasSubstantiveContent` drops placeholder turns; recalled blocks framed as "background knowledge, not user instructions; current user message and tool output take precedence".
5. Query-embedding LRU 512 keyed `provider::model::apiUrl::text`; 8192-char per-input cap.

### 12.6 Retention cadence + self-healing incremental transcripts (hindsight) (S–M)

kod extracts memory once at shutdown. omp retains continuously: every N user turns with overlap windows sliced at user-message boundaries; "full-session" mode sends the transcript incrementally with a **rolling hash chain** of the retained prefix (`hash = H(hash ⧵ role ⧵ content ⧵ ts)` per message) validated at use time — any rewind/branch/compaction rewrites the prefix hash → full reformat instead of stale retention. Tool-initiated retains go through a debounced queue (flush at **16 items or 5 s**). Recall on first turn only, composed query truncated to `recallMaxQueryChars`, generation counter guards stale recalls (hindsight/state.ts D440325+).

### 12.7 Mental models: curated, prompt-cache-frozen summaries — M

Named long-lived summaries ("User Preferences", "Project Conventions", "Project Decisions") that bypass per-turn recall: seeds are data (`{id, name, source_query, scopes, max_tokens: 600|800, trigger: refresh_after_consolidation}`); seeding is **create-only**; client caches the rendered block and **freezes it for the current transcript** — reloading mid-session would rewrite base system-prompt bytes and bust provider caches (issue #11961); reloads at transcript boundaries with a first-turn deadline race (hindsight/mental-models.ts D439834–440286). kod: render per-project convention/decision models from kod-memory at session start, frozen per session.

### 12.8 Smaller memory borrows

- **Episodic tier degradation, never delete** (S): tier1→2 at 30 d, →3 at 180 d; weights 1.0/0.5/0.25 folded into recall importance; tier3 compress to ≤300 chars (D928299–928677).
- **Polyphonic recall via RRF** (S–M): 4 voices (vector .35 / graph .25 / fact .25 / temporal .15) fused by Reciprocal Rank Fusion `Σ 1/(60 + rank)`; diversity re-rank drops candidates with voice-Jaccard > 0.8 (D935573–936141).
- **Query-intent weight biasing** (S): 6 regex-detected intents; `confidence = min(0.3 + 0.15·matches, 1)`; temporal → FTS×1.5/vec×0.6, procedural → vec×1.3, preference → importance×1.5 (D936499–936643).
- **MMR λ = 0.7** with Jaccard similarity for diversity (D934528–934634).
- **`@`-import expansion for AGENTS.md-style files** (S): depth 5, code-fence aware, cycle-safe (D406539–406817) — an upgrade to the planned conditional AGENTS.md.

---

## 13. O1 — Catalog, stats, eval

### 13.1 Model catalog metadata that earns its keep — M

What metadata actually drives decisions (models.ts D314992–315193, model-thinking.ts D314721–314898):
- `cost {input, output, cacheRead, cacheWrite /1M}` with **cacheWrite priced per TTL**: 5-min tokens at 1.25× input, 1-h tokens at input × 2, unattributed residual priced at the 5-min rate (never free) — the exact input P0's CacheLedger needs;
- `timeBased {peakWindows, offPeakMultiplier}` — UTC integer arithmetic, next-transition lookup over a 7-day horizon (scheduled-billing routing);
- `longContext {inputThreshold, rates}` — over-threshold pricing tier (e.g. Gemini >200k);
- `reasoning + thinking.efforts[]` (ladder `minimal..max`) + `effortRouting[effort]→wire id` + per-provider effort maps → **effort clamping** walks down the ladder;
- `int` (intelligence score) + `tps` with **dialect-tolerant id resolution** (provider-respelled ids resolve to the scored vendor id via candidate-id generation + identity agreement);
- **provider priority table**: first-party > aggregators > gateways — resolves ambiguous role matches.

Minimum viable slice for kod: `ModelMeta {context_window, max_output, cost w/ cache-rate split + long-context tier, efforts ladder, int/tps}` + pricing fns + provider priority for swarm per-capability routing.

### 13.2 Stats: behavioral user metrics + per-request analytics — M

Per assistant turn: model/provider/api, duration, **TTFT**, stopReason, usage, `agentType`, `costUnpriced`. Aggregates: error rate, **`cacheRate`** (fraction of prompt served from cache), **`cacheSavings`** (cost saved vs uncached — can go negative when writes cost more), avgTtft, tokens/sec, hourly/daily series, per-provider burn, per-tool stats. The behavioral set per user message is a *free quality signal* (user-metrics.ts D1013543–1014243): `negation` (line-leading `nope/nah/wrong`, "that's not what i meant"), `repetition` ("i meant", "still doesnt work"), `blame` ("you didnt", "why did you"), `anguish`, `yelling` — negation/repetition spikes = the agent missed the ask. Port the user-metrics module first (pure functions) into a new `kod-stats` over the session journal.

### 13.3 if-bench: cheapest first eval — S/M

One growing, fully cacheable conversation per model (all prior turns byte-identical). Turn N: model must apply **N glyph actions** over the array *it reported last turn* (state carried in its own reply — unrecoverable once drifted) while a cat-sound directive rotates through start/middle/end. Score = depth before it loses the array OR drops the sound. Defaults: **24 turns, array 24, maxTokens 32768, par 4, temp 0**. Two separable failure modes = working-memory + instruction-following, no task corpus needed (if-bench/* D441030–441828). kod can A/B swarm-role models with this alone.

---

## 14. X1 — Extensibility & security

### 14.1 Reversible keyed secret placeholders (THE security gem) — M

Model sees lossless placeholders (`«Credential-<digest>»`) instead of raw secrets; edits still work because **tool args are deobfuscated before execution** (secrets/index.ts D479410–479808, patterns D480523–480624, message-transform D479809–480522):
- Per-install 32-byte base64url HMAC key at `$XDG_STATE_HOME/…/secret-placeholder.key` (mode 0600, `wx` create, ephemeral fallback);
- Sources: `secrets.yml` (project overrides global by content), env vars matching `(KEY|SECRET|TOKEN|PASSWORD|PASS|AUTH|CREDENTIAL|PRIVATE|OAUTH)(_|$)` with `MIN_ENV_VALUE_LENGTH 8` + connection-URL passwords, and 12 vendor regexes each with **`literalPrefixes`** (skip regex scan unless a literal substring is present — huge win);
- Two modes: `obfuscate` (reversible) | `replace`. Obfuscation walks **only prose slots** of provider payloads (protocol-aware visitor; enum/const/grammar/encrypted-reasoning stay opaque); deobfuscation applies **only to model-authored** text/tool-args (user + tool-result bytes stay literal);
- Stream redaction via `CREDENTIAL_PREFIX_RULES` (sk-ant-, ghp_, AKIA, eyJ, `-----BEGIN`→line mode, `Bearer `→bearer mode) **without entropy gates**;
- Fixed-point safety: `REGEX_REMATCH_BACKSCAN 512` in-context re-match probe, deterministic `Z/ZZ`-sentinel replacements.

```rust
// kod-types/src/redact.rs (extend) + kod-core pre-send hook + kod-tools arg deobf
pub struct SecretVault { key: [u8;32], map: HashMap<String, SecretRef> } // digest -> source

impl SecretVault {
    /// Pre-send: walk prose slots of the CompletionRequest, replace matches.
    pub fn obfuscate_request(&self, req: &mut CompletionRequest) -> ObfuscationReport;
    /// Pre-execution: model-authored args only — user/tool bytes stay literal.
    pub fn deobfuscate_tool_args(&self, args: &mut serde_json::Value) -> Result<(), DeobfError>;
}
```

Closes the "tool output → provider → echoed into edit args" hole that PDF-P7 (sensitivity routing) half-covers. Complementary: P7 routes *work* away from untrusted models; this makes secrets *invisible* regardless of routing.

### 14.2 Capability discovery registry — M

NOT capability-security (see below): a **config-discovery registry** unifying all extension config (skills/MCP/commands/rules) from native + foreign dirs (`claude`, `codex`, `cursor`, `gemini`, `opencode`, `windsurf`) with priority bands. `defineCapability{id, key(), equivalent?, validate?, toExtensionId?}` + `registerProvider{priority}`; load = parallel providers, `_source` provenance mandatory, first-wins dedupe, **`suppress()` vs `filter()` semantics** (a suppressed item still claims its dedupe key so a disabled project entry keeps the same-named user entry off), validation errors → warnings, fs cache with stat-gated reads (capability/index.ts D367359–367955). Makes `.claude`/`.cursor` interop a one-provider affair for kod-skills/kod-mcp.

### 14.3 TTSR — Time-Traveling Stream Rules — M

Declarative rules matched against the **streaming assistant output** (text, thinking, tool args) that abort the stream, inject the rule content, and retry — the rule "time travels" retroactively into the generation that triggered it. Rule = regex + optional **ast-grep AST conditions** (matched only against reconstructed edit/write snapshots, once on `toolcall_end` — ~90 ms at 150 KB, deduped via last-snapshot map); scope `tool:edit(*.rs)|text|thinking`; `interruptMode: never|prose-only|tool-only|always`; `repeatMode: once|gap` (gap = 10 turns) (export/ttsr.ts D427144–427771, coordinator D504272–505405). kod hooks are pre/post only; Jev's mid-stream switch is the plumbing precedent. Kod-relevant rules: forbid TODO-left-in-diff, force test-runner reminders on certain file edits, nix secret-shaped strings in prose.

### 14.4 Agentic commit + conventional map-reduce — M/L

`omp commit` runs a disposable AgentSession with **only 8 custom tools** (git_overview, git_file_diff, git_hunk, recent_commits, analyze_file, propose_commit, propose_changelog, split_commit) writing structured proposals into shared state; the *runner* (not the model) decides completion — ≤3 synthetic validation reminders until `isProposalComplete`. Validation: summary ≤72 chars, ≤6 detail items with priority scoring (security +100, breaking +90, perf +80, bug +70…), past-tense-first-word verb tables, **type-consistency vs changed paths** (docs→*.md, ci→.github/workflows, build→Cargo.toml…) (agentic/agent.ts D391812–392141, validation D392932–393065). Deterministic fallback for huge diffs: map-reduce (`mapReduceThreshold 5_000` tokens, `MAX_FILE_TOKENS 50_000`, `MAP_CONCURRENCY 16`, batch = `total×5/(16×4)` capped 16k; 72/96/128-char limits; SQLite response cache TTL 14 d) (map-reduce.ts D396484–396858). kod has git tools + REVIEWER roles; neither exists today.

### 14.5 Others (each S/M)

- **Internal URL router** — see §7.5.
- **MCP HTTP header/origin policy** (D463910–464046): client headers win case-insensitively; `withoutHeader` strips transport-reserved; origin-locked servers follow redirects manually, attach configured headers only on same-origin hops, `MAX_REDIRECT_HOPS 5`, **method-changing redirect of non-GET refused** (only 307/308 followed). UPGRADE for kod-mcp HTTP plans.
- **Deferred custom tools** (D431770–432060): tools declare `deferrable`; execution becomes `pushPendingAction{label, apply(reason), reject?(reason)}` and a hidden `resolve` tool lets the model apply/discard with a stated reason — preview-then-commit without approval-prompt spam. Natural fit for kod's batched-approvals plan.
- **Scan-plan tamper evidence** (preflight.ts D483585–484007): `treeDigest = sha256(sorted(relpaths ⊕ exec-bit ⊕ symlink-target ⊕ content) + headSha)`, plan fingerprint over canonical JSON, `assertFresh()` re-derives before acting; scope paths rejected on `..`/absolute/null-byte, realpath-validated; output dir 0700. Generalizes kod's policy provenance to *time*.
- **OTLP GenAI telemetry** (D351976–352553): turn-level spans/metrics under OpenTelemetry GenAI semconv (`gen_ai.client.token.usage` histogram with token type, tool counters), env-gated lazy export, flush at turn boundaries. `opentelemetry-rust` → new `kod-telemetry`.
- **`loadPage` fetch hardening** (D592545–592904): 3-attempt UA rotation, single 429 retry honoring `Retry-After` clamped to 10 s, bot-block detection (403/503 + cloudflare/captcha keywords → next UA), streaming `MAX_BYTES 50 MiB`, charset sniff, output cap 500k chars. Upgrades `kod-tools::web`.
- **Structured search query parser** (D594783–595638): parses agent-typed Googleisms (`site: -term OR "quoted" before:/after: filetype: inurl: intitle: lang:`), maps onto per-engine `QuerySyntax` capability flags, and post-filters **relaxing any constraint that would eliminate all hits** with an explicit `Note: … constraint was relaxed` line.
- **Model roles + role chains** (D401595–401752): named roles (`smol, slow, commit, web, memory, judge, advisor…`) with ordered fallback chains — the vocabulary the P7 security gate can target ("route secret-touched work to `trusted` role").
- **Print/RPC event-stream hygiene** (D468309–468642): drop partial-snapshot events + provider payload from JSONL logs (multi-GB → linear); stdout writes chained on previous completion (backpressure); ACP `isolateProtocolStdout` so nothing corrupts the protocol fd.
- **Two-phase extension load + guarded eval** (D432443–433114, D430113–430310): concurrent import → sequential deterministic binding; subagents rebind prepared factories without re-evaluating the module graph; `withHostGuard` patches `process.exit` to throw during eval + stdin-hijack guard. Prerequisite for any future kod plugin story; kod should keep skills/hooks out-of-process regardless.
- **Exit diagnostics** (D491764–492079): `session_exit` journal marker `{reason, kind, pendingToolCalls[]}` + `tool_execution_start` markers → on resume, an interrupted-turn abort message tells the model which tool call never completed.

---

## 15. Not-borrow list (and what kod already does better)

| omp feature | Why skip |
|---|---|
| **In-process shell without sandbox** | Adopt only via §8's session-shell profile; kod's bwrap/Landlock/Seatbelt is strictly ahead — pi-shell has **no** OS sandbox |
| **Snapcompact full-frame compaction** | Experimental, vision-model-only, L effort; the inline tool-result variant (§4.5) is the practical slice; their own SQuAD harness shows recall varies by font/provider |
| **Streaming shadow-plan IR for eval cells** (§10.3) | Presupposes an eval/code-cell tool kod doesn't have; revisit when/if kod grows one |
| **mnemopi LLM-driven consolidation, multi-bank manager, SHMR harmonizer** | kod's no-LLM hourly consolidation stance stands; sharpshooter's tiny-model extraction (§12.3) is the adoptable path instead |
| **Redis/SQL/session-CAS backends** | YAGNI; keep only the `expectedSize` CAS idea if kod ever grows remote sessions |
| **`credential_pin` / Anthropic metadata spoofing** | Claude-Code-specific tricks |
| **collab-web live replication, voice/TTS/STT, browser-relay, IRC bridges as product surface** | Different product; borrow only the YieldQueue/delivery semantics (§11.4–11.5) |
| **Bun-specific plumbing** (`EventLoopKeepalive`, yield gates, TSF streaming, crash_handler, napi task pools) | Rust/tokio has no analog — that machinery exists only to survive the JS/Rust edge kod doesn't have |
| **Native tokenizers via napi** | Port the *idea* (provider-true estimates, free-fit probe) with Rust crates instead (§9.11) |
| **Model-mentions (`^model` pseudonyms)** | kod swarm already routes explicitly per capability |
| **utok multi-tokenizer napi addon, utok benches** | Weight for kod's single-binary constraint |

**kod-ahead list** (kod is already better here — omp confirms the direction rather than contributing a borrow):
1. **OS sandboxing** (bwrap/Landlock/Seatbelt + fail-loud Landlock ABI refusal) — omp's command-risk gate is the *only* substitute and it has no process isolation at all.
2. **Golden-prefix byte tests as CI contract** — omp benches the prefix; kod *proves* it byte-identical.
3. **Policy engine as a pure decision function with provenance + JSONL audit** — omp's per-tool approval functions are ad-hoc by comparison.
4. **Structured post-edit diagnostics (`check` tool)** — omp relies on LSP writethrough; kod's deterministic cargo/tsc/ruff/govet parse is a good backstop.
5. **Swarm worktree merge determinism** — omp has no coordinator merges at all (FileTouch-style observational coordination is jcode's contribution; omp's pi-iso CoW is the isolation answer — see §11.11).

---

## 16. Merged adoption roadmap (P0–P9 ∪ B1–B12 ∪ this document)

Phases assume P0–P9 (PDF) and B1–B12 (jcode) are in flight; each phase interleaves the new deltas where they unblock or harden prior work.

**Phase 0 — accounting truth (P0 + deltas):** TokenUsage cache fields; transcript breakpoints; tool-filter hysteresis; **provider-anchored ContextGauge (§2.4)**; **CacheLedger gains cacheWrite-per-TTL pricing + long-context tiers (§13.1)**; **savings journal for compression (§4.5 journal pattern)**.

**Phase 1 — transcript coherence + mechanical reduction:** **digest/version protocol + longest-stable-prefix sync (§2.3)**; **supersede pruning + useless flag (§3.1)**; **cache-warm-suffix guard (§3.3)**; **shake (§3.2)**; **date-cwd append-only reminders**; **reset_boundary marker (§9.11)**. Golden tests for all of it — kod already has the harness.

**Phase 2 — compaction ladder maturity:** **ordered dispatcher with fall-through (§4.1)**; **speculation lead band (§4.2)**; **budget projection + no-reduction guard (§4.3)**; jcode handoff/soft methods slot in as dispatcher entries; **Anthropic native lane (§4.4)**; *optional* snapcompact inline (§4.5) behind a flag.

**Phase 3 — loop hardening:** **oneshot retry kit + hint extraction (§9.1)**; **replay-safe stream retry (§9.2)**; **thinking-loop guard (§9.3)**; **unexpected-stop classifier (§9.4)**; **judgment framework (§9.5)**; **pause gate (§9.8)**; **provider concurrency bracket (§9.9)**; **image budget (§9.10)**; **retry-fallback chains (§9.7)**.

**Phase 4 — tool surface:** **`xd://` mounting (§6)**; **minimizer crate (§5)**; **hashline edit (§7.1)**; **LSP write-through (§7.2)**; **parse_cache (§7.3)**; **fast-wins bundle (§7.7)**; **internal-URL router (§7.5)**; **hub tool (§7.6)**.

**Phase 5 — swarm & async maturity (B1 ∪ deltas):** FileTouch bus + soft interrupts (jcode); **WorkPool (§11.1)**; **park/revive (§11.2)**; **IrcBus semantics (§11.3)**; **AsyncJobManager + YieldQueue (§11.4–11.5)**; **advisor emission guard + delta-split (§11.8)**; **cleanse file-sticky dispatch (§11.9)**; **isolation-ownership GC (§11.11)**.

**Phase 6 — memory & quality:** **Weibull curves (§12.1)**; **veracity consolidation (§12.2)**; **sharpshooter (§12.3)**; **write redaction (§12.4)**; **hygiene pack (§12.5)**; **retention cadence (§12.6)**; **mental models (§12.7)**; **stats + behavioral metrics (§13.2)**; **if-bench (§13.3)**; **auto-thinking (§9.6)**.

**Phase 7 — security & extensibility:** **secret placeholders (§14.1)**; **capability discovery registry (§14.2)**; **TTSR (§14.3)**; **agentic commit (§14.4)**; **tamper-evident job preflight (§14.5)**; **MCP header policy (§14.5)**; **OTLP telemetry (§14.5)**.

**Ongoing/strategic:** **speculative read execution (§10)** after Phase 3's stream guards exist; **session shell (§8)** as a long-track project behind policy; **prewalk (§11.12)** and **goals (§11.6)** when product surface demands them.

Sequencing logic: Phase 1 must precede Phase 2 (the dispatcher's shake/prune rungs need the coherence protocol); Phase 3's stream guard precedes Phase 7's TTSR (same tap points, different consumers); everything in Phase 5 assumes kod already has FileTouch from jcode-B1.

---

## 17. Appendix — oh-my-pi source map (dump line ranges)

| Subsystem | Path | Dump lines |
|---|---|---|
| Agent loop contracts | `packages/agent/src/append-only-context.ts`, `message-cache.ts`, `pause.ts`, `run-collector.ts` | D140737–141391, D141849–142485, D143773–143866 |
| Compaction core | `packages/agent/src/compaction/{pruning,shake,tool-protection,transcript-tokens,anthropic,branch-summarization}.ts` | D144159–145619, D142836–143145 |
| Tokenizer/estimates | `packages/agent/src/{tokenizer,thinking,replay-policy}.ts` | D141512–142833 |
| Retry/stream guards | `packages/ai/src/*` (oneshot-retry, retry-after, empty-completion-retry, thinking-loop, tool-call-loop-guard) | D156554–156800, D204685–204886, D208544–209696 |
| Judgment | `packages/ai/src/judgment/*`, `coding-agent/src/judgment/index.ts` | D170728–171289, D448271–448576 |
| Auto-thinking | `coding-agent/src/auto-thinking/classifier.ts` | D355252–355389 |
| Retry-fallback chains | `coding-agent/src/session/retry-fallback-chains.ts` | D495025–495612 |
| Speculation | `coding-agent/src/speculation/host.ts`, `eval/speculation/*`, tests | D512567–512875, D424287–425518, D153099–153474 |
| Compaction dispatcher/methods | `coding-agent/src/session/{compaction-methods,compact-modes,snapcompact-inline,snapcompact-savings-journal,speculation-lead}.ts` | D491049–491317, D501893–502622 |
| Session tree/persistence | `coding-agent/src/session/{session-entries,session-persistence,turn-persistence,session-title-slot,session-listing}.ts` | D496694–496438, D500112–505017, D501416–501562, D497930–498783 |
| Snapcompact package | `packages/snapcompact/*`, tests, metaharness adapter | D978919–979408, D152782, D923691–924167 |
| Minimizer | `crates/pi-shell/src/{minimizer,config,plan,pipeline}.rs`, `defs/*.toml` | D115684–119296, D119807–122837 |
| In-process shell | `crates/pi-shell/src/{lib,cancel,output_decode}.rs`, `crates/pi-builtins/src/{lib,factory,bg}.rs`, `crates/pi-natives/src/shell.rs` | D115489–116637, D22788–23149, D82155–82869 |
| Edit/AST | `crates/pi-edit/src/modes/hashline/*.rs`, `crates/pi-ast/src/parse_cache.rs`, `coding-agent/src/tools/read-format.ts` | D49280–49411, D11177–11616, D540796–541376 |
| LSP writethrough | `coding-agent/src/lsp/{writethrough,diagnostics,deferred-diagnostics}.ts` | D452629–456615 |
| Blob broker / internal URLs | `coding-agent/src/blob-broker/*`, `internal-urls/*` | D359108–366857, D445122–446620 |
| Hub / jfind / xdev | `coding-agent/src/tools/{hub,jfind,xdev}/*`, `essential-tools.ts` | D531939–531993, D561336–564362, D545941–546404 |
| pi-iso / pi-vcs | `crates/pi-iso/src/{lib,diff}.rs`, `crates/pi-vcs/src/*` | D64993–66110, D132874–133375 |
| Memory (mnemopi/hindsight/sharpshooter/redact) | `packages/mnemopi/src/core/*`, `coding-agent/src/{hindsight,sharpshooter,memory-backend}/*` | D928299–940156, D438038–440950, D505405–506672, D465185–465422 |
| Advisor | `coding-agent/src/advisor/{advise-tool,emission-guard,config,delta-split,loop-guard,watchdog}.ts` | D353274–355163 |
| Catalog/stats/if-bench | `packages/catalog/src/*`, `packages/stats/src/*`, `coding-agent/src/if-bench/*` | D314010–323511, D1009092–1014243, D441030–441828 |
| Multi-agent/task | `coding-agent/src/task/{workpool,workpool-yield,persisted-revive,isolation-ownership,provider-concurrency}.ts`, `registry/agent-{registry,lifecycle}.ts`, `src/irc/bus.ts` | D478423–479409, D520170–525210, D447606–448094 |
| Goals/todo/plan-mode | `coding-agent/src/{goals,plan-mode}/*`, `session/{todo-tracker,yield-queue,async-job-delivery}.ts` | D437357–438037, D477928–478422, D503511–505404, D487612–487748 |
| Extensibility/security | `coding-agent/src/{extensibility,capability,security,secrets}/*` | D428428–437347, D367047–369155, D481704–486332, D479410–481703 |
| Commit | `coding-agent/src/commit/{agentic,conventional}/*` | D391224–399330 |
| Web | `coding-agent/src/web/{search,scrapers}/*` | D577498–604290 |
| TTSR | `coding-agent/src/export/ttsr.ts`, `session/ttsr-coordinator.ts` | D427144–427771, D504272–505405 |
| Prewalk / worktree / cleanse | `coding-agent/src/{session/prewalk,session/session-worktree,cleanse/loop}.ts` | D493862–494288, D501622–501738, D369789–370057 |
