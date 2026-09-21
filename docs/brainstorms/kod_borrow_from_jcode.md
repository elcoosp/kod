# Borrowing from jcode — A Brainstorm & Design Notebook for kod

*Companion to the earlier "Kod Harness Engineering Review" (PDF-observations audit). This notebook deep-dives the **jcode** codebase (v0.86, ~84 crates, ~564K lines dumped) and answers one question: **what should kod borrow, and what would the code look like?***

---

## 0. TL;DR — the ten borrows that matter most

| # | Borrow | What it replaces in kod | Effort | Expected impact |
|---|--------|------------------------|--------|-----------------|
| 1 | **Same-branch swarm via a file-touch bus + conflict notifications** (worktree optional) | worktree-per-agent + expected_writes deny-globs + git merge + re-plan | M | Unlocks jcode's "swarm of agents on one branch" workflow; kills the merge machinery for the common case |
| 2 | **Soft interrupts** (mid-turn, safe-point injection, persisted queue) | round-boundary-only steers; swarm agents with no inbound channel while running | S | Makes #1 workable; free upgrade to user steering |
| 3 | **Anthropic 4-breakpoint caching** (tools + system + sliding 2-marker message window) | single system-segment breakpoint; transcript re-billed every turn | S | Directly attacks the #1 cost leak found in the PDF audit |
| 4 | **TokenUsage with cache fields + split-vs-subset cost model** | 3-field TokenUsage that drops cache tokens and over-counts cost | S | Correct cost, measurable hit-rate, feeds routing |
| 5 | **Tool-list freeze + one-shot late-MCP rebuild** | per-turn Jev tool filter mutating the tools array inside the cached prefix | S | Stops a silent full-prefix invalidation every turn |
| 6 | **CacheTracker + invalidation journal** (client-side append-only validation) | nothing — kod can't even see cache misses | S | Makes the cache a *debuggable* resource; pairs with golden-prefix tests |
| 7 | **Two-tier compaction (80%/95%) with tool-pair-safe cutoff + observed-token authority** | drop-oldest truncation only | M | The PDF's "compaction assumes single shared state" fix, in its cheapest useful form |
| 8 | **`command-risk` gate** (blast-radius classification + Reflect gate) | removed-as-bypassable text blocklist; sandbox-only defense for `execute_command` | M | ~1.5k dependency-free lines; upgrades approvals inside and outside the sandbox |
| 9 | **Background shell tasks + durable soft-interrupt store + wake ladder** | no background job concept at all | M | Fills the "batteries not included" gap at the smallest useful granularity |
| 10 | **AGENTS.md snapshot-in-static-prefix** (+ `.kod/prompt-overlay.md`) | zero instruction-file support | S | Cheap, cache-safe, aligns with the PDF's conditional-instructions observation |

Everything below is organized from **most interesting to you** (the swarm section) outward: performance, context/sessions, memory, safety, background, and the rest. Each item states *what jcode does* (with dump line references), *why kod should care* (tied to the earlier audit findings P0–P9), and *the designed Rust solution* mapped onto kod's actual crates. A merged roadmap with the previous PDF-review items is in §13, and §14 lists what **not** to borrow plus where kod is already ahead.

**Reference key:** `D<line>` = line in `1jehuang-jcode-8a5edab282632443.txt`. Kod references cite crate/module from your dump (`dump.txt` line numbers where the earlier audit pinned them).

---

## 1. What jcode is, in ten minutes

jcode is a single-vendor Rust workspace (~84 crates) built around three convictions that show up in every subsystem:

1. **One long-lived daemon owns everything.** `jcode run` spawns a `setsid`-detached server on `/run/user/$UID/jcode.sock`; TUI/CLI/SDK clients are disposable viewers that attach, detach, and reconnect. Every agent is an `Arc<Mutex<Agent>>` living **inside the daemon process** (`ServerRuntime.sessions`, D62869–62898). This is what makes "10 active sessions ≈ 117 MB PSS, ~10 MB per added session" possible — there is exactly one copy of providers, registries, and caches (README perf tables, D2255–D2455).
2. **Coordination is observational, not declarative.** jcode does not ask a planner to *predict* writes (kod's `expected_writes` globs). Instead, every file tool publishes a semantic touch event to a process-global bus, a server-side service tracks who-touched-what, and peers get `file_conflict` notifications with intent + line ranges + diff preview. Agents on the **same branch, same dirty checkout** resolve conflicts by talking to each other. The system prompt says it outright: *"Prefer swarm coordination over branches and git worktrees unless isolation is needed"* (D181681), and the README's roadmap admits git itself is the limitation: *"Git was clearly not built for multi-agent workflows, and git worktrees is not a good solution"* (D2852).
3. **The cache is a first-class, measured resource.** jcode parses cache-read/cache-creation tokens on every provider, places four cache breakpoints with a sliding message window, hashes the cache-relevant prefix client-side to *detect* violations the provider doesn't report, keeps a journal of *documented* invalidations ("an empty journal around a harness-caused miss is itself signal", D118956), and freezes the tool list after the first request so async MCP registration can't silently bust the prefix.

Everything else — the 4-tier compaction ladder, the memory graph with category half-lives, the command-risk gate, the overnight manifests — is downstream of those three convictions plus a ruthless efficiency culture (`Arc`-heavy, `LazyLock` statics, `#[serde(skip_serializing_if)]` wire types, bounded speculative work everywhere).

### 1.1 Subsystem map (where things live in the dump)

| Subsystem | jcode crate(s) | Dump anchors |
|---|---|---|
| Daemon & session registry | `jcode-app-core/src/server/*` | D62869 (ServerRuntime), D455037 (daemon spawn/reload) |
| Swarm core types | `jcode-swarm-core` | D302010–302852 |
| File-touch tracking | `jcode-base/src/file_watch.rs` + `app-core/server` | D118372–118404, D55568–55720 |
| Conflict notifications | `jcode-base/src/protocol/notifications.rs` | D181701–181714 |
| Soft interrupts | `jcode-agent-runtime` + `agent/turn_execution.rs` | D8803–8819, D25281–25340 |
| Swarm persistence | `app-core/server/swarm_persistence.rs` | D65782–66448 |
| Plan/task DAG | `jcode-plan` | D228948–229371, D230973 (HandoffArtifact) |
| Token usage & cost | `message-types` / `session-types` / TUI pricing | D9508, D291027, D348906–348979 |
| Prompt caching | `jcode-provider-anthropic/src/lib.rs` | D240484–241814 |
| Cache tracker | `jcode-base/src/cache_tracker.rs` | D119046–119448 |
| Transports (WS v2, prewarm, health) | `jcode-provider-openai-runtime` | D266835–267156, D264664–264976 |
| Schema dialects | `jcode-schema-dialect` | D280901–283401 |
| Compaction | `jcode-compaction-core` | D210002–211049 |
| Sessions & journal | `jcode-session-types`, `jcode-base/src/session/persistence.rs` | D290744–291851, D194976–195672 |
| Memory graph | `jcode-memory-types/src/graph.rs` | D222908–224263 |
| Command risk | `jcode-command-risk` | D207381–209843 |
| Batch tool | `jcode-app-core/src/tool/batch.rs` | D82221–82608 |
| Background/overnight | `jcode-background-types`, `jcode-overnight-core` | D116039–116245, D226784–228011 |

### 1.2 Philosophy diff: jcode vs kod (one table)

| Axis | jcode | kod (today) | Verdict |
|---|---|---|---|
| Process model | one daemon, many sessions, unix-socket clients | in-process engine per run; TUI inline | Don't borrow the daemon yet (big lift); borrow its *state discipline* |
| Swarm isolation | same branch by default; worktrees opt-in | worktree per agent, always | **Borrow** (§2) — make worktrees optional |
| Conflict strategy | observe + notify + talk (no locks) | declare globs + deny + merge + re-plan | **Borrow the bus**, keep kod's `path_lock` for hard cases |
| Cache accounting | cache tokens parsed, tracked, journaled, priced | dropped at the wire layer | **Borrow** (§4.1–4.3) — this was P0 in the PDF audit |
| Tool surface | frozen after first request; MCP deferred via `mcp_search`/`mcp_call` | re-derived per turn (Jev filter) | **Borrow** (§4.4) |
| Compaction | 80% background summarize / 95% hard, tool-pair-safe | drop-oldest truncation | **Borrow** (§5) |
| Bash safety | command-risk gate only, no OS sandbox | sandbox (bwrap/landlock/seatbelt) + policy engine + quotas | kod is ahead; **add their gate as a layer** (§8.1) |
| Permissions | 3 ad-hoc layers, file-based approvals | pure policy function w/ provenance + JSONL audit | kod is ahead; borrow only the risk gate |
| Memory | graph + Jev verification + half-life decay | redb + hybrid retrieval + flat 60d archive | Borrow the *upgrades* (§7) |
| Sessions | snapshot + journal, full-fidelity resume, crash forks | turns.jsonl audit + text-only TUI resume | **Borrow** (§6) |

---

## 2. THE HEADLINE — swarm on one branch, worktree optional

This is the behavior you said you like, and jcode's implementation is genuinely different from kod's — not a variation on worktrees, but a deliberate replacement of *declarative isolation* with *observational coordination*.

### 2.1 How jcode actually does it

**The stance.** Every swarm agent is a session inside the same daemon, pointed at the **same working directory — same branch, same dirty state**. There are no per-agent checkouts by default, no locks, no merge step. The system prompt bakes in: *"Prefer swarm coordination over branches and git worktrees unless isolation is needed"* and *"Commit as you go"* (D181680–181681). Worktrees exist only as an optional SDK utility (`jcode-sdk/src/worktrees.rs`, D287459–287782: list/create via `git worktree list --porcelain -z`, refuses `-B`, never destructive on failure) and a role ("Worktree Manager") that only appears when a human asks for isolation (D456688–456694).

**Detection — the file-touch bus.** The harness itself reports every file operation at the moment it happens, with semantic intent. That's the whole trick: no file watcher, no mtime polling, no git diff.

```rust
// jcode-base/src/file_watch.rs (D118372–118404, abridged)
pub enum FileOp { Read, Write, Edit }
impl FileOp { pub fn is_modification(&self) -> bool { matches!(self, Self::Write | Self::Edit) } }

pub struct FileTouch {
    pub session_id: String,
    pub path: String,
    pub op: FileOp,
    pub intent: Option<String>,   // model-declared purpose (injected into every tool schema)
    pub summary: Option<String>,  // "edited lines 18-25 (2 occurrences)"
    pub detail: Option<String>,   // compact diff preview
}
// published on a global tokio broadcast bus:
Bus::global().publish(BusEvent::FileTouch(FileTouch { .. }));
```

Every file tool publishes — `edit` after a successful replace (D91125–91144), `write` (D102695), `read` (D98017), `apply_patch` (D80103). So both **read-tracking and write-tracking** exist, and the notification payload is exactly what agent B needs to decide "ignore or investigate".

**The server-side tracker.** `FileTouchService` (D55568–55720) keeps a forward index `path → Vec<FileAccess>` (chronological) and a reverse index `session → HashSet<path>`, with age-based expiry and per-session cleanup on disconnect. On a modification event it computes `latest_peer_touches` (D64501–64526): per path, the **latest modification per other swarm session**, excluding readers of old versions — pinned by unit tests (D55526–55563). It even grades overlap by parsing "lines N-M" out of summaries: *"overlapping lines" / "same file, non-overlapping lines" / "same file"* (D55482–55498) — so a same-file-different-lines edit is presented as low-risk instead of alarmist.

**What agent B receives.** A first-class wire notification:

```rust
// jcode-base/src/protocol/notifications.rs (D181701–181714)
pub enum NotificationType {
    FileConflict { path, operation, intent, summary, detail }, // "Another agent touched a file you've worked with"
    SharedContext { key, value },
    Message { scope, channel, tldr },
}
```

**Delivery.** Notifications are **soft interrupts**: queued in a lock-outside-the-agent `Mutex<Vec<SoftInterruptMessage>>` and injected at safe points *mid-turn* — after a model response with no tool calls (point B), or post-tool (point D); `urgent` ones can skip remaining tools (D456742–456745, D25281–25340, D8803–8819). If the agent can't be locked (or doesn't exist yet), the queue falls back to a **durable on-disk store** (D65131–65184). Nothing is lost, nothing is cancelled.

