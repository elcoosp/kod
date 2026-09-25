//! Park/revive agent lifecycle (borrow from oh-my-pi, delta §11.2).
//!
//! # The shape this replaces
//!
//! A finished subagent is torn down. If the caller later wants to
//! ask it a follow-up, the whole agent — session, transcript,
//! model, tool surface — is gone; the answer is "spawn a fresh one
//! and re-explain the task."
//!
//! A **registry** keeps finished agents *addressable*. When an
//! agent goes quiet it moves through three lifecycle states:
//!
//! * **Idle** — between turns, still live. A message reaches it and
//!   it answers.
//! * **Parked** — its live session is disposed, but the `AgentRef`
//!   (id, kind, transcript file, history, activity) survives. A
//!   message *revives* it: the caller rebuilds the session from the
//!   persisted transcript and the agent answers.
//! * **Dead** — explicitly killed. A tombstone marks the id; a
//!   message is refused.
//!
//! # The coalescing rule
//!
//! `park` and `ensure_live` are keyed on `(id, generation)` — the
//! `AgentRef`'s own lifecycle counter. A park request that arrives
//! after the agent has already been revived is a no-op for the
//! *older* generation, so a stale finalizer cannot clobber a newer
//! session that happens to reuse the same id. This is the design's
//! "bound to the exact AgentRef" rule.
//!
//! # What this does NOT do
//!
//! * Not persistence. The registry is in-memory: a park that
//!   survives a process restart (the design's "cold revive") needs
//!   the transcript file and a `session_init` peek that are a
//!   separate layer. `AgentRef.session_file` is the handle that
//!   layer would use; `revive` here builds a *fresh* session from
//!   the ref's metadata, which is the in-memory analogue.
//! * Not the agent loop. The registry tracks who is alive and when
//!   they park; running a turn is the caller's job.

use std::collections::HashMap;

/// What kind of agent a ref names. The dispatch policy can treat
/// them differently (an advisor is never parked, a main agent is
/// never killed by a subagent).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum AgentKind {
    Main,
    Sub,
    Advisor,
}

/// The lifecycle state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Lifecycle {
    /// Live, between turns.
    Idle,
    /// Live, mid-turn.
    Active,
    /// Session disposed; ref retained. A message revives.
    Parked,
    /// Explicitly killed. A message is refused.
    Dead,
}

/// The metrics a UI reads. Not used by the lifecycle itself.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AgentHistory {
    /// The model the agent last ran under, if any.
    pub resolved_model: Option<String>,
    /// Total context tokens the agent has consumed across its life.
    pub context_tokens: u64,
    /// Number of turns it has completed.
    pub turns: u64,
}

/// One registered agent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentRef {
    pub id: String,
    pub kind: AgentKind,
    pub lifecycle: Lifecycle,
    /// The transcript file the agent's session is (or was) backed
    /// by. `None` for an agent that never persisted.
    pub session_file: Option<std::path::PathBuf>,
    /// Ids of the agents this one spawned. Walks up to derive depth.
    pub parent: Option<String>,
    /// Monotonic per-ref generation. Bumped on every revive; a
    /// park request for an older generation is a no-op.
    pub generation: u64,
    /// Metrics the caller updates.
    pub history: AgentHistory,
    /// Unix-millis when the ref last changed state. Feeds TTL.
    pub last_state_change_ms: u128,
}

impl AgentRef {
    /// The agent's depth in the spawn tree. A main agent is 0; a
    /// child of a main agent is 1; a grandchild is 2.
    pub fn depth(&self) -> u32 {
        // The registry walks the chain; this method answers from
        // the ref itself only when the parent is absent. Callers
        // that need the true depth use `AgentRegistry::depth_of`.
        if self.parent.is_some() { 1 } else { 0 }
    }
}

/// Why a registry operation was refused.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RegistryError {
    /// No agent registered under the id.
    Unknown,
    /// The agent is `Dead`; a tombstone refuses every operation.
    Tombstoned,
    /// A park or revive was requested with a stale generation.
    StaleGeneration { expected: u64, got: u64 },
}

impl std::fmt::Display for RegistryError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Unknown => write!(f, "no such agent"),
            Self::Tombstoned => write!(f, "agent has been killed"),
            Self::StaleGeneration { expected, got } => {
                write!(f, "stale generation: expected {expected}, got {got}")
            }
        }
    }
}

impl std::error::Error for RegistryError {}

/// The registry.
pub struct AgentRegistry {
    agents: HashMap<String, AgentRef>,
    /// Milliseconds an idle agent may stay live before park is
    /// suggested. `None` disables TTL parking — the caller parks
    /// explicitly.
    park_after_ms: Option<u128>,
}

impl AgentRegistry {
    pub fn new() -> Self {
        Self {
            agents: HashMap::new(),
            park_after_ms: None,
        }
    }

