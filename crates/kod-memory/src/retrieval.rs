//! Hybrid retrieval scoring (D2-B2).
//!
//! Three weighted components, combined per entry:
//!
//! 1. **Semantic** (weight 0.6): cosine similarity against the query
//!    embedding. Unavailable when no embedder is configured; the
//!    weight is then redistributed to the keyword component.
//! 2. **Keyword** (weight 0.3): a BM25-lite score. Term-frequency
//!    saturation (`1 + ln(tf)`) times an inverse-document-frequency
//!    computed over the retrieved set. Stopwords and stemming come
//!    from [`crate::stopwords`].
//! 3. **Recency** (weight 0.1): exponential decay with a configurable
//!    half-life. An entry that has not been touched in six months
//!    scores near zero; a fact from yesterday scores near one.

use crate::stopwords;
use kod_types::MemoryEntry;
use time::OffsetDateTime;

/// The three weights and the recency half-life.
#[derive(Debug, Clone, Copy)]
pub struct HybridScorer {
    pub w_semantic: f32,
    pub w_keyword: f32,
    pub w_recency: f32,
    pub half_life_days: f32,
}

impl Default for HybridScorer {
    fn default() -> Self {
        Self {
            w_semantic: 0.6,
            w_keyword: 0.3,
            w_recency: 0.1,
            half_life_days: 14.0,
        }
    }
}

/// Redistribute the semantic weight onto the keyword weight when the
/// cosine is unavailable. The recency weight is untouched: it applies
/// regardless of the other two.
pub fn redistribute(w_semantic: f32, w_keyword: f32) -> (f32, f32) {
    if w_semantic == 0.0 {
        return (0.0, w_keyword);
    }
    (0.0, w_semantic + w_keyword)
}

/// Precomputed query terms for keyword scoring.
pub struct QueryTerms {
    pub tokens: Vec<String>,
    pub df: std::collections::HashMap<String, usize>,
    pub n_docs: usize,
}

impl QueryTerms {
    pub fn build(query: &str, candidates: &[MemoryEntry]) -> Self {
        let tokens = stopwords::tokens(query);
        let mut df = std::collections::HashMap::new();
        for entry in candidates {
            let doc_tokens: std::collections::HashSet<String> =
                stopwords::tokens(&entry.content).into_iter().collect();
            for tok in &tokens {
                if doc_tokens.contains(tok) {
                    *df.entry(tok.clone()).or_insert(0) += 1;
                }
            }
        }
        let n_docs = candidates.len().max(1);
        Self { tokens, df, n_docs }
    }

    /// BM25-lite inverse document frequency for one token.
    pub fn idf(&self, token: &str) -> f32 {
        let df = self.df.get(token).copied().unwrap_or(0) as f32;
        (1.0 + (self.n_docs as f32 - df + 0.5) / (df + 0.5)).ln()
    }
}

impl HybridScorer {
    /// Score one entry against the query terms + optional cosine.
    pub fn score(
        &self,
        query: &QueryTerms,
        entry: &MemoryEntry,
        cosine: Option<f32>,
        now: OffsetDateTime,
    ) -> f32 {
        let (ws, wk) = if cosine.is_some() {
            (self.w_semantic, self.w_keyword)
        } else {
            redistribute(self.w_semantic, self.w_keyword)
        };
        let semantic_component = ws * cosine.unwrap_or(0.0).clamp(0.0, 1.0);
        let keyword_component = wk * self.keyword_bm25_lite(query, entry);
        let recency_component = self.w_recency * recency(entry.timestamp, now, self.half_life_days);
        semantic_component + keyword_component + recency_component
    }

    /// Term-frequency-saturated keyword score for one entry. Value in
    /// `[0, 1]` when the query has tokens; `0` when it has none.
    fn keyword_bm25_lite(&self, query: &QueryTerms, entry: &MemoryEntry) -> f32 {
        if query.tokens.is_empty() {
            return 0.0;
        }
        let doc_tokens = stopwords::tokens(&entry.content);
        if doc_tokens.is_empty() {
            return 0.0;
        }
        let mut tf: std::collections::HashMap<&str, u32> = std::collections::HashMap::new();
        for t in &doc_tokens {
            *tf.entry(t.as_str()).or_insert(0) += 1;
        }
        let mut score = 0.0f32;
        let mut max_possible = 0.0f32;
        for q in &query.tokens {
            let idf = query.idf(q);
            max_possible += idf;
            if let Some(&count) = tf.get(q.as_str()) {
                let count = count as f32;
                // BM25 TF term (with k1 = 1.2), normalised by the
                // same idf so the sum lies in [0, idf * tf_sat].
                let tf_sat = (count * 2.2) / (count + 1.2);
                score += idf * tf_sat;
            }
        }
        if max_possible <= 0.0 {
            return 0.0;
        }
        (score / max_possible).clamp(0.0, 1.0)
    }
}

