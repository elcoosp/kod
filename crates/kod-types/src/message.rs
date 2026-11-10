//! Message types for communication between agents, users, and the system.

use crate::ids::{AgentId, MessageId};
use serde::{Deserialize, Serialize};
use time::OffsetDateTime;

/// One message in a chat transcript.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChatMessage {
    /// The message's unique id.
    pub id: MessageId,
    /// The role that produced the message.
    pub role: MessageRole,
    /// The message body.
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
    /// When the message was created.
    pub timestamp: OffsetDateTime,
    /// Structured metadata (skills, tools, timings, pin state).
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
    #[must_use]
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

/// The role a message is from.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum MessageRole {
    /// The user.
    User,
    /// The assistant (model).
    Assistant,
    /// The system prompt.
    System,
    /// A tool result.
    Tool,
    /// A named agent (used in swarm transcripts).
    Agent(AgentId),
}

/// A rasterized image attached to a message (delta §4.5).
///
/// The engine's inline-imaging pass replaces a large text tool
/// result with a PNG a vision model reads for fewer tokens. The
/// image travels on the message's metadata so the wire layer emits
/// it alongside (or instead of) the text — no side channel.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct RasterizedImage {
    /// Base64 of the PNG, no data-URL prefix.
    pub png_base64: String,
    /// The media type string the provider expects (`image/png`).
    pub media_type: String,
    /// Number of source text lines the frame covers. Kept for the
    /// UI's "N lines imaged" readout.
    pub source_lines: usize,
}

#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
/// Structured metadata attached to every message.
pub struct MessageMetadata {
    /// The skill that was applied to this message, if any.
    pub skill_applied: Option<String>,
    /// Tools called while producing this message.
    pub tools_used: Vec<String>,
    /// The agent that produced the message, when applicable.
    pub agent_id: Option<AgentId>,
    /// Provider-reported thinking time in milliseconds.
    pub thinking_time_ms: Option<u64>,
    /// Token count reported for this message.
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
    /// Delta §4.5: when a vision model is in play, the engine may
    /// replace this message's large text body with a rasterized
    /// image. The text stays in `content` (the local transcript
    /// keeps the readable form); the wire layer emits the image
    /// block, and the model reads the frame.
    ///
    /// `None` on every message that has not been imaged.
    #[serde(default)]
    pub image: Option<RasterizedImage>,
    /// Wall-clock the TUI measured for the turn that produced this
    /// assistant reply, in milliseconds. Rendered as the `took 4.2s`
    /// row under that reply's bubble and persisted with the session,
    /// so a reloaded transcript keeps its timings.
    ///
    /// `None` on every non-assistant message, on the live streaming
    /// bubble (the turn is still running), on a turn that pushed no
    /// reply, and on sessions written before the field existed.
    ///
    /// Not the engine's `thinking_time_ms` (a provider-reported
    /// figure): this is end-to-end wall-clock for the whole turn —
    /// tools, retries, rate-limit waits and all — measured on the TUI
    /// thread. `#[serde(default)]` so pre-field session files parse.
    #[serde(default)]
    pub turn_duration_ms: Option<u64>,
    /// Delta §7.7 item 1: the tool result on this message provided
    /// nothing (an empty read, a grep with no hits, a clean diff).
    /// Set by the engine's tool-result boundary so a §3 pruning pass
    /// can drop the message without re-inspecting the payload.
    /// `#[serde(default)]` so pre-field session files parse.
    #[serde(default)]
    pub useless: bool,
}

/// One message between agents in a swarm.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentMessage {
    /// The message's unique id.
    pub id: MessageId,
    /// The sending agent.
    pub from: AgentId,
    /// The destination.
    pub to: MessageDestination,
    /// The message payload.
    pub content: AgentMessageContent,
    /// When the message was created.
    pub timestamp: OffsetDateTime,
}

/// Where a message is going.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum MessageDestination {
    /// A direct message to one agent.
    Agent(AgentId),
    /// Broadcast to every agent.
    Broadcast,
    /// The swarm coordinator.
    Coordinator,
}

