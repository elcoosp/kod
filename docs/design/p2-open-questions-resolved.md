# P2 open questions — resolved

Each question from § P2 of the design has a decision, so the code
does not have to guess.

## 1. Where does a chunk digest come from?

**Decision: extractive, not generative.** A digest is the turn's role
prefix, its first sentence (up to 200 chars), and an ellipsis if
truncated. No model call, no per-chunk Jev invocation. Rationale: a
digest is only ever chosen for chunks that scored between Stub and
Full — the model has *some* signal that the chunk is relevant but not
enough to justify the full text. Replacing that with a Jev call per
chunk per turn is the expensive path for a marginal gain; the
extractive form is deterministic, cheap, and testable. A Jev- or
model-generated digest can be added later as a scorer option if the
extractive form proves insufficient.

## 2. Cache-breakpoint stability for the last K turns

**Decision: pin the last 5 turns to Full unconditionally.** Anthropic's
transcript cache breakpoint sits on the last block of the last message
(P0). If the fidelity of an early turn changes between two calls, the
cached prefix invalidates from that turn forward. The last 5 turns are
the edge that grows every call — pinning them Full means the two
endpoints always agree on the tail's shape, and the change to an
older turn's fidelity is a cost, not a correctness event.

Additionally: fidelity decisions are **cached per turn** and only
recomputed when the query's term set changes (Jaccard < 0.3 with the
last query that scored the turn). A turn that has been scored Omit
stays Omit across calls with the same query; only a real topic change
re-scores.

## 3. Pinned-count warning threshold

**Decision: warn at 50 pins.** A pinned turn is always Full — that is
the whole point. But 50 pinned turns on a 40-turn budget means the
feature has been defeated: the budget cannot include 50 Full turns, so
the FIFO fallback reasserts itself anyway. The warning tells the user
their pin set has outgrown the budget so they can prune it. Not a hard
limit; a system message.

## 4. P3 — retire the per-turn Jev tool filter

**Decision: skip the filter when `tool_search` is registered.** The
filter exists to keep the tools array small; `tool_search` makes the
filter unnecessary because the model can pull schemas on demand. The
filter was also cache-hostile (P0 hysteresis mitigated that; retiring
it eliminates the cause). The hysteresis state stays in place — a kod
build without `tool_search` (a test harness, a minimal embedder)
still filters.

Golden-prompt impact: none. The golden tests already render the full
tool list because they run without Jev. With the filter off by
default (when `tool_search` is present), the tool list in the request
is the full registry — which is what the golden tests already expect.

## 5. P4 — `lang=` guards

**Decision: plumb repomap language stats through the router.** The
repomap's `extract_symbols_and_imports` already knows each file's
language (it picks the parser by extension). Expose a
`repo_map_languages() -> Vec<String>` on `TaskRouter` that returns the
distinct language identifiers from the current map, and pass them to
`InstructionChain::render`. A `::: when lang=rust` section then fires
whenever the current repo has any Rust file. A section that names a
language the repo does not use stays off, which is the intent.

## 6. P5 + P6 integration tests

**Decision: stub the provider, not the engine.** Both features are
engine-level, so the tests construct a real `KodEngine` with a
scripted `MockProvider` (already in `kod-provider/src/testkit.rs`)
and drive the real code path. No trait-object mocking of the engine;
that would test the mock, not the integration.

- P5: drive `SwarmRunner::run` with a one-subtask plan whose
  scripted reply is a valid JSON `SubagentReport`. Assert the
  parent's decisions log gained an entry and the report's summary
  is on the blackboard.
- P6: `spawn_background_review`, await the job, assert the
  summary is non-empty and the job status is `Completed`.

## 7. P6 tool-equipped review

**Decision: use the child-engine constructor.** Build a child
`KodEngine` with `enable_background_mode()`, `set_registry(...)` on
the parent's provider registry (so the reviewer uses the same
endpoints), and `start()` — which now registers only the read-only
tools. Then `child.process_for("review", &prompt).await`. The
reviewer can read files, grep, and check the turn's claims; the
`run_tool` gate refuses writes.

A single-turn `generate` call (the current implementation) becomes a
fallback for endpoints that cannot run an agentic loop (a small local
model). The default is the tool-equipped child.