/// Exponential recency decay: `2^(-Δt / half_life)`.
pub fn recency(ts: OffsetDateTime, now: OffsetDateTime, half_life_days: f32) -> f32 {
    let delta = now - ts;
    let secs = delta.whole_seconds().max(0) as f32;
    if secs == 0.0 {
        return 1.0;
    }
    let half_life_secs = half_life_days * 86_400.0;
    if half_life_secs <= 0.0 {
        return 0.0;
    }
    let exponent = -secs / half_life_secs;
    2f32.powf(exponent)
}

#[cfg(test)]
mod tests {
    use super::*;
    use kod_types::{MemoryId, MemoryType};
    use time::Duration;

    fn entry(content: &str, age_days: i64) -> MemoryEntry {
        MemoryEntry {
            id: MemoryId::new(),
            memory_type: MemoryType::LongTerm,
            content: content.to_string(),
            timestamp: OffsetDateTime::now_utc() - Duration::days(age_days),
            relevance: 1.0,
            metadata: Default::default(),
        }
    }

    #[test]
    fn redistribute_moves_semantic_to_keyword() {
        let (ws, wk) = redistribute(0.6, 0.3);
        assert_eq!(ws, 0.0);
        assert!((wk - 0.9).abs() < 1e-6);
    }

    #[test]
    fn recency_decays_with_half_life() {
        let now = OffsetDateTime::now_utc();
        let fresh = entry("x", 0);
        let half = entry("x", 14);
        let old = entry("x", 60);
        let r_fresh = recency(fresh.timestamp, now, 14.0);
        let r_half = recency(half.timestamp, now, 14.0);
        let r_old = recency(old.timestamp, now, 14.0);
        assert!((r_fresh - 1.0).abs() < 0.01, "fresh ≈ 1, got {r_fresh}");
        assert!((r_half - 0.5).abs() < 0.05, "half ≈ 0.5, got {r_half}");
        assert!(r_old < 0.1, "60d should be < 0.1, got {r_old}");
    }

    #[test]
    fn score_rewards_keyword_overlap() {
        let scorer = HybridScorer::default();
        let now = OffsetDateTime::now_utc();
        let candidates = vec![
            entry("the user prefers dark mode", 0),
            entry("the build uses cargo", 0),
        ];
        let query = QueryTerms::build("dark mode preference", &candidates);
        let s_dark = scorer.score(&query, &candidates[0], None, now);
        let s_cargo = scorer.score(&query, &candidates[1], None, now);
        assert!(
            s_dark > s_cargo,
            "dark-mode entry should beat cargo: {s_dark} vs {s_cargo}"
        );
    }

    #[test]
    fn score_prefers_fresher_entry_all_else_equal() {
        let scorer = HybridScorer::default();
        let now = OffsetDateTime::now_utc();
        let candidates = vec![
            entry("same content here", 0),
            entry("same content here", 60),
        ];
        let query = QueryTerms::build("content here", &candidates);
        let s_fresh = scorer.score(&query, &candidates[0], None, now);
        let s_old = scorer.score(&query, &candidates[1], None, now);
        assert!(
            s_fresh > s_old,
            "fresh should beat old when content is identical: {s_fresh} vs {s_old}"
        );
    }

    #[test]
    fn score_uses_semantic_when_available() {
        let scorer = HybridScorer::default();
        let now = OffsetDateTime::now_utc();
        let candidates = vec![entry("nothing matches keywords", 0)];
        let query = QueryTerms::build("dark mode", &candidates);
        let with_cos = scorer.score(&query, &candidates[0], Some(0.9), now);
        let without = scorer.score(&query, &candidates[0], None, now);
        assert!(with_cos > without, "cosine must raise the score");
        assert!(
            with_cos > 0.5,
            "cosine 0.9 with weight 0.6 → at least 0.54; got {with_cos}"
        );
    }

