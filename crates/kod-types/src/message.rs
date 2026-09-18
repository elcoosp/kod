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

#[cfg(test)]
mod coverage_message_render {
    //! `render_text` produces the exact bytes the engine feeds into
    //! the legacy text prompt. A regression changes every golden
    //! prompt snapshot in `kod-core`; the tests here pin the
    //! per-role prefix and the metadata default that those snapshots
    //! depend on.
    use crate::ids::{AgentId, MessageId};
    use crate::{ChatMessage, MessageMetadata, MessageRole};
    use time::OffsetDateTime;

    #[test]
    fn render_text_prefixes_each_role() {
        let now = OffsetDateTime::now_utc();
        let cases: &[(MessageRole, &str)] = &[
            (MessageRole::User, "User"),
            (MessageRole::Assistant, "Assistant"),
            (MessageRole::System, "System"),
            (MessageRole::Tool, "Tool"),
            (MessageRole::Agent(AgentId::new()), "Agent"),
        ];
        for (role, prefix) in cases {
            let m = ChatMessage::text(MessageId::new(), role.clone(), "body", now);
            let rendered = m.render_text();
            assert!(
                rendered.starts_with(prefix),
                "role {role:?}: expected prefix {prefix:?}, got {rendered:?}",
            );
            assert!(rendered.ends_with("body"), "content lost: {rendered:?}");
        }
    }

    #[test]
    fn render_text_with_empty_content_is_just_the_prefix() {
        let m = ChatMessage::text(
            MessageId::new(),
            MessageRole::User,
            "",
            OffsetDateTime::now_utc(),
        );
        assert_eq!(m.render_text(), "User: ");
    }

    #[test]
    fn multiline_content_preserves_internal_newlines() {
        let content = "line one\nline two\nline three";
        let m = ChatMessage::text(
            MessageId::new(),
            MessageRole::Assistant,
            content,
            OffsetDateTime::now_utc(),
        );
        let rendered = m.render_text();
        assert!(rendered.contains("line one\nline two\nline three"));
        assert_eq!(rendered.matches('\n').count(), 2);
    }

    #[test]
    fn text_constructor_defaults_to_no_tool_fields() {
        let m = ChatMessage::text(
            MessageId::new(),
            MessageRole::Assistant,
            "hi",
            OffsetDateTime::now_utc(),
        );
        assert!(m.tool_calls.is_empty());
        assert!(m.tool_call_id.is_none());
    }

    #[test]
    fn metadata_default_is_all_empty_and_unpinned() {
        let m = MessageMetadata::default();
        assert!(m.skill_applied.is_none());
        assert!(m.tools_used.is_empty());
        assert!(m.agent_id.is_none());
        assert!(m.thinking_time_ms.is_none());
        assert!(m.token_count.is_none());
        assert!(!m.pinned);
    }

    #[test]
    fn pinned_flag_round_trips_through_json() {
        let mut m = MessageMetadata::default();
        m.pinned = true;
        let json = serde_json::to_string(&m).unwrap();
        assert!(json.contains("\"pinned\":true"), "got: {json}");
        let parsed: MessageMetadata = serde_json::from_str(&json).unwrap();
        assert!(parsed.pinned);
    }

    #[test]
    fn metadata_without_pinned_key_defaults_to_false() {
        // `MessageMetadata` only carries a per-field default on
        // `pinned`; the other five fields are required. A session
        // file written before `pinned` existed carries those five
        // and no `pinned` key; it must parse with `pinned` defaulting
        // to false.
        let legacy = r#"{
            "skill_applied": null,
            "tools_used": [],
            "agent_id": null,
            "thinking_time_ms": null,
            "token_count": null
        }"#;
        let parsed: MessageMetadata = serde_json::from_str(legacy).unwrap();
        assert!(!parsed.pinned);
    }
}

#[cfg(test)]
mod coverage_agent_message_content {
    //! `AgentMessageContent` and its inner enums are the wire
    //! shape a swarm runner emits and a viewer reads. None of them
    //! derive `PartialEq`, so the round-trips here compare
    //! re-serialized JSON rather than parsed values — an
    //! approach that also pins the on-disk schema (externally
    //! tagged variants, camel-free field names) instead of just
    //! "deserialization succeeded".
    use super::*;

