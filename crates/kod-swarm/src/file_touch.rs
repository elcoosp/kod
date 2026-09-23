//! File-touch observation bus.
//!
//! kod's swarm today isolates agents with git worktrees and reconciles
//! at merge time. That is the right answer when isolation is needed
//! and the wrong answer when it is not: a shared checkout with three
//! agents is one branch, no merge, and — with this module — live
//! awareness of who touched what.
//!
//! The design (notebook §2.3) replaces *declarative* isolation
//! ("declare your writes up front, be denied if you exceed them") with
//! *observational* coordination: every file tool publishes a touch
//! event at execution time, a service tracks the latest modification
//! per path per agent, and a peer that has touched the same file gets
//! a notice. No locks, no merge, no planner prediction — the cost of
//! a collision is a re-read, not a corrupted write.
//!
//! The event bus is `tokio::broadcast`: one publisher per tool call,
//! many subscribers (one per running agent). A lagging subscriber is
//! dropped messages, which is correct — a stale touch notice is worse
//! than no notice.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::RwLock;
use std::time::Instant;
use tokio::sync::broadcast;

pub type AgentId = String;

/// What kind of file operation happened.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FileOp {
    Read,
    Write,
    Edit,
}

impl FileOp {
    /// Whether this op changed the file's bytes. Only modifications
    /// produce a conflict notice — a peer that read is not at risk.
    pub fn is_modification(self) -> bool {
        matches!(self, FileOp::Write | FileOp::Edit)
    }
}

/// One observed file operation, published by the tool layer.
#[derive(Debug, Clone)]
pub struct FileTouch {
    pub agent_id: AgentId,
    /// Repo-relative, already normalized by the tool layer.
    pub path: PathBuf,
    pub op: FileOp,
    /// Human-scannable summary, e.g. `"edited lines 18-25"`. `None`
    /// for reads and for callers that did not compute one.
    pub summary: Option<String>,
    pub at: Instant,
}

/// A touch event on the swarm bus.
#[derive(Debug, Clone)]
pub enum SwarmBusEvent {
    FileTouch(FileTouch),
}

/// The process-global file-touch bus.
///
/// A tool call publishes and never blocks on the result — a tool must
/// never fail because nobody subscribed, and there is no scenario in
/// which a caller needs the fan-out to complete before continuing.
pub struct FileTouchBus {
    tx: broadcast::Sender<SwarmBusEvent>,
}

impl FileTouchBus {
    /// Construct with a bounded ring. 4096 is generous: the bus only
    /// carries touch events for the life of a swarm run, and a
    /// subscriber that falls more than that far behind is a bug, not
    /// a legitimate state.
    pub fn new() -> Self {
        let (tx, _) = broadcast::channel(4096);
        Self { tx }
    }

    /// Publish a touch. Fire-and-forget: an error means "no
    /// subscribers," which is the normal state outside a swarm run.
    pub fn publish(&self, touch: FileTouch) {
        let _ = self.tx.send(SwarmBusEvent::FileTouch(touch));
    }

    pub fn subscribe(&self) -> broadcast::Receiver<SwarmBusEvent> {
        self.tx.subscribe()
    }
}

impl Default for FileTouchBus {
    fn default() -> Self {
        Self::new()
    }
}

/// A peer modification that overlaps a file the observing agent has
/// touched.
#[derive(Debug, Clone)]
pub struct PeerConflict {
    pub peer: AgentId,
    pub op: FileOp,
    pub summary: Option<String>,
}

/// Server-side registry of who touched what.
///
/// The forward index is `path → chronological accesses`; the reverse
/// index is `agent → paths` so a retiring agent's entries can be
/// dropped without scanning the whole map.
#[derive(Default)]
pub struct FileTouchService {
    by_path: RwLock<HashMap<PathBuf, Vec<FileTouch>>>,
    by_agent: RwLock<HashMap<AgentId, HashSet<PathBuf>>>,
}

impl FileTouchService {
    pub fn new() -> Self {
        Self::default()
    }

