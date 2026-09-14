#!/usr/bin/env bash
set -uo pipefail

COMM=crates/kod-swarm/src/communication.rs

echo "=== Current get_agent_history + record_message ==="
awk '/pub async fn get_agent_history/,/^    \}$/' "$COMM" | head -15
echo "---"
awk '/async fn record_message/,/^    \}$/' "$COMM" | head -15

echo
echo "Patching $COMM"

python3 - "$COMM" << 'PYEOF'
import os
import sys

target = sys.argv[1]
with open(target, "r") as f:
    src = f.read()

def patch(old, new, label, expect=1):
    global src
    n = src.count(old)
    if n == 0:
        print(f"  SKIP (anchor absent): {label}")
        return False
    if expect and n != expect:
        print(f"  ERROR: expected {expect} occurrence(s) of {label}, found {n}")
        sys.exit(2)
    src = src.replace(old, new, expect if expect else n)
    print(f"  patched: {label}")
    return True

# ----------------------------------------------------------------------
# 1. Document what send_direct and broadcast do to each side's history.
# ----------------------------------------------------------------------
patch(
    '''    pub async fn send_direct(
        &self,
        from: &AgentId,
        to: &AgentId,
        content: MessageContent,
    ) -> Result<()> {''',
    '''    /// Send `content` from `from` to `to`.
    ///
    /// On success the message is appended to **both** the sender's and
    /// the recipient's history. `get_agent_history(&a)` therefore
    /// returns every message `a` participated in, sent or received —
    /// not just the messages addressed to `a`. That is the useful
    /// reading for a debug panel ("show me everything this agent said
    /// and heard") and the intended one; it is documented here because
    /// the method name alone suggests "inbox."
    ///
    /// Use [`AgentCommunicationHub::get_sent_history`] or
    /// [`AgentCommunicationHub::get_received_history`] when the
    /// distinction matters.
    pub async fn send_direct(
        &self,
        from: &AgentId,
        to: &AgentId,
        content: MessageContent,
    ) -> Result<()> {''',
    "send_direct: document dual recording",
)

# ----------------------------------------------------------------------
# 2. Same for broadcast.
# ----------------------------------------------------------------------
patch(
    '''    pub async fn broadcast(&self, from: &AgentId, content: MessageContent) -> Result<()> {''',
    '''    /// Broadcast `content` from `from` to every other online agent.
    ///
    /// The message is appended to the sender's history once and to
    /// each recipient's history once — same as
    /// [`AgentCommunicationHub::send_direct`], per-recipient. See that
    /// method's doc for the semantics of `get_agent_history`.
    pub async fn broadcast(&self, from: &AgentId, content: MessageContent) -> Result<()> {''',
    "broadcast: document per-recipient recording",
)

# ----------------------------------------------------------------------
# 3. Rewrite get_agent_history doc + add the two convenience
#    accessors. Anchor on the existing method body.
# ----------------------------------------------------------------------
patch(
    '''    pub async fn get_agent_history(&self, agent_id: &AgentId) -> Vec<SwarmMessage> {
        self.history
            .read()
            .await
            .get(agent_id)
            .cloned()
            .unwrap_or_default()
    }''',
    '''    /// Every message `agent_id` participated in, oldest first —
    /// whether `agent_id` sent it, received it directly, or received
    /// it via broadcast.
    ///
    /// The method name reads as "inbox" and the previous lack of a
    /// doc let that reading stand. It is not an inbox: a message from
    /// A to B is recorded in both A's and B's history (see
    /// [`AgentCommunicationHub::send_direct`]), so this returns the
    /// agent's full conversation as an observer would see it.
    ///
    /// Use [`AgentCommunicationHub::get_sent_history`] or
    /// [`AgentCommunicationHub::get_received_history`] when only one
    /// side of the participation matters.
    ///
    /// Returns an empty vec for an unregistered agent. The history
    /// entry is dropped at [`AgentCommunicationHub::unregister_agent`]
    /// time, so an ID that names a retired agent and an ID that was
    /// never registered are both empty.
    pub async fn get_agent_history(&self, agent_id: &AgentId) -> Vec<SwarmMessage> {
        self.history
            .read()
            .await
            .get(agent_id)
            .cloned()
            .unwrap_or_default()
    }

    /// Messages `agent_id` sent, oldest first.
    ///
    /// Filters [`AgentCommunicationHub::get_agent_history`] by
    /// `message.from == agent_id`. Useful for a caller that wants to
    /// show "what this agent has said" without interleaving what it
    /// heard.
    pub async fn get_sent_history(&self, agent_id: &AgentId) -> Vec<SwarmMessage> {
        self.get_agent_history(agent_id)
            .await
            .into_iter()
            .filter(|m| m.from == *agent_id)
            .collect()
    }

    /// Messages `agent_id` received, oldest first — that is, every
    /// message in its history that it did not itself send. Includes
    /// direct messages and broadcasts that reached it.
    pub async fn get_received_history(&self, agent_id: &AgentId) -> Vec<SwarmMessage> {
        self.get_agent_history(agent_id)
            .await
            .into_iter()
            .filter(|m| m.from != *agent_id)
            .collect()
    }''',
    "get_agent_history doc + sent/received accessors",
)

