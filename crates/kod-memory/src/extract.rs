//! Extraction pass over a session transcript (D2-B3b, channel 1).
//!
//! One LLM call, one prompt, one JSON array parsed back. The prompt
//! asks for short standalone facts — a preference, a project
//! convention, a decision. The model's response is parsed with the
//! same defensive JSON-slice technique the swarm runner uses for
//! subtask decomposition.
//!
//! # Why one pass at the end and not per turn
//!
//! Per-turn extraction would cost one LLM call per user prompt, and
//! most turns do not produce a durable fact. An end-of-session pass
//! amortises the cost across the session and sees the whole
//! transcript at once.
//!
//! # Failure modes
//!
//! Best-effort: a provider error, a malformed reply, an empty reply,
//! or a timeout all leave the store unchanged. The caller logs a
//! warning and shutdown continues.

use kod_error::Result;
use kod_provider::{GenerationOptions, LlmProvider, ModelRef};
use kod_types::{ChatMessage, MemoryMetadata, MessageRole};
use std::sync::Arc;

/// A candidate fact produced by the extractor.
#[derive(Debug, Clone)]
pub struct ExtractedFact {
    pub content: String,
    pub kind: FactKind,
}

/// The fact kinds the prompt asks for. The kind is stored as a tag
/// so a caller can filter later (`kod memory search --tag decision`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FactKind {
    Preference,
    Decision,
    Pattern,
    Fact,
}

impl FactKind {
    fn as_tag(self) -> &'static str {
        match self {
            FactKind::Preference => "auto-preference",
            FactKind::Decision => "auto-decision",
            FactKind::Pattern => "auto-pattern",
            FactKind::Fact => "auto-fact",
        }
    }
}

/// Build the extraction prompt from a transcript.
pub fn build_prompt(messages: &[ChatMessage], max_entries: usize) -> String {
    let mut transcript = String::new();
    for m in messages {
        let role = match m.role {
            MessageRole::User => "User",
            MessageRole::Assistant => "Assistant",
            MessageRole::Tool => "Tool",
            MessageRole::System => "System",
            MessageRole::Agent(_) => "Agent",
        };
        if matches!(m.role, MessageRole::Tool) {
            continue;
        }
        transcript.push_str(&format!("{role}: {}\n", m.content.trim()));
    }

    format!(
        "You are a memory extractor for a coding assistant. Read the \
         transcript below and extract durable facts worth remembering \
         across sessions. Do NOT extract transient state, tool output, \
         or chit-chat.\n\n\
         What counts:\n\
         - user preferences\n\
         - project conventions\n\
         - decisions\n\
         - recurring patterns\n\n\
         Return a JSON array of at most {max_entries} objects, each \
         {{\"type\": \"preference\"|\"decision\"|\"pattern\"|\"fact\", \
         \"content\": \"one standalone sentence\"}}. No prose, just the \
         array.\n\n\
         Transcript:\n{transcript}\n\
         Array:"
    )
}

/// Parse the model's response into facts. Tolerant of prose around
/// the JSON. Returns an empty vec on any parse failure.
pub fn parse_reply(reply: &str, max_entries: usize) -> Vec<ExtractedFact> {
    let start = match reply.find('[') {
        Some(i) => i,
        None => return Vec::new(),
    };
    let end = match reply.rfind(']') {
        Some(i) if i > start => i,
        _ => return Vec::new(),
    };
    let v: serde_json::Value = match serde_json::from_str(&reply[start..=end]) {
        Ok(v) => v,
        Err(_) => return Vec::new(),
    };
    let arr = match v.as_array() {
        Some(a) => a,
        None => return Vec::new(),
    };
    let mut out = Vec::new();
    for item in arr {
        if out.len() >= max_entries {
            break;
        }
        let content = item
            .get("content")
            .and_then(|c| c.as_str())
            .map(|s| s.trim())
            .filter(|s| !s.is_empty())
            .map(String::from);
        let content = match content {
            Some(c) => c,
            None => continue,
        };
        let kind = item
            .get("type")
            .and_then(|t| t.as_str())
            .map(|t| match t {
                "preference" => FactKind::Preference,
                "decision" => FactKind::Decision,
                "pattern" => FactKind::Pattern,
                _ => FactKind::Fact,
            })
            .unwrap_or(FactKind::Fact);
        if content.len() < 12 {
            continue;
        }
        out.push(ExtractedFact { content, kind });
    }
    out
}