**On real conflict.** Nothing automatic. No write serialization, no re-read protocol, no merge. The docs are honest: *"The system is optimistic by default (no locks). Conflicts should prompt the involved agents to communicate directly"* (D456840–456844). Observability replaces locking: debug commands `swarm:touches` and `swarm:conflicts` = "files touched by multiple sessions" (D52933–53068). Task-level de-confliction happens upstream: plan items carry `file_scope` + `blocked_by` (D228977–228979), and assignment conflicts are detected via fresh-heartbeat checks (D72533–72633).

**Swarm identity.** `swarm_id_for_dir` maps any directory to its **git common dir** (resolving a worktree's `.git` file back to the main repo), so all worktrees of one repo share one swarm — messaging and plans span worktrees if you do use them (D69155–69180).

**Ownership is an ancestry tree, not roles.** One field — `report_back_to_session_id` — defines spawn edges: a child reports to its spawner; walking the chain gives ancestry; an agent may stop anyone in its own subtree (`force=true` required outside it); orphans reparent to the nearest live ancestor (D456567–456578). Broadcast is **subtree-scoped** so a 1000-member swarm can't be notification-stormed; only the coordinator has whole-swarm reach (D39579–39598). Recursion is mode-gated: normal/light = root spawns one level; `swarm-deep` = any descendant may spawn (D456555–456565). Caps: hard `MAX_SWARM_MEMBERS = 1000` plus a configurable RAM budget `swarm_max_concurrent_agents = 32` (D302071, D171835).

**Root vs worker effort.** `swarm_root_effort` (coordinator reasoning, default max) is an independent axis from `swarm_effort` (worker default, e.g. medium), applied per spawn via `set_reasoning_effort` and surfaced to the model through a prompt directive (D171796–171835, D55871–55881, D25797–25800). The swarm prompt even recommends per-task effort: implementation → "low", context-fetch/bulk-read → "none" (D181637–181643). Spawn also **verifies the model actually switched** and refuses otherwise (D55846–55868).

**Persistence & crash semantics.** Per-swarm snapshots in `~/.jcode/state/swarm/<id>.json` with version guards, `.bak` fallback, tombstone-then-delete removal, and recovery rules that encode hard-won lessons: persisted `Running → Crashed`, persisted `Ready → Stopped` ("ghost that can never enter terminal-member GC"), terminal GC after retention, dormant plans expire after 7 days (D65782–66448). Mutations are idempotent via a persisted mutation state with single-lock begin/replay — the double-spawn TOCTOU is dead (D65295–65561).

### 2.2 The honest tradeoffs (what you give up vs kod's worktrees)

- **No FS-level serialization.** Two agents can genuinely edit the same lines concurrently; the last writer wins and the notification tells the loser *after the fact*. jcode accepts this and leans on the task graph (`file_scope`, `blocked_by`) to make it rare.
- **Dirty-state commits.** Concurrent `git commit` in one working tree races on `.git/index.lock` — jcode acknowledges the problem is unsolved ("opportunity for a new git-like primitive", D2852). §2.3 gives kod a concrete answer (commit serialization + pathspec-scoped commits).
- **Test interference.** Two agents running the test suite in one checkout will stomp each other's build artifacts. jcode doesn't solve this either; its answer is "effort discipline + scope discipline". For kod, worktree mode remains the answer for test-heavy phases — which is exactly why the mode should be **optional, per-subtask**.

### 2.3 Design for kod: `kod-swarm` file-touch coordination

kod already has the right bones: `kod-swarm` primitives (Agent/Blackboard/TaskCoordinator/hub), `kod-core/swarm_runner.rs` orchestration in one process, per-dispatch transcript keys, `path_lock.rs` advisory locks, and the engine's `apply_steers` drain at round boundaries. The design below adds a **shared-isolation mode** in ~600 lines across three files, makes worktrees optional, and reuses your existing steering pipeline as the notification transport — no daemon required.

#### (a) Events and bus — `kod-swarm/src/file_touch.rs`

```rust
use std::{collections::HashSet, path::PathBuf, sync::OnceLock, time::Instant};
use serde::{Deserialize, Serialize};
use tokio::sync::broadcast;

/// kod-swarm agent identity (aligns with swarm_runner's per-dispatch transcript keys).
pub type AgentId = String;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum FileOp { Read, Write, Edit }

impl FileOp {
    pub fn is_modification(self) -> bool { matches!(self, FileOp::Write | FileOp::Edit) }
}

/// One observed file operation, published by the tool layer at execution time.
/// Mirrors jcode's FileTouch (D118394) with kod-specific fields.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FileTouch {
    pub agent_id: AgentId,
    /// Repo-relative, lexically normalized path (no FS access on the event path).
    pub path: PathBuf,
    pub op: FileOp,
    /// Model-declared purpose; injected centrally into tool schemas (see §8.4).
    pub intent: Option<String>,
    /// Human/model-scannable summary, e.g. "edited lines 18-25 (2 occurrences)".
    pub summary: Option<String>,
    /// Compact diff preview for write/edit ops (cap ~2 KiB, per jcode D91144).
    pub detail: Option<String>,
    pub at_ms: u64,
}

/// In-process coordination bus. tokio broadcast is enough while swarm_runner
/// orchestrates in one process; swap for the hub mpsc if you shard later.
#[derive(Debug, Clone)]
pub enum SwarmBusEvent {
    FileTouch(FileTouch),
    Status { agent_id: AgentId, status: crate::agent::AgentStatus },
}

pub struct Bus { tx: broadcast::Sender<SwarmBusEvent> }

impl Bus {
    pub fn global() -> &'static Bus {
        static BUS: OnceLock<Bus> = OnceLock::new();
        BUS.get_or_init(|| {
            let (tx, _) = broadcast::channel(4096);
            Bus { tx }
        })
    }
    /// Fire-and-forget; a tool must NEVER fail because nobody subscribed.
    pub fn publish(&self, ev: SwarmBusEvent) {
        let _ = self.tx.send(ev);
    }
    pub fn subscribe(&self) -> broadcast::Receiver<SwarmBusEvent> {
        self.tx.subscribe()
    }
}
```

#### (b) Tracking + conflict computation — `kod-swarm/src/conflicts.rs`

```rust
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::RwLock;
use std::time::{Duration, Instant};
use super::file_touch::{AgentId, FileOp};

#[derive(Debug, Clone)]
pub struct FileAccess {
    pub agent_id: AgentId,
    pub op: FileOp,
    pub at: Instant,
    pub intent: Option<String>,
    pub summary: Option<String>,
    pub detail: Option<String>,
}

/// How badly a peer's modification overlaps this agent's own touch.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Overlap {
    /// Parsed "lines N-M" ranges intersect -> act now.
    OverlappingLines,
    /// Same file, provably disjoint line ranges -> likely fine.
    SameFileDisjointLines,
    /// Same file, granularity unknown -> check the diff.
    SameFile,
}

#[derive(Debug, Clone)]
pub struct PeerConflict {
    pub peer: AgentId,
    pub op: FileOp,
    pub overlap: Overlap,
    pub intent: Option<String>,
    pub summary: Option<String>,
    pub detail: Option<String>,
}

/// Server-side touch registry (jcode FileTouchService, D55568–55720).
pub struct FileTouchService {
    /// forward index: path -> chronological accesses
    by_path: RwLock<HashMap<PathBuf, Vec<FileAccess>>>,
    /// reverse index: agent -> paths it has touched (for disconnect cleanup)
    by_agent: RwLock<HashMap<AgentId, HashSet<PathBuf>>>,
    max_age: Duration,
}

impl FileTouchService {
    pub fn new(max_age: Duration) -> Self {
        Self {
            by_path: RwLock::new(HashMap::new()),
            by_agent: RwLock::new(HashMap::new()),
            max_age,
        }
    }

    pub fn record_touch(&self, agent_id: AgentId, path: PathBuf, op: FileOp,
                        intent: Option<String>, summary: Option<String>,
                        detail: Option<String>) {
        let access = FileAccess {
            agent_id: agent_id.clone(), op, at: Instant::now(), intent, summary, detail,
        };
        self.by_path.write().unwrap().entry(path.clone()).or_default().push(access);
        self.by_agent.write().unwrap().entry(agent_id).or_default().insert(path);
    }

    /// Latest *modification* per peer agent on `path`, excluding `except`.
    /// Mirrors jcode's latest_peer_touches (D64501–64526): previous readers of
    /// an old version are never alerted; only one notification per peer.
    pub fn conflicts_for(&self, path: &PathBuf, except: &AgentId) -> Vec<PeerConflict> {
        let guard = self.by_path.read().unwrap();
        let mut latest: HashMap<&AgentId, &FileAccess> = HashMap::new();
        for a in guard.get(path).into_iter().flatten() {
            if &a.agent_id == except || !a.op.is_modification() { continue; }
            match latest.get(&a.agent_id) {
                Some(prev) if prev.at >= a.at => {}
                _ => { latest.insert(&a.agent_id, a); }
            }
        }
        let mut out: Vec<PeerConflict> = latest.into_values()
            .map(|a| PeerConflict {
                peer: a.agent_id.clone(),
                op: a.op,
                overlap: Self::grade(a.summary.as_deref()),
                intent: a.intent.clone(),
                summary: a.summary.clone(),
                detail: a.detail.clone(),
            })
            .collect();
        out.sort_by(|x, y| y.op.is_modification().cmp(&x.op.is_modification()));
        out
    }

    /// Parse "lines N-M" out of touch summaries and grade overlap against the
    /// current agent's own edit span (jcode file_activity_scope_label, D55482).
    fn grade(peer_summary: Option<&str>) -> Overlap {
        let Some(s) = peer_summary.and_then(|s| parse_line_span(s)) else {
            return Overlap::SameFile;
        };
        // If *we* have no span (we only read), treat any modification as SameFile:
        // the agent should look at the diff. Callers with their own span intersect it.
        if s.is_empty() { Overlap::SameFile } else { Overlap::SameFileDisjointLines }
    }

    pub fn clear_agent(&self, agent_id: &AgentId) {
        let paths = self.by_agent.write().unwrap().remove(agent_id);
        if let Some(paths) = paths {
            let mut guard = self.by_path.write().unwrap();
            for p in paths { if let Some(v) = guard.get_mut(&p) { v.retain(|a| &a.agent_id != agent_id); } }
        }
    }

    pub fn expire_older_than(&self, cutoff: Instant) {
        let mut guard = self.by_path.write().unwrap();
        guard.retain(|_, v| {
            v.retain(|a| a.at >= cutoff);
            !v.is_empty()
        });
    }
}

fn parse_line_span(summary: &str) -> Option<std::ops::Range<u32>> { /* "lines 18-25" -> 18..25 */ None }
```

> Implementation note: `grade` should take **both** spans and compute a real intersection; the single-argument version above is the skeleton. jcode's exact parsing rules are pinned by tests at D67750–67765 — steal those test cases.

#### (c) Wiring into kod's tool layer — publish on every touch

In `kod-tools`, the four file tools already funnel through `ToolContext`. Add one call at each success path:

```rust
// kod-tools/src/context.rs — extend ToolContext
pub struct ToolContext {
    // ... existing fields (allowed_write_globs, path_lock table, ...)
    pub swarm_agent: Option<crate::swarm_types::AgentId>, // None outside swarm runs
}

// kod-tools/src/tools.rs — after a successful edit_file / write_file / patch_file:
if let Some(agent) = &ctx.swarm_agent {
    crate::swarm_bus::publish_touch(FileTouch {
        agent_id: agent.clone(),
        path: rel_path.clone(),
        op: FileOp::Edit,
        intent: params.intent.clone(),                       // see §8.4 — inject `intent` into every schema
        summary: Some(format!("edited lines {}-{} ({} occurrence{})",
                              start_line, end_line, n, if n == 1 { "" } else { "s" })),
        detail: Some(preview_diff_capped(&old, &new, 2048)),
        at_ms: now_ms(),
    });
}
// read_file: same with op: FileOp::Read, summary: None.
```

#### (d) Notification → steering — `kod-core/swarm_runner.rs`

kod's engine already drains steers at round boundaries (`apply_steers`, engine.rs:29257) and supports per-transcript steering (`steer_for`, 31744). That drain **is** the safe point. The runner subscribes to the bus and forwards conflicts as steers — jcode's B/D delivery points map 1:1 onto kod's existing per-round drain:

````rust
// kod-core/src/swarm_runner.rs — during a swarm run, one task per agent loop:
async fn forward_conflicts(
    mut rx: broadcast::Receiver<SwarmBusEvent>,
    engine: Arc<KodEngine>,
    agent_id: AgentId,
    transcript_key: String,      // "swarm:{id}" key this agent dispatches under
    touches: Arc<FileTouchService>,
) {
    while let Ok(ev) = rx.recv().await {
        let SwarmBusEvent::FileTouch(t) = ev else { continue };
        if t.agent_id == agent_id { continue; }
        if !t.op.is_modification() { continue; }
        let conflicts = touches.conflicts_for(&t.path, &agent_id);
        if conflicts.is_empty() { continue; }          // we never touched it -> not our business
        let msg = render_conflict_notice(&t, &conflicts); // typed system message, §below
        // kod's existing steering pipeline; delivered at the next round boundary:
        let _ = engine.steer_for(&transcript_key, msg).await;
    }
}

fn render_conflict_notice(t: &FileTouch, peers: &[PeerConflict]) -> String {
    let mut s = format!(
        "## Swarm notification: shared file changed\n`{}` was {} by agent `{}`",
        t.path.display(), t.op_name(), peers[0].peer,
    );
    if let Some(i) = &t.intent { s.push_str(&format!(" (intent: {i})")); }
    if let Some(su) = &t.summary { s.push_str(&format!(" — {su}")); }
    s.push_str("\n\nIf this may affect your current work, re-read the file or `git diff` before continuing; otherwise ignore this notice. Do not revert the other agent's change.");
    if let Some(d) = &t.detail { s.push_str(&format!("\n\n```diff\n{d}\n```")); }
    s
}
````

The runner also records its own agents' touches into the shared `FileTouchService` (subscribe once, `record_touch` per event), and calls `clear_agent` on the existing 90s-idle watchdog / completion paths.

#### (e) Making worktree optional — config + policy

```rust
// kod-config/src/swarm.rs — extend SwarmingConfig
#[derive(Debug, Clone, Deserialize)]
pub struct SwarmingConfig {
    // ... existing fields ...
    /// Isolation policy for subtask execution.
    ///   Shared   = same checkout, same branch (jcode-style, notification-guarded)
    ///   Worktree = kod's current per-agent worktree + deterministic merge
    ///   Auto     = shared unless the planner's expected_writes overlap across
    ///              >= 2 subtasks, or the subtask is tagged `isolated`.
    #[serde(default)]
    pub isolation: Isolation,
    /// In Shared mode: serialize commits through a repo-level lock and require
    /// pathspec-scoped commits (see below).
    #[serde(default = "default_true")]
    pub shared_commit_serialized: bool,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Isolation { #[default] Auto, Shared, Worktree }
```

Auto-policy (deterministic, no LLM):

```rust
impl Isolation {
    pub fn resolve_for(self, sub: &SubTask, overlap: bool) -> Isolation {
        match self {
            Isolation::Worktree => Isolation::Worktree,
            Isolation::Shared => Isolation::Shared,
            Isolation::Auto => if overlap || sub.tags.contains("isolated") {
                Isolation::Worktree
            } else {
                Isolation::Shared
            },
        }
    }
}
// overlap = any pair of subtasks' expected_writes globs intersect (kod already
// computes conservative glob overlap for re-planning — reuse that function).
```

**The commit problem, solved kod-style.** In Shared mode, instruct agents to *stage and commit only their own paths* through a repo-level commit mutex (analogous to your `path_lock.rs`, but keyed on the git common dir):

```rust
// kod-swarm/src/commit_lock.rs
/// Serializes git commits across swarm agents sharing one checkout.
/// Commit protocol per agent, executed under this lock:
///   git add -- <my_expected_write_paths>
///   git commit --only -- <my_expected_write_paths> -m "<agent>: <milestone>"
/// `--only` commits the named paths regardless of what else is staged/dirty,
/// so agent A never accidentally commits agent B's in-flight work.
pub struct CommitLock { inner: Arc<Mutex<()>> } // keyed per git common dir
```

This is kod improving on a gap jcode explicitly left open (D2852): agents can "commit as you go" (their system prompt's advice) without racing `.git/index.lock` or swallowing each other's changes. Your existing checkpoint system remains the file-level undo net.

#### (f) Keep, don't delete

- **`path_lock.rs`** stays — it is the hard backstop for same-process write/write races (e.g. two agents patching one file in the same round). jcode has nothing equivalent.
- **`expected_writes` globs** demote from deny-list to **planner hints**: they feed `Isolation::Auto` and the commit pathspec, but no longer gate tool calls in Shared mode.
- **Worktree merge path** stays intact for `Isolation::Worktree` subtasks — mixed runs are fine (one agent in a worktree, three shared).

#### (g) Tests (the part that makes it real)

1. **No false positives:** A reads `f.rs`; B reads `f.rs`; B edits `f.rs` → A gets exactly one notice; B gets none.
2. **Reader exclusion:** A read `f.rs` *before* B's earlier edit, then never touched it again → A is still notified on B's *second* edit only if A re-read afterwards (mirror jcode's `latest_peer_touches` tests, D55526–55563).
3. **Overlap grading:** summaries "edited lines 1-10" vs "edited lines 20-30" → `SameFileDisjointLines`; "edited lines 18-25" vs own span 20..30 → `OverlappingLines`.
4. **Bus isolation:** with no subscribers, `publish` never errors and tool execution never fails.
5. **Commit protocol:** two agents, interleaved `add`+`commit --only` under the lock → each commit contains only its author's paths; no `index.lock` failures.
6. **Steer delivery:** conflict notice produced mid-turn surfaces at the next `apply_steers` drain and appears in the transcript as a system-role message.

### 2.4 Effort split & completion reports (small swarm upgrades worth taking the same day)

Two small mechanics from the same subsystem round out the shared-swarm design:

**Root vs worker effort.** Add to `[swarm]`:

```rust
pub root_effort: EffortLevel,        // coordinator planning quality; default Max
pub worker_effort: EffortLevel,      // default Medium
// EffortLevel { None, Minimal, Low, Medium, High, Xhigh, Max } — map per provider
```

Apply the worker default at dispatch (`process_streaming_with_model_for` already takes a model ref — add an effort param through to the provider request), and surface it in the subtask preamble like jcode's `append_swarm_effort_directive` (D25797–25800). The planner should also emit a per-subtask effort hint — jcode's table (implementation → low; review/debug → default; bulk-read → none, D181637–181643) is a good default policy and is pure token savings.

**Typed completion reports.** jcode appends a system reminder to every spawn/assign prompt requiring a structured report and forwards it to the owner with status-differentiated follow-ups (D302366–302596). kod's `SwarmResponse` is a single LLM synthesis over results. Add a required tail block to each subtask prompt:

```
End your final message with:
<completion-report status=done|failed|blocked>
summary: <what changed, files touched>
validation: <commands run + results, or "none">
followups: <optional, one per line>
</completion-report>
```

Parse it in `swarm_runner` (fall back to raw text if malformed), store on the `Agent` record, feed the Blackboard, and let the re-planner consume `followups` directly. Cheap, typed, and it upgrades your merge-synthesis prompt with per-agent status signal.

---

## 3. Soft interrupts — the transport that makes everything else work

jcode's single most reusable control-plane idea (used by swarm notifications, user steering, background-task wakeups, and permission grants alike) is the **soft interrupt**: a message queued against a running agent that is injected at the next *safe point* without cancelling anything.

```rust
// jcode-agent-runtime (D8803–8819)
pub struct SoftInterruptMessage { content, images, urgent: bool, source: InterruptSource }
pub enum InterruptSource { User, System, BackgroundTask }
pub type SoftInterruptQueue = Arc<std::sync::Mutex<Vec<SoftInterruptMessage>>>;
// std::sync::Mutex on purpose: clients enqueue while the agent's tokio lock is held.
```

Safe points in jcode's turn loop (D25281–25340): **B** — after a model response with no tool calls (inject, then continue the turn); **D** — after each tool round; urgent interrupts may skip remaining tool calls (point C). Delivery is announced to clients (`ServerEvent::SoftInterruptInjected{point, tools_skipped}`), and the queue is **persisted** to `~/.jcode/pending-soft-interrupts/<session>.json` so steers survive crashes (D24900–24956, D148636–148766).

**kod design.** You already have 70% of this: `apply_steers` drains a per-transcript steer channel at each round boundary, and `steer_for` writes into it. Generalize it into the jcode shape:

```rust
// kod-core/src/steer.rs (evolve existing steer_for/apply_steers)
#[derive(Debug, Clone)]
pub struct SoftInterrupt {
    pub content: String,
    pub source: InterruptSource,   // User | System | BackgroundTask | Swarm
    pub urgent: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub enum InterruptSource { User, System, BackgroundTask, Swarm }

impl KodEngine {
    /// Existing apply_steers gains source-grouping + urgency:
    pub fn apply_steers(&self, key: &TranscriptKey, messages: &mut Vec<ChatMessage>) -> SteerSummary {
        let drained = self.steers.drain(key);
        // Group by source into separate messages so the model can attribute them,
        // and so background noise is distinguishable from user intent (jcode D25206–25279).
        // urgent=true interrupts may set a flag that skips remaining tool rounds
        // for non-mutating calls — mirroring jcode's point C, but gated by your
        // policy engine (never skip an in-flight mutating tool).
        ...
    }
}
```

Then add the **durable fallback**: when the engine is mid-`provider.stream` and cannot drain, or the transcript key doesn't exist yet, append to `~/.kod/pending-steers/<key>.jsonl` and replay on next drain. This is what lets *any* producer (swarm conflict forwarder, background task, API client) hand messages to a busy agent without dropping them.

Tests: (1) busy agent receives steer at next round boundary with correct source label; (2) urgent user steer skips remaining read-only rounds but not an in-flight mutating tool; (3) kill -9 between queue-write and drain → steer replayed on restart.

---

## 4. The performance pack

Ordered by cost impact. Items 4.1–4.4 are one coherent unit — they attack the same leak (the PDF audit's P0) from four sides and share tests.

### 4.1 Anthropic 4-breakpoint caching

**jcode's scheme** (all in `jcode-provider-anthropic/src/lib.rs`):

1. **Tools**: `cache_control` on the **last tool** (D240484–240612, test-pinned at D241206–241220). Tool schemas are byte-stable → this block never re-bills.
2. **System**: static/dynamic split — `build_system_param_split` puts the breakpoint on the static block (instructions, AGENTS.md, skills) and never on the dynamic one (date, git status) (D240702–240768). The static part is **snapshotted once per session** so a tool editing AGENTS.md mid-session cannot mutate the cached prefix (D9584–9588).
3. + 4. **Messages — sliding two-marker window** on the two most recent **assistant** messages (D240783–240814):

```rust
/// Strategy: sliding two-marker window
///   - Second-to-last assistant message → READ marker (reuses the cache snapshot
///     from the previous turn)
///   - Last assistant message           → WRITE marker (creates the snapshot for
///     the next turn)
/// Budget: system (1) + tools (1) + messages (up to 2) = 4 total, within Anthropic's limit.
pub fn add_message_cache_breakpoints(messages: &mut [ApiMessage], cache_ttl_1h: bool) { .. }
```

Anchoring on **assistant** messages (not the trailing user/tool messages) is the subtle part: ephemeral suffixes (memory injections, steers) land *after* the markers and can't move them. jcode pins this with byte-identity tests that serialize the exact cached span with and without memory injections (D240930–241084). TTL is configurable (`ephemeral` vs `ephemeral_1h`, D240664–240691).

**kod design.** Your `build_grounded_request` already splits system at `## Volatile suffix` into cacheable/volatile `SystemSegment`s — that's breakpoint 2, done. Add:

```rust
// kod-provider-anthropic/src/wire.rs

/// Place Anthropic cache breakpoints across the whole request (4 max).
/// kod already places one on the last cacheable system segment; this completes the set.
pub fn place_cache_breakpoints(
    tools: &mut [serde_json::Value],
    system: &mut [SystemSegment],
    messages: &mut [ChatMessage],
    ttl: CacheTtl,
) {
    let mut budget = MAX_BREAKPOINTS; // 4
    // 1) last tool definition (tool schemas are sorted+stable in kod — engine 28026)
    if let Some(last) = tools.last_mut() { set_cache_control(last, ttl); budget -= 1; }
    // 2) last cacheable system segment (kod's existing rposition logic — keep it)
    if let Some(idx) = system.iter().rposition(|s| s.cacheable) {
        set_segment_cache_control(&mut system[idx], ttl); budget -= 1;
    }
    // 3+4) sliding window on the two most recent ASSISTANT messages
    let mut placed = 0;
    for msg in messages.iter_mut().rev() {
        if msg.role == Role::Assistant && placed < 2 && budget > 0 {
            set_message_cache_control(msg, ttl); // WRITE for last, READ for prior
            placed += 1; budget -= 1;
        }
    }
}
```

Wire it in `build_streaming_chain`'s request construction, and extend the existing Anthropic wire test (`dump` 58244) to assert the message markers. **Invariant test to steal wholesale:** render the full cached prefix twice — once with a memory/steer injection appended, once without — and assert byte-identity up to and including the last marker (jcode D240930–241084).

### 4.2 TokenUsage with cache fields + split-vs-subset cost model

jcode's usage type (D9508–9515) and session aggregation (D291027–291052) carry `cache_read_input_tokens` / `cache_creation_input_tokens` as **`Option<u64>`** — "missing details mean unknown, not zero" — and the cost model handles the two providers' incompatible conventions explicitly (D348906–348979):

- **Split (Anthropic):** `input_tokens` *excludes* cache tokens. Billing = `input·rate + cache_read·cache_rate + cache_creation·rate·1.25 (2.0 with 1h TTL) + output·rate`.
- **Subset (OpenAI):** cached tokens are *inside* `input_tokens`. Billing subtracts them first.

```rust
// kod-provider/src/types.rs — replaces today's 3-field TokenUsage (dump 62105)
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TokenUsage {
    pub prompt: u64,
    pub completion: u64,
    pub total: u64,
    /// Anthropic: full-prompt tokens served from cache. OpenAI: cached subset
    /// (already inside `prompt`). None = provider didn't report (unknown ≠ 0).
    pub cache_read: Option<u64>,
    /// Anthropic only: tokens written to the cache this request (billed at a premium).
    pub cache_creation: Option<u64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CacheConvention { /// cache tokens reported separately from `prompt`
                           Split,
                           /// cache tokens are a subset of `prompt`
                           Subset }

#[derive(Debug, Clone, Copy)]
pub struct ModelPricing {
    pub input_per_mtok_usd: f64,
    pub output_per_mtok_usd: f64,
    pub cache_read_per_mtok_usd: Option<f64>, // defaults to input rate
    /// 1.25x standard; 2.0 when anthropic cache_ttl_1h is enabled (jcode D348960).
    pub cache_write_multiplier: f64,
}

impl ModelPricing {
    pub fn cost_usd(&self, u: &TokenUsage, conv: CacheConvention) -> f64 {
        let read = u.cache_read.unwrap_or(0) as f64 / 1e6;
        let write = u.cache_creation.unwrap_or(0) as f64 / 1e6;
        let fresh = match conv {
            CacheConvention::Split => u.prompt as f64 / 1e6,
            CacheConvention::Subset => (u.prompt.saturating_sub(u.cache_read.unwrap_or(0))) as f64 / 1e6,
        };
        let r = self.cache_read_per_mtok_usd.unwrap_or(self.input_per_mtok_usd);
        fresh * self.input_per_mtok_usd
            + read * r
            + write * self.input_per_mtok_usd * self.cache_write_multiplier
            + u.completion as f64 / 1e6 * self.output_per_mtok_usd
    }
}
```

Wire changes (mechanical): Anthropic non-stream + SSE state machines read `cache_read_input_tokens` / `cache_creation_input_tokens` (today dropped at wire.rs 57282, 57929/58020); OpenAI maps `prompt_tokens_details.cached_tokens` (or `input_tokens_details`) into `cache_read` with `CacheConvention::Subset`. Extend `cost.rs` to use the new pricing, and add `CacheConvention` to `ModelPricing`-carrying types. Downstream this feeds `SessionEntry::Cost` and — later — the routing ledger from the PDF audit (P1).

### 4.3 CacheTracker + invalidation journal — make misses *explainable*

Even with provider-side counters, you can't tell "provider dropped my cache" from "my harness mutated the prefix". jcode's answer is client-side, provider-agnostic, and cheap (D119046–119448):

```rust
// kod-core/src/cache_tracker.rs
/// Rolling hash over the cache-relevant projection of the transcript.
/// A violation means: something mutated or removed an earlier message —
/// the provider's prefix cache is dead and we KNOW it's our fault.
pub struct CacheTracker { hashes: Vec<u64>, max_history: usize }

#[derive(Debug, thiserror::Error)]
#[error("cache violation at turn {turn}: {reason}")]
pub struct CacheViolation { pub turn: usize, pub reason: String }

impl CacheTracker {
    pub fn observe(&mut self, msgs: &[ChatMessage]) -> Result<(), CacheViolation> {
        for (i, m) in msgs.iter().enumerate() {
            let h = stable_message_hash(m);
            match self.hashes.get(i) {
                None => self.hashes.push(h),                       // append-only growth: fine
                Some(&prev) if prev == h => {}
                Some(_) => return Err(CacheViolation {
                    turn: i,
                    reason: "Prefix modified (message content changed)".into(),
                }),
            }
        }
        if msgs.len() < self.hashes.len() {
            return Err(CacheViolation { turn: msgs.len(),
                reason: "Messages removed (truncation/compaction?)".into() });
        }
        Ok(())
    }
    pub fn reset(&mut self) { self.hashes.clear(); } // call after compaction / model switch
}

/// FNV-1a over the CACHE-RELEVANT projection: role + content + tool_call_id.
/// MUST skip MessageMetadata (pinned, timestamps, durations) — jcode strips
/// `timestamp`/`tool_duration_ms` to avoid false misses (D225287–225309).
fn stable_message_hash(m: &ChatMessage) -> u64 { /* fnv1a(role||content||tool_ids) */ 0 }
```

Pair it with the **invalidation journal** — an append-only JSONL where the *code site* that legitimately breaks the prefix records itself:

```rust
// kod-core/src/cache_journal.rs
pub enum InvalidationCause {
    Compaction { removed: usize },
    ModelSwitch { from: String, to: String },
    ToolSetChange { detail: String },     // should be rare after §4.4
    SystemPromptChange { detail: String },
    HistoryTruncation { removed: usize },
}
pub fn record(cause: InvalidationCause); // ~/.kod/cache_journal.jsonl (redacted, bounded)
```

Then `/debug cache` prints: hit-rate (from §4.2 counters), last violations from the tracker, and recent *documented* invalidations. jcode's rule is worth adopting as a comment: "An empty journal around a harness-caused miss is itself signal" (D118956–118960) — i.e., if the tracker screams and the journal is empty, you found a bug. This composes perfectly with your existing golden-prefix tests (they pin bytes at build time; the tracker pins behavior at runtime).

### 4.4 Tool-surface freeze + hysteresis + deferred MCP

**The kod problem (from the audit):** per-turn Jev category filtering rebuilds the tools array; the tools array sits inside the cached prefix (wire order: tools → system → messages); every silent filter flip invalidates the entire prefix. Also MCP hot-reload can swap definitions mid-session.

**jcode's answer, three parts** (D27453–27588, D9569–9583):

1. **Freeze after first request.** `locked_tools: Option<Vec<ToolDefinition>>` snapshots at request #1; per-turn registry scans stop.
2. **One intentional rebuild.** Late-arriving MCP tools trigger exactly **one** latched rebuild (`mcp_late_register_resolved`), logged as a documented invalidation (feeds §4.3's journal), then re-freeze.
3. **Deferred MCP above a token threshold.** If serialized MCP definitions exceed N tokens, don't attach them — attach a fixed two-tool surface (`mcp_search`, `mcp_call`) and resolve on demand. This is jcode's production-grade version of the PDF's "tool search" idea (your P3) — and kod already has the MCP trim primitive to build it on (engine.rs 25942–26039).

```rust
// kod-core/src/tool_surface.rs
pub struct ToolSurface {
    defs: Vec<ToolDefinition>,
    state: SurfaceState,
}
enum SurfaceState { Unfrozen, Frozen, NeedsRebuild { reason: &'static str } }

impl ToolSurface {
    /// Called once per turn. After the first request of a session, the surface
    /// is frozen; changes require an explicit rebuild + journal entry.
    pub fn definitions_for_request(&mut self, engine: &KodEngine) -> &[ToolDefinition] {
        match self.state {
            SurfaceState::Frozen => {}
            SurfaceState::Unfrozen | SurfaceState::NeedsRebuild { .. } => {
                let defs = engine.compute_tool_definitions(); // sorted, Jev-filtered ONCE
                if let SurfaceState::NeedsRebuild { reason } = self.state {
                    cache_journal::record(InvalidationCause::ToolSetChange { detail: reason.into() });
                }
                self.defs = defs;
                self.state = SurfaceState::Frozen;
            }
        }
        &self.defs
    }
}
```

Jev tool filtering then moves from "per-turn mutation" to one of: (a) applied only at freeze time; or (b) **hysteresis** — a category may flip only after N consecutive turns of Jev votes, each flip costing one journal entry. Given schemas measure ≈1.4k tokens total in kod (audit measured), option (a) is almost certainly right: the filter's savings are smaller than one prefix re-bill.

### 4.5 The `batch` tool — model-controlled parallelism

jcode's batch tool (D82221–82608) is ~380 lines and slots into kod's registry as one more tool:

- `MAX_PARALLEL = 10`; subcalls execute on `FuturesUnordered`; progress published per completion (`BatchProgress{total, completed, running, subcalls}`, D203407–203449); **results re-sorted to original order**; per-subcall output budget `50_000 / num_tools`; nested `batch` rejected.
- The underrated part is `normalize_batch_input` (D82350–82424): it *repairs* common model mistakes (`name`→`tool`, `arguments/args/input`→`parameters`, flat params nested, `intent` forwarded) instead of 400-ing. That's the difference between a tool the model can use first try and one that burns rounds.

```rust
// kod-tools/src/batch.rs
pub struct BatchTool { registry: Weak<ToolRegistry> }

impl Tool for BatchTool {
    fn definition(&self) -> ToolDefinition { ToolDefinition {
        name: "batch".into(),
        category: ToolCategory::Orchestration,
        // schema: { invocations: [ { tool, parameters, intent? } ] }  — tiny on purpose
        ..}}
    async fn execute(&self, params: Value, ctx: ToolContext) -> ToolOutput {
        let calls = normalize_batch_input(params)?;          // repair LLM mistakes
        ensure!(calls.len() <= MAX_PARALLEL, "batch: max {MAX_PARALLEL} subcalls");
        let results: Vec<_> = FuturesUnordered::from_iter(calls.into_iter().enumerate()
            .map(|(i, c)| dispatch_subcall(self.registry.clone(), i, c, ctx.for_subcall(i))))
            .collect().await;
        render_ordered(results)                              // original order, per-subcall budget
    }
}
```

kod-specific notes: enforce the policy engine per subcall (read-only subcalls can skip approval paths exactly like your all-read-only parallel rounds today); count each subcall against `tool_quota`; and keep subcalls off the mutating path unless `path_lock`-protected. This complements — not replaces — your existing all-read-only parallel rounds: batch gives the *model* the choice.

### 4.6 Transports: prewarming, WebSocket v2, circuit breaker (design sketch)

Larger lift; sequence after 4.1–4.4.

- **Prewarm slot** (D266835–267103): a `StdMutex<Option<PrewarmJob>>` fires a speculative warm request (`input:[]`, `generate:false`, 5s timeout, 30s TTL, Drop = abort) when the user starts typing / before context prep; foreground *never waits* (`try_recv` adoption with full settings+credential equality checks). Measured −26.6% TTFT (D452872–452897). HTTP-route equivalent for kod: open the connection + send the cached-prefix-only request head. Speculation must never rotate OAuth tokens (jcode guards this explicitly).
- **OpenAI Responses WebSocket v2** (D452753–452851, D269051–269318): persistent per-conversation socket; subsequent turns send only `input[cursor..]` with `previous_response_id`; per-item hashes detect prefix mutation → fresh socket + full replay (never a corrupt delta chain). kod has no WS transport; the delta-send discipline is the borrow, the socket itself is optional.
- **WS health circuit breaker** (D264664–264972): per-model cooldowns, streak-scaled exponential (60s base → 600s max), `WebsocketFallbackReason` taxonomy, success resets streak. This is the small, testable circuit breaker your audit flagged as missing (P9) — port the shape even if you skip WS.
- **Effort-scaled idle timeouts + forced reasoning summaries** (D190394–190436, D266132–266140): base 180s × (high=2, xhigh=3, max=4); force `summary:"auto"` because "without summaries the stream stays completely silent while the model thinks … and gets killed mid-thought". Free reliability for kod's reasoning-model routes.

### 4.7 Retry kit upgrades

Three small ports from jcode (D248205–253316):

1. **`RetryRollback` stream event** — the retry wrapper tracks whether *replay-visible* output was emitted and emits `StreamEvent::RetryRollback{attempt, max}` so consumers discard partials before replay. kod's TUI currently has no contract for "forget what you saw"; this is the fix for duplicated text on mid-stream retries.
2. **Capped `Retry-After`** — parse both delta and HTTP-date, saturate, **cap at 60s**, and prefer it over backoff (D252195–252308).
3. **Shared transient-error classifier** — ~40 patterns harvested from real logs (GOAWAY, close_notify, `stream_read_error`, …) with a fixture test (D253261–253431). kod's `is_retryable` gets a vocabulary upgrade.

```rust
// kod-provider/src/retry.rs — additions
pub fn retry_after_from_error(err: &ProviderError) -> Option<Duration>; // capped 60s
pub fn is_transient_transport_error(msg: &str) -> bool;                 // curated table
// StreamEvent::RetryRollback { attempt: u32, max: u32 } — consumed by kod-tui
```

### 4.8 Schema-dialect engine (the end of per-provider schema 400s)

jcode's `jcode-schema-dialect` (D280901–283401) exists because one JSON-Schema construct one provider dislikes 400s **every turn** — they fixed the same bug by hand for seven issue reports before building the engine. Three layers:

1. **Prevention**: per-provider allow-list `DialectSpec{id, supported_keywords, supported_string_formats, transforms}` + one recursive `walk()` over the schema (keyword-role classified: SubschemaMap/SubschemaArray/Subschema/Data). Unknown keywords are inert by default; renames (`oneOf→anyOf`) happen *before* support checks; structural transforms flatten combiners, prune dangling `required`, `const`→`enum`.
2. **Recovery**: `rejection::classify` parses real provider error text (incl. Gemini `fieldViolations`, which yields *all* offending keywords in one 400) → `RetryWithoutConstruct` **at most once per distinct construct**.
3. **Memory**: learned rejections persist to `~/.jcode/schema-quirks.json` — "it costs one wasted round trip ever, not one per request, and the fix propagates without waiting for a jcode release" (D282566).

For kod this matters most on the **OpenAI-compatible + MCP adapter path**, where third-party tool schemas meet heterogeneous gateways. Design: new crate `kod-schema-dialect` (port, ~1.5k lines with tests), invoked in `mcp_adapters.rs` at definition-admission time and in the OpenAI provider at request shaping; learned quirks keyed by `provider_kind + gateway_fingerprint`. Registry of `DialectSpec`s starts with `OpenAi`, `Anthropic`, `OpenAiCompat{gateway}`; conformance tests assert every provider runtime actually routes through it (jcode's cross-crate test `every_provider_sends_clean_schemas`, D258631).

### 4.9 Context-window resolution cascade (small, unglamorous, load-bearing)

jcode resolves per-model context limits through a documented cascade — provider hint → live-verified classification → configured/catalog values → per-family rules for open-weight models (GLM/Kimi/DeepSeek/Qwen/…) → optimistic Claude guess, with `DEFAULT_CONTEXT_LIMIT = 200_000` and the explicit stance "**Failing closed at 200K is the worse error**" (D250073–250446), memoized in a `LazyLock<RwLock<HashMap>>` (D185304). kod has no per-model window introspection (audit P9); your `budget.rs` fixed-window assumption silently mis-sizes allocations per endpoint. Port the cascade shape into `kod-config` (a `[[llm.endpoints]].models.context_window` override + family table), feed `prompt_allocation`, and record it in `TaskResponse.pricing` rationale where endpoint choice is already documented.

---

## 5. Compaction that preserves state (not just truncation)

The PDF audit's third observation — "compaction assumes a single shared state" — lands hardest here, because kod's `compact_history_for` (engine.rs:32165) is pure drop-oldest: no summarization, no tool-pair integrity, no trigger discipline. jcode's ladder (constants D210009–210025, behavior spec pinned by tests D120190–120355) is the cheapest complete fix I've seen:

- **80% of budget** → start a **background** LLM summarization; the current turn proceeds unaffected.
- **95%** → synchronous hard compact (abort any in-flight background summary; stale results are discarded, never double-applied).
- **Observed tokens beat estimates**: `effective_context_tokens_from_usage` is the single source of truth, computed with split-vs-subset cache accounting so Anthropic and OpenAI never disagree (D210364, "issue #441"). Char-based estimation (kod's current 4 chars/token) is the fallback, not the authority.
- **Flat `IMAGE_TOKEN_COST = 1_600`** — charging base64 length overestimated ~100× and caused "repeated back-to-back (triple) compactions" (D210046–210066). Steal the constant and the comment the day kod transcripts carry screenshots.
- **`SYSTEM_OVERHEAD_TOKENS = 18_000`** for system+tools, added only when the budget is ≥ half default (D210070).

### 5.1 The design for kod — `kod-core/src/compaction.rs`

```rust
pub const COMPACTION_THRESHOLD: f64 = 0.80;
pub const CRITICAL_THRESHOLD: f64 = 0.95;
pub const RECENT_TURNS_TO_KEEP: usize = 10;
pub const MIN_TURNS_TO_KEEP: usize = 2;

pub enum CompactionAction { None, BackgroundStarted { trigger: u64 }, HardCompacted(usize) }

pub struct Compactor {
    budget_tokens: u64,                    // from §4.9's context-window cascade
    state: Option<PersistedCompactionState>,
}

impl Compactor {
    /// Called each turn before request build, fed REAL usage (§4.2).
    pub fn ensure_context_fits(&mut self, usage: &TokenUsage,
                               msgs: &mut Vec<ChatMessage>) -> CompactionAction {
        let used = effective_context_tokens(usage);   // split/subset aware
        let ratio = used as f64 / self.budget_tokens as f64;
        if ratio < COMPACTION_THRESHOLD { return CompactionAction::None; }
        if ratio >= CRITICAL_THRESHOLD {
            let removed = self.hard_compact(msgs);
            return CompactionAction::HardCompacted(removed);
        }
        if !self.background_running() {
            self.spawn_background_summary(msgs);      // turn proceeds; result applied next turn
            return CompactionAction::BackgroundStarted { trigger: used };
        }
        CompactionAction::None
    }

    /// Walk the cutoff backwards so no ToolResult survives without its ToolUse
    /// (jcode safe_compaction_cutoff, D210274–210330). Returns 0 => don't compact.
    fn safe_cutoff(&self, msgs: &[ChatMessage], keep_recent: usize) -> usize {
        let max = msgs.len().saturating_sub(keep_recent).max(MIN_TURNS_TO_KEEP.min(msgs.len()));
        let mut cut = max;
        while cut > 0 {
            let mut open_tools: HashSet<&str> = msgs[..msgs.len() - cut].iter()
                .filter_map(|m| m.tool_call_id()).collect();
            // if any message AFTER the cutoff references a tool_call_id opened
            // BEFORE it in a way that would orphan a ToolResult -> move cutoff back
            if msgs[msgs.len()-cut..].iter().all(|m| !m.references_unresolved(&open_tools)) { break; }
            cut -= 1;
        }
        cut
    }

    /// No-LLM emergency summary (jcode build_emergency_summary_text, D210446):
    /// previous summary + "[Emergency compaction]: N messages dropped, ~Xk tokens
    /// exceeded Yk limit" + Tools used + Files referenced (<=30, file-shaped only).
    fn emergency_summary(&self, dropped: &[ChatMessage]) -> String { ... }

    fn hard_compact(&mut self, msgs: &mut Vec<ChatMessage>) -> usize {
        let Some(cut) = self.safe_cutoff(msgs, RECENT_TURNS_TO_KEEP) else { return 0 };
        if cut == 0 { return 0; }
        let summary = self.summarize_blocking(&msgs[..cut]);   // LLM; falls back to emergency
        let block = ChatMessage::user(format!(
            "## Previous Conversation Summary\n{summary}\n\nYou can search the full \
             conversation later if you need exact error messages or code snippets."));
        msgs.drain(..cut);
        msgs.insert(0, block);
        cache_tracker_reset_and_journal(InvalidationCause::Compaction { removed: cut });
        self.state = Some(self.persist_state(cut));            // see §5.2
        cut
    }
}
```

The summary prompt itself is worth copying nearly verbatim (D210077–210088): sections *Context / What we did / Current state / User preferences*, tool results truncated to 500 chars in the summarization view, reasoning blocks dropped, and — important for kod — **prepend the prior summary** ("## Previous Summary" → "## New Conversation", D210169) so compaction chains instead of forgetting.

Two kod-specific integrations:

1. **Decisions and plan survive by construction.** Your `decisions.rs` log and plan re-render already survive FIFO; render them *outside* the summarized span (they're cheap and typed) and mark the summary block with `MessageMetadata::pinned` so `render_history_for`'s pin logic protects it.
2. **Reset the cache surface after every compaction** (tracker reset + journal entry + tool-surface re-freeze). jcode does exactly this (`note_compaction_applied`, D24035–24041) because compaction is the one *legitimate* prefix break.

### 5.2 Persisted compaction state + full-fidelity resume

jcode persists `StoredCompactionState { summary, covers_up_to_turn, compacted_count, openai_encrypted_content }` **per session** (D291054–291061) and seeds the manager on resume (`seed_compaction_from_session`, D9860) with the budget taken from the provider context window. A resumed session *remembers what was summarized and up to where*; kod's resume re-truncates from scratch.

For kod: extend `EngineState` (state.rs, keyed per transcript, atomic temp+rename — the machinery exists) with `compaction: Option<PersistedCompactionState>`. Then close the resume gap the audit flagged (TUI `load_session` seeds only User/Assistant text, tool calls lost — dump 79644/84148): resume from `session_log.jsonl` + the turn trace, replaying `ToolCall`/`ToolOutcome` entries back into typed `ChatMessage`s. **Order matters:** replay tools first, then `safe_cutoff` guarantees integrity, and the missing-tool-output repair (§8.3) covers anything that never completed. jcode additionally synthesizes an error `tool_result` for orphaned `tool_use` on resume (D10195–10294) — with the `inflight` registry (§8.3) preventing the duplicate-result bug that made Anthropic *permanently reject* their sessions (D93380–93397). Adopt both together or neither.

### 5.3 413 payload recovery (distinct from token overflow)

jcode classifies "request payload too large" separately from context-window overflow (D210594, D24178–24240): `emergency_strip_large_images` drops oldest images first with text markers, then retries; whole flow is `try_recover_after_payload_too_large` + `try_auto_compact_after_context_limit` (hard-compact and **retry the same turn**). kod hits 413s today only on pathological greps, but the `execute_command` 64KB stream cap and web_fetch make it reachable — worth the small classifier now, essential the day images arrive.

---

## 6. Sessions: snapshot + journal, crash forks, presence

kod's `turns.jsonl` is append-only audit; jcode's session store is a **primary datastore with recovery semantics** (D194976–195672, D301200–301995):

- **Snapshot + delta journal**: `~/.jcode/sessions/<id>.json` (full snapshot) + `<id>.json.journal.jsonl` (appended deltas). Appends are a single `O_APPEND write_all` of a whole line (torn-write safe, D301829–301847); when the journal exceeds a size cap (or on append failure/corrupt replay) → write a fresh atomic snapshot and delete the journal.
- **Crash/torn-line tolerance**: replay never truncates at the first bad byte; glued lines are salvaged by rescanning for `{"meta":` starts (D195041); a corrupt journal is backed up as `.corrupt.jsonl` and forces a full checkpoint next save.
- **Anti-data-loss guards**: a checkpoint **refuses** to replace a >4KiB persisted transcript with an empty in-memory session (D195338) and writes `.pre-wipe-<ts>.bak` copies first (D195296). Durable writes use fsync + **hard-link `.bak`** so concurrent readers never see ENOENT (D301774–301790).
- **Crash recovery forks**: dead-PID scan of `active_pids/` finds crashed sessions; recovery forks them as `session_recovery_*` with a header and **Text blocks only** (tool calls deliberately dropped — the model gets a clean narrative), grouped within a 60s crash window (D193423–194181).
- **Presence trio**: `active_pids/<session>` (owner PID), `streaming_pids/` (only while generating, RAII-cleared on every exit path incl. panic), `internal_pids/` (hidden swarm/debug sessions) (D300789–301199).

**kod design.** Two increments, in order:

1. **Journal-ize `state.json` + turns**: keep `state.json` as the snapshot; add `state.journal.jsonl` for plans/decisions/compaction-state deltas; reuse your existing atomic-write helper for checkpoints; add the refuse-empty guard (it costs 5 lines and prevents the classic "crash during first-run migration wiped my state" class).
2. **Resume fidelity** (with §5.2): replay `SessionEntry::ToolCall/ToolOutcome` from `session_log.jsonl` into the transcript on `load_session`, guarded by `inflight` (§8.3). Full jcode-style snapshot+journal can wait; the replay path is the high-value half and it's also what the PDF audit's P2 "TranscriptRehydrator" wanted.

The presence trio is worth taking as-is (three tiny marker dirs + `kill(pid,0)` liveness) — it gives kod's TUI an honest session list and a crash detector that doesn't scan "tens of thousands" of files (jcode's fast path, D194118–194124).

---

## 7. Memory upgrades (small diffs on kod's existing redb system)

kod's memory stack is good (hybrid retrieval, typed entries, consolidation); jcode's add four surgical upgrades:

### 7.1 Category half-life decay + reinforcement provenance

jcode's `MemoryEntry.effective_confidence` (D223838) decays confidence **per category** — Correction 365d, Preference 90d, Entity 60d, Fact 30d, `e^(-age/half_life·ln2)` with a log access-count boost — and every memory carries `reinforcements: Vec<{session_id, message_index, timestamp}>` (D223708–223757) so "why is this memory strong" is always answerable. kod's episodic archive is a flat 60 days.

```rust
// kod-memory/src/long_term.rs
impl MemoryEntry {
    pub fn effective_confidence(&self, now_ms: u64) -> f64 {
        let half_life_ms = match self.category {
            Category::Decision => 365.0 * DAY,   // kod analogue of Correction
            Category::Preference => 90.0 * DAY,
            Category::Pattern => 60.0 * DAY,
            Category::Fact => 30.0 * DAY,
        } as f64;
        let age = (now_ms.saturating_sub(self.updated_at_ms)) as f64;
        let decay = (-age / half_life_ms * std::f64::consts::LN_2).exp();
        (self.confidence * decay).min(1.0) + (self.reinforcements.len() as f64).ln() * 0.05
    }
    pub fn reinforce(&mut self, session_id: &str, message_index: usize) {
        self.reinforcements.push(Reinforcement { session_id: session_id.into(),
            message_index, at_ms: now_ms() });
        self.confidence = (self.confidence + 0.05).min(1.0);
    }
}
```

Feed `effective_confidence` into retrieval's existing 0.6/0.3/0.1 blend (replace the flat recency term) and into the hourly consolidation's archive threshold.

### 7.2 Supersede + contradiction (conflict handling kod lacks entirely)

jcode never deletes: `supersede()` tombstones (active=false, superseded_by) and `mark_contradiction` **keeps both** entries with a `Contradicts` edge for later resolution (D223415–223424, D224015). For kod: add `superseded_by: Option<MemoryId>` to `MemoryEntry`, filter `!active` in retrieval, and add a `contradicts: Vec<MemoryId>` field the consolidation pass populates when it fuses near-duplicates (your union-find pass at cosine>0.95 becomes: >0.95 fuse, 0.80–0.95 with contradictory categories → link, don't fuse).

### 7.3 Injection hygiene (kod injects per-turn with no dedup)

jcode's pending-injection layer (D179019–179188): pending memory must be **fresh (<120s)**, is **revalidated at consume time** against the graph (still exists, still active, **semantic signature unchanged**), dedupes by prompt-signature (90s) and set-overlap (≥0.8 within 180s), and enforces a **45-minute per-memory injected TTL** — "re-injecting is pure noise because the response that consumed it is already in the transcript". kod's router re-renders memory context every turn with none of these guards.

```rust
// kod-memory/src/context.rs
pub struct PendingMemory { prompt: String, computed_at_ms: u64,
                           memory_ids: Vec<MemoryId>, signatures: Vec<u64> }
impl PendingMemory {
    pub fn is_fresh(&self, now: u64) -> bool { now - self.computed_at_ms < 120_000 }
    /// Consume-time revalidation: re-read the store; drop anything whose
    /// semantic signature (content+category+tags+trust+updated_at) changed.
    pub fn revalidate(&mut self, store: &LongTerm) { ... }
}
```

Add `injected_at_ms: HashMap<MemoryId, u64>` per transcript; skip entries inside 45min. This is a real token saving on long sessions and kills the "same preference re-injected ten times" failure mode.

### 7.4 Pipeline observability

jcode exposes the retrieval flow as state (`PipelineState{search → verify → inject → maintain}` with per-step status/result, D223625–223705) so users can *see* why a memory did or didn't fire. For kod: a `MemoryTrace` on `SessionEntry::MemoryRetrieval` (your JSONL already has the entry type) with `candidates_considered / injected / why_dropped`. Zero behavior change, pure debuggability — matches how you already treat policy decisions.

---

## 8. Safety & control

### 8.1 The `command-risk` gate — port the whole crate

jcode's `jcode-command-risk` (D207381–209843) was born from "a user lost their home directory" (issue #604). It is stage-1 of a two-stage design: a cheap deterministic **high-recall** classifier that classifies **by blast radius, not command name**, explicitly "defense in depth, not a sandbox" (D208360) — stage 2 is a *model-facing reflection gate*, not a user approval.

```rust
pub enum RiskLevel { Safe, Low, Confirm, Catastrophic }
impl RiskLevel {
    pub fn runs_immediately(self) -> bool { matches!(self, RiskLevel::Safe | RiskLevel::Low) }
    pub fn is_absolute_deny(self) -> bool { self == RiskLevel::Catastrophic }
}
pub struct RiskFinding { level: RiskLevel, reason: String, target: String } // reason shown to model verbatim
pub struct RiskAssessment { findings: Vec<RiskFinding> }  // level = max(findings)
pub struct RiskContext { working_dir: PathBuf, home_dir: PathBuf, scratch_dir: PathBuf } // pure, no I/O
```

The scoring rules that make it good (all with dump refs in §Appendix):

- **Wrapper unwrapping** through `sudo/env/nice/timeout/xargs/...` with per-wrapper flag tables; `env -S` or unidentifiable payloads → `Confirm`; running off the end of an unwrap → `Confirm` ("could not be identified statically") (D208495–208659).
- **Pipe-fed operands escalate**: `find ~ -type f | xargs rm` → `Confirm` — "the set of affected files cannot be checked" (D208721–208730). Unparsable targets escalate rather than allow.
- **`paths.rs` is the safety core**: credential stores protected *recursively* (`.ssh .gnupg .aws .kube .docker`), home subpaths exact, system paths exact + recursive subset, `/home`+`/Users` deliberately *not* recursive; lexical-only `$VAR` expansion (never touches the FS, `$HOME_BACKUP` lookalikes stay unresolved, `~/../..` normalizes to `/`); globs → `Catastrophic` if `parent/*` of a protected dir; device nodes → `Catastrophic` (D208900–209218).
- **Redirects**: `>` = truncating write, `>>` correctly not; safe sinks `/dev/null|/dev/stdout|/dev/stderr` (D208562, D209694).
- **Tokenizer keeps `$VAR` intact** ("unknown-and-therefore-risky") and strips heredoc bodies so prose containing `rm` doesn't trip the gate (D209515–209835).

**The gate is the clever part** (D208070–208193): `GateOutcome::{Allow, Reflect{prompt}, Deny{reason}}`. `Reflect` returns a prompt the *model* must answer by re-issuing the command with a `justification` field (schema-injected only for re-issues); `Justification::is_substantive` requires ≥25 chars and rejects empty affirmations (`yes/ok/sure/proceed…`) — and **a blind retry of the identical call fails identically** (tested, D208272). `Catastrophic` deny text: "If the user genuinely wants this, they must run it themselves outside the agent."

**kod integration.** New crate `kod-risk` (direct port, zero deps, fully testable). Call it in `gate_tool_calls` (engine.rs:30133) **before** the policy engine, only for `execute_command` (and `batch` subcalls):

```
runs_immediately → proceed to policy engine (kod unchanged)
Reflect{prompt}  → deny the call with the reflection prompt as the tool error;
                   the model re-issues with justification → re-run assess; substantive → policy engine
Catastrophic     → deny outright (kod policy may never widen a Catastrophic —
                   enforce in the preset-narrowing logic)
```

Your sandbox stays exactly as-is — the gate catches *blast radius* (sandbox allows `rm -rf ~` if home is mounted), the sandbox catches *escape*, the policy engine catches *project intent*. Three orthogonal layers. And unlike the text blocklist you deliberately removed as bypassable, this one assumes bypass (`env -S` → Confirm, not allow).

### 8.2 `InterruptSignal` with epochs (fix the cancel race)

jcode's cancel primitive (D8831–8916, race-hammered in tests D8940–9081): an `AtomicBool` + a **fire epoch** (`AtomicU64`) + `tokio::Notify`. The epoch exists because a deferred `reset()` erased a newer cancel (their issue #428): `reset_if_epoch(e)` only clears the flag if no newer fire happened; `notified()` registers the waiter *before* checking the flag to avoid lost wakeups under fast streams. kod's per-transcript cancel (engine 23143) has the same shape and likely the same latent race — 40 lines to fix:

```rust
#[derive(Clone)]
pub struct InterruptSignal { flag: Arc<AtomicBool>, epoch: Arc<AtomicU64>, notify: Arc<Notify> }
impl InterruptSignal {
    pub fn fire(&self) { self.epoch.fetch_add(1, Ordering::AcqRel); self.flag.store(true, Ordering::Release); self.notify.notify_waiters(); }
    pub async fn notified(&self) {
        if self.flag.load(Ordering::Acquire) { return; }
        let notified = self.notify.notified();
        if self.flag.load(Ordering::Acquire) { return; }  // re-check AFTER registering
        notified.await;
    }
    pub fn reset_if_epoch(&self, epoch: u64) {
        if self.epoch.load(Ordering::Acquire) == epoch { self.flag.store(false, Ordering::Release); }
    }
}
```

### 8.3 `inflight` registry + missing-tool-output repair

A process-global refcounted RAII map of executing `tool_call_id`s (D93378–93455), needed the moment kod replays tool calls on resume (§5.2): the resume path wants to synthesize an error `tool_result` for every orphaned `tool_use` — but doing that for a tool **still running** duplicates its result and made Anthropic *permanently reject* jcode's sessions (D93380–93397). Rule: repair only `tool_use` ids not in `inflight`; skip-and-wait otherwise. 60 lines, prevents a production wedge.

### 8.4 Central `intent` + `accept_large_output` schema injection

jcode injects two fields into **every** tool schema (including MCP proxies) at `to_definition()` time (D308390–308434):

- `intent` (required string): "short label shown in the UI: why this call is being made" — also feeds the FileTouch bus (§2.3c).
- `accept_large_output` (optional bool): the context guard **withholds** oversized results, states their token cost, and the model re-issues with the flag to accept — an explicit, model-controlled cost gate (D308354–308366). jcode's comment is your philosophy too: "this rides on every tool schema on every request", so the text is terse.

For kod: do it in `kod-tools` at `ToolDefinition` finalization (one function over the JSON schema, ~40 lines), wire `intent` into UI/touch events, and give the context guard a threshold (e.g. >8KiB withheld). Your 16KiB `STRUCTURED_TOOL_MSG_CAP` already truncates; this adds the *consent* step.

### 8.5 Tool-name aliases (one table, zero risk)

`resolve_tool_name` (D308766–308806) maps `communicate→swarm`, `task→subagent`, Claude-Code vocab (`shell_exec`, `file_grep`), `functions.` prefixes, PascalCase — because models trained on other harnesses call kod's tools by *their* names. kod's `RetryAction::ReinjectTools` handles hallucinated tools reactively; a 20-line alias table handles them proactively.
---

## 9. Background work: shell tasks, wake ladder, overnight

kod's only background infra today is memory consolidation + skill watchers (audit Task 1-d). jcode has three layers, in increasing ambition — take the first two.

### 9.1 Background shell tasks + stall watchdog + wake ladder

**Types** (jcode-background-types, D116039–116245):

```rust
pub enum BackgroundTaskStatus { Running, Completed, Superseded, Failed }
pub struct BackgroundTaskProgress {
    pub kind: ProgressKind,             // Determinate | Indeterminate
    pub percent: Option<f32>, pub current: Option<u64>, pub total: Option<u64>,
    pub unit: Option<String>, pub eta_seconds: Option<u64>,
    pub source: ProgressSource,         // Reported | ParsedOutput | Heuristic
}
// normalize(): measurable counts override conflicting percents (D116059–116105)
pub enum BackgroundTaskEvent {
    Completed { notify: bool, wake: bool, output_file: PathBuf, output_preview: String },
    /// "check on me" — fired at most once per silence episode, re-armed by output.
    Stalled { silent_for_secs: u64 },
}
```

**Ownership reconciliation** (D169085–169364): each task persists `owner_pid` + `owner_instance` (per-process-image UUID) to `$TMPDIR/jcode-bg-tasks/`, so phantom `Running` entries after a crash can be reconciled without clobbering another live process. Event history capped at 50.

**The wake ladder** (D35432–36147) is the design pattern worth adopting verbatim. On completion (or stall):

1. Notify attached clients (progress events — "any API client can draw the same bar instead of a spinner that says only 'still working'", D217036–217043);
2. If the owning agent is **idle** → start a live turn with a guidance message ("…completed; continue if useful");
3. If **busy** → queue a `SoftInterrupt` (source: `BackgroundTask`) — which lands via §3's machinery;
4. Nothing is lost across restarts (durable soft-interrupt store).

**kod design**: `execute_command` grows `run_in_background: bool` + `stall_wake_seconds` (min 30, resets on output — jcode's schema D80927–80941). New `kod-core/src/background.rs` holds the registry (`owner_pid`, `owner_instance`), the output spool (file + preview, byte caps), and the wake ladder into `apply_steers`. This is the P6 "background read-only jobs" item from the PDF audit in its smallest useful form — and because delivery rides soft interrupts, the swarm gets it for free too (a background grep can wake a swarm agent).

### 9.2 Overnight runs (borrow the *contract*, not the scheduler)

jcode's `/overnight <2h|30m> [mission]` (D226784–228011) is an unattended long-run scaffold: an `OvernightManifest` with `target_wake_at`, `handoff_ready_at` (target − min(30m, dur/4)), `post_wake_grace_until`, artifact paths, `max_agents_guidance: 2`; a **preflight** snapshot (usage projection per provider limits, RAM/swap/load/battery, git state) sampled every 5min; **task cards** (`{problem, evidence, change, files_changed, validation, risk, status, outcome, followups}`) driving both live TUI progress and a static morning `review.html`; and a phase machine (running → wind-down → morning-report → post-wake → finalizing) where each phase is just a targeted poke prompt. The operating contract is one line: "Optimize for verified, low-risk progress… reproduce before fixing… draft an issue otherwise… do not wait for the user" (D227793–227830).

For kod, the borrowable core is the **manifest + task-card contract + phase prompts**, which slots into `swarm_runner` as a profile: `swarm --overnight 2h "ship X"` = decompose as usual, but with validation-required subtask tails (§2.4's completion reports map 1:1 onto task cards), a preflight gate, and timed phase pokes through the soft-interrupt queue. Skip the hidden-supervisor mode; kod's in-process runner is already the "visible current-session mode" jcode also supports (D227833–227890).

---

## 10. AGENTS.md — snapshot-in-static-prefix (closes the P4 gap)

kod has zero instruction-file support today (audit Task 1-d). jcode's implementation is simple *and* answers the cache-stability trap:

- Load `<workdir>/AGENTS.md` ("Project Instructions") + `~/AGENTS.md` ("Global Instructions"), dedup by canonical path, join with headers (D139948–139976).
- Render into the **static, cacheable** half of the system prompt — memory and active-skill state stay in the dynamic half (D139557–139640).
- **Snapshot once per session** (`agents_md_snapshot` captured at session start) so a tool writing AGENTS.md mid-session cannot mutate the provider-cacheable prefix (D9584–9588); refreshed only on clear/restore (D27265).
- No `@import`, no per-directory nesting, no CLAUDE.md (jcode doesn't support it either — CLAUDE.md in their repo is just their own instruction file that imports AGENTS.md).

```rust
// kod-core/src/agents_md.rs
pub struct AgentsMd { project: Option<String>, global: Option<String>, snapshot: String }

impl AgentsMd {
    /// Called once at engine start (and on /clear). NOT re-read per turn —
    /// the snapshot IS the cacheability guarantee.
    pub fn load(cwd: &Path, home: &Path) -> Self {
        let read = |p: &Path| (p.is_file()).then(|| std::fs::read_to_string(p).ok()).flatten();
        let project = read(&cwd.join("AGENTS.md"));
        let global = read(&home.join("AGENTS.md"));
        let mut snapshot = String::new();
        if let Some(g) = &global { snapshot.push_str("## Global Instructions (~/AGENTS.md)\n"); snapshot.push_str(g); }
        if let Some(p) = &project { snapshot.push_str("\n\n## Project Instructions (AGENTS.md)\n"); snapshot.push_str(p); }
        Self { project, global, snapshot }
    }
    /// Rendered by router.rs build_prompt_with_budget into the CACHEABLE
    /// segment, right after ## Identity, before the repo map.
    pub fn segment(&self) -> Option<&str> {
        (!self.snapshot.is_empty()).then_some(self.snapshot.as_str())
    }
}
```

Integration points in kod: `router.rs` inserts `segment()` after `## Identity` (inside the cacheable prefix — this is exactly the segment your golden-prefix test already pins); `EngineState` persists the snapshot hash so a changed AGENTS.md across restarts is a *documented* invalidation (§4.3's journal, cause `SystemPromptChange`); optionally add `.kod/prompt-overlay.md` (additive, project+global) the way jcode splits replacement vs overlay files (D138980–139003). The PDF's conditional-loading idea (load only sections relevant to the task) remains a later refinement — snapshot-in-prefix is the 80/20.

---

## 11. Agent API & SDK (the template for kod's external surface)

jcode's `jcode-harness-api` (D217394–217729) is a curated, versioned NDJSON protocol with rules worth adopting wholesale for kod's daemon/TUI/ACP boundary:

- Every frame carries `v`; unknown fields/events are skipped via `#[serde(other)]` catch-alls; additive changes = minor bump, breaking = major + `Hello{min,max,client}` handshake negotiation; version-coverage and schema-snapshot tests in CI.
- Request surface includes the control-plane primitives this notebook kept reaching for: `SoftInterrupt{content, urgent}` + `CancelSoftInterrupts`, `Compact`, `Rewind` + `RewindUndo`, **`PeekSession`** (tail of *any* session without attaching — "would disturb the very sessions it is trying to preview"), `PermissionResponse{Allow, AllowAlways, Deny}`, `SetReasoningEffort`, sandboxed `ReadFile/FindFiles/SearchText` scoped under the session root.
- Event surface includes `TextDelta/Done/**TextReplace**` (replacement can **retract** already-streamed text after a provider retry — the client-side half of §4.7's RetryRollback), `TokenUsage{cache_read, cache_creation}`, `BackgroundProgress{task_id, percent, summary, done}`, `ConnectionPhase` (fine-grained "retrying (2/4)" separate from coarse session status).
- Bridge discipline: JSON-to-JSON translation so internal changes can't break it, socket chmod 0600 ("a bridge must never be more permissive than the thing it bridges to", D219685–219700), 64MiB frame caps with cancellation-safe reads.
- SDK: `run_structured` (D286908–287117) — client-side `jsonschema` validation with bounded corrective retries (default 2) and typed decode; a natural primitive for kod's `ask`/swarm planner calls too.

Priority for kod: low until you expose a stable external API, but when you do, start from this surface rather than inventing one — especially `PeekSession`, the retraction event, and the versioning rules.

---

## 12. Semantic todos & the "gates emit continuations" philosophy

jcode's task-types crate (D303280–303499, spec D563651–563793) upgrades the humble todo list with:

- **Enum states, not scores**: `ConfidenceState{Speculative..Verified}`, `DeliveryState{ChangeMade..OutcomeDelivered}`; **tool-maintained histories that ignore model-supplied values** — the harness tracks what actually happened (tests ran, files changed) and the model can't self-report "done".
- **"Never reject a write… gates only emit continuations"** — the completion gate doesn't block todo closure; it emits the next action ("run the tests, then close"). Delivery bars calibrate by difficulty (`Involved+ → OutcomeDelivered` required).
- Confidence-spike detection: a ≥2-level jump on completion = "spike-finished" and re-opens scrutiny.

For kod: extend the `todo` tool's item type with `confidence_history` + `blocked_by` maintained by the *check* tool's results (your cargo/tsc/ruff diagnostics already produce the evidence), and have `plan.rs` render gate continuations instead of blocking. Small, and it directly matches your existing check-tool verification loop.

---

## 13. Merged roadmap: PDF-review items × jcode borrows

The earlier audit (P0–P9) and this notebook (B1–B12) interleave cleanly. Sequencing assumes the P0 cache unit ships first because B3/B4/B5/B6 all depend on its plumbing.

| Priority | Item | Source | Depends on | Effort |
|---|---|---|---|---|
| **P0-a** | TokenUsage cache fields + wire parsing + split/subset cost | PDF P0 + B4 | — | S |
| **P0-b** | Anthropic 4-breakpoint placement + byte-identity tests | B3 | — | S |
| **P0-c** | Tool-surface freeze + one-shot rebuild + journal | B5 (+PDF P0) | P0-a | S |
| **P1-a** | CacheTracker + invalidation journal + `/debug cache` | B6 | P0-a | S |
| **P1-b** | Soft interrupts (sources, urgency, durable store) | B2 | — | S |
| **P1-c** | Same-branch swarm: FileTouch bus + conflicts + steer delivery + isolation modes | B1 | P1-b | M |
| **P1-d** | Retry kit (RetryRollback, capped Retry-After, transient table) | B7 | — | S |
| **P2-a** | Compaction ladder (80/95, safe_cutoff, observed tokens) | B7 (PDF P2) | P0-a | M |
| **P2-b** | Resume fidelity: replay tools from session_log + inflight guard | §5.2, §8.3 | P2-a | M |
| **P2-c** | Command-risk crate port + gate integration | B8 | — | M |
| **P2-d** | Background shell tasks + wake ladder | B9 (PDF P6) | P1-b | M |
| **P3-a** | AGENTS.md snapshot-in-prefix | B10 (PDF P4) | — | S |
| **P3-b** | Memory: half-life decay + reinforcement + supersede/contradiction | §7.1–7.2 | — | S |
| **P3-c** | Memory injection hygiene (freshness, revalidation, TTL) | §7.3 | — | S |
| **P3-d** | InterruptSignal epochs + intent/accept_large_output injection | §8.2, §8.4 | — | S |
| **P3-e** | Batch tool + effort split + completion-report contract | §4.5, §2.4 | — | S |
| **P4-a** | Schema-dialect crate (OpenAI-compat/MCP path) | B8 (PDF P3) | — | L |
| **P4-b** | Session snapshot+journal + crash forks + presence trio | §6 | P2-b | M |
| **P4-c** | Overnight manifest/task-cards profile for swarm | §9.2 | P1-c, P2-d | M |
| **P5** | Context-window cascade; effort-scaled timeouts; circuit breaker shape | §4.6, §4.9 | P0-a | S |
| **P5+** | WS v2 transport + prewarming | §4.6 | P0-b | L |
| **P5+** | Versioned agent API + `run_structured` | §11 | daemon decision | L |
| **Later** | Semantic todos; pipeline observability; `/overnight` TUI | §12, §7.4 | — | S |

**S** ≈ a day; **M** ≈ a focused week; **L** = a project. The three "units" that deserve single PRs: the **cache unit** (P0-a/b/c + P1-a — one coherent behavior change with golden tests), the **swarm unit** (P1-b/c + P3-e — the feature you asked for), and the **compaction unit** (P2-a/b).

---

## 14. What NOT to borrow — and where kod is already ahead

**Skip (jcode itself is retreating from these):**
- **Topic channels + shared-context KV** — jcode's own docs demote them as "largely redundant with the repo… a second source of truth" (D457305–457313). kod's typed Blackboard should stay but stay typed.
- **Whole-swarm broadcast** — subtree-scoped reach (ancestry) is the anti-storm design; copy that, not the blast radius.
- **Automatic worktrees** — jcode keeps them as an explicit SDK utility for humans; kod should keep them as an isolation *mode*, never a default.
- **FS-level edit locking** — jcode deliberately has none (D456840); kod's `path_lock` remains the better backstop for same-process races.
- **The daemon, for now** — the biggest architectural delta and the least necessary for kod's current single-user workflows. Revisit when multi-client/multi-session demand appears; borrow its state discipline (presence markers, durable-vs-tmpfs split, reload recovery) meanwhile.
- **Gateway/device-pairing, iOS/mobile, mermaid renderer, telemetry worker** — product surface, not harness.

**Where kod is already ahead (don't regress):**
- **OS sandboxing** — jcode has *no* bwrap/landlock/seatbelt for bash; the risk gate is its only shield, self-described as "defense in depth, not a sandbox". Keep your sandbox; add the gate as a layer.
- **Policy engine with provenance** (`kod policy explain`, JSONL audit) — jcode has three ad-hoc permission layers and file-based external approvals; your pure decision function is cleaner.
- **Tool quotas + path locks + diff-carrying batched approvals with edited-args** — no jcode equivalent.
- **Golden-prefix tests** — jcode achieves the same via CacheTracker at runtime; keep both (build-time pins + runtime detection).
- **Honest fail-loud sandboxing** (Landlock refuses net-deny on old ABI) — keep this instinct.

---

## 15. Appendix — jcode source map (for follow-up reading)

| Topic | File (dump line) |
|---|---|
| Swarm types/lifecycle/caps | `crates/jcode-swarm-core/src/lib.rs` (302010) |
| File-touch events + bus | `jcode-base/src/file_watch.rs` (118372) |
| Touch service + conflict grading | `app-core/server/debug_swarm_read.rs` (52538), `server.rs` helpers (55482–55720) |
| Conflict notifications | `jcode-base/src/protocol/notifications.rs` (181701) |
| Soft interrupts | `jcode-agent-runtime/src/lib.rs` (8803), `agent/turn_execution.rs` (25281–25340) |
| Durable steer store | `jcode-base/src/soft_interrupt_store.rs` (148636) |
| Swarm persistence + recovery | `app-core/server/swarm_persistence.rs` (65782) |
| Idempotent mutations | `app-core/server/swarm_mutation_state.rs` (65283) |
| Plan DAG + HandoffArtifact | `crates/jcode-plan/src/…` (228948–231054) |
| swarm prompt config | `jcode-base/src/prompt/swarm_prompt.md` (181620) |
| Worktree SDK | `crates/jcode-sdk/src/worktrees.rs` (287459) |
| TokenUsage (agent/session/protocol) | `app-core/src/agent.rs` (9508), `session-types` (291027), `protocol` (236573) |
| Cost model | `jcode-tui/src/tui/app/misc_ui.rs` (348906), `provider-core/src/pricing.rs` (251599) |
| Anthropic caching | `jcode-provider-anthropic/src/lib.rs` (240484–241814) |
| CacheTracker + journal | `jcode-base/src/cache_tracker.rs` (119046), `cache_invalidation.rs` (118947) |
| Tool freeze / MCP modes | `app-core/src/agent.rs` (27441–27626) |
| Batch tool | `app-core/src/tool/batch.rs` (82221) |
| WS v2 + prewarm + health | `provider-openai-runtime` (266835–269318, 264664–264976), `docs/OPENAI_WEBSOCKET.md` (452753) |
| Schema dialect | `crates/jcode-schema-dialect/src/*` (280901–283401) |
| Compaction core | `crates/jcode-compaction-core/src/lib.rs` (210002) |
| Session persistence + journal | `jcode-base/src/session/persistence.rs` (194976), `journal.rs` (194195) |
| Crash recovery + presence | `jcode-base/src/session/crash.rs` (193423), `jcode-storage/src/active_pids.rs` (300789) |
| Memory graph | `crates/jcode-memory-types/src/graph.rs` (222908) |
| Memory pipeline + pending | `app-core/src/agent/memory_agent.rs` (130844), `memory/pending.rs` (178964) |
| Command risk | `crates/jcode-command-risk/src/*` (207381–209843) |
| Background types + wake ladder | `jcode-background-types` (116039), `app-core/server/background_tasks.rs` (35432) |
| Overnight | `crates/jcode-overnight-core/src/*` (226784–228011) |
| AGENTS.md + prompt files | `jcode-base/src/prompt/…` (138980–140067) |
| Harness API + bridge | `crates/jcode-harness-api/src/lib.rs` (217394), `harness-api-server` (219518) |
| Interrupt signal | `jcode-agent-runtime/src/lib.rs` (8831) |
| inflight registry | `app-core/src/tool/inflight.rs` (93378) |
| Hooks (6 events) | `jcode-base/src/hooks.rs` (127447) |
