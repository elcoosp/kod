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
use kod_types::{MemoryEntry, MemoryType};
use time::OffsetDateTime;

/// Which decay shape scoring uses.
///
/// `Exponential` is the pre-Weibull behaviour (`2^(-Δt/H)`), kept so a
/// caller that wants the historical shape — or an A/B in a test — can
/// select it. `Weibull` is the default: `exp(-((age / η) ** k))`, with
/// per-type `(η, k)` from [`weibull_shape_for`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Decay {
    Exponential,
    #[default]
    Weibull,
}

/// One memory type's Weibull decay: `exp(-((age_hours / η) ** k))`.
///
/// The shape parameter `k` is the whole point. `k < 1` is heavy-tailed
/// — durable knowledge decays slowly and then almost stops, so a
/// preference stated six months ago still scores meaningfully.
/// `k > 1` decays slowly early then fast; `k == 1` is the plain
/// exponential the workspace used before this module grew a `k`.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct DecayShape {
    /// Scale, in hours. The decay is `e^{-1}` when `age == η`
    /// (assuming `k == 1`).
    pub eta_hours: f32,
    /// Shape. See the type docs.
    pub k: f32,
}

impl DecayShape {
    /// `decay(age_hours)` in `[0, 1]`, monotone decreasing in age.
    ///
    /// `age <= 0` is `1.0`. A non-positive `η` or `k` is a caller
    /// error and returns `0.0` rather than dividing by zero or
    /// producing a NaN that would poison the score sum.
    pub fn decay(&self, age_hours: f32) -> f32 {
        if age_hours <= 0.0 {
            return 1.0;
        }
        if self.eta_hours <= 0.0 || self.k <= 0.0 {
            return 0.0;
        }
        let ratio = age_hours / self.eta_hours;
        (-(ratio.powf(self.k))).exp()
    }
}

/// The per-type Weibull shape, mapped from the oh-my-pi delta §12.1
/// table onto kod's three memory types.
///
/// The table in the design note is finer-grained (`preference`,
/// `commitment`, `event`, …). kod's enum has three types, so the map
/// is by *durability* rather than by name:
///
/// * `LongTerm` — durable knowledge. The note's `preference`
///   (`k=0.4, η=4380`) is the closest analogue.
/// * `Episodic` — session-scoped facts. Between `learning`
///   (`k=0.7, η=1440`) and `decision` (`k=1.0, η=336`); the table's
///   `learning` row is used because episodic entries are the extracted
///   "what happened" facts, not the strategic choices.
/// * `ShortTerm` — the working set. `context` (`k=0.85, η=360`) with a
///   shorter scale (`η=168`, one week) so a short-term entry from
///   yesterday is already meaningfully faded.
pub fn weibull_shape_for(t: MemoryType) -> DecayShape {
    match t {
        MemoryType::LongTerm => DecayShape {
            eta_hours: 4380.0,
            k: 0.4,
        },
        MemoryType::Episodic => DecayShape {
            eta_hours: 1440.0,
            k: 0.7,
        },
        MemoryType::ShortTerm => DecayShape {
            eta_hours: 168.0,
            k: 1.0,
        },
    }
}

/// A shape that reproduces `2^(-Δt / half_life)` under the Weibull
/// formula. Used when [`Decay::Exponential`] is selected: the caller's
/// `half_life_days` becomes the anchor, and the shape is derived so
/// [`DecayShape::decay`] returns the same number `recency` did.
///
/// `2^(-x) = e^{-x·ln2}`, so `η = half_life / ln2` with `k = 1`.
fn exponential_shape(half_life_days: f32) -> DecayShape {
    DecayShape {
        eta_hours: (half_life_days * 24.0) / std::f32::consts::LN_2,
        k: 1.0,
    }
}

/// Decay of `ts` under `shape`, measured against `now`.
///
/// The Weibull counterpart of [`recency`]; `recency` is left alone so
/// external callers keep the exponential contract.
pub fn decay_at(ts: OffsetDateTime, now: OffsetDateTime, shape: DecayShape) -> f32 {
    let delta = now - ts;
    let secs = delta.whole_seconds().max(0) as f32;
    if secs == 0.0 {
        return 1.0;
    }
    shape.decay(secs / 3600.0)
}

