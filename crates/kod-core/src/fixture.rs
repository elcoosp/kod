//! Deterministic fixture replay (Tier 1.5).
//!
//! A fixture captures one complete streaming session as an ordered
//! list of rounds. Each round carries the request (system, messages,
//! tools, model, options) and the model's response (text, tool
//! calls, usage). Replaying the fixture drives the same engine code
//! path with a `ReplayProvider` that answers rounds in order.
//!
//! Identity is a hash over the *request shape*: `(system_chars,
//! message_count, tool_names, model, options_summary)`. That is
//! enough to detect a drift in what the model sees without depending
//! on exact message contents (which change with every run).

use serde::{Deserialize, Serialize};

/// One round's captured request + response.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RoundFixture {
    pub seq: u32,
    /// The user's input that triggered this round (Tier 1.5). Replay
    /// re-feeds this to a fresh engine so the same shape of request
    /// is rebuilt from scratch. An empty string is allowed for a
    /// round whose input is not reproducible (e.g. a summary call);
    /// replay skips such rounds.
    #[serde(default)]
    pub user_prompt: String,
    /// FNV-1a of the request shape. What replay compares against
    /// today's engine to catch a prompt drift.
    pub request_hash: String,
    /// A short summary of the request, for the diff display.
    pub request_summary: RequestSummary,
    /// What the model returned. Replay uses this verbatim.
    pub response: ResponseFixture,
    /// Capture time.
    pub at_ms: u64,
}

/// The pieces of the request that participate in the hash and in
/// human-readable diffs.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RequestSummary {
    pub system_chars: usize,
    pub message_count: usize,
    pub tool_names: Vec<String>,
    pub model: String,
    pub endpoint: String,
}

impl RequestSummary {
    /// Build a summary from a live `CompletionRequest`. Used by replay
    /// to capture what the current engine actually sent so it can be
    /// diffed against the fixture.
    pub fn from_request(req: &kod_provider::CompletionRequest) -> Self {
        let model = req.model.model.clone();
        let endpoint = req.model.endpoint.clone();
        let mut tool_names: Vec<String> = req.tools.iter().map(|t| t.name.clone()).collect();
        tool_names.sort();
        Self {
            // `system` renders to text; its length is the proxy for
            // the invariant prefix. `messages.len()` counts the
            // conversation slice.
            system_chars: req.system.render_text().len(),
            message_count: req.messages.len(),
            tool_names,
            model,
            endpoint,
        }
    }

    /// FNV-1a hash of the summary fields. Stable across runs; the
    /// diff is meaningful because the components are stable strings
    /// or counts.
    pub fn hash(&self) -> String {
        let mut h: u64 = 0xcbf2_9ce4_8422_2325;
        fn feed(h: &mut u64, b: &[u8]) {
            for x in b {
                *h ^= *x as u64;
                *h = h.wrapping_mul(0x0000_0100_0000_01b3);
            }
        }
        feed(&mut h, self.system_chars.to_string().as_bytes());
        feed(&mut h, &[0u8]);
        feed(&mut h, self.message_count.to_string().as_bytes());
        feed(&mut h, &[0u8]);
        for n in &self.tool_names {
            feed(&mut h, n.as_bytes());
            feed(&mut h, &[0u8]);
        }
        feed(&mut h, self.model.as_bytes());
        feed(&mut h, &[0u8]);
        feed(&mut h, self.endpoint.as_bytes());
        format!("{h:016x}")
    }
}

/// What the model returned for one round.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResponseFixture {
    pub text: String,
    #[serde(default)]
    pub tool_calls: Vec<ToolCallFixture>,
    #[serde(default)]
    pub usage: Option<UsageFixture>,
    /// Tool results that follow this round's tool calls (Tier 1.5).
    /// Ordered the same as `tool_calls`; a caller that replays can
    /// swap these in without re-running the tools. Empty for a
    /// round that made no tool calls.
    #[serde(default)]
    pub tool_results: Vec<ToolResultFixture>,
}

/// One tool result captured from a trace round.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolResultFixture {
    /// The call this result answers; matches `ToolCallFixture.name`.
    pub tool_name: String,
    /// True when the result was an error.
    pub is_error: bool,
    /// The result payload, verbatim. A `Success` carries its
    /// `serde_json::Value`; an `Error` carries the message string.
    pub value: serde_json::Value,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolCallFixture {
    pub id: Option<String>,
    pub name: String,
    pub arguments: serde_json::Value,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UsageFixture {
    pub prompt_tokens: usize,
    pub completion_tokens: usize,
}

/// A complete fixture: one session, many rounds.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Fixture {
    pub name: String,
    pub created_at_ms: u64,
    pub engine_version: String,
    /// Rounds in playback order.
    pub rounds: Vec<RoundFixture>,
}

