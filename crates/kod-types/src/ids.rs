//! Strongly-typed identifiers to prevent mixing IDs across domains.
//!
//! These types use UUIDs internally but expose type-safe wrappers to prevent
//! accidentally using an agent ID where a message ID is expected.

use serde::{Deserialize, Serialize};
use std::fmt;

macro_rules! define_id {
    ($name:ident, $prefix:literal) => {
        #[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize, PartialOrd, Ord)]
        pub struct $name(pub uuid::Uuid);

        impl $name {
            /// Create a new unique identifier
            pub fn new() -> Self {
                Self(uuid::Uuid::new_v4())
            }

            /// Create from an existing UUID
            pub fn from_uuid(id: uuid::Uuid) -> Self {
                Self(id)
            }

            /// Get the underlying UUID
            pub fn as_uuid(&self) -> &uuid::Uuid {
                &self.0
            }

            /// Get string representation with prefix
            pub fn to_prefixed_string(&self) -> String {
                format!("{}-{}", $prefix, self.0)
            }
        }

        impl Default for $name {
            fn default() -> Self {
                Self::new()
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                write!(f, "{}-{}", $prefix, &self.0.to_string()[..8])
            }
        }

        impl std::str::FromStr for $name {
            type Err = uuid::Error;

            fn from_str(s: &str) -> Result<Self, Self::Err> {
                // Strip prefix if present
                let uuid_str = s.strip_prefix(concat!($prefix, "-")).unwrap_or(s);
                Ok(Self(uuid::Uuid::parse_str(uuid_str)?))
            }
        }
    };
}

define_id!(AgentId, "agent");
define_id!(MessageId, "msg");
define_id!(SkillId, "skill");
define_id!(MemoryId, "mem");
define_id!(ToolId, "tool");
define_id!(TaskId, "task");
define_id!(SessionId, "session");

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_agent_id_creation() {
        let id = AgentId::new();
        assert!(!id.as_uuid().is_nil());
    }

    #[test]
    fn test_id_display() {
        let id = AgentId::new();
        let display = id.to_string();
        assert!(display.starts_with("agent-"));
        assert_eq!(display.len(), 14); // "agent-" + 8 chars
    }

    #[test]
    fn test_id_from_str() {
        let id = AgentId::new();
        let string = id.to_prefixed_string();
        let parsed: AgentId = string.parse().unwrap();
        assert_eq!(id, parsed);
    }

    #[test]
    fn test_different_id_types_not_equal() {
        let agent_id = AgentId::new();
        let message_id = MessageId::new();

        // This should not compile if uncommented:
        // assert_eq!(agent_id, message_id);

        // But their UUIDs can be compared
        assert_ne!(agent_id.as_uuid(), message_id.as_uuid());
    }
}
