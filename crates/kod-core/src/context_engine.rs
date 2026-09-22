//! Meta-attention context engine (P2).
//!
//! The transcript is scored against the current query and each turn
//! is rendered at one of four fidelity levels. This replaces the
//! recency FIFO: a tool call from thirty turns ago that is exactly
//! the call site the current prompt needs now survives; a "thanks"
//! from five turns ago may not.
//!
//! Two design decisions fix what this module does *not* decide per
//! call:
//!
//! - The **last K turns are always Full** (K = 5). The transcript
//!   cache breakpoint sits on the last block of the last message;
//!   pinning the tail keeps the growing edge stable for a provider
//!   with explicit caching (P0).
//! - **Fidelity is cached per turn.** A turn that scored Omit is not
//!   re-scored on the next call unless the query's term set has
//!   changed substantially (Jaccard < 0.3 with the query that last
//!   scored it). This makes a sequence of calls on the same topic
//!   produce a byte-stable transcript.

use std::collections::HashSet;

use kod_types::ChatMessage;

/// How much of a turn to render.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub enum Fidelity {
    Omit,
    Stub,
    Digest,
    Full,
}

/// The terms a query was scored against.
#[derive(Debug, Clone, Default)]
pub struct Query {
    pub terms: HashSet<String>,
}

impl Query {
    pub fn from_text(text: &str) -> Self {
        let terms = text
            .split_whitespace()
            .map(|t| {
                t.trim_matches(|c: char| !c.is_alphanumeric() && c != '_' && c != '/' && c != '.')
                    .to_lowercase()
            })
            .filter(|t| t.len() >= 3)
            .collect();
        Self { terms }
    }

    pub fn jaccard(&self, other: &Query) -> f64 {
        if self.terms.is_empty() && other.terms.is_empty() {
            return 1.0;
        }
        let intersection = self.terms.intersection(&other.terms).count();
        let union = self.terms.union(&other.terms).count();
        if union == 0 {
            return 1.0;
        }
        intersection as f64 / union as f64
    }
}

/// A turn plus the metadata the scorer uses.
#[derive(Debug, Clone)]
pub struct Chunk<'a> {
    pub message: &'a ChatMessage,
    /// Zero = newest.
    pub age_turns: u32,
    pub pinned: bool,
}

pub trait ChunkScorer: Send + Sync {
    fn score(&self, chunk: &Chunk<'_>, query: &Query) -> Fidelity;
}

#[derive(Debug, Clone, Copy)]
pub struct LexicalScorer {
    pub half_life_turns: f32,
    pub tail_full_turns: u32,
}

impl Default for LexicalScorer {
    fn default() -> Self {
        Self {
            half_life_turns: 64.0,
            tail_full_turns: 5,
        }
    }
}

impl LexicalScorer {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with_tail(mut self, n: u32) -> Self {
        self.tail_full_turns = n;
        self
    }
}

impl ChunkScorer for LexicalScorer {
    fn score(&self, chunk: &Chunk<'_>, query: &Query) -> Fidelity {
        if chunk.pinned {
            return Fidelity::Full;
        }
        if chunk.age_turns < self.tail_full_turns {
            return Fidelity::Full;
        }
        if chunk.message.content.contains("Error:") {
            return Fidelity::Stub;
        }
        let overlap = term_overlap(&chunk.message.content, &query.terms);
        let recency = (-(chunk.age_turns as f32) / self.half_life_turns).exp2();
        let combined = 0.7 * overlap as f32 + 0.3 * recency;
        match combined {
            c if c > 2.5 => Fidelity::Full,
            c if c > 1.2 => Fidelity::Digest,
            c if c > 0.4 => Fidelity::Stub,
            _ => Fidelity::Omit,
        }
    }
}

fn term_overlap(text: &str, terms: &HashSet<String>) -> usize {
    if terms.is_empty() {
        return 0;
    }
    let lower = text.to_lowercase();
    terms.iter().filter(|t| lower.contains(t.as_str())).count()
}

pub fn render_at(message: &ChatMessage, fidelity: Fidelity) -> Option<String> {
    match fidelity {
        Fidelity::Omit => None,
        Fidelity::Full => Some(message.render_text()),
        Fidelity::Digest => Some(digest_line(message)),
        Fidelity::Stub => Some(stub_line(message)),
    }
}

fn digest_line(message: &ChatMessage) -> String {
    let prefix = role_prefix(&message.role);
    let body = first_sentence(&message.content, 200);
    format!("{prefix}{body}")
}

fn stub_line(message: &ChatMessage) -> String {
    let prefix = role_prefix(&message.role);
    let first = message.content.lines().next().unwrap_or("");
    let body = truncate_chars(first, 80);
    format!("{prefix}{body}")
}

fn role_prefix(role: &kod_types::MessageRole) -> &'static str {
    match role {
        kod_types::MessageRole::User => "User: ",
        kod_types::MessageRole::Assistant => "Assistant: ",
        kod_types::MessageRole::System => "System: ",
        kod_types::MessageRole::Tool => "Tool: ",
        kod_types::MessageRole::Agent(_) => "Agent: ",
    }
}

