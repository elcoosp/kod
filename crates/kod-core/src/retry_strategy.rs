//! Error taxonomy and strategy retry (Tier 2.2).
//!
//! `is_retryable` classifies transport errors. That is necessary but
//! not sufficient: a refused prompt, a malformed tool call, a
//! context-window overflow, and a hallucinated tool name all need
//! different recovery. This module turns a raw provider error into a
//! `TurnFailure` enum and picks a `RetryStrategy` for it.

use serde::{Deserialize, Serialize};

/// A classified failure. Populated from the raw error string; every
/// provider's errors pass through `classify` before the retry loop
/// decides what to do.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum TurnFailure {
    /// Connection or read timed out.
    TransportTimeout,
    /// DNS, connection refused, TLS failure — the network layer.
    TransportNetwork,
    /// Rate limited. `retry_after_secs` when the server sent a hint.
    TransportRateLimit {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        retry_after_secs: Option<u64>,
    },
    /// The provider refused to answer (safety, policy, "cannot help").
    ProviderRefused { reason: String },
    /// Authentication or authorization failed (401/403). Never
    /// recoverable — the API key is wrong or the endpoint is
    /// forbidden for this account.
    ProviderAuthError { detail: String },
    /// The request exceeded the model's context window.
    ContextWindowExceeded { over_by: Option<usize> },
    /// The model produced JSON that did not parse.
    MalformedJson { snippet: String },
    /// The model called a tool that does not exist.
    HallucinatedTool { name: String },
    /// The provider filtered the response (content policy).
    ContentFiltered { category: String },
    /// The user cancelled the request.
    UserCancelled,
    /// A cost or token cap fired.
    BudgetExhausted,
    /// A policy rule refused the call.
    PolicyDenied { rule: String },
    /// Anything else.
    Unknown { raw: String },
}

impl TurnFailure {
    /// Classify a raw provider error string. The heuristic covers the
    /// common shapes; a provider that needs finer control exposes its
    /// own error type and the caller dispatches on that before
    /// falling through to this.
    pub fn classify(raw: &str) -> Self {
        let l = raw.to_lowercase();
        // Context window first — it can look like a client error.
        if l.contains("context length")
            || l.contains("context window")
            || l.contains("maximum context")
            || l.contains("too many tokens")
            || l.contains("exceeds the maximum")
        {
            return TurnFailure::ContextWindowExceeded { over_by: None };
        }
        if l.contains("rate limit") || l.contains("429") || l.contains("too many requests") {
            return TurnFailure::TransportRateLimit { retry_after_secs: None };
        }
        if l.contains("timed out") || l.contains("timeout") {
            return TurnFailure::TransportTimeout;
        }
        if l.contains("401")
            || l.contains("403")
            || l.contains("unauthorized")
            || l.contains("forbidden")
            || l.contains("invalid api key")
            || l.contains("authentication")
        {
            return TurnFailure::ProviderAuthError {
                detail: raw.to_string(),
            };
        }
        if l.contains("connection") || l.contains("dns") || l.contains("tls") {
            return TurnFailure::TransportNetwork;
        }
        if l.contains("content policy")
            || l.contains("content_filter")
            || l.contains("filtered")
        {
            return TurnFailure::ContentFiltered {
                category: "unknown".to_string(),
            };
        }
        if l.contains("i can't help") || l.contains("i cannot help") || l.contains("i'm unable to") {
            return TurnFailure::ProviderRefused {
                reason: raw.to_string(),
            };
        }
        if l.contains("unknown tool") || l.contains("tool not found") {
            return TurnFailure::HallucinatedTool {
                name: String::new(),
            };
        }
        if l.contains("invalid json") || l.contains("failed to parse json") {
            return TurnFailure::MalformedJson {
                snippet: raw.to_string(),
            };
        }
        if l.contains("budget") {
            return TurnFailure::BudgetExhausted;
        }
        if l.contains("denied") || l.contains("policy") {
            return TurnFailure::PolicyDenied {
                rule: raw.to_string(),
            };
        }
        TurnFailure::Unknown {
            raw: raw.to_string(),
        }
    }

