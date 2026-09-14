//! Agent-to-agent communication hub for direct messaging and broadcasting.

use kod_error::{KodError, Result};
use kod_types::{AgentId, AgentMessageContent, Priority};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::{Mutex, RwLock, mpsc};

/// Alias for message content type (re-exports AgentMessageContent)
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
const MAX_HISTORY_PER_AGENT: usize = 100;

/// A message with routing information and priority
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SwarmMessage {
    pub id: uuid::Uuid,
    pub from: AgentId,
    pub to: MessageDestination,
    pub content: MessageContent,
    pub priority: Priority,
    pub created_at: chrono::DateTime<chrono::Utc>,
}

impl SwarmMessage {
    pub fn new(
        from: AgentId,
        to: MessageDestination,
        content: MessageContent,
        priority: Priority,
    ) -> Self {
        Self {
            id: uuid::Uuid::new_v4(),
            from,
            to,
            content,
            priority,
            created_at: chrono::Utc::now(),
        }
    }
}

/// Re-export types from kod_types for convenience
pub use kod_types::MessageDestination;

/// Receiver for agent messages — wraps an UnboundedReceiver that can be cloned
#[derive(Clone)]
pub struct AgentMessageReceiver {
    inner: Arc<Mutex<Option<mpsc::UnboundedReceiver<SwarmMessage>>>>,
}

impl AgentMessageReceiver {
    pub fn new(rx: mpsc::UnboundedReceiver<SwarmMessage>) -> Self {
        Self {
            inner: Arc::new(Mutex::new(Some(rx))),
        }
    }

    pub async fn recv(&self) -> Option<SwarmMessage> {
        let mut guard = self.inner.lock().await;
        if let Some(ref mut rx) = *guard {
            rx.recv().await
        } else {
            None
        }
    }
}

/// Hub for inter-agent communication
#[derive(Clone)]
pub struct AgentCommunicationHub {
    agents: Arc<RwLock<HashMap<AgentId, AgentInfo>>>,
    history: Arc<RwLock<HashMap<AgentId, Vec<SwarmMessage>>>>,
}

#[derive(Clone)]
struct AgentInfo {
    tx: mpsc::UnboundedSender<SwarmMessage>,
    rx: Arc<Mutex<Option<mpsc::UnboundedReceiver<SwarmMessage>>>>,
    online: bool,
}

impl Default for AgentCommunicationHub {
    fn default() -> Self {
        Self::new()
    }
}

impl AgentCommunicationHub {
    pub fn new() -> Self {
        Self {
            agents: Arc::new(RwLock::new(HashMap::new())),
            history: Arc::new(RwLock::new(HashMap::new())),
        }
    }

    pub async fn register_agent(&self, agent_id: AgentId) -> Result<()> {
        let (tx, rx) = mpsc::unbounded_channel();
        let mut agents = self.agents.write().await;
        if agents.contains_key(&agent_id) {
            return Err(KodError::InvalidState(format!(
                "Agent {} already registered",
                agent_id
            )));
        }
        agents.insert(
            agent_id.clone(),
            AgentInfo {
                tx,
                rx: Arc::new(Mutex::new(Some(rx))),
                online: true,
            },
        );
        drop(agents);

        let mut history = self.history.write().await;
        history.entry(agent_id).or_default();
        Ok(())
    }

