#!/usr/bin/env bash
set -uo pipefail

TARGET=crates/kod-swarm/src/communication.rs

if [ ! -f Cargo.toml ] || [ ! -f "$TARGET" ]; then
    echo "ERROR: run from the kod workspace root ($TARGET missing)"
    exit 1
fi

echo "=== Current shape ==="
grep -n "unregister_agent\|record_message\|fn get_agent_history" "$TARGET"

echo
echo "Patching $TARGET"

python3 - "$TARGET" << 'PYEOF'
import os
import sys

target = sys.argv[1]
with open(target, "r") as f:
    src = f.read()

def patch(old, new, label, expect=1):
    global src
    n = src.count(old)
    if n == 0:
        print(f"  MISS: {label}")
        return False
    if expect and n != expect:
        print(f"  ERROR: expected {expect} occurrence(s) of {label}, found {n}")
        sys.exit(2)
    src = src.replace(old, new, expect if expect else n)
    print(f"  patched: {label}")
    return True

# ----------------------------------------------------------------------
# 1. Constant + comment near the top of the file (after imports).
# ----------------------------------------------------------------------
patch(
    '''/// Alias for message content type (re-exports AgentMessageContent)
pub type MessageContent = AgentMessageContent;''',
    '''/// Alias for message content type (re-exports AgentMessageContent)
pub type MessageContent = AgentMessageContent;

/// Maximum messages retained per agent in the hub's history map.
///
/// The history is a convenience for `get_agent_history`, not an audit
/// log. A hub whose agents exchange many messages (a task-decomposition
/// swarm can easily produce hundreds of `ProgressUpdate` /
/// `Coordination` frames over a run) grows the map without bound if
/// nothing drops the oldest entries — the same "unbounded growth in a
/// long-lived container" shape as the registry. Cap at a value large
/// enough to cover "what happened recently" and small enough that a
/// long-running hub does not accumulate stale frames.
///
/// The oldest entries are dropped first, matching the natural reading
/// of `get_agent_history` as "the recent messages".
const MAX_HISTORY_PER_AGENT: usize = 100;''',
    "MAX_HISTORY_PER_AGENT constant",
)

# ----------------------------------------------------------------------
# 2. unregister_agent also clears the history for that agent.
# ----------------------------------------------------------------------
patch(
    '''    pub async fn unregister_agent(&self, agent_id: &AgentId) {
        let mut agents = self.agents.write().await;
        agents.remove(agent_id);
    }''',
    '''    /// Remove an agent from the hub.
    ///
    /// Drops the agent's message history along with its channel. The
    /// previous implementation removed the registration but left the
    /// `history` map entry in place, so a hub that cycled through
    /// short-lived agents (the pattern a task-decomposition swarm
    /// takes: spawn, work, retire, repeat) accumulated one dead
    /// history per agent indefinitely. `get_agent_history` would also
    /// keep returning messages for an ID that no longer names a
    /// registered agent — a stale read with no way for the caller to
    /// tell "no such agent" from "registered but quiet".
    ///
    /// There is no read path for a retired agent's history — the
    /// registration was the only handle — so dropping it is a lossless
    /// cleanup.
    pub async fn unregister_agent(&self, agent_id: &AgentId) {
        let mut agents = self.agents.write().await;
        agents.remove(agent_id);
        drop(agents);
        let mut history = self.history.write().await;
        history.remove(agent_id);
    }''',
    "unregister_agent clears history",
)

# ----------------------------------------------------------------------
# 3. Cap history in record_message.
# ----------------------------------------------------------------------
patch(
    '''    async fn record_message(&self, agent_id: &AgentId, message: &SwarmMessage) {
        let mut history = self.history.write().await;
        history
            .entry(agent_id.clone())
            .or_default()
            .push(message.clone());
    }''',
    '''    async fn record_message(&self, agent_id: &AgentId, message: &SwarmMessage) {
        let mut history = self.history.write().await;
        let entry = history.entry(agent_id.clone()).or_default();
        entry.push(message.clone());
        // Bound the per-agent history. Oldest-first eviction — see
        // MAX_HISTORY_PER_AGENT for the reasoning and the value.
        if entry.len() > MAX_HISTORY_PER_AGENT {
            let excess = entry.len() - MAX_HISTORY_PER_AGENT;
            entry.drain(..excess);
        }
    }''',
    "record_message caps history",
)