    /// Should the caller retry at all? `false` means the loop should
    /// surface the failure to the user.
    pub fn recoverable(&self) -> bool {
        !matches!(
            self,
            TurnFailure::UserCancelled
                | TurnFailure::BudgetExhausted
                | TurnFailure::PolicyDenied { .. }
                | TurnFailure::ContentFiltered { .. }
                | TurnFailure::ProviderAuthError { .. }
        )
    }

    /// A one-line description for the log and the TUI.
    pub fn summary(&self) -> String {
        match self {
            TurnFailure::TransportTimeout => "transport timeout".to_string(),
            TurnFailure::TransportNetwork => "network error".to_string(),
            TurnFailure::TransportRateLimit { retry_after_secs } => match retry_after_secs {
                Some(s) => format!("rate limited; retry after {s}s"),
                None => "rate limited".to_string(),
            },
            TurnFailure::ProviderRefused { .. } => "provider refused".to_string(),
            TurnFailure::ProviderAuthError { .. } => {
                "provider auth error".to_string()
            }
            TurnFailure::ContextWindowExceeded { .. } => {
                "context window exceeded".to_string()
            }
            TurnFailure::MalformedJson { .. } => "malformed JSON".to_string(),
            TurnFailure::HallucinatedTool { name } => {
                format!("hallucinated tool: {name}")
            }
            TurnFailure::ContentFiltered { category } => {
                format!("content filtered ({category})")
            }
            TurnFailure::UserCancelled => "cancelled".to_string(),
            TurnFailure::BudgetExhausted => "budget exhausted".to_string(),
            TurnFailure::PolicyDenied { rule } => format!("policy denied: {rule}"),
            TurnFailure::Unknown { .. } => "unknown failure".to_string(),
        }
    }
}

/// What action the retry loop takes for a failure.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RetryAction {
    /// Retry the same endpoint with a backoff.
    SameEndpointBackoff,
    /// Retry the same endpoint with a lower temperature.
    SameEndpointLowerTemp,
    /// Retry with a nudge telling the model its JSON was malformed.
    SameEndpointConstrained,
    /// Retry with the tool list re-injected.
    ReinjectTools,
    /// Drop the oldest 25 % of messages and retry.
    ShrinkHistory,
    /// Move to the next endpoint in the chain, same prompt.
    NextEndpoint,
    /// Do not retry; surface the error.
    NoRetry,
}

