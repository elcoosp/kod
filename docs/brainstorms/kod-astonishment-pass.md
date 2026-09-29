# kod — The Astonishment Pass

*A feature-freeze brainstorm: how to make kod astonishing without adding a single
feature. Companion to `kod-production-readiness-review.md`,
`kod_borrow_from_oh_my_pi.md`, `kod_borrow_from_jcode.md`, and
`kod-tui-uiux-production-plan.md`. All line numbers verified against `main` at
commit `956ae31`.*

---

## 0. The thesis

The borrows are done. oh-my-pi is wired (all of §2–14.5 landed), jcode's cache
stack, file-touch bus, and compaction ladder are in, and the production-readiness
review's critical findings are fixed. The workspace now has ~3,700 test
attributes across 20 crates, a golden-prefix invariant, a cache ledger, an
admission-controlled compaction dispatcher, TOCTOU-evidenced speculative reads,
and a TUI whose state discipline is pinned by regression tests.

And yet the product is not astonishing. Reading the whole tree, the gap is never
"missing capability." It is four recurring patterns:

1. **Built but parked.** Some of the best mechanisms in the codebase are one
   wiring-step short of affecting behavior. They are paid for (written, tested,
   documented) and never received by the user.
2. **Feedback asymmetry.** The harness observes the model obsessively
   (context gauge, stop classifier, loop guard, off-track detector) but almost
   never *talks back* to the model, and the TUI observes the user but rarely
   *responds* (no clicks, no capability detection, no motion).
3. **Per-turn taxes.** A handful of O(tree) or O(transcript) costs run on the
   latency-critical path every single turn, each individually justified, none
   audited as a sum.
4. **Trust leaks.** A "Build: Passing" badge with no CI, an MSRV that cannot
   compile the code, a broken bench harness, and docs that describe a workspace
   that does not exist. Astonishment starts with the product never lying to you.

**The three laws of astonishment under a feature freeze:**

- **Law of completion.** An unwired mechanism is a feature you already built but
  never shipped. Wiring it is not "adding a feature" — it is collecting on one.
- **Law of feedback.** Astonishment is the product reacting to you: the model
  reacting to its own context pressure, the TUI reacting to a click, the theme
  reacting to the terminal.
- **Law of trust.** Every claim the product makes (badge, MSRV, doc line,
  error message) must be true. One lie costs more than ten missing features.

Everything below is tagged **[A] agent capabilities**, **[U] UI/UX**,
**[P] performance**, **[D] DX**, with effort (S ≤ 1 day, M ≤ 1 week, L > 1 week)
and impact (★ = notable, ★★★ = signature).

---

## 1. Wire the parked engines [A]

These exist, are tested, and do nothing. Each is a one-PR collect.

### 1.1 Make the unexpected-stop classifier act ★★★ (S)

`kod-core/src/engine/mod.rs:7363-7432` classifies "the model stopped without
finishing" with a real judge call and a measured rubric — then logs the verdict
and discards it ("diagnostic only"). Worse, `stop_reason` is never threaded out
of the streaming loop (`:7377-7385`), so even truncation is invisible.

The astonishment version: a verdict of *unexpected* triggers the same corrective
path the tool-loop guard already uses (`engine/mod.rs:11739-11773` has the
template): one System nudge — "You wrote 'let me check the code' and stopped.
Continue from exactly there." The user never sees it; they just notice the agent
no longer dies mid-thought. This is the single highest-leverage wiring in the
repo: the difference between an agent that needs babysitting and one that
doesn't.

### 1.2 Register remote + snapcompact in the compaction ladder ★★★ (S)

