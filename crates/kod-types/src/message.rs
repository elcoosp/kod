//! Message types for communication between agents, users, and the system.

use crate::ids::{AgentId, MessageId};
use serde::{Deserialize, Serialize};
use time::OffsetDateTime;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChatMessage {
    pub id: MessageId,
    pub role: MessageRole,
    pub content: String,
    /// Tool calls issued by an `Assistant` message. Empty for every
    /// other role. Populated when the model responds with tool calls;
    /// the assistant message is then followed by one `Role::Tool`
    /// message per call (AD-02).
    #[serde(default)]
    pub tool_calls: Vec<crate::tool::ToolCall>,
    /// For `Role::Tool` messages: the id of the tool call this message
    /// answers. `None` for other roles, and `None` for legacy
    /// transcripts written before the field existed.
    #[serde(default)]
    pub tool_call_id: Option<String>,
    pub timestamp: OffsetDateTime,
    pub metadata: MessageMetadata,
}

impl ChatMessage {
    /// Construct a message with no tool calls or tool-result linkage.
    /// The common case for user / system / plain assistant turns.
    pub fn text(
        id: MessageId,
        role: MessageRole,
        content: impl Into<String>,
        timestamp: OffsetDateTime,
    ) -> Self {
        Self {
            id,
            role,
            content: content.into(),
            tool_calls: Vec::new(),
            tool_call_id: None,
            timestamp,
            metadata: MessageMetadata::default(),
        }
    }

    /// Render this message as a single line for a plain-text prompt.
    /// `Assistant` and `Tool` messages may span multiple lines; the
    /// role prefix stays on the first line only.
    pub fn render_text(&self) -> String {
        let prefix = match &self.role {
            MessageRole::User => "User",
            MessageRole::Assistant => "Assistant",
            MessageRole::System => "System",
            MessageRole::Tool => "Tool",
            MessageRole::Agent(_) => "Agent",
        };
        format!("{prefix}: {}", self.content)
    }
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
    /// When true, the engine never drops this turn from the rendered
    /// history, even when the budget is exhausted. Set by the TUI's
    /// `/pin` command; persisted with the session so a pin set in one
    /// session survives a restart.
    ///
    /// `#[serde(default)]` for the same reason the whole struct uses
    /// it: a session file written before the field existed still
    /// parses, with every message unpinned.
    #[serde(default)]
    pub pinned: bool,
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
        let msg = ChatMessage::text(
            MessageId::new(),
            MessageRole::User,
            "Hello, world",
            OffsetDateTime::now_utc(),
        );

        let json = serde_json::to_string(&msg).unwrap();
        let deserialized: ChatMessage = serde_json::from_str(&json).unwrap();
        assert_eq!(msg.id, deserialized.id);
        assert_eq!(msg.content, deserialized.content);
        assert!(deserialized.tool_calls.is_empty());
        assert!(deserialized.tool_call_id.is_none());
    }

    /// A legacy transcript (no `tool_calls` / `tool_call_id` keys) must
    /// still parse. The `#[serde(default)]` attributes are the contract;
    /// this test fails loudly if someone removes them.
    ///
    /// The test builds the JSON programmatically from a real
    /// `ChatMessage`, then strips the two new keys — this is the only
    /// safe way to hand-write the fixture, because `time::OffsetDateTime`
    /// uses the `time` crate's own serde format (not ISO 8601) unless
    /// the `serde-well-known` feature is on. Hand-writing the timestamp
    /// in ISO would test the wrong format.
    #[test]
    fn test_chat_message_legacy_json_parses() {
        let msg = ChatMessage::text(
            MessageId::new(),
            MessageRole::User,
            "legacy",
            OffsetDateTime::now_utc(),
        );
        let mut v: serde_json::Value = serde_json::to_value(&msg).expect("serialize");
        let obj = v.as_object_mut().expect("object");
        obj.remove("tool_calls");
        obj.remove("tool_call_id");

        let legacy = serde_json::to_string(&v).expect("re-serialize");
        let parsed: ChatMessage = serde_json::from_str(&legacy).expect("legacy json must parse");
        assert_eq!(parsed.content, "legacy");
        assert!(parsed.tool_calls.is_empty());
        assert!(parsed.tool_call_id.is_none());
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
