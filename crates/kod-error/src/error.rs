//! Comprehensive error types for the KOD system.

use kod_types::{AgentId, SkillId};
use thiserror::Error;

#[derive(Error, Debug)]
pub enum KodError {
    // Core errors
    #[error("Configuration error: {0}")]
    Config(String),

    #[error("Invalid state: {0}")]
    InvalidState(String),

    // LLM Provider errors
    #[error("Provider error: {0}")]
    Provider(String),

    #[error("Provider timeout after {timeout_ms}ms")]
    ProviderTimeout { timeout_ms: u64 },

    #[error("Rate limited by provider, retry after {retry_after_secs}s")]
    RateLimited { retry_after_secs: u64 },

    #[error("Model not found: {model}")]
    ModelNotFound { model: String },

    // Skill errors
    #[error("Skill not found: {skill_id}")]
    SkillNotFound { skill_id: SkillId },

    #[error("Skill parse error in {path}: {reason}")]
    SkillParseError { path: String, reason: String },

    #[error("Skill validation failed: {reason}")]
    SkillValidationFailed { reason: String },

    // Memory errors
    #[error("Memory storage error: {0}")]
    MemoryStorage(String),

    #[error("Memory database error: {0}")]
    MemoryDatabase(String),

    #[error("Embedding generation failed: {0}")]
    EmbeddingGeneration(String),

    // Agent Swarm errors
    #[error("Agent not found: {agent_id}")]
    AgentNotFound { agent_id: AgentId },

    #[error("Agent communication error: {0}")]
    AgentCommunication(String),

    #[error("Swarm coordination failed: {0}")]
    SwarmCoordination(String),

    #[error("File lock timeout for {path}")]
    LockTimeout { path: String },

    #[error("Conflict detected in {path}: {description}")]
    ConflictDetected { path: String, description: String },

    // Tool errors
    #[error("Tool not found: {tool_name}")]
    ToolNotFound { tool_name: String },

    #[error("Tool execution failed: {tool_name}: {reason}")]
    ToolExecution { tool_name: String, reason: String },

    #[error("Permission denied for {action}: {reason}")]
    PermissionDenied { action: String, reason: String },

    #[error("Invalid tool parameters: {reason}")]
    InvalidParameters { reason: String },

    // File system errors
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),

    #[error("File not found: {path}")]
    FileNotFound { path: String },

    #[error("File modified since edit was created: {path}")]
    FileModifiedSinceEdit { path: String },

    // Serialization errors
    #[error("Serialization error: {0}")]
    Serialization(String),

    #[error("Deserialization error: {0}")]
    Deserialization(String),

    // Network errors
    #[error("Network error: {0}")]
    Network(String),

    // Sandbox violations
    #[error("Sandbox violation: {0}")]
    SandboxViolation(String),

    // Internal errors
    #[error("Internal error: {0}")]
    Internal(String),
}

impl KodError {
    /// Construct a `RateLimited` from an HTTP 429 response.
    /// `retry_after` is parsed from the `Retry-After` header, if present.
    pub fn rate_limited(retry_after: Option<std::time::Duration>, status: u16, body: &str) -> Self {
        let secs = retry_after.map(|d| d.as_secs()).unwrap_or(30);
        let _ = (status, body);
        KodError::RateLimited {
            retry_after_secs: secs,
        }
    }

    /// Classify an HTTP error status + body into the closest typed variant.
    pub fn provider_status(status: u16, body: &str) -> Self {
        let snippet = kod_types::strutil::truncate_chars(body, 300);
        match status {
            401 | 403 => KodError::Provider(format!("auth error {status}: {snippet}")),
            404 => KodError::Provider(format!("not found {status}: {snippet}")),
            408 => KodError::ProviderTimeout { timeout_ms: 0 },
            429 => KodError::RateLimited {
                retry_after_secs: 30,
            },
            500..=599 => KodError::Provider(format!("server error {status}: {snippet}")),
            _ => KodError::Provider(format!("http {status}: {snippet}")),
        }
    }