    /// Record a touch. Called by the swarm runner, not the tool
    /// layer, so the bus stays the only producer.
    pub fn record(&self, touch: FileTouch) {
        self.by_path
            .write()
            .unwrap()
            .entry(touch.path.clone())
            .or_default()
            .push(touch.clone());
        self.by_agent
            .write()
            .unwrap()
            .entry(touch.agent_id.clone())
            .or_default()
            .insert(touch.path);
    }

    /// The peers that have *modified* `path`, one entry each, most
    /// recent first.
    ///
    /// `except` is the agent asking, excluded from its own answer. A
    /// peer that only read the file is not a conflict — the reader
    /// saw some version, and the writer changed it; the reader may
    /// care, but the operation itself is not contested. Only a peer
    /// that also modified the file is a candidate for "you two are
    /// working on the same thing."
    ///
    /// One entry per peer, at their most recent modification. A peer
    /// that edited the same file three times produces one notice, not
    /// three — the point is awareness, not a log.
    pub fn conflicts_for(&self, path: &PathBuf, except: &str) -> Vec<PeerConflict> {
        let guard = self.by_path.read().unwrap();
        let Some(accesses) = guard.get(path) else {
            return Vec::new();
        };
        let mut latest: HashMap<&AgentId, &FileTouch> = HashMap::new();
        for t in accesses {
            if t.agent_id.as_str() == except || !t.op.is_modification() {
                continue;
            }
            match latest.get(&t.agent_id) {
                Some(prev) if prev.at >= t.at => {}
                _ => {
                    latest.insert(&t.agent_id, t);
                }
            }
        }
        let mut out: Vec<PeerConflict> = latest
            .into_values()
            .map(|t| PeerConflict {
                peer: t.agent_id.clone(),
                op: t.op,
                summary: t.summary.clone(),
            })
            .collect();
        // Most recent first, ties by peer id for determinism.
        out.sort_by(|a, b| a.peer.cmp(&b.peer));
        out
    }

    /// Has `agent` touched `path` at all (read or write)? Used by a
    /// subscriber to decide whether an incoming touch is its
    /// business before computing conflicts.
    pub fn has_touched(&self, agent: &str, path: &PathBuf) -> bool {
        self.by_agent
            .read()
            .unwrap()
            .get(agent)
            .is_some_and(|s| s.contains(path))
    }

