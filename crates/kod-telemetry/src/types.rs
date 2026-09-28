//! The record types the engine emits. Pure data, no behaviour —
//! every field a caller might want to set, and nothing else.

/// Where the exporter points, plus the resource attributes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TelemetryConfig {
    /// The collector base URL. `None` means the handle is disabled.
    pub endpoint: Option<String>,
    /// `service.name` resource attribute.
    pub service_name: String,
    /// Extra HTTP headers on every POST.
    pub headers: Vec<(String, String)>,
}

/// One provider call's telemetry, keyed to the GenAI semconv.
///
/// A `Default` record is a no-op: an empty model string, zero tokens.
/// The emitter does not skip a default record — a `record_turn` call
/// is an explicit "this turn completed" signal — but the fields are
/// all `Default`-friendly so a caller can fill in what it has.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct TurnRecord {
    /// Provider name (`anthropic`, `openai`, a custom label).
    pub system: String,
    /// Wire model id.
    pub model: String,
    /// Endpoint name in the kod registry.
    pub endpoint: String,
    pub prompt_tokens: u64,
    pub completion_tokens: u64,
    pub cache_read_tokens: u64,
    pub cache_creation_tokens: u64,
    /// Time to first token, when the provider streamed.
    pub ttft_ms: Option<u64>,
    /// Wall time of the call in milliseconds.
    pub duration_ms: u64,
    /// The provider's stop reason, when it reported one.
    pub stop_reason: Option<String>,
    /// Whether the call failed.
    pub error: bool,
}

/// One tool call's telemetry.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ToolRecord {
    /// Tool name (`read_file`, `execute_command`, …).
    pub name: String,
    /// `ok` / `error` / `skipped` / `aborted` — a string, not an enum,
    /// so a caller can add a status without touching this type.
    pub status: String,
    pub duration_ms: u64,
}
