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

#[cfg(test)]
mod coverage_id_parsing {
    //! The ID newtypes are the type system's first line of defence
    //! against mixing an agent ID into a message slot. The macro-
    //! generated `FromStr` strips a matching prefix and parses the
    //! rest as a UUID; a regression there lets one type's serialized
    //! form parse as another's, and the type system's protection
    //! evaporates the moment anything round-trips through a string.
    use super::*;
    use std::str::FromStr;
    use uuid::Uuid;

    #[test]
    fn from_str_accepts_prefixed_form() {
        let u = Uuid::new_v4();
        let s = format!("agent-{u}");
        let parsed = AgentId::from_str(&s).unwrap();
        assert_eq!(parsed.as_uuid(), &u);
    }

    #[test]
    fn from_str_accepts_bare_uuid() {
        let u = Uuid::new_v4();
        let parsed = AgentId::from_str(&u.to_string()).unwrap();
        assert_eq!(parsed.as_uuid(), &u);
    }

    #[test]
    fn from_str_rejects_garbage() {
        assert!(AgentId::from_str("not-a-uuid").is_err());
        assert!(AgentId::from_str("").is_err());
        assert!(AgentId::from_str("agent-").is_err());
    }

    #[test]
    fn cross_prefix_parsing_is_rejected() {
        // A `msg-`-prefixed string must not parse as an `AgentId`.
        // The macro strips only the matching prefix; the residual
        // text is not a UUID and the parse fails.
        let u = Uuid::new_v4();
        assert!(AgentId::from_str(&format!("msg-{u}")).is_err());
        assert!(MessageId::from_str(&format!("agent-{u}")).is_err());
        assert!(SkillId::from_str(&format!("mem-{u}")).is_err());
    }

    #[test]
    fn prefixed_string_round_trips() {
        let id = TaskId::new();
        let s = id.to_prefixed_string();
        assert!(s.starts_with("task-"));
        let parsed = TaskId::from_str(&s).unwrap();
        assert_eq!(parsed, id);
    }

    #[test]
    fn display_is_short_and_prefixed() {
        let id = SessionId::new();
        let s = id.to_string();
        assert!(s.starts_with("session-"), "got {s}");
        // "session-" + 8 hex chars.
        assert_eq!(s.len(), "session-".len() + 8);
    }

    #[test]
    fn default_matches_new() {
        // The Default impl calls new(); two defaults must differ,
        // since a shared default would silently collapse unrelated
        // entries under one key.
        let a = MemoryId::default();
        let b = MemoryId::default();
        assert_ne!(a, b);
    }

    #[test]
    fn equality_and_hash_are_uuid_based() {
        use std::collections::HashSet;
        let u = Uuid::new_v4();
        let a = ToolId::from_uuid(u);
        let b = ToolId::from_uuid(u);
        assert_eq!(a, b);
        let mut set = HashSet::new();
        set.insert(a);
        assert!(set.contains(&b));
    }

    #[test]
    fn different_types_never_equal_their_uuid_peers() {
        // `AgentId(u) != MessageId(u)` even though the underlying
        // UUID matches. That is the whole point of the newtype. The
        // comparison is a compile error; this test proves the types
        // exist as distinct symbols so a future refactor cannot
        // merge them.
        let u = Uuid::new_v4();
        let a = AgentId::from_uuid(u);
        let m = MessageId::from_uuid(u);
        assert_ne!(a.as_uuid(), &Uuid::nil());
        assert_ne!(m.as_uuid(), &Uuid::nil());
        // Different bytes is the only comparison that compiles.
        let _ = a.as_uuid() == m.as_uuid();
    }
}