# ----------------------------------------------------------------------
# 4. Tests. Anchor on the existing broadcast tests.
# ----------------------------------------------------------------------
if "test_get_sent_and_received_split_history" not in src:
    anchor = '''    /// The sender should not receive their own broadcast.'''
    if anchor not in src:
        print("  ERROR: test anchor not found")
        sys.exit(2)
    new_test = '''    /// `get_sent_history` and `get_received_history` partition
    /// `get_agent_history` by direction: for any agent, sent +
    /// received == full history, and each is the correct subset.
    #[tokio::test]
    async fn test_get_sent_and_received_split_history() {
        let hub = AgentCommunicationHub::new();
        let a = AgentId::new();
        let b = AgentId::new();
        hub.register_agent(a.clone()).await.unwrap();
        hub.register_agent(b.clone()).await.unwrap();

        let _rx_b = hub.get_agent_receiver(&b).await.unwrap();

        // A -> B twice, B -> A once. A's full history is 3; sent is 2,
        // received is 1. B's is the mirror.
        for text in ["one", "two"] {
            hub.send_direct(
                &a,
                &b,
                MessageContent::ResultDelivery {
                    result: text.to_string(),
                },
            )
            .await
            .unwrap();
        }
        hub.send_direct(
            &b,
            &a,
            MessageContent::ResultDelivery {
                result: "reply".to_string(),
            },
        )
        .await
        .unwrap();

        let a_all = hub.get_agent_history(&a).await;
        let a_sent = hub.get_sent_history(&a).await;
        let a_recv = hub.get_received_history(&a).await;
        assert_eq!(a_all.len(), 3, "A participated in 3 messages");
        assert_eq!(a_sent.len(), 2, "A sent 2");
        assert_eq!(a_recv.len(), 1, "A received 1");
        assert_eq!(a_sent.len() + a_recv.len(), a_all.len());
        // Every sent message has from == a.
        for m in &a_sent {
            assert_eq!(m.from, a);
        }
        for m in &a_recv {
            assert_ne!(m.from, a);
        }

        let b_all = hub.get_agent_history(&b).await;
        let b_sent = hub.get_sent_history(&b).await;
        let b_recv = hub.get_received_history(&b).await;
        assert_eq!(b_all.len(), 3);
        assert_eq!(b_sent.len(), 1, "B sent 1");
        assert_eq!(b_recv.len(), 2, "B received 2");
    }

    /// The sender should not receive their own broadcast.'''
    src = src.replace(anchor, new_test, 1)
    print("  added test_get_sent_and_received_split_history")
else:
    print("  test already present")

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

cat > /tmp/kod_commit_msg.txt <<'MSG'
docs(swarm): clarify history direction; add sent/received accessors

AgentCommunicationHub::get_agent_history returns every message an
agent participated in — sent, received directly, or received via
broadcast. The name reads as "inbox" and the previous absence of a
doc let that misreading stand: a caller looking for the messages
addressed to an agent got a transcript that also contained
everything the agent itself had said.

Document the semantics on get_agent_history, send_direct, and
broadcast. Add two convenience accessors:

  get_sent_history(&a)      messages with from == a
  get_received_history(&a)  messages with from != a

A caller that wants the inbox reads get_received_history; a debug
panel that wants "everything this agent said and heard" keeps
get_agent_history.

Adds test_get_sent_and_received_split_history: three messages
between two agents (A->B twice, B->A once), asserts the split is
2/1 for A and 1/2 for B, that sent + received == full history, and
that every message in each subset has the correct from field. No
behavior change; the new accessors are thin filters over the
existing method.
MSG

git add -A
git commit -F /tmp/kod_commit_msg.txt
rm -f /tmp/kod_commit_msg.txt