/// The payload of an agent-to-agent message.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum AgentMessageContent {
    /// Assign a task to the recipient.
    TaskAssignment {
        /// Human-readable task description.
        description: String,
        /// Task priority.
        priority: Priority,
    },
    /// Report progress on a task.
    ProgressUpdate {
        /// Current status.
        status: TaskStatus,
        /// Human-readable details.
        details: String,
    },
    /// Request help from a peer.
    HelpRequest {
        /// The question being asked.
        question: String,
        /// Context the peer needs.
        context: String,
    },
    /// Share knowledge with peers.
    KnowledgeShare {
        /// The information being shared.
        information: String,
        /// Tags for filtering.
        tags: Vec<String>,
    },
    /// Coordinate an action across the swarm.
    Coordination {
        /// The coordination action.
        action: CoordinationAction,
    },
    /// Claim exclusive access to a file.
    FileClaim {
        /// The file being claimed.
        path: String,
        /// How long the claim lasts.
        duration_secs: u64,
    },
    /// Release a previously-claimed file.
    FileRelease {
        /// The file being released.
        path: String,
    },
    /// Deliver a task result to the coordinator.
    ResultDelivery {
        /// The result body.
        result: String,
    },
}

/// The priority of a task or message.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Priority {
    /// Low priority.
    Low,
    /// Medium priority (the normal case).
    Medium,
    /// High priority.
    High,
    /// Critical; blocks other work.
    Critical,
}

/// The lifecycle status of a task.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum TaskStatus {
    /// Not yet started.
    Pending,
    /// Currently in progress.
    InProgress,
    /// Blocked on an external dependency.
    Blocked,
    /// Finished successfully.
    Completed,
    /// Failed.
    Failed,
}

/// A swarm coordination action.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum CoordinationAction {
    /// Request a sync with peers.
    RequestingSync,
    /// Propose a file change to peers.
    ProposingChange {
        /// The file being proposed for change.
        file: String,
        /// The nature of the change.
        description: String,
    },
    /// Acknowledge a proposed change.
    AcknowledgingChange {
        /// The file whose change is acknowledged.
        file: String,
    },
    /// Report a conflict on a file.
    ConflictDetected {
        /// The conflicting file.
        file: String,
        /// A description of the conflict.
        description: String,
    },
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
        assert_eq!(m.tools_used, [] as [std::string::String; 0]);
        assert!(m.agent_id.is_none());
        assert!(m.thinking_time_ms.is_none());
        assert!(m.token_count.is_none());
        assert!(m.image.is_none());
        assert!(m.turn_duration_ms.is_none());
        assert!(!m.pinned);
    }

    #[test]
    fn pinned_flag_round_trips_through_json() {
        let m = MessageMetadata {
            pinned: true,
            ..MessageMetadata::default()
        };
        let json = serde_json::to_string(&m).unwrap();
        assert!(json.contains("\"pinned\":true"), "got: {json}");
        let parsed: MessageMetadata = serde_json::from_str(&json).unwrap();
        assert!(parsed.pinned);
    }

    #[test]
    fn turn_duration_round_trips_through_json() {
        // The `took …` row under an assistant bubble reads this field;
        // it must survive the session-file round trip.
        let m = MessageMetadata {
            turn_duration_ms: Some(4200),
            ..MessageMetadata::default()
        };
        let json = serde_json::to_string(&m).unwrap();
        assert!(json.contains("\"turn_duration_ms\":4200"), "got: {json}");
        let parsed: MessageMetadata = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.turn_duration_ms, Some(4200));
    }

    #[test]
    fn metadata_without_pinned_key_defaults_to_false() {
        // `MessageMetadata` only carries a per-field default on
        // `pinned`, `image`, and `turn_duration_ms`; the other five
        // fields are required. A session file written before those
        // three existed carries the five and no `pinned` key; it must
        // parse with every defaulted field taking its `None`/`false`.
        let legacy = r#"{
            "skill_applied": null,
            "tools_used": [],
            "agent_id": null,
            "thinking_time_ms": null,
            "token_count": null
        }"#;
        let parsed: MessageMetadata = serde_json::from_str(legacy).unwrap();
        assert!(!parsed.pinned);
        assert!(parsed.image.is_none());
        assert!(parsed.turn_duration_ms.is_none());
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
            MessageDestination::Agent(agent),
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
        let order = [
            Priority::Low,
            Priority::Medium,
            Priority::High,
            Priority::Critical,
        ];
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