/// Run the extraction pass. Returns the extracted facts; storage is
/// the caller's job.
pub async fn extract(
    provider: Arc<dyn LlmProvider>,
    model_ref: &ModelRef,
    transcript: &[ChatMessage],
    max_entries: usize,
) -> Result<Vec<ExtractedFact>> {
    if transcript.is_empty() {
        return Ok(Vec::new());
    }
    let prompt = build_prompt(transcript, max_entries);
    let opts = GenerationOptions {
        temperature: Some(0.1),
        max_tokens: Some(1024),
        ..Default::default()
    };
    let reply = provider.generate(&prompt, &opts).await?;
    let facts = parse_reply(&reply, max_entries);
    tracing::info!(
        model = %model_ref.display(),
        candidates = facts.len(),
        "memory extraction produced candidate facts"
    );
    Ok(facts)
}

/// Build the metadata for an extracted fact.
pub fn metadata_for(fact: &ExtractedFact, project_key: Option<String>) -> MemoryMetadata {
    MemoryMetadata {
        tags: vec![fact.kind.as_tag().to_string()],
        project_key,
        ..Default::default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use kod_types::{MessageId, MessageRole};
    use time::OffsetDateTime;

    fn msg(role: MessageRole, content: &str) -> ChatMessage {
        ChatMessage::text(MessageId::new(), role, content, OffsetDateTime::now_utc())
    }

    #[test]
    fn prompt_includes_transcript_and_cap() {
        let transcript = vec![
            msg(MessageRole::User, "please remember I prefer dark mode"),
            msg(MessageRole::Assistant, "noted"),
        ];
        let p = build_prompt(&transcript, 5);
        assert!(p.contains("prefer dark mode"));
        assert!(p.contains("at most 5 objects"));
        assert!(p.contains("User:"));
    }

    #[test]
    fn prompt_skips_tool_rows() {
        let transcript = vec![
            msg(MessageRole::User, "hi"),
            msg(MessageRole::Tool, "SHOULD NOT APPEAR"),
        ];
        let p = build_prompt(&transcript, 5);
        assert!(!p.contains("SHOULD NOT APPEAR"));
    }

    #[test]
    fn parse_plain_array() {
        let reply = r#"[{"type":"preference","content":"The user prefers dark mode."}]"#;
        let facts = parse_reply(reply, 10);
        assert_eq!(facts.len(), 1);
        assert_eq!(facts[0].kind, FactKind::Preference);
    }

    #[test]
    fn parse_with_surrounding_prose() {
        let reply = "Sure! Here you go:\n[{\"type\":\"fact\",\"content\":\"The project uses cargo.\"}]\nDone.";
        let facts = parse_reply(reply, 10);
        assert_eq!(facts.len(), 1);
        assert_eq!(facts[0].content, "The project uses cargo.");
    }

    #[test]
    fn parse_respects_cap() {
        let reply = r#"[
            {"type":"fact","content":"aaaaa aaaaa aaaaa."},
            {"type":"fact","content":"bbbbb bbbbb bbbbb."},
            {"type":"fact","content":"ccccc ccccc ccccc."}
        ]"#;
        let facts = parse_reply(reply, 2);
        assert_eq!(facts.len(), 2);
    }

    #[test]
    fn parse_drops_short_content() {
        let reply = r#"[{"type":"fact","content":"short"}]"#;
        assert!(parse_reply(reply, 10).is_empty());
    }

    #[test]
    fn parse_bad_json_is_empty() {
        assert!(parse_reply("no json here", 10).is_empty());
        assert!(parse_reply("[not, valid]", 10).is_empty());
        assert!(parse_reply("", 10).is_empty());
    }

    #[test]
    fn metadata_tags_reflect_kind() {
        let f = ExtractedFact {
            content: "x".to_string(),
            kind: FactKind::Decision,
        };
        let m = metadata_for(&f, Some("proj".to_string()));
        assert_eq!(m.tags, vec!["auto-decision".to_string()]);
        assert_eq!(m.project_key.as_deref(), Some("proj"));
    }
}