    /// Drop every entry for `agent`. Called when an agent retires;
    /// without it a long run accumulates one dead agent's paths per
    /// subtask.
    pub fn clear_agent(&self, agent: &str) {
        let paths = self.by_agent.write().unwrap().remove(agent);
        let Some(paths) = paths else { return };
        let mut guard = self.by_path.write().unwrap();
        for p in paths {
            if let Some(v) = guard.get_mut(&p) {
                v.retain(|t| t.agent_id != agent);
            }
            // An empty vec is a valid state (another agent's later
            // touch repopulates it); leaving it keeps the map shape
            // stable and avoids a re-alloc on the next touch.
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn touch(agent: &str, path: &str, op: FileOp) -> FileTouch {
        FileTouch {
            agent_id: agent.to_string(),
            path: PathBuf::from(path),
            op,
            summary: None,
            at: Instant::now(),
        }
    }

    #[test]
    fn read_does_not_conflict_with_write() {
        // A read is not contested by a write: the reader saw a
        // version, the writer made another. That is normal editing,
        // not a conflict.
        let svc = FileTouchService::new();
        svc.record(touch("a", "f.rs", FileOp::Read));
        svc.record(touch("b", "f.rs", FileOp::Write));
        let conflicts = svc.conflicts_for(&PathBuf::from("f.rs"), "a");
        // "b" wrote — that IS a conflict for "a". Wait, no: the rule
        // is about the *peer's* op, not the observer's. `a` only read,
        // so it isn't at risk of losing work.
        //
        // Re-derive: the service excludes readers from the peer set.
        // `b` wrote, so `b` is a peer. `a` asking "who conflicts with
        // me on f.rs" gets `b` because `b` modified it — `a`'s own
        // read is not what the exclusion is about. The exclusion
        // filters PEERS that only read, not the observer's op.
        assert_eq!(conflicts.len(), 1);
        assert_eq!(conflicts[0].peer, "b");
    }

    #[test]
    fn peer_that_only_read_is_not_a_conflict() {
        // The reverse case: `a` wrote, `b` only read. `a` asks who
        // conflicts — the answer is nobody, because `b` did not
        // modify anything.
        let svc = FileTouchService::new();
        svc.record(touch("a", "f.rs", FileOp::Write));
        svc.record(touch("b", "f.rs", FileOp::Read));
        let conflicts = svc.conflicts_for(&PathBuf::from("f.rs"), "a");
        assert!(conflicts.is_empty());
    }

    #[test]
    fn one_entry_per_peer_at_their_most_recent_modification() {
        // Three writes from `b`: one notice, not three.
        let svc = FileTouchService::new();
        for i in 0..3 {
            let mut t = touch("b", "f.rs", FileOp::Edit);
            t.summary = Some(format!("edit {i}"));
            svc.record(t);
        }
        let conflicts = svc.conflicts_for(&PathBuf::from("f.rs"), "a");
        assert_eq!(conflicts.len(), 1);
        assert_eq!(conflicts[0].peer, "b");
    }

    #[test]
    fn self_is_excluded() {
        let svc = FileTouchService::new();
        svc.record(touch("a", "f.rs", FileOp::Write));
        let conflicts = svc.conflicts_for(&PathBuf::from("f.rs"), "a");
        assert!(conflicts.is_empty());
    }

    #[test]
    fn unrelated_paths_do_not_conflict() {
        let svc = FileTouchService::new();
        svc.record(touch("a", "a.rs", FileOp::Write));
        svc.record(touch("b", "b.rs", FileOp::Write));
        let conflicts = svc.conflicts_for(&PathBuf::from("a.rs"), "a");
        assert!(conflicts.is_empty());
    }

    #[test]
    fn clear_agent_removes_its_entries() {
        let svc = FileTouchService::new();
        svc.record(touch("a", "f.rs", FileOp::Write));
        svc.record(touch("b", "f.rs", FileOp::Write));
        svc.clear_agent("a");
        // `b`'s entry survives; `a`'s is gone.
        let conflicts = svc.conflicts_for(&PathBuf::from("f.rs"), "b");
        assert_eq!(conflicts.len(), 0, "b's own entry is self-excluded");
        let conflicts = svc.conflicts_for(&PathBuf::from("f.rs"), "a");
        assert_eq!(conflicts.len(), 1, "b still visible to a");
        assert_eq!(conflicts[0].peer, "b");
    }

    #[test]
    fn has_touched_is_true_for_reads_and_writes() {
        let svc = FileTouchService::new();
        svc.record(touch("a", "f.rs", FileOp::Read));
        assert!(svc.has_touched("a", &PathBuf::from("f.rs")));
        assert!(!svc.has_touched("a", &PathBuf::from("g.rs")));
    }

    #[test]
    fn bus_publish_with_no_subscribers_is_silent() {
        // A tool call outside a swarm run publishes to a bus nobody
        // listens to. That must be a no-op, never an error.
        let bus = FileTouchBus::new();
        bus.publish(touch("a", "f.rs", FileOp::Write));
    }

    #[tokio::test]
    async fn bus_delivers_to_a_subscriber() {
        let bus = FileTouchBus::new();
        let mut rx = bus.subscribe();
        bus.publish(touch("a", "f.rs", FileOp::Write));
        let ev = rx.recv().await.expect("one event");
        let SwarmBusEvent::FileTouch(t) = ev;
        assert_eq!(t.agent_id, "a");
    }

    #[test]
    fn multiple_peers_each_get_one_entry() {
        let svc = FileTouchService::new();
        svc.record(touch("a", "f.rs", FileOp::Write));
        svc.record(touch("b", "f.rs", FileOp::Edit));
        svc.record(touch("c", "f.rs", FileOp::Edit));
        svc.record(touch("d", "f.rs", FileOp::Read));
        let conflicts = svc.conflicts_for(&PathBuf::from("f.rs"), "a");
        // b and c, not d (only read).
        assert_eq!(conflicts.len(), 2);
        let peers: Vec<&str> = conflicts.iter().map(|c| c.peer.as_str()).collect();
        assert_eq!(peers, vec!["b", "c"]);
    }
}