    fn round_trips(c: &AgentMessageContent) {
        let json = serde_json::to_string(c).unwrap();
        let parsed: AgentMessageContent = serde_json::from_str(&json).unwrap();
        let re = serde_json::to_string(&parsed).unwrap();
        assert_eq!(json, re, "roundtrip mismatch for {json}");
    }

    #[test]
    fn every_variant_round_trips_through_json() {
        round_trips(&AgentMessageContent::TaskAssignment {
            description: "write the schema".into(),
            priority: Priority::High,
        });
        round_trips(&AgentMessageContent::ProgressUpdate {
            status: TaskStatus::InProgress,
            details: "half done".into(),
        });
        round_trips(&AgentMessageContent::HelpRequest {
            question: "which db?".into(),
            context: "postgres or sqlite".into(),
        });
        round_trips(&AgentMessageContent::KnowledgeShare {
            information: "column missing".into(),
            tags: vec!["schema".into(), "urgent".into()],
        });
        round_trips(&AgentMessageContent::Coordination {
            action: CoordinationAction::RequestingSync,
        });
        round_trips(&AgentMessageContent::FileClaim {
            path: "src/a.rs".into(),
            duration_secs: 60,
        });
        round_trips(&AgentMessageContent::FileRelease {
            path: "src/a.rs".into(),
        });
        round_trips(&AgentMessageContent::ResultDelivery {
            result: "done".into(),
        });
    }

    #[test]
    fn variants_use_the_externally_tagged_shape() {
        // Default `derive(Serialize)` puts the variant name as the
        // JSON key. A caller (a log viewer, a filter in an external
        // tool) relies on the exact spelling, so a rename is a
        // breaking wire change and worth pinning.
        let v = AgentMessageContent::TaskAssignment {
            description: "d".into(),
            priority: Priority::Low,
        };
        let json = serde_json::to_string(&v).unwrap();
        assert!(json.starts_with("{\"TaskAssignment\""), "got: {json}");
        assert!(json.contains("\"description\":\"d\""), "got: {json}");
        assert!(json.contains("\"priority\":\"Low\""), "got: {json}");
    }

    #[test]
    fn message_destination_round_trips_its_three_forms() {
        let agent = AgentId::new();
        for dest in [
            MessageDestination::Agent(agent.clone()),
            MessageDestination::Broadcast,
            MessageDestination::Coordinator,
        ] {
            let json = serde_json::to_string(&dest).unwrap();
            let parsed: MessageDestination = serde_json::from_str(&json).unwrap();
            // `MessageDestination` derives `PartialEq`, so compare
            // directly rather than round-tripping the JSON.
            assert_eq!(dest, parsed, "roundtrip mismatch for {json}");
        }
    }

    #[test]
    fn priority_orders_low_to_critical() {
        // The derive order is the on-the-wire order; a regression
        // that reshuffled the variants would silently invert every
        // "prioritise this" decision a coordinator makes.
        let order = [Priority::Low, Priority::Medium, Priority::High, Priority::Critical];
        for (i, a) in order.iter().enumerate() {
            for (j, b) in order.iter().enumerate() {
                if i < j {
                    assert_ne!(a, b, "duplicate priority level");
                }
            }
        }
        // Every priority has its own distinct serialized form.
        let mut seen = std::collections::HashSet::new();
        for p in order {
            let json = serde_json::to_string(&p).unwrap();
            assert!(seen.insert(json.clone()), "collision at {json}");
        }
    }

    #[test]
    fn task_status_round_trips_every_variant() {
        for s in [
            TaskStatus::Pending,
            TaskStatus::InProgress,
            TaskStatus::Blocked,
            TaskStatus::Completed,
            TaskStatus::Failed,
        ] {
            let json = serde_json::to_string(&s).unwrap();
            let parsed: TaskStatus = serde_json::from_str(&json).unwrap();
            assert_eq!(s, parsed, "roundtrip mismatch for {json}");
        }
    }

    #[test]
    fn agent_message_round_trips_with_a_direct_destination() {
        let msg = AgentMessage {
            id: MessageId::new(),
            from: AgentId::new(),
            to: MessageDestination::Agent(AgentId::new()),
            content: AgentMessageContent::ResultDelivery {
                result: "ok".into(),
            },
            timestamp: OffsetDateTime::now_utc(),
        };
        let json = serde_json::to_string(&msg).unwrap();
        let parsed: AgentMessage = serde_json::from_str(&json).unwrap();
        assert_eq!(msg.id, parsed.id);
        assert_eq!(msg.from, parsed.from);
    }
}