fn first_sentence(text: &str, max: usize) -> String {
    let trimmed = text.trim_start();
    let end = trimmed
        .find(". ")
        .map(|i| i + 1)
        .or_else(|| trimmed.find(".\n").map(|i| i + 1))
        .unwrap_or(trimmed.len());
    let sentence = &trimmed[..end];
    if sentence.len() > max {
        format!("{}…", truncate_chars(sentence, max))
    } else {
        sentence.to_string()
    }
}

fn truncate_chars(s: &str, max: usize) -> String {
    if s.len() <= max {
        return s.to_string();
    }
    let mut end = max;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    s[..end].to_string()
}

/// Per-transcript cache of fidelity decisions. Keyed by the turn's
/// `MessageId` so a re-render does not re-score.
///
/// A turn is re-scored when the query's term set has changed
/// substantially (Jaccard < 0.3 with the query that last scored the
/// turn) or the turn's content changed (its hash differs). Between
/// those changes the cached fidelity holds, which makes a sequence
/// of calls on the same topic produce a byte-stable transcript.
#[derive(Debug, Default, Clone)]
pub struct FidelityCache {
    /// The query that last scored this cache's turns.
    pub last_query: Query,
    /// MessageId -> (content hash, cached fidelity).
    pub entries: std::collections::HashMap<kod_types::MessageId, (u64, Fidelity)>,
}

impl FidelityCache {
    pub fn new() -> Self {
        Self::default()
    }

    /// Look up a turn's cached fidelity. Returns `None` when the
    /// query changed substantially or the content hash differs.
    pub fn lookup(
        &self,
        id: &kod_types::MessageId,
        content_hash: u64,
        query: &Query,
    ) -> Option<Fidelity> {
        if self.last_query.jaccard(query) < 0.3 {
            return None;
        }
        self.entries
            .get(id)
            .filter(|(h, _)| *h == content_hash)
            .map(|(_, f)| *f)
    }

    /// Store a fidelity decision. Called after a miss.
    pub fn insert(
        &mut self,
        id: kod_types::MessageId,
        content_hash: u64,
        fidelity: Fidelity,
    ) {
        self.entries.insert(id, (content_hash, fidelity));
    }

    /// Record that this cache's turns were just scored against
    /// `query`. Called at the end of a scoring pass.
    pub fn commit_query(&mut self, query: Query) {
        self.last_query = query;
    }