/// Pick the first strategy that applies from a fixed preference list.
pub fn choose_action(f: &TurnFailure) -> RetryAction {
    match f {
        TurnFailure::TransportTimeout | TurnFailure::TransportNetwork => {
            RetryAction::SameEndpointBackoff
        }
        TurnFailure::TransportRateLimit { .. } => RetryAction::SameEndpointBackoff,
        TurnFailure::ProviderRefused { .. } => RetryAction::SameEndpointLowerTemp,
        TurnFailure::ProviderAuthError { .. } => RetryAction::NoRetry,
        TurnFailure::ContextWindowExceeded { .. } => RetryAction::ShrinkHistory,
        TurnFailure::MalformedJson { .. } => RetryAction::SameEndpointConstrained,
        TurnFailure::HallucinatedTool { .. } => RetryAction::ReinjectTools,
        TurnFailure::ContentFiltered { .. } => RetryAction::NoRetry,
        TurnFailure::UserCancelled => RetryAction::NoRetry,
        TurnFailure::BudgetExhausted => RetryAction::NoRetry,
        TurnFailure::PolicyDenied { .. } => RetryAction::NoRetry,
        TurnFailure::Unknown { .. } => RetryAction::NextEndpoint,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classify_timeout() {
        let f = TurnFailure::classify("request timed out after 30s");
        assert!(matches!(f, TurnFailure::TransportTimeout));
    }

    #[test]
    fn classify_rate_limit() {
        let f = TurnFailure::classify("HTTP 429 too many requests");
        assert!(matches!(f, TurnFailure::TransportRateLimit { .. }));
    }

    #[test]
    fn classify_context_window() {
        let f = TurnFailure::classify(
            "This model's maximum context length is 128000 tokens",
        );
        assert!(matches!(f, TurnFailure::ContextWindowExceeded { .. }));
    }

    #[test]
    fn classify_refusal() {
        let f = TurnFailure::classify("I can't help with that request.");
        assert!(matches!(f, TurnFailure::ProviderRefused { .. }));
    }

    #[test]
    fn classify_unknown_tool() {
        let f = TurnFailure::classify("unknown tool: frobnicate");
        assert!(matches!(f, TurnFailure::HallucinatedTool { .. }));
    }

    #[test]
    fn classify_401_is_auth_error() {
        let f = TurnFailure::classify("HTTP 401 unauthorized");
        assert!(matches!(f, TurnFailure::ProviderAuthError { .. }));
        assert!(!f.recoverable());
        assert_eq!(choose_action(&f), RetryAction::NoRetry);
    }

    #[test]
    fn classify_403_is_auth_error() {
        let f = TurnFailure::classify("403 forbidden: invalid api key");
        assert!(matches!(f, TurnFailure::ProviderAuthError { .. }));
        assert!(!f.recoverable());
    }

    #[test]
    fn classify_falls_through_to_unknown() {
        let f = TurnFailure::classify("something strange happened");
        assert!(matches!(f, TurnFailure::Unknown { .. }));
    }

    #[test]
    fn recoverable_is_false_for_terminal_failures() {
        assert!(!TurnFailure::UserCancelled.recoverable());
        assert!(!TurnFailure::BudgetExhausted.recoverable());
        assert!(!TurnFailure::PolicyDenied { rule: "x".into() }.recoverable());
        assert!(
            !TurnFailure::ContentFiltered {
                category: "x".into()
            }
            .recoverable()
        );
    }

    #[test]
    fn recoverable_is_true_for_transient_failures() {
        assert!(TurnFailure::TransportTimeout.recoverable());
        assert!(TurnFailure::TransportNetwork.recoverable());
        assert!(TurnFailure::TransportRateLimit { retry_after_secs: None }.recoverable());
    }

    #[test]
    fn choose_action_dispatch() {
        assert_eq!(
            choose_action(&TurnFailure::TransportTimeout),
            RetryAction::SameEndpointBackoff
        );
        assert_eq!(
            choose_action(&TurnFailure::ProviderRefused { reason: "x".into() }),
            RetryAction::SameEndpointLowerTemp
        );
        assert_eq!(
            choose_action(&TurnFailure::ContextWindowExceeded { over_by: None }),
            RetryAction::ShrinkHistory
        );
        assert_eq!(
            choose_action(&TurnFailure::UserCancelled),
            RetryAction::NoRetry
        );
    }

    #[test]
    fn summary_is_non_empty_for_every_variant() {
        let variants = [
            TurnFailure::TransportTimeout,
            TurnFailure::TransportNetwork,
            TurnFailure::TransportRateLimit { retry_after_secs: None },
            TurnFailure::ProviderRefused { reason: "x".into() },
            TurnFailure::ProviderAuthError { detail: "x".into() },
            TurnFailure::ContextWindowExceeded { over_by: None },
            TurnFailure::MalformedJson { snippet: "x".into() },
            TurnFailure::HallucinatedTool { name: "x".into() },
            TurnFailure::ContentFiltered { category: "x".into() },
            TurnFailure::UserCancelled,
            TurnFailure::BudgetExhausted,
            TurnFailure::PolicyDenied { rule: "x".into() },
            TurnFailure::Unknown { raw: "x".into() },
        ];
        for v in &variants {
            assert!(!v.summary().is_empty());
        }
    }

    #[test]
    fn round_trip_through_json() {
        let variants = [
            TurnFailure::TransportTimeout,
            TurnFailure::TransportRateLimit { retry_after_secs: Some(30) },
            TurnFailure::ProviderRefused { reason: "x".into() },
            TurnFailure::ProviderAuthError { detail: "401".into() },
            TurnFailure::Unknown { raw: "y".into() },
        ];
        for v in &variants {
            let s = serde_json::to_string(v).unwrap();
            let back: TurnFailure = serde_json::from_str(&s).unwrap();
            assert_eq!(*v, back);
        }
    }
}