`RemoteMethod` (replays Anthropic's server-side `compact-2026-01-12`) and
`SnapcompactMethod` (rasterizes the older half to a 1-bit PNG) are fully
implemented in `compaction_dispatcher.rs:603-828` — and unreachable, because the
engine's ladder registers only `[Shake, Prune, Handoff]`
(`engine/mod.rs:3591-3643`). Two of the highest-fidelity, cheapest reductions in
the system cannot run in the product. Add them as ladder rungs (remote first for
Anthropic endpoints, snapcompact before handoff) and the compaction story goes
from "good" to "no other harness has this."

### 1.3 Tell the model how full the context is ★★★ (S)

`context_gauge.rs` is used *about* the model (compaction triggers, `/debug`,
TUI meter) and never *with* the model. The model re-reads files and pads answers
with no idea it is at 90% of its window. Inject one volatile-suffix line when
usage crosses 70%: "Context is 87% full. Prefer summaries over re-reads;
compaction will preserve your last 10 turns verbatim." The gauge already
computes this (`anchored_context_tokens`, engine `:2976-2993`); the injection
point already exists (the task-type guidance paragraph, router `:972-1004`).
This is the cheapest change that changes *behavior* rather than plumbing.

### 1.4 Turn on prune's cache-warm guard (S)

`engine/mod.rs:3002-3056` always passes `prefix_is_warm: false`, and prune's
warm band defaults are empty (`prune.rs:107-146`) — the guard the docs describe
is inert, so pruning happily mutates a cache-warm prefix and re-bills it. Feed
the CacheLedger's warmth verdict (which already exists,
`cache_ledger.rs:39-106`) into the flag and pick non-empty warm-band defaults.
Compaction stops being cache-hostile on the turn it matters most.

### 1.5 Give the swarm work-stealing dispatch (S)

`TaskCoordinator::least_loaded_agent` and `find_agents_with_capability` exist
(`kod-swarm/src/agent_registry.rs`) and the runner never consults them — the
README admits dispatch is 1:1 per subtask index. Wire the capability pool the
runner already builds and swarms stop idling while one agent drowns.

### 1.6 Act on Jev semantic overlap in swarm re-planning (S)

`swarm_runner.rs:440-457` computes semantic overlap between subtasks, logs it,
and re-plans only on syntactic glob overlap. Feed the semantic verdict into the
re-plan hint ("agents 2 and 3 will both touch auth/") — the computation is paid
for; only the consumer is missing.

### 1.7 Retire the double prompt build (M)

`engine/mod.rs:2807-2828` renders the full prompt twice per turn (once for the
`pending` string, once inside `build_prompt_plan`), each pass re-walking
AGENTS.md from the filesystem (`router.rs:1277-1287`) and re-rendering the repo
map. Halving pre-first-token I/O on the hottest path in the product is a
refactor with a user-visible latency dividend, not a feature.

---

## 2. Sharpen the feedback the model receives [A]

### 2.1 Word-boundary skill matching (S)

`kod-skills/src/matcher.rs:137-166` matches triggers with raw `contains`, so a
trigger `test` fires on "testify." The router's task classifier already applies
whole-word discipline (`router.rs:740-854`); it was never ported. A false
positive injects full skill instructions into the volatile tail — a false
positive costs budget *and* behavior. Port the whole-word guard; done in an
afternoon.

### 2.2 Catch alternating tool loops (M)

`tool_loop_guard.rs:39-44` fingerprints each round as a *set*, so it catches
exact repeats but not A→B→A→B oscillation (its own doc admits it is not
semantic). Add a rolling n-gram fingerprint (last 4 rounds as a sequence) at
threshold 2 repeats. The corrective-injection plumbing, wording, and reset
semantics all exist; only the detector widens.

### 2.3 Relevance-gate working memory (S)

`kod-memory/src/manager.rs:761` injects the last 10 short-term entries purely by
recency, and the engine's 45-minute TTL (`engine/mod.rs:1181`) re-injects the
same fact on a clock even when the transcript already carries it. Score the
10 candidates against the current prompt with the word-overlap scorer that
already exists (`fusion.rs`) and inject by relevance, not recency. The
machinery is built; the gate is missing.

### 2.4 Repo-map hysteresis (M)

The repo map sits inside the cacheable prefix (`router.rs:1096-1127`), and the
first file edit of the session changes map bytes and re-bills the whole prefix.
The fingerprint walk is *also* a full-depth stat of every file on every prompt
(`router.rs:1465-1499` — see §4.1). Freeze the map bytes for N turns or T
seconds after a rebuild: one controlled, journaled re-bill per session instead
of one per edit, and the cache ledger already knows how to journal it.

### 2.5 Let `patch_file` count in swarm conflict detection (S)

`swarm_runner.rs:2100-2114` collects only `write_file` claims, while checkpoints
treat both writes and patches as mutating (`engine/mod.rs:11896`). An agent that
only ever patches is invisible to merge-time conflict detection. One-line
class of bug, embarrassing to hit, trivial to fix.

### 2.6 An agent scorecard: measure astonishment (M)