    pub fn with_park_ttl(mut self, ms: u128) -> Self {
        self.park_after_ms = Some(ms);
        self
    }

    /// Register a new agent. A second registration for the same id
    /// is a no-op — the caller that wants to replace a ref kills it
    /// first.
    pub fn register(&mut self, id: impl Into<String>, kind: AgentKind) {
        let id = id.into();
        self.agents.entry(id.clone()).or_insert_with(|| AgentRef {
            id,
            kind,
            lifecycle: Lifecycle::Idle,
            session_file: None,
            parent: None,
            generation: 0,
            history: AgentHistory::default(),
            last_state_change_ms: now_ms(),
        });
    }

    /// Register an agent as the child of another.
    pub fn register_child(
        &mut self,
        id: impl Into<String>,
        kind: AgentKind,
        parent: impl Into<String>,
    ) {
        let id = id.into();
        let parent = parent.into();
        self.agents.entry(id.clone()).or_insert_with(|| AgentRef {
            id,
            kind,
            lifecycle: Lifecycle::Idle,
            session_file: None,
            parent: Some(parent),
            generation: 0,
            history: AgentHistory::default(),
            last_state_change_ms: now_ms(),
        });
    }

    /// Look up a ref.
    pub fn get(&self, id: &str) -> Option<&AgentRef> {
        self.agents.get(id)
    }

    /// Attach a session file to a ref.
    pub fn set_session_file(&mut self, id: &str, path: std::path::PathBuf) -> Result<(), RegistryError> {
        let a = self.agents.get_mut(id).ok_or(RegistryError::Unknown)?;
        if a.lifecycle == Lifecycle::Dead {
            return Err(RegistryError::Tombstoned);
        }
        a.session_file = Some(path);
        Ok(())
    }

    /// The agent's depth in the spawn tree. Walks the parent chain;
    /// a cycle (which a caller cannot produce through this API, but
    /// a corrupted ref could) is treated as depth 0.
    pub fn depth_of(&self, id: &str) -> u32 {
        let mut depth = 0u32;
        let mut cur = id.to_string();
        let mut seen = std::collections::HashSet::new();
        while let Some(a) = self.agents.get(&cur) {
            if !seen.insert(cur.clone()) {
                break; // cycle guard
            }
            match &a.parent {
                Some(p) => {
                    depth += 1;
                    cur = p.clone();
                }
                None => break,
            }
        }
        depth
    }

    /// Mark an agent active (a turn started).
    pub fn mark_active(&mut self, id: &str) -> Result<(), RegistryError> {
        let a = self.agents.get_mut(id).ok_or(RegistryError::Unknown)?;
        if a.lifecycle == Lifecycle::Dead {
            return Err(RegistryError::Tombstoned);
        }
        a.lifecycle = Lifecycle::Active;
        a.last_state_change_ms = now_ms();
        Ok(())
    }

    /// Mark an agent idle (a turn ended).
    pub fn mark_idle(&mut self, id: &str) -> Result<(), RegistryError> {
        let a = self.agents.get_mut(id).ok_or(RegistryError::Unknown)?;
        if a.lifecycle == Lifecycle::Dead {
            return Err(RegistryError::Tombstoned);
        }
        a.lifecycle = Lifecycle::Idle;
        a.last_state_change_ms = now_ms();
        Ok(())
    }

    /// Park an agent. `expected_generation` — a park issued against
    /// a generation older than the ref's current one is a no-op that
    /// returns `StaleGeneration`, so a stale finalizer cannot clobber
    /// a newer session.
    pub fn park(
        &mut self,
        id: &str,
        expected_generation: u64,
    ) -> Result<(), RegistryError> {
        let a = self.agents.get_mut(id).ok_or(RegistryError::Unknown)?;
        if a.lifecycle == Lifecycle::Dead {
            return Err(RegistryError::Tombstoned);
        }
        if a.generation != expected_generation {
            return Err(RegistryError::StaleGeneration {
                expected: a.generation,
                got: expected_generation,
            });
        }
        a.lifecycle = Lifecycle::Parked;
        a.last_state_change_ms = now_ms();
        Ok(())
    }

    /// Revive a parked agent. Bumps the ref's generation so a park
    /// issued before the revive becomes stale.
    ///
    /// Returns the ref so the caller can rebuild the session from
    /// its metadata.
    pub fn revive(&mut self, id: &str) -> Result<AgentRef, RegistryError> {
        let a = self.agents.get_mut(id).ok_or(RegistryError::Unknown)?;
        match a.lifecycle {
            Lifecycle::Dead => Err(RegistryError::Tombstoned),
            Lifecycle::Idle | Lifecycle::Active => {
                // Already live: return the ref unchanged.
                Ok(a.clone())
            }
            Lifecycle::Parked => {
                a.generation += 1;
                a.lifecycle = Lifecycle::Idle;
                a.last_state_change_ms = now_ms();
                Ok(a.clone())
            }
        }
    }

