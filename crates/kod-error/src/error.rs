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
        KodError::RateLimited { retry_after_secs: secs }
    }

    /// Classify an HTTP error status + body into the closest typed variant.
    pub fn provider_status(status: u16, body: &str) -> Self {
        let snippet = if body.len() > 300 { &body[..300] } else { body };
        match status {
            401 | 403 => KodError::Provider(format!("auth error {status}: {snippet}")),
            404 => KodError::Provider(format!("not found {status}: {snippet}")),
            408 => KodError::ProviderTimeout { timeout_ms: 0 },
            429 => KodError::RateLimited { retry_after_secs: 30 },
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