    #[test]
    fn keyword_score_is_zero_for_empty_query() {
        let scorer = HybridScorer::default();
        let now = OffsetDateTime::now_utc();
        let candidates = vec![entry("anything", 0)];
        let query = QueryTerms::build("the and of", &candidates);
        let s = scorer.score(&query, &candidates[0], None, now);
        assert!(s <= 0.11, "expected near-zero, got {s}");
    }

    #[test]
    fn idf_rewards_rare_tokens() {
        let candidates = vec![
            entry("rare common", 0),
            entry("common only", 0),
            entry("common only", 0),
            entry("common only", 0),
        ];
        let query = QueryTerms::build("rare common", &candidates);
        assert!(
            query.idf("rare") > query.idf("common"),
            "rare idf {} should exceed common idf {}",
            query.idf("rare"),
            query.idf("common")
        );
    }
}

#[cfg(test)]
mod coverage_scoring_composition {
    //! The existing tests probe each scoring component on its own.
    //! The scorer's behaviour at the boundaries — an empty corpus, a
    //! full cosine, a zero cosine supplied by the caller versus
    //! `None`, a zero half-life — is what a retrieval regression
    //! changes first, and it is what these tests pin.
    use super::*;
    use kod_types::{MemoryId, MemoryType};
    use time::Duration;

    fn entry_at(content: &str, age_days: i64) -> MemoryEntry {
        MemoryEntry {
            id: MemoryId::new(),
            memory_type: MemoryType::LongTerm,
            content: content.to_string(),
            timestamp: OffsetDateTime::now_utc() - Duration::days(age_days),
            relevance: 1.0,
            metadata: Default::default(),
        }
    }

    #[test]
    fn empty_corpus_does_not_panic() {
        let scorer = HybridScorer::default();
        let now = OffsetDateTime::now_utc();
        let empty: Vec<MemoryEntry> = Vec::new();
        let q = QueryTerms::build("anything", &empty);
        let entry = entry_at("nothing", 0);
        let s = scorer.score(&q, &entry, None, now);
        // Recency is 1.0 * 0.1 = 0.1; the keyword component is 0
        // because there is nothing to score against. The exact upper
        // bound is loose on purpose: the assertion is that the call
        // returns a finite value in the plausible range.
        assert!((0.0..=0.2).contains(&s), "got {s}");
    }

    #[test]
    fn score_is_bounded_above_by_one() {
        // Every component maxed: cosine 1.0, a perfect keyword match
        // on a single-token document, a fresh timestamp. The weights
        // sum to 1.0, so the score cannot exceed it.
        let scorer = HybridScorer::default();
        let now = OffsetDateTime::now_utc();
        let candidates = vec![entry_at("dark mode", 0)];
        let q = QueryTerms::build("dark mode", &candidates);
        let s = scorer.score(&q, &candidates[0], Some(1.0), now);
        assert!((0.0..=1.0).contains(&s), "score {s} out of range");
    }

    #[test]
    fn zero_cosine_is_not_the_same_as_none() {
        // A caller that supplies `Some(0.0)` is saying "the semantic
        // component exists and scored zero" — the weight is applied
        // and contributes nothing. A caller that supplies `None` is
        // saying "no semantic component today", and the weight is
        // redistributed onto keyword. The keyword-only score is
        // therefore at least as high as the one that supplied an
        // explicit zero.
        let scorer = HybridScorer::default();
        let now = OffsetDateTime::now_utc();
        let candidates = vec![entry_at("the user prefers dark mode", 0)];
        let q = QueryTerms::build("dark mode", &candidates);
        let with_zero = scorer.score(&q, &candidates[0], Some(0.0), now);
        let without = scorer.score(&q, &candidates[0], None, now);
        assert!(
            without >= with_zero,
            "redistribution must not lower the score: {without} vs {with_zero}",
        );
    }

    #[test]
    fn older_entries_have_lower_recency_than_fresh_ones() {
        let now = OffsetDateTime::now_utc();
        let fresh = entry_at("x", 0);
        let old = entry_at("x", 100);
        assert!(recency(fresh.timestamp, now, 14.0) > recency(old.timestamp, now, 14.0));
    }

    #[test]
    fn zero_half_life_yields_zero_recency() {
        // A nonsensical half-life (0.0) must not produce a division
        // by zero or an infinite exponent. The contract is a bounded
        // value: zero.
        let now = OffsetDateTime::now_utc();
        let e = entry_at("x", 1);
        assert_eq!(recency(e.timestamp, now, 0.0), 0.0);
    }
}
