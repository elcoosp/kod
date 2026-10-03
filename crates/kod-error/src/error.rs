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

    #[error("{}", format_provider_timeout(*timeout_ms))]
    ProviderTimeout { timeout_ms: u64 },

    #[error("Rate limited by provider, retry after {retry_after_secs}s")]
    RateLimited { retry_after_secs: u64 },

    #[error("Server busy, retry after {retry_after_secs}s")]
    ServerBusy { retry_after_secs: u64 },

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

/// Whether a transport-layer error message names a transient failure
/// worth retrying.
///
/// Distinct from [`KodError::is_retryable`], which classifies a typed
/// error. This one takes the raw text a transport library produced —
/// hyper, rustls, reqwest — where a `GOAWAY` frame or a `close_notify`
/// alert arrives as a string, not a code.
///
/// The vocabulary is harvested from real failure logs. A phrase earns
/// a place only if it means "the same request may succeed later":
/// a DNS failure for a typo'd host is *not* here, because retrying it
/// wastes the budget on a permanent error. When in doubt, leave it
/// out; a missed retry costs one failure, a spurious retry costs the
/// whole retry budget on something that will never succeed.
pub fn is_transient_transport_error(msg: &str) -> bool {
    let m = msg.to_lowercase();
    const PATTERNS: &[&str] = &[
        // Rate / overload, in prose.
        "429",
        "rate limit",
        "too many requests",
        "overloaded",
        "temporarily",
        "try again",
        // Server-side, in prose and as a bare code. `"500 "` (trailing
        // space) missed `server error 500: …` (the workspace's own
        // shape) and a bare `"500"` a transport library would emit;
        // `"500"` catches both, matching `"502"` / `"503"` / `"504"`.
        "500",
        "502",
        "503",
        "504",
        "bad gateway",
        "service unavailable",
        "gateway timeout",
        "internal server error",
        "server error 5",
        // Connection lifecycle.
        "connection reset",
        "connection closed",
        "connection refused",
        "broken pipe",
        "unexpected eof",
        "eof occurred",
        // HTTP/2 + TLS teardown. A `GOAWAY` frame during a streaming
        // response is the classic "the server rotated a node mid-flight"
        // case; the request is safe to replay.
        "goaway",
        "close_notify",
        "stream_read_error",
        "transport error",
        "h2 protocol error",
        "http2 error",
        // Timeouts.
        "timeout",
        "timed out",
        "deadline exceeded",
        // Provider-side capacity.
        "capacity",
        "no capacity available",
    ];
    PATTERNS.iter().any(|p| m.contains(p))
}

/// Render the `ProviderTimeout` message. A `0` sentinel (the HTTP
/// 408 case, where no client-side timeout was measured) reads as
/// "server returned 408" rather than the misleading "after 0ms".
fn format_provider_timeout(timeout_ms: u64) -> String {
    if timeout_ms == 0 {
        "Provider timeout (server returned 408)".to_string()
    } else {
        format!("Provider timeout after {timeout_ms}ms")
    }
}

impl KodError {
    /// Construct a `RateLimited` from an HTTP 429 response.
    /// `retry_after` is parsed from the `Retry-After` header, if present.
    ///
    /// The status code and body are not carried: the variant holds only
    /// the wait. A caller that wants to log them does so before
    /// constructing the error (the earlier signature accepted and
    /// discarded them, which was worse than not accepting them).
    pub fn rate_limited(retry_after: Option<std::time::Duration>) -> Self {
        let secs = retry_after.map(|d| d.as_secs()).unwrap_or(30);
        KodError::RateLimited {
            retry_after_secs: secs,
        }
    }

    /// Construct a `ServerBusy` from an HTTP 503 overload response.
    /// Defaults to the tab-bridge overload cooldown (10 min) when the
    /// server sent no parseable hint.
    pub fn server_busy(retry_after: Option<std::time::Duration>) -> Self {
        let secs = retry_after.map(|d| d.as_secs()).unwrap_or(600);
        KodError::ServerBusy {
            retry_after_secs: secs,
        }
    }