    /// Ensure an agent is live, reviving if parked. The "coalesced"
    /// operation: a caller that wants to send a message does not
    /// have to check the state first.
    pub fn ensure_live(&mut self, id: &str) -> Result<AgentRef, RegistryError> {
        self.revive(id)
    }

    /// Kill an agent. The ref is retained as a tombstone — the id
    /// is refused forever.
    pub fn kill(&mut self, id: &str) -> Result<(), RegistryError> {
        let a = self.agents.get_mut(id).ok_or(RegistryError::Unknown)?;
        a.lifecycle = Lifecycle::Dead;
        a.last_state_change_ms = now_ms();
        Ok(())
    }

    /// Ids of every agent whose idle time exceeds the TTL. The
    /// caller parks them; the registry does not auto-park because
    /// parking disposes a session and the caller owns that.
    pub fn park_candidates(&self) -> Vec<String> {
        let Some(ttl) = self.park_after_ms else {
            return Vec::new();
        };
        let now = now_ms();
        let mut out: Vec<String> = self
            .agents
            .values()
            .filter(|a| {
                a.lifecycle == Lifecycle::Idle
                    && now.saturating_sub(a.last_state_change_ms) >= ttl
            })
            .map(|a| a.id.clone())
            .collect();
        out.sort();
        out
    }

    /// Every live (non-dead) agent id, sorted.
    pub fn live_ids(&self) -> Vec<String> {
        let mut v: Vec<String> = self
            .agents
            .values()
            .filter(|a| a.lifecycle != Lifecycle::Dead)
            .map(|a| a.id.clone())
            .collect();
        v.sort();
        v
    }

    /// How many agents are in each state. For a UI readout.
    pub fn counts(&self) -> (usize, usize, usize, usize) {
        let mut idle = 0;
        let mut active = 0;
        let mut parked = 0;
        let mut dead = 0;
        for a in self.agents.values() {
            match a.lifecycle {
                Lifecycle::Idle => idle += 1,
                Lifecycle::Active => active += 1,
                Lifecycle::Parked => parked += 1,
                Lifecycle::Dead => dead += 1,
            }
        }
        (idle, active, parked, dead)
    }
}

impl Default for AgentRegistry {
    fn default() -> Self {
        Self::new()
    }
}