    /// True iff a retry of the same request has a real chance of success.
    /// 401/403/404/422 are permanent — retrying only wastes the user's time.
    pub fn is_retryable(&self) -> bool {
        match self {
            KodError::RateLimited { .. } | KodError::ProviderTimeout { .. } => true,
            KodError::Provider(msg) => {
                let m = msg.to_lowercase();
                m.contains("429")
                    || m.contains("rate limit")
                    || m.contains("timeout")
                    || m.contains("timed out")
                    || m.contains("connection reset")
                    || m.contains("connection closed")
                    || m.contains("temporarily")
                    || m.contains("try again")
                    || m.contains("502")
                    || m.contains("503")
                    || m.contains("504")
                    || m.contains("bad gateway")
                    || m.contains("service unavailable")
                    || m.contains("gateway timeout")
                    || m.contains("server error 5")
            }
            KodError::Network(_) => true,
            _ => false,
        }
    }

    /// Check if this error is recoverable (can retry)
    pub fn is_recoverable(&self) -> bool {
        matches!(
            self,
            KodError::ProviderTimeout { .. }
                | KodError::RateLimited { .. }
                | KodError::Network(_)
                | KodError::LockTimeout { .. }
        )
    }

    /// Get a user-friendly error message
    pub fn user_message(&self) -> String {
        match self {
            KodError::Provider(msg) => format!("The AI provider encountered an issue: {}", msg),
            KodError::SkillNotFound { skill_id } => {
                format!("The skill '{}' could not be found", skill_id)
            }
            KodError::AgentNotFound { agent_id } => {
                format!("The agent '{}' is not available", agent_id)
            }
            KodError::PermissionDenied { action, reason } => {
                format!("Permission denied for '{}': {}", action, reason)
            }
            _ => self.to_string(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_error_display() {
        let error = KodError::SkillNotFound {
            skill_id: kod_types::SkillId::new(),
        };
        assert!(error.to_string().contains("Skill not found"));
    }

    #[test]
    fn test_recoverable_errors() {
        assert!(KodError::ProviderTimeout { timeout_ms: 1000 }.is_recoverable());
        assert!(
            KodError::RateLimited {
                retry_after_secs: 30
            }
            .is_recoverable()
        );
        assert!(!KodError::Internal("test".to_string()).is_recoverable());
    }

    #[test]
    fn test_user_message() {
        let error = KodError::PermissionDenied {
            action: "write_file".to_string(),
            reason: "path not allowed".to_string(),
        };
        let message = error.user_message();
        assert!(message.contains("Permission denied"));
    }
}

#[cfg(test)]
mod coverage_error_classification {
    //! Focused tests for the three predicates that decide how the
    //! engine responds to a provider failure: `is_retryable` (does
    //! the fallback chain try again), `is_recoverable` (does the
    //! caller treat this as a transient), and `provider_status` (how
    //! is a raw HTTP code classified). A regression here is invisible
    //! from the outside — the run either retries too much or too
    //! little — so the string-matching branches are pinned one by one.
    use super::*;
    use kod_types::{AgentId, SkillId};

    #[test]
    fn is_retryable_recognizes_rate_limit_variants() {
        for msg in [
            "429 Too Many Requests",
            "rate limit exceeded",
            "please try again in 30s",
            "temporarily unavailable",
        ] {
            assert!(KodError::Provider(msg.into()).is_retryable(), "{msg}");
        }
    }

    #[test]
    fn is_retryable_recognizes_server_errors() {
        for code in ["502", "503", "504"] {
            let msg = format!("server error {code}: bad gateway");
            assert!(KodError::Provider(msg).is_retryable(), "{code}");
        }
        assert!(KodError::Provider("bad gateway".into()).is_retryable());
        assert!(KodError::Provider("service unavailable".into()).is_retryable());
        assert!(KodError::Provider("gateway timeout".into()).is_retryable());
        assert!(KodError::Provider("server error 5xx".into()).is_retryable());
    }

    #[test]
    fn is_retryable_rejects_client_errors() {
        for msg in [
            "401 unauthorized",
            "403 forbidden",
            "404 not found",
            "422 unprocessable",
            "invalid parameters",
            "malformed request",
        ] {
            assert!(!KodError::Provider(msg.into()).is_retryable(), "{msg}");
        }
    }

    #[test]
    fn is_retryable_covers_timeout_and_network_variants() {
        assert!(KodError::ProviderTimeout { timeout_ms: 100 }.is_retryable());
        assert!(
            KodError::RateLimited {
                retry_after_secs: 5
            }
            .is_retryable()
        );
        assert!(KodError::Network("connection refused".into()).is_retryable());
        assert!(KodError::Provider("timed out".into()).is_retryable());
        assert!(KodError::Provider("connection reset".into()).is_retryable());
        assert!(KodError::Provider("connection closed".into()).is_retryable());
    }

    #[test]
    fn is_retryable_rejects_non_provider_errors() {
        assert!(!KodError::Config("x".into()).is_retryable());
        assert!(!KodError::Internal("x".into()).is_retryable());
        assert!(!KodError::InvalidState("x".into()).is_retryable());
        assert!(
            !KodError::PermissionDenied {
                action: "a".into(),
                reason: "r".into(),
            }
            .is_retryable()
        );
    }

    #[test]
    fn provider_status_classifies_by_http_code() {
        assert!(matches!(
            KodError::provider_status(401, "no"),
            KodError::Provider(ref m) if m.contains("auth error 401")
        ));
        assert!(matches!(
            KodError::provider_status(403, "no"),
            KodError::Provider(ref m) if m.contains("auth error 403")
        ));
        assert!(matches!(
            KodError::provider_status(404, "no"),
            KodError::Provider(ref m) if m.contains("not found 404")
        ));
        assert!(matches!(
            KodError::provider_status(408, "no"),
            KodError::ProviderTimeout { .. }
        ));
        assert!(matches!(
            KodError::provider_status(429, "no"),
            KodError::RateLimited { .. }
        ));
        assert!(matches!(
            KodError::provider_status(503, "no"),
            KodError::Provider(ref m) if m.contains("server error 503")
        ));
        assert!(matches!(
            KodError::provider_status(418, "no"),
            KodError::Provider(ref m) if m.contains("http 418")
        ));
    }

    #[test]
    fn provider_status_truncates_long_bodies() {
        let body = "x".repeat(1000);
        let err = KodError::provider_status(500, &body);
        let msg = err.to_string();
        assert!(
            msg.len() < 500,
            "message not truncated: {} chars",
            msg.len()
        );
        assert!(msg.contains("server error 500"));
    }

    #[test]
    fn is_recoverable_is_a_strict_subset_of_the_typed_variants() {
        assert!(KodError::ProviderTimeout { timeout_ms: 1 }.is_recoverable());
        assert!(
            KodError::RateLimited {
                retry_after_secs: 1
            }
            .is_recoverable()
        );
        assert!(KodError::Network("x".into()).is_recoverable());
        assert!(KodError::LockTimeout { path: "p".into() }.is_recoverable());
        // A Provider error whose text happens to look retryable is
        // not "recoverable" — the two predicates have different
        // shapes on purpose: is_retryable is a text heuristic,
        // is_recoverable is a typed classification. The distinction
        // is what stops a caller from treating "429 rate limit" and
        // "not found 404" the same way.
        assert!(!KodError::Provider("429".into()).is_recoverable());
        assert!(!KodError::Internal("x".into()).is_recoverable());
    }

    #[test]
    fn user_message_rewrites_known_variants() {
        let e = KodError::Provider("boom".into());
        assert!(e.user_message().starts_with("The AI provider"));
        let e = KodError::SkillNotFound {
            skill_id: SkillId::new(),
        };
        assert!(e.user_message().contains("skill"));
        let e = KodError::AgentNotFound {
            agent_id: AgentId::new(),
        };
        assert!(e.user_message().contains("agent"));
        let e = KodError::PermissionDenied {
            action: "x".into(),
            reason: "y".into(),
        };
        assert!(e.user_message().contains("Permission denied"));
        // Anything else falls through to Display verbatim.
        let e = KodError::Internal("idk".into());
        assert_eq!(e.user_message(), e.to_string());
    }

    #[test]
    fn rate_limited_uses_retry_after_or_default() {
        let e = KodError::rate_limited(Some(std::time::Duration::from_secs(7)), 429, "");
        assert!(matches!(
            e,
            KodError::RateLimited {
                retry_after_secs: 7
            }
        ));
        let e = KodError::rate_limited(None, 429, "");
        assert!(matches!(
            e,
            KodError::RateLimited {
                retry_after_secs: 30
            }
        ));
    }
}