    /// Drop entries for ids not in `keep`. Called after a render so a
    /// cleared or compacted transcript does not leak cache entries.
    pub fn retain_ids(&mut self, keep: &std::collections::HashSet<kod_types::MessageId>) {
        self.entries.retain(|id, _| keep.contains(id));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use kod_types::{MessageId, MessageMetadata, MessageRole};
    use time::OffsetDateTime;

    fn msg(role: MessageRole, content: &str) -> ChatMessage {
        ChatMessage {
            id: MessageId::new(),
            role,
            content: content.to_string(),
            timestamp: OffsetDateTime::now_utc(),
            metadata: MessageMetadata::default(),
            tool_calls: Vec::new(),
            tool_call_id: None,
        }
    }

    fn chunk<'a>(m: &'a ChatMessage, age: u32) -> Chunk<'a> {
        Chunk {
            message: m,
            age_turns: age,
            pinned: false,
        }
    }

    #[test]
    fn query_extracts_terms() {
        let q = Query::from_text("parse the unified diff in src/lib.rs");
        assert!(q.terms.contains("parse"));
        assert!(q.terms.contains("unified"));
        assert!(q.terms.contains("diff"));
    }

    #[test]
    fn query_jaccard_is_one_for_identical() {
        let a = Query::from_text("parse diff");
        let b = Query::from_text("parse diff");
        assert!((a.jaccard(&b) - 1.0).abs() < 1e-9);
    }

    #[test]
    fn query_jaccard_is_low_for_disjoint() {
        let a = Query::from_text("parse diff");
        let b = Query::from_text("render html");
        assert!(a.jaccard(&b) < 0.1);
    }

    #[test]
    fn tail_is_always_full() {
        let s = LexicalScorer::default();
        let q = Query::from_text("nothing in common");
        let m = msg(MessageRole::User, "an old turn");
        assert_eq!(s.score(&chunk(&m, 0), &q), Fidelity::Full);
        assert_eq!(s.score(&chunk(&m, 4), &q), Fidelity::Full);
        assert_ne!(s.score(&chunk(&m, 5), &q), Fidelity::Full);
    }

    #[test]
    fn pinned_is_always_full() {
        let s = LexicalScorer::default();
        let q = Query::from_text("nothing");
        let m = msg(MessageRole::User, "a pinned old turn");
        let mut c = chunk(&m, 100);
        c.pinned = true;
        assert_eq!(s.score(&c, &q), Fidelity::Full);
    }

    #[test]
    fn matching_query_outranks_non_matching() {
        let s = LexicalScorer::default();
        let q = Query::from_text("parse unified diff");
        let matching = msg(MessageRole::User, "please parse the unified diff");
        let nonmatching = msg(MessageRole::User, "please write a test");
        let fm = s.score(&chunk(&matching, 10), &q);
        let fnm = s.score(&chunk(&nonmatching, 10), &q);
        assert!(fm > fnm, "matching {fm:?} should outrank {fnm:?}");
    }

    #[test]
    fn error_turn_gets_stub_at_minimum() {
        let s = LexicalScorer::default();
        let q = Query::from_text("nothing");
        let m = msg(MessageRole::Tool, "Error: file not found");
        let f = s.score(&chunk(&m, 20), &q);
        assert!(f >= Fidelity::Stub, "errors must stay legible, got {f:?}");
    }

    #[test]
    fn render_full_is_the_raw_text() {
        let m = msg(MessageRole::User, "hello");
        let r = render_at(&m, Fidelity::Full).unwrap();
        assert!(r.contains("hello"));
    }

    #[test]
    fn render_omit_is_none() {
        let m = msg(MessageRole::User, "hello");
        assert!(render_at(&m, Fidelity::Omit).is_none());
    }

    #[test]
    fn digest_truncates_at_a_char_boundary() {
        let m = msg(MessageRole::User, "ĉ".repeat(300).as_str());
        let r = render_at(&m, Fidelity::Digest).unwrap();
        assert!(r.len() < 500);
        assert!(r.is_char_boundary(r.len()));
    }

    #[test]
    fn stub_takes_only_the_first_line() {
        let m = msg(MessageRole::User, "first line\nsecond line");
        let r = render_at(&m, Fidelity::Stub).unwrap();
        assert!(r.contains("first line"));
        assert!(!r.contains("second line"));
    }

    #[test]
    fn fidelity_ladder_is_strictly_ordered() {
        // The four fidelities form a strict ladder: Omit < Stub < Digest <
        // Full. The PDF's meta-attention design (section 5.1) treats these
        // as comparable levels so a caller can express "at least Stub" or
        // take max() across two candidate scores. Pin the ordering here so
        // a future variant reorder cannot silently invert the comparison.
        assert!(Fidelity::Omit < Fidelity::Stub);
        assert!(Fidelity::Stub < Fidelity::Digest);
        assert!(Fidelity::Digest < Fidelity::Full);
    }
}
