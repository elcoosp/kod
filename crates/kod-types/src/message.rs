//! Message types for communication between agents, users, and the system.

use crate::ids::{AgentId, MessageId};
use serde::{Deserialize, Serialize};
use time::OffsetDateTime;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChatMessage {
    pub id: MessageId,
    pub role: MessageRole,
    pub content: String,
    pub timestamp: OffsetDateTime,
    pub metadata: MessageMetadata,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum MessageRole {
    User,
    Assistant,
    System,
    Tool,
    Agent(AgentId),
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct MessageMetadata {
    pub skill_applied: Option<String>,
    pub tools_used: Vec<String>,
    pub agent_id: Option<AgentId>,
    pub thinking_time_ms: Option<u64>,
    pub token_count: Option<usize>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentMessage {
    pub id: MessageId,
    pub from: AgentId,
    pub to: MessageDestination,
    pub content: AgentMessageContent,
    pub timestamp: OffsetDateTime,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum MessageDestination {
    Agent(AgentId),
    Broadcast,
    Coordinator,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum AgentMessageContent {
    TaskAssignment {
        description: String,
        priority: Priority,
    },
    ProgressUpdate {
        status: TaskStatus,
        details: String,
    },
    HelpRequest {
        question: String,
        context: String,
    },
    KnowledgeShare {
        information: String,
        tags: Vec<String>,
    },
    Coordination {
        action: CoordinationAction,
    },
    FileClaim {
        path: String,
        duration_secs: u64,
    },
    FileRelease {
        path: String,
    },
    ResultDelivery {
        result: String,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Priority {
    Low,
    Medium,
    High,
    Critical,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum TaskStatus {
    Pending,
    InProgress,
    Blocked,
    Completed,
    Failed,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum CoordinationAction {
    RequestingSync,
    ProposingChange { file: String, description: String },
    AcknowledgingChange { file: String },
    ConflictDetected { file: String, description: String },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_chat_message_serialization() {
        let msg = ChatMessage {
            id: MessageId::new(),
            role: MessageRole::User,
            content: "Hello, world".to_string(),
            timestamp: OffsetDateTime::now_utc(),
            metadata: MessageMetadata::default(),
        };

        let json = serde_json::to_string(&msg).unwrap();
        let deserialized: ChatMessage = serde_json::from_str(&json).unwrap();
        assert_eq!(msg.id, deserialized.id);
        assert_eq!(msg.content, deserialized.content);
    }

    #[test]
    fn test_agent_message_serialization() {
        let msg = AgentMessage {
            id: MessageId::new(),
            from: AgentId::new(),
            to: MessageDestination::Broadcast,
            content: AgentMessageContent::TaskAssignment {
                description: "Implement feature".to_string(),
                priority: Priority::High,
            },
            timestamp: OffsetDateTime::now_utc(),
        };

        let json = serde_json::to_string(&msg).unwrap();
        let deserialized: AgentMessage = serde_json::from_str(&json).unwrap();
        assert_eq!(msg.id, deserialized.id);
    }
}