`kod-stats` already has behavioral stats, per-request stats, and if-bench
(`request.rs`, `behavioral.rs`, `if_bench.rs`). Add a per-session scorecard
surface (`/debug scorecard`): rounds-to-goal, redundant-read rate (supersede
events), loop-guard fires, stop-classifier verdicts, compactions, cache hit
ratio, TTFT p50/p95. Nothing new is measured — the counters already exist in
`run_collector.rs`, `cache_tracker.rs`, and `metrics.rs`; they are just never
assembled into one lens. "Astonishing agents are measured agents" — and this
scorecard becomes the acceptance gate for every other item in this document.

---

## 3. The TUI: pixel truth, responsiveness, information design [U]

The TUI's *state* polish is exceptional (sticky scroll, partial-answer
preservation, TTFT/tok-s telemetry, batch approvals, hunk selection). The
*visual and interactive* layers lag. In order of what a user feels:

### 3.1 Pixel truth (correctness first)

1. **One width model everywhere.** `markdown.rs:753-761` counts every char as
   width 1 while `chat.rs::wrap_text` and the input caret use real display
   width — CJK/emoji paragraphs paint 2N cells into an N-cell wrap, overflowing
   the `╭─ ai ─╮` frame, and the caret drifts after wide chars
   (`input.rs:93-98`). Adopt `unicode-width` in the markdown renderer; this is
   the most visible bug in the product for half the planet's users.
2. **Light theme is second-class.** Code-block background is hardcoded
   `Color::Rgb(28,30,38)` (`markdown.rs:453`) — dark-gray-on-dark under light —
   scrollbar colors are hardcoded (`chat.rs:655-658`), and there is no
   `NO_COLOR` / `COLORFGBG` / truecolor detection anywhere. Derive the
   code-block bg from the theme, route every color through the 12 theme slots,
   autodetect dark/light at startup, and hot-reload theme TOML.
3. **Unmapped keys should be ignored, not typed.** `event.rs:72` maps unknown
   keys to `Char(' ')` — pressing Insert or a media key types a space (a bug
   enshrined as a test, `event.rs:907-908`). Ignore instead; invert the test.