impl Fixture {
    pub fn new(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            created_at_ms: std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_millis() as u64)
                .unwrap_or(0),
            engine_version: env!("CARGO_PKG_VERSION").to_string(),
            rounds: Vec::new(),
        }
    }

    /// Pretty JSON. Fixtures are read by humans as often as by CI.
    pub fn to_json(&self) -> String {
        serde_json::to_string_pretty(self).unwrap_or_default()
    }

    /// Save to `path` in pretty JSON, creating parents.
    pub fn save_to(&self, path: &std::path::Path) -> std::io::Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(path, self.to_json())
    }

    /// Load from `path`.
    pub fn load_from(path: &std::path::Path) -> std::io::Result<Self> {
        let s = std::fs::read_to_string(path)?;
        Self::from_json(&s).map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))
    }

    /// The default directory for user fixtures.
    /// `<home>/.kod/fixtures/`. Returns `None` when no home dir is
    /// available.
    pub fn fixtures_dir() -> Option<std::path::PathBuf> {
        dirs::home_dir().map(|h| h.join(".kod").join("fixtures"))
    }

    /// The default path for a fixture by name.
    pub fn default_path(name: &str) -> Option<std::path::PathBuf> {
        Self::fixtures_dir().map(|d| d.join(format!("{name}.json")))
    }

    /// Parse a fixture. Errors carry the field path when possible.
    pub fn from_json(s: &str) -> Result<Self, String> {
        serde_json::from_str(s).map_err(|e| e.to_string())
    }

    /// Compare this fixture against a fresh replay. Returns the first
    /// divergent round index, if any.
    pub fn first_divergence(&self, other: &Fixture) -> Option<usize> {
        let n = self.rounds.len().min(other.rounds.len());
        for i in 0..n {
            if self.rounds[i].request_hash != other.rounds[i].request_hash {
                return Some(i);
            }
        }
        if self.rounds.len() != other.rounds.len() {
            return Some(n);
        }
        None
    }
}