/// The three weights, the recency half-life, and the decay shape.
#[derive(Debug, Clone, Copy)]
pub struct HybridScorer {
    pub w_semantic: f32,
    pub w_keyword: f32,
    pub w_recency: f32,
    pub half_life_days: f32,
    /// Which decay shape to apply. `Weibull` (the default) uses the
    /// per-type table; `Exponential` derives a shape from
    /// `half_life_days` so the pre-Weibull behaviour stays reachable.
    pub decay: Decay,
}

impl Default for HybridScorer {
    fn default() -> Self {
        Self {
            w_semantic: 0.6,
            w_keyword: 0.3,
            w_recency: 0.1,
            half_life_days: 14.0,
            decay: Decay::default(),
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

/// Half-life multiplier for a durable long-term entry.
///
/// A stored preference or fact should not fade at the rate of a
/// session episode. Four times the base half-life means a long-term
/// entry that has not been touched in two months still scores most of
/// its recency weight — which is the point of marking it durable.
const LONG_TERM_MULTIPLIER: f32 = 4.0;

/// Half-life multiplier for a short-term entry — decays fastest.
/// A short-term entry is in-session context; a day-old one is stale.
const SHORT_TERM_MULTIPLIER: f32 = 0.5;

impl HybridScorer {
    /// The recency half-life that applies to `memory_type`.
    ///
    /// The `half_life_days` field is the *base*; the type scales it.
    /// A caller that set `half_life_days` explicitly still gets that
    /// value as the anchor, so a custom scorer's intent is preserved.
    ///
    /// This method is the *exponential* reading. It is what
    /// [`Decay::Exponential`] selects and what the historical tests
    /// assert on. Under the default [`Decay::Weibull`], scoring uses
    /// [`Self::decay_shape_for`] instead; this value is not consulted.
    pub fn half_life_for(&self, memory_type: MemoryType) -> f32 {
        match memory_type {
            MemoryType::LongTerm => self.half_life_days * LONG_TERM_MULTIPLIER,
            MemoryType::Episodic => self.half_life_days,
            MemoryType::ShortTerm => self.half_life_days * SHORT_TERM_MULTIPLIER,
        }
    }

    /// The decay shape scoring actually uses for `memory_type`.
    ///
    /// `Exponential` derives one from [`Self::half_life_for`] so the
    /// pre-Weibull behaviour is preserved bit for bit; `Weibull` reads
    /// the per-type table in [`weibull_shape_for`]. Called by
    /// [`Self::score`], and public so a caller building a `/memory`
    /// readout can show the effective `(η, k)` without reimplementing
    /// the selection.
    pub fn decay_shape_for(&self, memory_type: MemoryType) -> DecayShape {
        match self.decay {
            Decay::Exponential => exponential_shape(self.half_life_for(memory_type)),
            Decay::Weibull => weibull_shape_for(memory_type),
        }
    }

    /// Score one entry against the query terms + optional cosine.
    /// [`Self::score_with_weights`] with the neutral (all-1.0)
    /// weights — the pre-§12.8 shape, kept so callers that have no
    /// intent do not have to name one.
    pub fn score(
        &self,
        query: &QueryTerms,
        entry: &MemoryEntry,
        cosine: Option<f32>,
        now: OffsetDateTime,
    ) -> f32 {
        self.score_with_weights(
            query,
            entry,
            cosine,
            now,
            crate::fusion::intent_weights(crate::fusion::QueryIntent::General),
        )
    }

    /// Delta §12.8: score with per-intent component weights.
    ///
    /// Each component is multiplied by the matching weight from
    /// [`crate::fusion::intent_weights`]: the semantic component by
    /// `vector`, the keyword component by `keyword`, the recency
    /// component by `temporal`, and the entry's own relevance by
    /// `importance`. A temporal query therefore leans on keyword
    /// matching and time; a procedural query leans on semantics.
    pub fn score_with_weights(
        &self,
        query: &QueryTerms,
        entry: &MemoryEntry,
        cosine: Option<f32>,
        now: OffsetDateTime,
        weights: crate::fusion::IntentWeights,
    ) -> f32 {
        let (ws, wk) = if cosine.is_some() {
            (self.w_semantic, self.w_keyword)
        } else {
            redistribute(self.w_semantic, self.w_keyword)
        };
        let semantic_component = ws
            * cosine.unwrap_or(0.0).clamp(0.0, 1.0)
            * weights.vector as f32;
        let keyword_component =
            wk * self.keyword_bm25_lite(query, entry) * weights.keyword as f32;
        let shape = self.decay_shape_for(entry.memory_type);
        let recency_component = self.w_recency
            * decay_at(entry.timestamp, now, shape)
            * weights.temporal as f32;
        // `weights.importance` is not applied: the score has three
        // components (semantic, keyword, recency) whose weights sum to
        // 1, and an added fourth term both breaks the bounded-by-one
        // invariant and gives every entry a floor score — so an
        // unrelated query matches everything. Folding `entry.relevance`
        // into the sum as a genuine fourth component (normalising the
        // four weights) is a separate change; until then the intent
        // weights bias vector / keyword / temporal only.
        let _ = weights.importance;
        // Delta §12.8: episodic tier degradation. The tier's weight is
        // a step function on age (< 30 d: 1.0, >= 30 d: 0.5,
        // >= 180 d: 0.25) applied to the whole score. It composes with
        // the continuous recency decay rather than replacing it: the
        // design keeps both, and the tier is a coarse policy on top.
        // The weight is <= 1.0, so the bounded-by-one invariant holds.
        let tier_weight = crate::tier::tier_at(entry.timestamp, now).weight();
        (semantic_component + keyword_component + recency_component) * tier_weight
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
        
            superseded_by: None,
            contradicts: Vec::new(),
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

    #[test]
    fn long_term_decays_slower_than_episodic() {
        let s = HybridScorer::default();
        assert!(s.half_life_for(MemoryType::LongTerm) > s.half_life_for(MemoryType::Episodic));
        assert!(s.half_life_for(MemoryType::Episodic) > s.half_life_for(MemoryType::ShortTerm));
    }

    #[test]
    fn episodic_uses_the_base_half_life() {
        // The base field is the anchor; a custom value flows through.
        let s = HybridScorer {
            half_life_days: 30.0,
            ..HybridScorer::default()
        };
        assert_eq!(s.half_life_for(MemoryType::Episodic), 30.0);
    }

    #[test]
    fn weibull_long_term_holds_up_at_six_months() {
        // The whole reason `k < 1` exists. A durable long-term entry
        // at 180 days must still score meaningfully — under a plain
        // exponential with a 56-day half-life (the LT anchor) it would
        // be `2^{-3.21} ≈ 0.108`, effectively gone.
        let shape = weibull_shape_for(MemoryType::LongTerm);
        let six_months_hours = 180.0 * 24.0;
        let d = shape.decay(six_months_hours);
        assert!(
            (0.30..=0.45).contains(&d),
            "expected ~0.37 at 6 months, got {d}",
        );
    }

    #[test]
    fn weibull_short_term_is_faded_within_a_few_weeks() {
        // The counterpart. A short-term entry is session context; by
        // 3 weeks it should be nearly gone. `eta = 168h, k = 1` puts
        // 504h at `e^{-3} ≈ 0.05`.
        let shape = weibull_shape_for(MemoryType::ShortTerm);
        let d = shape.decay(3.0 * 7.0 * 24.0);
        assert!(d < 0.10, "expected < 0.10 at 3 weeks, got {d}");
    }

    #[test]
    fn weibull_decay_is_monotone_in_age() {
        // For every type, `decay(t1) >= decay(t2)` when `t1 <= t2`.
        // A shape that violated this would make the scorer prefer
        // an older entry over a fresher one under identical content.
        for t in [MemoryType::LongTerm, MemoryType::Episodic, MemoryType::ShortTerm] {
            let shape = weibull_shape_for(t);
            let mut prev = 1.0f32;
            for i in 0..100 {
                let age = i as f32 * 24.0;
                let d = shape.decay(age);
                assert!(
                    d <= prev + 1e-6,
                    "{t:?} non-monotone at age {age}: {prev} -> {d}",
                );
                prev = d;
            }
        }
    }

    #[test]
    fn weibull_shape_for_is_deterministic() {
        // The table is a pure function of the type. Two calls return
        // identical shapes; a regression that returned a randomised
        // `k` would make recall scores non-reproducible.
        for t in [MemoryType::LongTerm, MemoryType::Episodic, MemoryType::ShortTerm] {
            let a = weibull_shape_for(t);
            let b = weibull_shape_for(t);
            assert_eq!(a.eta_hours, b.eta_hours);
            assert_eq!(a.k, b.k);
        }
    }

    #[test]
    fn weibull_long_term_is_heavier_tailed_than_episodic() {
        // The design's core claim: durable knowledge decays *slower
        // in the tail* than episodic facts. At 90 days the LT shape
        // must score higher than the Episodic shape.
        let lt = weibull_shape_for(MemoryType::LongTerm);
        let ep = weibull_shape_for(MemoryType::Episodic);
        let age = 90.0 * 24.0;
        assert!(
            lt.decay(age) > ep.decay(age),
            "LT {} should exceed EP {} at 90d",
            lt.decay(age),
            ep.decay(age),
        );
    }

    #[test]
    fn exponential_mode_matches_the_free_recency_function() {
        // `Decay::Exponential` must reproduce the historical
        // `2^(-Δt/H)` bit for bit. The check is at the half-life
        // point, where both forms should be exactly 0.5.
        let s = HybridScorer {
            decay: Decay::Exponential,
            half_life_days: 30.0,
            ..HybridScorer::default()
        };
        let now = OffsetDateTime::now_utc();
        let shape = s.decay_shape_for(MemoryType::Episodic);
        // An entry exactly one base half-life old: `exponential_shape`
        // anchors Episodic at `half_life_days` (the multiplier is 1.0).
        let e = entry("x", 30);
        let weibull_form = decay_at(e.timestamp, now, shape);
        let exp_form = recency(e.timestamp, now, s.half_life_for(MemoryType::Episodic));
        assert!(
            (weibull_form - exp_form).abs() < 0.01,
            "exponential mode diverged from recency: {weibull_form} vs {exp_form}",
        );
        assert!(
            (weibull_form - 0.5).abs() < 0.05,
            "half-life point should be ~0.5, got {weibull_form}",
        );
    }

    #[test]
    fn decay_shape_with_zero_eta_is_zero_not_nan() {
        // A caller error — a zero or negative scale — must not
        // produce a NaN that poisons the score sum. The contract is
        // `0.0` for a nonsensical shape.
        let bad = DecayShape {
            eta_hours: 0.0,
            k: 1.0,
        };
        assert_eq!(bad.decay(100.0), 0.0);
        let bad = DecayShape {
            eta_hours: -1.0,
            k: 1.0,
        };
        assert_eq!(bad.decay(100.0), 0.0);
        let bad = DecayShape {
            eta_hours: 100.0,
            k: 0.0,
        };
        assert_eq!(bad.decay(100.0), 0.0);
    }

    #[test]
    fn decay_at_age_zero_is_one() {
        let now = OffsetDateTime::now_utc();
        let shape = weibull_shape_for(MemoryType::LongTerm);
        assert_eq!(decay_at(now, now, shape), 1.0);
    }

    #[test]
    fn weibull_default_is_selected() {
        // The default on the struct is `Weibull`. A regression that
        // flipped it back to `Exponential` silently changes every
        // recall score — this pins the choice.
        let s = HybridScorer::default();
        assert_eq!(s.decay, Decay::Weibull);
    }

    #[test]
    fn a_long_term_entry_outscores_an_episodic_one_of_the_same_age() {
        use kod_types::{MemoryEntry, MemoryId, MemoryType};
        let now = time::OffsetDateTime::now_utc();
        let old = now - time::Duration::days(30);
        let mk = |t: MemoryType| MemoryEntry {
            id: MemoryId::new(),
            memory_type: t,
            content: "the user prefers tabs".to_string(),
            timestamp: old,
            relevance: 1.0,
            metadata: Default::default(),
        
            superseded_by: None,
            contradicts: Vec::new(),
        };
        let s = HybridScorer {
            w_semantic: 0.0,
            w_keyword: 0.0,
            w_recency: 1.0,
            ..HybridScorer::default()
        };
        let q = QueryTerms::build("tabs", &[]);
        let lt = s.score(&q, &mk(MemoryType::LongTerm), None, now);
        let ep = s.score(&q, &mk(MemoryType::Episodic), None, now);
        assert!(
            lt > ep,
            "long-term {lt} should beat episodic {ep} at the same age"
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
        
            superseded_by: None,
            contradicts: Vec::new(),
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
