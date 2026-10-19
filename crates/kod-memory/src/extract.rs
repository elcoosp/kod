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
    let mut facts = parse_reply(&reply, max_entries);
    // Delta §12.5: an extracted fact that is itself a placeholder
    // ("ok", "…", "done") is noise — the model's reply is text, and
    // a malformed extraction can echo one. Drop anything that fails
    // `has_substantive_content` so the store only ever receives a
    // real sentence.
    let before = facts.len();
    facts.retain(|f| crate::hygiene::has_substantive_content(&f.content));
    let dropped = before - facts.len();
    tracing::info!(
        model = %model_ref.display(),
        candidates = facts.len(),
        dropped,
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

#[cfg(test)]
mod proptests {
    //! Properties of `parse_reply`.
    //!
    //! The parser is the boundary between the LLM's reply (arbitrary
    //! prose that *may* contain one JSON array) and every downstream
    //! consumer (the memory store, the `/memory` display path). The
    //! contract:
    //!
    //! - never panics on arbitrary input;
    //! - every returned fact has non-empty trimmed content and
    //!   `>= 12` bytes after trim (the documented filter);
    //! - `parse_reply(r, n).len() <= n`;
    //! - prefix-stable across `max_entries`;
    //! - a well-formed reply with `k <= n` entries round-trips with
    //!   `k` facts, in order, kind-preserved.
    //!
    //! Two strategy lessons, both from the L27 control drill on this
    //! suite:
    //!
    //! 1. Content must be `[a-zA-Z0-9]{12,40}` (no whitespace). The
    //!    first draft used `[a-zA-Z0-9 ]{12,40}`, which admits
    //!    leading/trailing spaces; `parse_reply` trims before the
    //!    `>= 12` check and before storing, so a 12-byte raw value
    //!    with a leading space became an 11-byte trimmed value and
    //!    was correctly dropped — then the expected-value comparison
    //!    (against the untrimmed string) failed on a correct parser.
    //!    L23: the checker, not the code.
    //!
    //! 2. `arb_valid_reply` must sometimes emit **short** content
    //!    (`"x"`, `"hello!"`). The first version only generated
    //!    `{12,40}`-byte content, so the `if content.len() < 12`
    //!    filter **never fired**; control B (remove the filter) then
    //!    passed all five properties. The edge was present in the
    //!    type domain but never sampled — L35 in a new shape. Forcing
    //!    short content into the corpus makes the filter observable:
    //!    with the filter removed, `prop_returned_facts_are_well_formed`
    //!    fails on the `>= 12` invariant.
    use super::*;
    use proptest::prelude::*;

    /// Content that is either short (below the 12-byte filter) or
    /// well-formed (12-40 bytes). `prop_oneof!` forces both into
    /// every 2-case window, so a filter regression is caught on the
    /// first short case (L35).
    fn arb_content() -> impl Strategy<Value = String> {
        prop_oneof![
            Just("x".to_string()),
            Just("hello!".to_string()),
            Just("12345678901".to_string()), // 11 bytes, one short
            "[a-zA-Z0-9]{12,40}",
        ]
    }

    /// A reply that is a well-formed JSON array of 1..=3 entries with
    /// arbitrary `arb_content` content and one of the four documented
    /// `type` strings (plus one unrecognized string to exercise the
    /// fallthrough). Short entries are dropped by the filter; the
    /// properties that only see the *output* are unaffected.
    fn arb_valid_reply() -> impl Strategy<Value = String> {
        prop::collection::vec(
            (
                prop::sample::select(vec![
                    "preference", "decision", "pattern", "fact", "unknown",
                ]),
                arb_content(),
            ),
            1..=3,
        )
        .prop_map(|entries| {
            let mut s = String::from("[");
            for (i, (kind, content)) in entries.iter().enumerate() {
                if i > 0 {
                    s.push(',');
                }
                s.push_str(&format!(
                    r#"{{"type":{},"content":{}}}"#,
                    serde_json::to_string(kind).unwrap(),
                    serde_json::to_string(content).unwrap(),
                ));
            }
            s.push(']');
            s
        })
    }

    fn arb_reply() -> impl Strategy<Value = String> {
        prop_oneof![arb_valid_reply(), any::<String>(),]
    }

    proptest! {
        #[test]
        fn prop_parse_reply_never_panics(
            reply in arb_reply(),
            max in any::<usize>(),
        ) {
            let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                let _ = parse_reply(&reply, max);
            }));
            prop_assert!(
                outcome.is_ok(),
                "parse_reply panicked on a {}-byte reply with max={}",
                reply.len(),
                max,
            );
        }

        /// Every returned fact has non-empty trimmed content and at
        /// least 12 bytes after trim. With `arb_content` forcing
        /// short strings into the corpus, this property fires on the
        /// `>= 12` invariant whenever the filter is present; with the
        /// filter removed (control B), a short entry survives and the
        /// property fails.
        #[test]
        fn prop_returned_facts_are_well_formed(
            reply in arb_reply(),
            max in any::<usize>(),
        ) {
            for fact in parse_reply(&reply, max) {
                prop_assert!(
                    !fact.content.trim().is_empty(),
                    "returned an empty-content fact on reply ({} bytes)",
                    reply.len(),
                );
                prop_assert!(
                    fact.content.len() >= 12,
                    "returned a fact with {}-byte content (< 12) on reply ({} bytes)",
                    fact.content.len(),
                    reply.len(),
                );
            }
        }

        #[test]
        fn prop_cap_holds(
            reply in arb_reply(),
            max in any::<usize>(),
        ) {
            let facts = parse_reply(&reply, max);
            prop_assert!(
                facts.len() <= max,
                "parse_reply({}-byte reply, {}) returned {} facts",
                reply.len(), max, facts.len(),
            );
        }

        #[test]
        fn prop_prefix_stable_across_caps(
            reply in arb_reply(),
            n1 in any::<usize>(),
            n2 in any::<usize>(),
        ) {
            let (n_lo, n_hi) = if n1 <= n2 { (n1, n2) } else { (n2, n1) };
            let small = parse_reply(&reply, n_lo);
            let big = parse_reply(&reply, n_hi);
            prop_assert!(
                small.len() <= big.len(),
                "monotonicity violated: caps {}={} facts, {}={} facts on reply ({} bytes)",
                n_lo, small.len(), n_hi, big.len(), reply.len(),
            );
            for (i, (a, b)) in small.iter().zip(big.iter()).enumerate() {
                prop_assert_eq!(
                    &a.content, &b.content,
                    "prefix content mismatch at index {} on reply ({} bytes)",
                    i, reply.len(),
                );
                prop_assert_eq!(
                    &a.kind, &b.kind,
                    "prefix kind mismatch at index {} on reply ({} bytes)",
                    i, reply.len(),
                );
            }
        }

        /// The strongest happy-path claim. Uses its **own** inline
        /// strategy (well-formed content only, `[a-zA-Z0-9]{12,40}`),
        /// deliberately decoupled from `arb_content`'s short branch:
        /// the round-trip claim is about well-formed inputs, and a
        /// short-content input is by design dropped by the filter.
        #[test]
        fn prop_valid_reply_is_fully_parsed(
            entries in prop::collection::vec(
                (
                    prop::sample::select(vec![
                        ("preference", FactKind::Preference),
                        ("decision", FactKind::Decision),
                        ("pattern", FactKind::Pattern),
                        ("fact", FactKind::Fact),
                    ]),
                    "[a-zA-Z0-9]{12,40}",
                ),
                1..=3,
            ),
        ) {
            let k = entries.len();
            let mut s = String::from("[");
            for (i, ((kind_str, _), content)) in entries.iter().enumerate() {
                if i > 0 {
                    s.push(',');
                }
                s.push_str(&format!(
                    r#"{{"type":{},"content":{}}}"#,
                    serde_json::to_string(kind_str).unwrap(),
                    serde_json::to_string(content).unwrap(),
                ));
            }
            s.push(']');

            let facts = parse_reply(&s, k);
            prop_assert_eq!(
                facts.len(), k,
                "expected {} facts, got {} from reply {:?}",
                k, facts.len(), s,
            );
            for (fact, ((_, expected_kind), expected_content)) in
                facts.iter().zip(entries.iter())
            {
                prop_assert_eq!(
                    &fact.content, expected_content,
                    "content mismatch on reply {:?}", s,
                );
                prop_assert_eq!(
                    &fact.kind, expected_kind,
                    "kind mismatch for content {:?}", fact.content,
                );
            }
        }
    }
}