4. **The TUI owns the terminal — enforce it.** The panic hook restores the
   alternate screen but not bracketed paste (`main_loop.rs:635-639` vs. the
   graceful path at `:605-611`), and the bell/OSC-9 notification `eprint!`s
   raw escapes (`ui_state.rs:205-207`), evading the crate's own tripwire test
   (`no_raw_terminal_writes.rs:73-76` doesn't match `eprint!`). Fix the hook,
   route notifications through the sanctioned writer, and widen the tripwire to
   `eprint!|eprintln!`.

### 3.2 Responsiveness (feel)

5. **Scrollable input box.** The box caps at 7 rows with no inner scroll
   (`input.rs:101-105`, `ui/input.rs:94-96`): a large paste scrolls its top out
   of view and the cursor can leave the visible box. `.scroll()` on the
   Paragraph + cursor clamping. This is the roughest edge heavy users hit.
6. **Scrollable, danger-aware approval dialog.** Diffs cap at 40 lines with the
   rest unreachable (`ui/approval.rs:63, 191-202`), and every approval gets the
   same amber border — a `rm -rf` and a doc-comment write look identical. Make
   the dialog scroll, and escalate border/glyph by risk class (kod-risk's
   classifier already exists — `crates/kod-risk/src/classify.rs` — and is
   simply not consumed by the TUI: another parked engine).
7. **Debounce resize.** Every Resize event during a drag does a full
   `terminal.clear()` (`main_loop.rs:846-853`) — a flicker storm. Coalesce to
   one repaint per ~50 ms.
8. **Incremental search.** `search_matches()` rebuilds and lowercases the whole
   transcript 2–3× per frame while a search is open (`search.rs:37-48,
   166-201`). Cache the match list per transcript version; the render cache
   already establishes the pattern.
9. **Mouse beyond the wheel.** Wheel scrolling exists; clicks are dropped
   (`event.rs:485-496`). Click-to-expand a tool row, click a completion, click
   a palette item — each is a mapping table entry, and together they change the
   perceived maturity of the product more than any other TUI item here.
10. **Event-driven streaming repaint.** Streaming renders at the 100 ms tick
    ceiling (`main_loop.rs:79`, `event.rs:258-260`) — fine for latency, but
    chunk floods are coalesced to at best 10 fps while the spinner runs on its
    own clock. Coalesce chunks with a ~30 fps min-interval repaint; the
    event-batching frame (`MAX_EVENTS_PER_FRAME`, `main_loop.rs:691-719`)
    already supports it.

### 3.3 Information design (delight)

11. **Use syntect or drop it.** `syntect` is a declared dependency of kod-tui
    (`crates/kod-tui/Cargo.toml:12`) with a vendored C regex engine
    (`onig_sys`) in the lockfile — and is imported nowhere (only a comment in
    `markdown.rs:29`). Two options, both astonishing-shaped: (a) syntax-
    highlight the fenced code blocks that already render every session — this
    is polish, not a feature, and the render cache absorbs the cost; or (b)
    remove the dep and cut real build time. Shipping it as dead weight is the
    only wrong answer.
12. **Header truncation policy.** The header renders one unbroken Line; badges
    push off-screen on narrow terminals with no ellipsis (`header.rs:29-121`).
    Prioritize segments by importance (the UI/UX plan already defines the
    4–7-segment order), truncate with `…`, and test at 80 cols the way
    `ui_state.rs:719-727` already does for hints.
13. **Restore or delete the dead status affordances.** `elapsed_label()`
    (`ui_state.rs:350-357`) and `accounting_label()` (`context.rs:216-231`) are
    unreferenced — per-turn elapsed time vanished entirely. Either surface
    elapsed seconds again (the UI/UX plan removed the *transcript row*, not the
    status bar) or delete the code. Dead polish code is unpaid-for weight.
14. **First-run as a signature moment.** The empty chat state is one dark-gray
    line (`chat.rs:555-560`), and bare `kod` prints an entry-point summary.
    Make first-run render a welcome panel: detected model + profile, sandbox
    badge (already in the header), skills found, three suggested prompts, and
    `doctor`-style inline checks. Everything displayed already exists; the
    composition is what's missing. Astonishing products nail second zero.
15. **Progressive markdown reveal.** Streaming shows raw `**` and bare
    headings until the turn ends (the whole bubble re-renders through the
    cache keyed on content). Parse markdown at paragraph boundaries during
    streaming so headings and bold appear *live* — the streaming-safe parser
    already handles unclosed fences (`markdown.rs:331-337`); it needs a
    per-paragraph cache key. Subtle, continuous polish users feel as "fast."

---

## 4. Performance: audit the per-turn tax bill [P]

Individually defensible; as a sum, the product pays a per-turn tax it never
totals up.

### 4.1 Kill the full-tree stat walk per prompt ★★★ (M)

The repo-map fingerprint does a full-depth stat of every non-ignored file on
every prompt (`router.rs:1465-1499`) — the H-R15 fix traded correctness for an
O(files) per-turn cost, now wrapped in `spawn_blocking`. The repo already
depends on `notify` for skills hot-reload (`router.rs:601+`). Watch the tree
instead of walking it: fs-event-driven fingerprint invalidation makes the map
O(edits) instead of O(files), and combines naturally with §2.4 hysteresis.

### 4.2 Parallel grep and repomap walk (M)

`grep` is a sequential `BufReader` loop over a sequential walk
(`kod-tools/src/tools.rs:1520-1650, 1281-1304`), and repomap symbol extraction
reads every file single-threaded (`repomap.rs:178-225`). The `ignore` crate's
`WalkParallel` is already in the dependency tree, unused. On a large tree this
is the difference between a 2 s and a 300 ms tool result — the model waits on
it every time.

### 4.3 Fix the output spool preview to actually read the tail (S)

`output_spool.rs:81-94` documents a 4 KiB tail read and then does
`std::fs::read` of the entire file. A gigabyte-emitting command costs a gigabyte
read at preview time — exactly what the doc promises it won't. Open, seek to
`len - 4096`, read forward. Also `append()` reopens the file per chunk
(`:60-66`); hold the handle.

### 4.4 Retire chars/4 where it decides things (M)

`PromptBudget::CHARS_PER_TOKEN = 4` (`budget.rs:67`) drives budget admission,
compaction estimates, and shake savings; the CLI separately uses chars/3
(`commands/mod.rs:168-175`); the snapcompact frame estimate is its own constant.
CJK- and JSON-dense content mis-sizes every threshold by 2–4×. One calibrated
estimator (a 20-entry heuristic table by script mix, or per-provider
calibration from observed `prompt_tokens` — the gauge already anchors on real
usage) replaces three conventions with one that's honest.

### 4.5 Panics: one flag, two worlds (S)

`panic = "abort"` in release (`Cargo.toml:262`) silently disables the swarm's
`catch_unwind` agent containment (`swarm_runner.rs:1364-1376`) — release
aborts the whole process where dev returns `Err("agent panicked")`. Tests run
in dev and can never catch this fork. Either switch to `panic = "unwind"` (the
binary is not so size-sensitive that one flag is worth a crash-the-swarm bug
class) or delete the containment and document it.

### 4.6 Startup: parallelize the serial prologue (S)

`engine.start()` registers ~25 tools sequentially, then spawns MCP servers
serially (`engine/mod.rs:7880-7900`); the compiler baseline was already moved
off the critical path (":7975-7994"), so the remaining serial prologue is
registration + MCP. Join the tool registrations; spawn MCP concurrently. First
prompt arrives seconds earlier, every session.

### 4.7 Make the bench suite run — then gate on it (S+M)

There is no `[[bench]]` section anywhere, yet
`benches/system_benchmarks.rs:441-455` emits `fn main` via `criterion_main!` —
under autodiscovery the bench target compiles against the libtest harness and
`cargo bench` / `cargo test --benches` on kod-core is likely broken (worth
verifying first thing). Add `[[bench]] name = "system_benchmarks" harness =
false`, then run a bench smoke job with loose thresholds in CI (§5.1) so the
per-turn taxes in this section can never silently return. The bench suite
itself is excellent (cold-start methodology, 10k-entry retrieval, repo-map
against the real workspace) — it just measures nothing today.

---

## 5. DX: never lie to a contributor [D]

### 5.1 CI that exists, or claims that don't ★★★ (S)

There is no `.github/` directory — yet the README ships a "Build: Passing"
badge, `deny.toml:4-7` references "the `deny` CI job," the docs site promises
`release.yml` artifacts, and `kod-core/tests/metrics.rs:41` cites
`.github/workflows/release.yml`. This is the #1 credibility item in the repo,
and the fix is the highest-leverage S-effort item in this document: one
workflow — fmt, clippy `-D warnings`, nextest (or cargo test), deny (with the
git-dep allowlist fixed, see §5.4), MSRV check. Alternatively delete every
claim. Never the current state.

### 5.2 MSRV honesty (S)

`rust-version = "1.85"` is declared (`Cargo.toml:30`) while the code uses
let-chains (stabilized in 1.88; e.g. `router.rs:1420-1422`,
`tools.rs:1599-1600`) and `rust-toolchain.toml` pins `stable` with no version.
A contributor honoring the declared MSRV cannot build. Bump to the real MSRV
and add the check job; the toolchain file gets a concrete version.

### 5.3 Regenerate the docs, mechanically (S)

`docs/TESTING.md` counts 353 tests across 12 crates (reality: ≈3,693 across
20, with two crates listed twice); root `CONTRIBUTING.md` documents
`kod-provider-ollama` (doesn't exist), top-level `tests/`/`benches/` dirs, and
a `status` command; the README says 15 crates; the CHANGELOG is one 706-line
"Unreleased" block. The docs-site versions are current — the root docs a
newcomer hits first are the stale ones. Write a generator for the test table
and crate list (the tripwire test proves the team likes enforced invariants),
and cut releases from the changelog.

### 5.4 Release engineering, smallest useful version (M)

Single version `0.1.0`, no tags, `scripts/build.sh` tarballs one platform
unsigned, and the git dependency `typesafe-ai-rs` (`Cargo.toml:256`) both
violates the repo's own `deny.toml` sources policy (`allow-git = []`) and
blocks any future `cargo publish`. Decide the policy: allow-git in deny.toml
+ explicit `publish = false` on crates that reach the git dep (or vendor it).
Then a tag-driven build matrix (linux/macos, x86_64/aarch64) + sha256, and
changelog versioning. Local-first products win on "I can get the binary" —
right now only source builds exist.

### 5.5 Split the two mega-modules (L, start now)

`kod-core/src/engine/mod.rs` is 17,602 lines and `kod-tui/src/main_loop.rs` is
7,927. Every engine edit recompiles a giant TU; navigation of the startup
sequence spans ~250 lines; `kod-core` already depends on 14 sibling crates so
most changes rebuild the world anyway. Carve engine/mod.rs into its natural
submodules (prompt assembly, compaction wiring, streaming loop, swarm runner
adapters, provider setup) — pure moves, no refactors — and dev iteration
speed improves measurably. Do it in slices behind unchanged public API.

### 5.6 Contributor onboarding as a product (S)

`just doctor`: verify toolchain ≥ real MSRV, bwrap/sandbox-exec presence,
ollama reachability, git-dep fetch, and a 2-minute smoke (`cargo test -p
kod-types`). Add the missing default `just` recipe, fix or remove `just wr`
(depends on a gitignored script), and give the fuzz targets a scheduled job so
`docs/FUZZING.md` describes something that runs.

---

## 6. Signature moments (polish, not features)

Small compositions of existing parts that create the "how did it know that?"
reaction:

1. **The compact preview.** `/compact` currently acts. With the admission
   projector that already exists (`CompactionAdmission`,
   `compaction_dispatcher.rs:246-305`), a dry-run could show "shake: −18k,
   prune: −12k, history preserved 10 turns" *before* committing — the context
   meter becomes a knob instead of a warning light.
2. **The approval that reads the room.** Risk-classified borders (§3.2.6) +
   the batch `n of N` model + hunk selection = an approval flow that already
   beats every competitor; only the styling escalation is missing.
3. **The resume banner.** Sessions and history persist today
   (`~/.kod/tui_history.json`, session JSONL). A three-line "resumed from 3h
   ago · 12 turns · goal: X" banner on startup is pure composition of
   existing state, and makes the product feel continuous.
4. **The doctor that helps mid-session.** Endpoint health tracking already
   exists (`endpoint_health.rs`) with circuit breakers. Surface it: a status
   bar glyph that degrades before the user hits a dead endpoint, with
   `/debug health` as the drill-down.
5. **The honest error.** The friendly-error advice pattern (error + advice,
   no mangled spacing) is already a repo value from the UI/UX plan. Extend it
   to the retry taxonomy: `retry_strategy.rs` knows *why* it is retrying
   (SameEndpointLowerTemp vs NextEndpoint) — that knowledge, shown to the
   user as one dim line, converts silent flailing into visible competence.

---

## 7. What not to do

- **No new feature surface.** No plugin marketplace, no new interface, no new
  tool categories, no themes gallery. The borrow corpus is closed; this pass
  is completion and polish.
- **No vanity benchmarks.** Absolute numbers don't matter; regression alarms
  (§4.7) do.
- **No fake motion.** The TUI should never animate for animation's sake; the
  braille spinner and live-updating rows are the correct amount of life.
- **No rewrite of the error type.** `KodError`'s string-shaped coarseness is
  documented and allowed (`result_large_err`); granular error taxonomy is a
  feature-sized project. The retry boundary is already typed enough.
- **Don't chase ANN/vector DBs.** Brute-force cosine is honest and benchmarked
  at ≤10k entries; the memory scaling cliff is a measurement problem until the
  scorecard (§2.6) says otherwise.

---

## 8. Sequencing

| Wave | Items | Why first |
|---|---|---|
| **1 — Trust & truth** (1 week) | §5.1 CI, §5.2 MSRV, §3.1.3-4 (space keys, panic hook, eprint tripwire), §4.3 spool tail, §5.3 doc regeneration, §4.5 panic flag | Everything else is built on the product not lying |
| **2 — Collect the parked engines** (1–2 weeks) | §1.1 stop-continuation, §1.2 remote+snapcompact, §1.3 context-pressure signal, §1.4 warm prune, §2.1 skill boundaries, §2.5 patch_file, §1.5-1.6 swarm wiring | Pure wiring; each is an S; compound effect is the astonishment |
| **3 — Pixel truth** (1–2 weeks) | §3.1.1 unicode width, §3.1.2 theme parity + detection, §3.2.5 input scroll, §3.2.6 approval scroll + danger, §3.3.11 syntect decision | The most felt UX corrections |
| **4 — The tax audit** (2–3 weeks) | §4.1 fs-watch fingerprint, §2.7→§1.7 single prompt build, §4.2 parallel walks, §4.4 token estimator, §4.6 startup, §4.7 bench harness + smoke gate, §2.4 map hysteresis | Per-turn latency and cost compound into the product's identity: fast *and* cheap |
| **5 — Signature & scorecard** (ongoing) | §2.6 scorecard, §6 moments, §3.2.8-10 incremental search/mouse/repaint, §3.3.14-15 welcome + progressive markdown, §5.5 module split | The long tail that makes it *memorable* |

**The one-sentence version:** kod has already paid for its astonishment —
wire the parked engines, make the pixels true, tax the per-turn costs, and
never lie to the user or the contributor.