fn now_ms() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_registered_agent_is_idle() {
        let mut r = AgentRegistry::new();
        r.register("a", AgentKind::Sub);
        assert_eq!(r.get("a").unwrap().lifecycle, Lifecycle::Idle);
    }

    #[test]
    fn registering_twice_is_a_no_op() {
        let mut r = AgentRegistry::new();
        r.register("a", AgentKind::Sub);
        r.mark_active("a").unwrap();
        r.register("a", AgentKind::Main);
        // The second register did not reset the lifecycle or the kind.
        let a = r.get("a").unwrap();
        assert_eq!(a.lifecycle, Lifecycle::Active);
        assert_eq!(a.kind, AgentKind::Sub);
    }

    #[test]
    fn mark_active_then_idle_round_trips() {
        let mut r = AgentRegistry::new();
        r.register("a", AgentKind::Sub);
        r.mark_active("a").unwrap();
        assert_eq!(r.get("a").unwrap().lifecycle, Lifecycle::Active);
        r.mark_idle("a").unwrap();
        assert_eq!(r.get("a").unwrap().lifecycle, Lifecycle::Idle);
    }

    #[test]
    fn mark_on_an_unknown_agent_errors() {
        let mut r = AgentRegistry::new();
        assert_eq!(r.mark_active("x"), Err(RegistryError::Unknown));
    }

    #[test]
    fn park_then_revive_round_trips() {
        let mut r = AgentRegistry::new();
        r.register("a", AgentKind::Sub);
        r.mark_idle("a").unwrap();
        r.park("a", 0).unwrap();
        assert_eq!(r.get("a").unwrap().lifecycle, Lifecycle::Parked);
        let a = r.revive("a").unwrap();
        assert_eq!(a.lifecycle, Lifecycle::Idle);
        assert_eq!(a.generation, 1, "revive bumps the generation");
    }

    #[test]
    fn a_stale_park_is_refused() {
        let mut r = AgentRegistry::new();
        r.register("a", AgentKind::Sub);
        r.park("a", 0).unwrap();
        r.revive("a").unwrap(); // generation now 1
        // A park issued against generation 0 is stale.
        match r.park("a", 0) {
            Err(RegistryError::StaleGeneration { expected: 1, got: 0 }) => {}
            other => panic!("expected StaleGeneration, got {other:?}"),
        }
    }

    #[test]
    fn revive_on_a_live_agent_is_a_no_op() {
        let mut r = AgentRegistry::new();
        r.register("a", AgentKind::Sub);
        let before = r.get("a").unwrap().generation;
        let a = r.revive("a").unwrap();
        assert_eq!(a.generation, before, "no bump when already live");
    }

    #[test]
    fn kill_marks_the_tombstone() {
        let mut r = AgentRegistry::new();
        r.register("a", AgentKind::Sub);
        r.kill("a").unwrap();
        assert_eq!(r.get("a").unwrap().lifecycle, Lifecycle::Dead);
        assert_eq!(r.revive("a"), Err(RegistryError::Tombstoned));
        assert_eq!(r.park("a", 0), Err(RegistryError::Tombstoned));
        assert_eq!(r.mark_active("a"), Err(RegistryError::Tombstoned));
    }

    #[test]
    fn a_dead_agent_is_excluded_from_live_ids() {
        let mut r = AgentRegistry::new();
        r.register("a", AgentKind::Sub);
        r.register("b", AgentKind::Sub);
        r.kill("a").unwrap();
        assert_eq!(r.live_ids(), vec!["b".to_string()]);
    }

    #[test]
    fn depth_walks_the_parent_chain() {
        let mut r = AgentRegistry::new();
        r.register("root", AgentKind::Main);
        r.register_child("child", AgentKind::Sub, "root");
        r.register_child("grandchild", AgentKind::Sub, "child");
        assert_eq!(r.depth_of("root"), 0);
        assert_eq!(r.depth_of("child"), 1);
        assert_eq!(r.depth_of("grandchild"), 2);
    }

    #[test]
    fn depth_on_a_missing_agent_is_zero() {
        let r = AgentRegistry::new();
        assert_eq!(r.depth_of("nope"), 0);
    }

    #[test]
    fn park_candidates_respect_the_ttl() {
        let mut r = AgentRegistry::new().with_park_ttl(1000);
        r.register("a", AgentKind::Sub);
        // Force the state-change timestamp into the past.
        r.agents.get_mut("a").unwrap().last_state_change_ms = now_ms() - 2000;
        assert_eq!(r.park_candidates(), vec!["a".to_string()]);
    }

    #[test]
    fn park_candidates_skip_a_fresh_idle_agent() {
        let mut r = AgentRegistry::new().with_park_ttl(60_000);
        r.register("a", AgentKind::Sub);
        assert!(r.park_candidates().is_empty());
    }

    #[test]
    fn park_candidates_skip_active_agents() {
        let mut r = AgentRegistry::new().with_park_ttl(1);
        r.register("a", AgentKind::Sub);
        r.mark_active("a").unwrap();
        r.agents.get_mut("a").unwrap().last_state_change_ms = now_ms() - 5000;
        assert!(r.park_candidates().is_empty());
    }

    #[test]
    fn a_registry_without_ttl_has_no_candidates() {
        let mut r = AgentRegistry::new();
        r.register("a", AgentKind::Sub);
        r.agents.get_mut("a").unwrap().last_state_change_ms = 0;
        assert!(r.park_candidates().is_empty());
    }

    #[test]
    fn counts_reflect_every_state() {
        let mut r = AgentRegistry::new();
        r.register("a", AgentKind::Sub);
        r.register("b", AgentKind::Sub);
        r.register("c", AgentKind::Sub);
        r.register("d", AgentKind::Sub);
        r.mark_active("a").unwrap();
        r.park("b", 0).unwrap();
        r.kill("c").unwrap();
        assert_eq!(r.counts(), (1, 1, 1, 1));
    }

    #[test]
    fn set_session_file_records_the_path() {
        let mut r = AgentRegistry::new();
        r.register("a", AgentKind::Sub);
        r.set_session_file("a", std::path::PathBuf::from("/tmp/a.jsonl")).unwrap();
        assert_eq!(
            r.get("a").unwrap().session_file.as_deref(),
            Some(std::path::Path::new("/tmp/a.jsonl")),
        );
    }

    #[test]
    fn set_session_file_on_a_dead_agent_is_refused() {
        let mut r = AgentRegistry::new();
        r.register("a", AgentKind::Sub);
        r.kill("a").unwrap();
        assert_eq!(
            r.set_session_file("a", std::path::PathBuf::from("/x")),
            Err(RegistryError::Tombstoned),
        );
    }

    #[test]
    fn ensure_live_revives_a_parked_agent() {
        let mut r = AgentRegistry::new();
        r.register("a", AgentKind::Sub);
        r.park("a", 0).unwrap();
        let a = r.ensure_live("a").unwrap();
        assert_eq!(a.lifecycle, Lifecycle::Idle);
    }
}