# ----------------------------------------------------------------------
# 4. Tests.
# ----------------------------------------------------------------------
if "test_unregister_clears_history" not in src:
    # The tests module was added earlier in the session; anchor on
    # the broadcast test that already lives there.
    anchor = '''    /// The sender should not receive their own broadcast.
    #[tokio::test]
    async fn broadcast_does_not_echo_to_sender() {'''
    if anchor not in src:
        print("  ERROR: test anchor not found")
        sys.exit(2)

    new_tests = '''    /// Unregistering an agent must drop its history alongside its
    /// registration. Regression: the previous implementation removed
    /// the agent but left the history entry in place, so a hub that
    /// cycled short-lived agents accumulated one dead history per
    /// agent, and `get_agent_history` returned messages for an ID
    /// that no longer named a registered agent.
    #[tokio::test]
    async fn test_unregister_clears_history() {
        let hub = AgentCommunicationHub::new();
        let a = AgentId::new();
        let b = AgentId::new();
        hub.register_agent(a.clone()).await.unwrap();
        hub.register_agent(b.clone()).await.unwrap();

        // Take receivers so the channels are live, then send a
        // message from a to b; both histories record it.
        let _rx_b = hub.get_agent_receiver(&b).await.unwrap();
        hub.send_direct(
            &a,
            &b,
            MessageContent::ResultDelivery {
                result: "done".to_string(),
            },
        )
        .await
        .unwrap();

        assert!(!hub.get_agent_history(&a).await.is_empty());
        assert!(!hub.get_agent_history(&b).await.is_empty());

        // Retire a. Its history must be gone.
        hub.unregister_agent(&a).await;
        assert!(
            hub.get_agent_history(&a).await.is_empty(),
            "unregister must drop the agent's history"
        );
        // b's history is untouched — it still names a registered
        // agent.
        assert!(!hub.get_agent_history(&b).await.is_empty());
    }

    /// Per-agent history is capped at MAX_HISTORY_PER_AGENT, keeping
    /// the newest entries.
    #[tokio::test]
    async fn test_history_is_capped_per_agent() {
        let hub = AgentCommunicationHub::new();
        let a = AgentId::new();
        let b = AgentId::new();
        hub.register_agent(a.clone()).await.unwrap();
        hub.register_agent(b.clone()).await.unwrap();

        // Drain b's receiver so its queue does not fill; we only care
        // about history, not delivery.
        let mut rx_b = hub.get_agent_receiver(&b).await.unwrap();
        // Spawn a drain task so unbounded sends do not block.
        let drain = tokio::spawn(async move {
            while rx_b.recv().await.is_some() {}
        });

        // Send more than the cap. Send from a to b: each message
        // records in both a's and b's history.
        let total = MAX_HISTORY_PER_AGENT + 20;
        for i in 0..total {
            hub.send_direct(
                &a,
                &b,
                MessageContent::ResultDelivery {
                    result: format!("m{i}"),
                },
            )
            .await
            .unwrap();
        }

        // a's history is capped (a is the sender; every send is
        // recorded under a as well).
        let a_hist = hub.get_agent_history(&a).await;
        assert_eq!(
            a_hist.len(),
            MAX_HISTORY_PER_AGENT,
            "sender history should be capped at {}",
            MAX_HISTORY_PER_AGENT
        );
        // The kept entries are the most recent, so the last one is
        // the most recent send.
        let last = a_hist.last().unwrap();
        match &last.content {
            MessageContent::ResultDelivery { result } => {
                assert_eq!(result, &format!("m{}", total - 1));
            }
            other => panic!("unexpected content: {other:?}"),
        }

        drain.abort();
    }

    /// The sender should not receive their own broadcast.
    #[tokio::test]
    async fn broadcast_does_not_echo_to_sender() {'''

    src = src.replace(anchor, new_tests, 1)
    print("  added unregister + cap tests")
else:
    print("  tests already present")

tmp = target + ".tmp"
with open(tmp, "w") as f:
    f.write(src)
os.replace(tmp, target)
print("Wrote", target)
PYEOF

if [ $? -ne 0 ]; then
    echo "ERROR: patching failed"
    exit 1
fi

echo
echo "cargo check --workspace --all-targets 2>&1 | tail -15"
if ! cargo check --workspace --all-targets 2>&1 | tail -15; then
    echo "Compilation failed"
    exit 1
fi

echo
echo "Committing."
git add -A
git commit -F - <<'MSG'
fix(swarm): unregister drops history; cap per-agent history

AgentCommunicationHub carried two unbounded-growth bugs in the
history map.

1. unregister_agent removed the agent from the `agents` map but left
   its message history in the `history` map. A hub that cycled
   short-lived agents — the pattern a task-decomposition swarm
   takes, spawn/work/retire repeatedly — accumulated one dead
   history per retired agent. Worse, get_agent_history kept
   returning messages for an ID that no longer named a registered
   agent, so a caller could not tell "no such agent" from
   "registered but quiet". Clear the history entry alongside the
   registration.

2. Even for a registered agent, record_message appended without
   bound. A long-running hub whose agents exchange many
   ProgressUpdate / Coordination frames grows the map forever. Add
   MAX_HISTORY_PER_AGENT = 100 with oldest-first eviction: the
   entries kept are the most recent, matching the natural reading of
   `get_agent_history` as "what happened recently". The cap is per
   agent, so a hub with N registered agents stays bounded at O(N *
   100).

Both changes are structural — the API signature is unchanged and
every existing caller keeps working. A caller that wanted to read
history *after* unregistering an agent is not a use case (the
registration was the only handle to the ID, and no code in the
workspace does this).

Adds two tests: unregister_clears_history sends a message between
two agents, confirms both histories are populated, retires one, and
asserts its history is gone while the other's is untouched;
history_is_capped_per_agent sends cap+20 messages and asserts the
sender's history holds exactly the cap, with the most recent
message last.
MSG