    /// True when `body` names a provider overload ("server busy, please
    /// try again later" and neighbours), as opposed to a generic 5xx.
    pub fn is_server_busy_body(body: &str) -> bool {
        let l = body.to_lowercase();
        l.contains("server_busy")
            || l.contains("server-busy")
            || l.contains("server busy")
            || l.contains("please try again later")
            || (l.contains("overloaded") && l.contains("try again"))
            || l.contains("capacity exceeded")
    }

    /// Classify an HTTP error status + body into the closest typed variant.
    pub fn provider_status(status: u16, body: &str) -> Self {
        Self::provider_status_with_hint(status, body, None)
    }

    /// Same as [`KodError::provider_status`], but a parsed retry hint
    /// (from `Retry-After` or a body-text cue) overrides the 30 s
    /// default on 429.
    pub fn provider_status_with_hint(
        status: u16,
        body: &str,
        retry_after: Option<std::time::Duration>,
    ) -> Self {
        let snippet = kod_types::strutil::truncate_chars(body, 300);
        // T5-C9: strip secrets before embedding the body in Display.
        let snippet = {
            use kod_types::redact::Redactor;
            Redactor::default().redact(&snippet).0
        };
        match status {
            401 | 403 => KodError::Provider(format!("auth error {status}: {snippet}")),
            404 => KodError::Provider(format!("not found {status}: {snippet}")),
            408 => KodError::ProviderTimeout { timeout_ms: 0 },
            429 => KodError::RateLimited {
                retry_after_secs: retry_after.map(|d| d.as_secs()).unwrap_or(30),
            },
            503 if Self::is_server_busy_body(&snippet) => KodError::ServerBusy {
                retry_after_secs: retry_after.map(|d| d.as_secs()).unwrap_or(600),
            },
            500..=599 => KodError::Provider(format!("server error {status}: {snippet}")),
            _ => KodError::Provider(format!("http {status}: {snippet}")),
        }
    }

    /// True iff a retry of the same request has a real chance of success.
    /// 401/403/404/422 are permanent — retrying only wastes the user's time.
    pub fn is_retryable(&self) -> bool {
        match self {
            KodError::RateLimited { .. }
            | KodError::ServerBusy { .. }
            | KodError::ProviderTimeout { .. } => true,
            KodError::Provider(msg) => is_transient_transport_error(msg),
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
                | KodError::ServerBusy { .. }
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
        for code in ["500", "502", "503", "504"] {
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
        // The cap is exactly 300 x's (see `truncate_chars(body, 300)`
        // in `provider_status_with_hint`). A loose `< 500` bound would
        // pass if the cap were silently raised to 499.
        let x_run = msg.chars().filter(|c| *c == 'x').count();
        assert_eq!(x_run, 300, "truncation cap must be 300: {msg}");
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
        assert!(
            KodError::ServerBusy {
                retry_after_secs: 600
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
        let e = KodError::rate_limited(Some(std::time::Duration::from_secs(7)));
        assert!(matches!(
            e,
            KodError::RateLimited {
                retry_after_secs: 7
            }
        ));
        let e = KodError::rate_limited(None);
        assert!(matches!(
            e,
            KodError::RateLimited {
                retry_after_secs: 30
            }
        ));
    }

    #[test]
    fn server_busy_uses_retry_after_or_ten_minute_default() {
        let e = KodError::server_busy(Some(std::time::Duration::from_secs(600)));
        assert!(matches!(
            e,
            KodError::ServerBusy {
                retry_after_secs: 600
            }
        ));
        let e = KodError::server_busy(None);
        assert!(matches!(
            e,
            KodError::ServerBusy {
                retry_after_secs: 600
            }
        ));
    }

    #[test]
    fn provider_status_maps_busy_503_to_server_busy() {
        let e = KodError::provider_status_with_hint(
            503,
            "provider reports overload (Server busy, please try again later); wait ~10 minutes",
            Some(std::time::Duration::from_secs(600)),
        );
        assert!(matches!(
            e,
            KodError::ServerBusy {
                retry_after_secs: 600
            }
        ));
        assert!(e.is_retryable());
        assert!(e.is_recoverable());
    }

    #[test]
    fn provider_status_keeps_plain_503_as_provider_error() {
        let e = KodError::provider_status(503, "upstream connect error");
        assert!(matches!(e, KodError::Provider(_)));
    }
}