/// Diff a specific round between a fixture and a fresh replay.
pub fn diff_rounds(expected: &RoundFixture, actual: &RoundFixture) -> String {
    let mut out = String::new();
    out.push_str(&format!(
        "Round {} request hash: expected {} got {}\n",
        expected.seq, expected.request_hash, actual.request_hash,
    ));
    out.push_str(&format!(
        "  system_chars:  {} → {}\n",
        expected.request_summary.system_chars, actual.request_summary.system_chars,
    ));
    out.push_str(&format!(
        "  message_count: {} → {}\n",
        expected.request_summary.message_count, actual.request_summary.message_count,
    ));
    out.push_str(&format!(
        "  model:         {} → {}\n",
        expected.request_summary.model, actual.request_summary.model,
    ));
    if expected.request_summary.tool_names != actual.request_summary.tool_names {
        out.push_str(&format!(
            "  tools: {} → {}\n",
            expected.request_summary.tool_names.join(", "),
            actual.request_summary.tool_names.join(", "),
        ));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample(name: &str, n: u32) -> Fixture {
        let mut f = Fixture::new(name);
        for seq in 0..n {
            let summary = RequestSummary {
                system_chars: 100 + seq as usize,
                message_count: 1 + seq as usize,
                tool_names: vec!["read_file".into(), "grep".into()],
                model: "test".into(),
                endpoint: "local".into(),
            };
            let hash = summary.hash();
            f.rounds.push(RoundFixture {
                seq,
                user_prompt: format!("prompt {seq}"),
                request_hash: hash,
                request_summary: summary,
                response: ResponseFixture {
                    text: format!("reply {seq}"),
                    tool_calls: vec![],
                    usage: None,
                    tool_results: Vec::new(),
                },
                at_ms: 0,
            });
        }
        f
    }

    #[test]
    fn from_request_captures_model_and_tool_names() {
        // The replay diff relies on the shape captured here. A
        // regression that dropped a field (or failed to sort tool
        // names) would mask a real prompt drift.
        let tool_b = kod_types::ToolDefinition {
            id: kod_types::ToolId::new(),
            name: "b_tool".into(),
            description: "b".into(),
            category: kod_types::ToolCategory::System,
            parameters_schema: serde_json::json!({}),
            permissions: kod_types::ToolPermissions::default(),
            trust_level: kod_types::trust::TrustLevel::default(),
        };
        let tool_a = kod_types::ToolDefinition {
            id: kod_types::ToolId::new(),
            name: "a_tool".into(),
            description: "a".into(),
            category: kod_types::ToolCategory::System,
            parameters_schema: serde_json::json!({}),
            permissions: kod_types::ToolPermissions::default(),
            trust_level: kod_types::trust::TrustLevel::default(),
        };
        let req = kod_provider::CompletionRequest {
            system: kod_provider::SystemPrompt::new().with("hello".to_string(), true),
            messages: vec![kod_types::ChatMessage::text(
                kod_types::MessageId::new(),
                kod_types::MessageRole::User,
                String::from("hi"),
                time::OffsetDateTime::now_utc(),
            )],
            // Deliberately mis-ordered to prove the sort.
            tools: vec![tool_b, tool_a],
            options: kod_provider::GenerationOptions::default(),
            model: kod_provider::ModelRef::new("ep", "m"),
            cache_transcript: false,
            native_compaction_block: None,
            image_frames: Vec::new(),
        };
        let s = RequestSummary::from_request(&req);
        assert_eq!(s.model, "m");
        assert_eq!(s.endpoint, "ep");
        assert_eq!(s.message_count, 1);
        assert!(s.system_chars > 0);
        assert_eq!(s.tool_names, vec!["a_tool", "b_tool"]);
    }

    #[test]
    fn summary_hash_is_stable() {
        let s = RequestSummary {
            system_chars: 42,
            message_count: 7,
            tool_names: vec!["read_file".into()],
            model: "m".into(),
            endpoint: "e".into(),
        };
        assert_eq!(s.hash(), s.hash());
    }

    #[test]
    fn summary_hash_changes_on_field_change() {
        let a = RequestSummary {
            system_chars: 42,
            message_count: 7,
            tool_names: vec!["read_file".into()],
            model: "m".into(),
            endpoint: "e".into(),
        };
        let mut b = a.clone();
        b.system_chars = 43;
        assert_ne!(a.hash(), b.hash());
    }

    #[test]
    fn identical_fixtures_have_no_divergence() {
        let a = sample("x", 3);
        let b = sample("x", 3);
        assert!(a.first_divergence(&b).is_none());
    }

    #[test]
    fn different_round_count_is_a_divergence() {
        let a = sample("x", 3);
        let b = sample("x", 2);
        assert_eq!(a.first_divergence(&b), Some(2));
    }

    #[test]
    fn differing_round_hash_is_a_divergence() {
        let a = sample("x", 3);
        let mut b = sample("x", 3);
        b.rounds[1].request_hash = "deadbeef".into();
        assert_eq!(a.first_divergence(&b), Some(1));
    }

    #[test]
    fn round_trip_through_json() {
        let f = sample("auth", 2);
        let s = f.to_json();
        let back = Fixture::from_json(&s).unwrap();
        assert_eq!(back.name, "auth");
        assert_eq!(back.rounds.len(), 2);
    }

    #[test]
    fn diff_rounds_mentions_all_mismatched_fields() {
        let mut a = sample("x", 1);
        let mut b = sample("x", 1);
        b.rounds[0].request_summary.system_chars = 999;
        b.rounds[0].request_summary.message_count = 999;
        b.rounds[0].request_summary.model = "different".into();
        let diff = diff_rounds(&a.rounds[0], &b.rounds[0]);
        assert!(diff.contains("system_chars"));
        assert!(diff.contains("message_count"));
        assert!(diff.contains("model"));
        // Silence the unused-mut lint.
        a.rounds.clear();
    }

    #[test]
    fn tool_call_fixture_round_trips() {
        let f = Fixture {
            name: "x".into(),
            created_at_ms: 1,
            engine_version: "test".into(),
            rounds: vec![RoundFixture {
                seq: 0,
                user_prompt: "hello".into(),
                request_hash: "abc".into(),
                request_summary: RequestSummary {
                    system_chars: 1,
                    message_count: 1,
                    tool_names: vec![],
                    model: "m".into(),
                    endpoint: "e".into(),
                },
                response: ResponseFixture {
                    text: "hi".into(),
                    tool_calls: vec![ToolCallFixture {
                        id: Some("call_1".into()),
                        name: "read_file".into(),
                        arguments: serde_json::json!({"path": "x"}),
                    }],
                    usage: Some(UsageFixture {
                        prompt_tokens: 10,
                        completion_tokens: 5,
                    }),
                    tool_results: Vec::new(),
                },
                at_ms: 0,
            }],
        };
        let s = f.to_json();
        let back = Fixture::from_json(&s).unwrap();
        assert_eq!(back.rounds[0].response.tool_calls.len(), 1);
        assert_eq!(
            back.rounds[0]
                .response
                .usage
                .as_ref()
                .unwrap()
                .prompt_tokens,
            10
        );
    }
}
