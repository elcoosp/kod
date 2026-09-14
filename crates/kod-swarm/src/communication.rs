//! Agent-to-agent communication hub for direct messaging and broadcasting.

use kod_error::{KodError, Result};
use kod_types::{AgentId, AgentMessageContent, Priority};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::{Mutex, RwLock, mpsc};

/// Alias for message content type (re-exports AgentMessageContent)
pub type MessageContent = AgentMessageContent;

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

    pub async fn unregister_agent(&self, agent_id: &AgentId) {
        let mut agents = self.agents.write().await;
        agents.remove(agent_id);
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
        history
            .entry(agent_id.clone())
            .or_default()
            .push(message.clone());
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
