Interesting — there's already a `citations.rs` in kod-core. Let me look at it and the engine directory.
The engine mod.rs is 12k lines. Let me fetch the raw file and search for the key integration points.
Good news — I went through the repo (latest commit `f27119d`, Sep 22 2026) and your idea slots into KOD more cleanly than you might expect, because the scaffold for it already exists. The verdict up front: **implement it as a deterministic post-processing pass next to `citations.rs`, not as a tool** — that's the only way it stays consistent with KOD's architecture, and the codebase already proves the pattern works.

## What KOD already has that you'd build on

**`kod-core/src/citations.rs` is the proof of concept.** It parses `file.ext:line` / `file.ext:line-end` references out of the model's reply text with a regex, verifies each against the filesystem (path resolves, line in range) purely locally — no LLM call, no network — and appends a `## Citation check (N/M verified)` block *only* when something fails. The doc comment explicitly states the philosophy: report only what can be proven, a clean reply stays clean【turn1fetch0】【turn2fetch0】. Your excerpt substitution is the same shape, one step further: instead of verifying the model's *claim* about a location, you resolve the location and splice in the real content.

**The two integration sites already exist.** `check_and_annotate` is called in exactly two places in `engine/mod.rs`: the collected path in `process()` and the streaming path in `process_streaming()`. In the streaming path, the citation block is pushed as an extra chunk over `chunk_tx` so the TUI renders it *as part of the reply, not as a separate message* — which is almost verbatim what you asked for【turn9fetch0】【turn9fetch1】. There's even an in-band marker precedent: `STREAM_RESET_MARKER` (`\0kod-stream-reset\0`) is a sentinel the engine sends over the chunk channel that the TUI interprets specially【turn3fetch0】.

**Ordering already works in your favor.** In both paths, the citation pass runs *before* `remember_turn_for`, which is what writes `final_text` into the transcript history and short-term memory. So if your expansion runs at the same site, the expanded text — not the sentinel — is what gets stored as history, which is the right call (more on that below).

## Proposed design: `kod-core/src/excerpts.rs`

A sibling module to `citations.rs` with the same constraints:

**Sentinel format.** Since you're on hosted/local models via the `LlmProvider` trait (Ollama default, qwen2.5-coder:7b, DeepSeek-R1 14B...), there's no fine-tuning and no reserved token — the sentinel is a text convention taught in the system prompt. Keep it dead simple for small models: a single line on its own, e.g.

```
⟦kod:excerpt src/parser.rs 42-80⟧
```

One line matters: your streaming buffer only needs to hold one line before deciding what to do with it, and single-line regexes are trivial. The private-use `⟦⟧` delimiters avoid collisions with Rust/Python code that legitimately contains `:::`, `[`, or template syntax.

**Resolution.** Read the file directly with `std::fs`, exactly like `citations.rs::verify()` does — resolved against `self.working_dir`, absolute paths allowed. Do **not** go through `ToolRegistry`. This is the single most important conflict-avoidance decision: a real `ToolCall` would (a) cost a full round trip (prefill + decode cycle), (b) show up as a live tool row in the TUI, (c) get recorded as a `SessionEntry::ToolCall` in the JSONL session log and be re-executed by `kod replay`, and (d) interact with the policy engine and per-path locks — all for a read you can do synchronously in microseconds. Your "not displayed as a tool call" requirement is achieved by it *not being a tool call at all*.

**One deviation from your original framing, deliberately:** don't hide the sentinel line — keep it visible as a one-line provenance header above the expanded block. KOD's whole identity is auditability (`kod replay` exists precisely so every read is checkable later), and the citations module's design notes are emphatic about visible, provable annotations over magic. The sentinel line *is* the citation; the code under it is verified content. You get the token savings and the inline rendering without breaking the audit story. If you later want collapsible rendering, the TUI already has the pattern — `t` toggles tool-output visibility and `o` expands collapsed rows — so a `codex`-style collapsed excerpt is a natural follow-up, not a v1 requirement.

**Failure behavior.** Copy the citations approach exactly: a sentinel whose path doesn't resolve or whose range exceeds the file gets a `⚠`-style inline annotation, never a silent drop and never a guess. This matters doubly for your target models — a 7B model *will* hallucinate ranges sometimes.

## Where it hooks in, concretely

Both call sites, immediately before the existing citation pass:

```rust
// process() — after final_text is settled
let final_text = crate::excerpts::expand(&final_text, &self.working_dir);
// then the existing Research-mode citation pass as today
```

For `process_streaming()`, same, plus send the expanded block over `chunk_tx` the way the citation block is sent today, so the TUI shows it inline【turn9fetch1】.

Teach the convention in the **cacheable prefix** of the system prompt. `build_grounded_request` splits the router's prompt at the `## Volatile suffix` marker into a byte-stable cacheable head (identity + repo map) and a volatile tail, with Anthropic `cache_control` wired through on the last cacheable segment【turn5fetch1】【turn7fetch0】. A 3–4 line instruction with one example belongs in the identity section — it's stable across the session, so it doesn't churn the cache breakpoint. Don't ship it as a *skill*: skills are trigger-matched, and this convention needs to be always-on.

Gate it in config: add an `[excerpts]` section (`enable`, `max_lines`, `max_bytes`) to `kod-config`, using the existing clamp-with-warning-at-load pattern. For v1 I'd enable it wherever citations run (Research task type) or just always — it's a no-op when no sentinel appears, same as `check_and_annotate`.

## Conflict checklist — what *not* to touch

- **kod-provider: zero changes.** The sentinel is plain text on the wire; `ToolCallStart`/`ToolCallDelta` SSE framing is untouched, and both OpenAI-compatible and Anthropic providers work identically.
- **Session log / replay: zero changes.** No new `SessionEntry` variant needed. If you want auditability of *what was expanded*, the visible sentinel line in the persisted reply already carries it.
- **History/memory: store the expanded text.** `record_turn_for` truncates at `MAX_TURN_CHARS` and the transcript budget trims by rendered char count — a truncated sentinel would leave a dangling marker the model misreads next turn, while truncated *expanded* text degrades harmlessly. The cost is input tokens on later turns, but KOD sessions cap at a 32k context window by default and the transcript budget bounds this anyway; prompt caching (which the transcript breakpoint already gets you) makes the repeat reads cheap.
- **Jev ordering:** run expansion *before* `semantic_verify_citations` (P4.5). That pass reads the cited code to check whether it supports the claim — with byte-exact excerpts in the text, its judgments get *more* accurate, not less.
- **One pre-existing gotcha to be aware of:** `extract_citations` scans the whole reply including fenced code blocks, so code containing `foo.rs:42`-shaped strings can false-positive today. Your expanded excerpts increase the surface for that slightly. Not a blocker, but worth a follow-up that skips fenced regions — the sentinel line itself will match the citation regex, which is actually fine (it verifies as a real location).

## The honest caveat

Your target models are small local ones, and a novel sentinel convention is exactly the kind of instruction a 7B model follows loosely — expect occasional pasted code anyway (harmless, just no savings) and occasional malformed sentinels (handled by the ⚠ fallback). The mechanism is worth building because it's cheap, deterministic, and never makes things worse; but the token savings will be proportional to how reliably qwen-coder adopts the convention, which you'll only know by instrumenting it. A quick measurement first — log what fraction of reply text is verbatim duplicates of `read_file` results across a few sessions — tells you the ceiling before you write the module.

Line numbers above will drift with the next commit — the anchors to grep for are `check_and_annotate`, `process_streaming`, `remember_turn_for`, and `build_grounded_request`, all in `engine/mod.rs`.