    /// Remove an agent from the hub.
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
    }

    pub async fn send_direct(
        &self,
        from: &AgentId,
        to: &AgentId,
        content: MessageContent,
    ) -> Result<()> {
        let agents = self.agents.read().await;
        if !agents.contains_key(from) {
            return Err(KodError::InvalidState(format!(
                "Sender {} not registered",
                from
            )));
        }
        let agent_info = agents.get(to);
        match agent_info {
            Some(info) if info.online => {
                let message = SwarmMessage::new(
                    from.clone(),
                    MessageDestination::Agent(to.clone()),
                    content,
                    Priority::Medium,
                );
                let tx = info.tx.clone();
                drop(agents);
                self.record_message(from, &message).await;
                self.record_message(to, &message).await;
                tx.send(message)
                    .map_err(|e| KodError::InvalidState(e.to_string()))?;
                Ok(())
            }
            Some(_) => Err(KodError::InvalidState(format!("Agent {} is offline", to))),
            None => Err(KodError::InvalidState(format!(
                "Agent {} not registered",
                to
            ))),
        }
    }

    pub async fn broadcast(&self, from: &AgentId, content: MessageContent) -> Result<()> {
        let agents = self.agents.read().await;
        if !agents.contains_key(from) {
            return Err(KodError::InvalidState(format!(
                "Sender {} not registered",
                from
            )));
        }
        let message = SwarmMessage::new(
            from.clone(),
            MessageDestination::Broadcast,
            content,
            Priority::Medium,
        );

        let recipients: Vec<_> = agents
            .iter()
            .filter(|(id, info)| **id != *from && info.online)
            .map(|(id, info)| (id.clone(), info.tx.clone()))
            .collect();
        drop(agents);

        self.record_message(from, &message).await;
        for (id, _) in &recipients {
            self.record_message(id, &message).await;
        }

        // Deliver to every recipient. `message` is Clone, so each send
        // gets its own copy. (The previous code moved `message` into
        // the loop; on a single recipient it happened to compile, but
        // the intent is a broadcast, not a one-shot.)
        for (_, tx) in recipients {
            tx.send(message.clone())
                .map_err(|e| KodError::InvalidState(e.to_string()))?;
        }
        Ok(())
    }

    pub async fn set_agent_offline(&self, agent_id: &AgentId) {
        if let Some(info) = self.agents.write().await.get_mut(agent_id) {
            info.online = false;
        }
    }

    pub async fn set_agent_online(&self, agent_id: &AgentId) {
        if let Some(info) = self.agents.write().await.get_mut(agent_id) {
            info.online = true;
        }
    }

    pub async fn get_agent_receiver(&self, agent_id: &AgentId) -> Result<AgentMessageReceiver> {
        let mut agents = self.agents.write().await;
        match agents.get_mut(agent_id) {
            Some(info) => {
                let rx = info.rx.lock().await.take();
                match rx {
                    Some(rx) => Ok(AgentMessageReceiver::new(rx)),
                    None => Err(KodError::InvalidState(format!(
                        "Receiver already taken for agent {}",
                        agent_id
                    ))),
                }
            }
            None => Err(KodError::InvalidState(format!(
                "Agent {} not registered",
                agent_id
            ))),
        }
    }

    pub async fn get_agent_history(&self, agent_id: &AgentId) -> Vec<SwarmMessage> {
        self.history
            .read()
            .await
            .get(agent_id)
            .cloned()
            .unwrap_or_default()
    }

    async fn record_message(&self, agent_id: &AgentId, message: &SwarmMessage) {
        let mut history = self.history.write().await;
        let entry = history.entry(agent_id.clone()).or_default();
        entry.push(message.clone());
        // Bound the per-agent history. Oldest-first eviction — see
        // MAX_HISTORY_PER_AGENT for the reasoning and the value.
        if entry.len() > MAX_HISTORY_PER_AGENT {
            let excess = entry.len() - MAX_HISTORY_PER_AGENT;
            entry.drain(..excess);
        }
    }
}


#[cfg(test)]
mod tests {
    use super::*;
    use super::MessageContent;

    /// Regression: the original broadcast moved `message` into the
    /// delivery loop, so a swarm with two online recipients other than
    /// the sender would deliver to at most one of them (the first
    /// send moved the value; the second could not). This test guards
    /// the fix: every online recipient other than the sender must
    /// receive the message.
    #[tokio::test]
    async fn broadcast_reaches_every_online_recipient() {
        let hub = AgentCommunicationHub::new();
        let sender = AgentId::new();
        let a = AgentId::new();
        let b = AgentId::new();
        hub.register_agent(sender.clone()).await.unwrap();
        hub.register_agent(a.clone()).await.unwrap();
        hub.register_agent(b.clone()).await.unwrap();

        // Take receivers before broadcasting so the channels are live.
        let rx_a = hub.get_agent_receiver(&a).await.unwrap();
        let rx_b = hub.get_agent_receiver(&b).await.unwrap();

        let content = MessageContent::ResultDelivery {
            result: "all done".to_string(),
        };
        hub.broadcast(&sender, content).await.unwrap();

        let got_a = rx_a.recv().await.expect("agent A should receive");
        let got_b = rx_b.recv().await.expect("agent B should receive");
        assert_eq!(got_a.from, sender);
        assert_eq!(got_b.from, sender);
    }

    /// Unregistering an agent must drop its history alongside its
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
    async fn broadcast_does_not_echo_to_sender() {
        let hub = AgentCommunicationHub::new();
        let sender = AgentId::new();
        let peer = AgentId::new();
        hub.register_agent(sender.clone()).await.unwrap();
        hub.register_agent(peer.clone()).await.unwrap();

        let rx_sender = hub.get_agent_receiver(&sender).await.unwrap();
        let _rx_peer = hub.get_agent_receiver(&peer).await.unwrap();

        hub.broadcast(
            &sender,
            MessageContent::ResultDelivery {
                result: "hi".to_string(),
            },
        )
        .await
        .unwrap();

        // The sender's own receiver should see nothing within a short
        // window — the broadcast filter excludes the sender.
        let recv = tokio::time::timeout(
            std::time::Duration::from_millis(50),
            rx_sender.recv(),
        )
        .await;
        assert!(recv.is_err(), "sender should not receive its own broadcast");
    }
}
