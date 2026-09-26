//! Result fusion and diversity (borrow from oh-my-pi, delta §12.8).
//!
//! # The problem
//!
//! Hybrid retrieval produces several ranked lists — a vector list, a
//! keyword list, a graph list, a temporal list. Merging them by a
//! weighted score requires the scores to be comparable, which they
//! are not: cosine similarity lives in `[-1, 1]`, BM25 is unbounded,
//! a graph distance is a hop count. Summing them is meaningless.
//!
//! **Reciprocal Rank Fusion** sidesteps the incomparability: it uses
//! only the *rank* of an item in each list, `Σ 1/(k + rank)`, which
//! is comparable across lists by construction. The constant `k`
//! (default 60, per the literature) damps the influence of the very
//! top ranks so one list cannot dominate.
//!
//! # Diversity
//!
//! RRF can return four near-identical results at the top. **Maximal
//! Marginal Relevance** re-ranks to trade relevance against novelty:
//! each pick maximizes `λ·relevance − (1−λ)·max_similarity_to_picked`.
//! The design's λ is 0.7 — relevance still leads, but a duplicate is
//! penalized.
//!
//! # Query intent
//!
//! A query phrased as a question wants different weighting than one
//! phrased as a command. [`QueryIntent`] classifies by simple lexical
//! cues and returns a weight bias a caller can apply to its lists.
//!
//! # What this is NOT
//!
//! * Not the retriever. It fuses lists the retriever produced.
//! * Not a learned model. The cues are lexical; the weights are the
//!   design's numbers.

use std::collections::HashMap;

/// The RRF constant. 60 is the value from the original Cormack et al.
/// paper and the design's number; it damps the top ranks so a single
/// list's #1 does not dominate the fusion.
pub const RRF_K: f64 = 60.0;

/// The MMR trade-off. `0.7` weights relevance over novelty; the
/// design's value.
pub const MMR_LAMBDA: f64 = 0.7;

/// Fuse several ranked lists by Reciprocal Rank Fusion.
///
/// Each input list is ordered best-first. The output is a single
/// ranking, best-first, by `Σ 1/(RRF_K + rank)` across the lists that
/// contain each item. `rank` is 1-based, so the first item in a list
/// contributes `1/(RRF_K + 1)`.
///
/// An item that appears in several lists accumulates from each — that
/// is the point: agreement across retrievers ranks higher than a
/// strong showing in one.
pub fn reciprocal_rank_fusion(lists: &[Vec<String>]) -> Vec<(String, f64)> {
    let mut scores: HashMap<String, f64> = HashMap::new();
    for list in lists {
        for (i, id) in list.iter().enumerate() {
            let rank = (i + 1) as f64;
            *scores.entry(id.clone()).or_insert(0.0) += 1.0 / (RRF_K + rank);
        }
    }
    let mut out: Vec<(String, f64)> = scores.into_iter().collect();
    // Sort by score descending; ties break on id for determinism.
    out.sort_by(|a, b| {
        b.1.partial_cmp(&a.1)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| a.0.cmp(&b.0))
    });
    out
}

/// Re-rank `candidates` by Maximal Marginal Relevance.
///
/// `candidates` is `(id, relevance)` pairs; `similarity(a, b)` returns
/// a similarity in `[0, 1]`. The result is the chosen order.
///
/// MMR picks, at each step, the candidate maximizing
/// `λ·relevance − (1−λ)·max_similarity_to_already_picked`. With
/// `λ = 1.0` it is pure relevance; with `λ = 0.0` it is pure novelty.
///
/// `limit` caps the output; `None` returns every candidate.
pub fn mmr_rerank<F>(
    candidates: &[(String, f64)],
    similarity: F,
    lambda: f64,
    limit: Option<usize>,
) -> Vec<(String, f64)>
where
    F: Fn(&str, &str) -> f64,
{
    let mut remaining: Vec<(String, f64)> = candidates.to_vec();
    let mut picked: Vec<(String, f64)> = Vec::new();
    let cap = limit.unwrap_or(remaining.len());

    while picked.len() < cap && !remaining.is_empty() {
        let mut best_idx = 0usize;
        let mut best_score = f64::NEG_INFINITY;
        for (i, (id, relevance)) in remaining.iter().enumerate() {
            // The maximum similarity to anything already picked.
            let max_sim = picked
                .iter()
                .map(|(pid, _)| similarity(id, pid))
                .fold(0.0_f64, f64::max);
            let score = lambda * relevance - (1.0 - lambda) * max_sim;
            if score > best_score {
                best_score = score;
                best_idx = i;
            }
        }
        picked.push(remaining.remove(best_idx));
    }
    picked
}

/// What a query is asking for, from its lexical shape.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QueryIntent {
    /// A question (`?`, or a leading question word).
    Question,
    /// A command to do something (`add`, `fix`, `write`, …).
    Procedural,
    /// A statement of preference (`prefer`, `always`, `never`).
    Preference,
    /// A request for a past event (`when`, `last time`, `yesterday`).
    Temporal,
    /// No cue matched.
    General,
}

/// Classify a query's intent by lexical cues.
///
/// The cues are the design's: a trailing `?` or a leading question
/// word marks a question; a leading imperative verb marks a
/// procedural ask; preference words mark a preference; temporal words
/// mark a temporal ask. A query with no cue is `General`.
///
/// Order matters: preference beats procedural (a `prefer` query
/// usually also contains a verb), temporal beats general.
pub fn classify_intent(query: &str) -> QueryIntent {
    let lower = query.to_ascii_lowercase();
    let trimmed = lower.trim();
    let first_word = trimmed.split_whitespace().next().unwrap_or("");

    const QUESTION_WORDS: &[&str] = &[
        "what", "why", "how", "when", "where", "who", "which", "does", "do", "is", "are",
    ];
    const PREFERENCE_WORDS: &[&str] =
        &["prefer", "always", "never", "convention", "style"];
    const TEMPORAL_WORDS: &[&str] =
        &["when", "last time", "yesterday", "recently", "earlier", "before"];
    const PROCEDURAL_VERBS: &[&str] = &[
        "add", "fix", "write", "create", "remove", "delete", "refactor", "rename", "move",
        "implement", "update",
    ];

    if trimmed.ends_with('?') || QUESTION_WORDS.contains(&first_word) {
        return QueryIntent::Question;
    }
    if PREFERENCE_WORDS.iter().any(|w| lower.contains(w)) {
        return QueryIntent::Preference;
    }
    if TEMPORAL_WORDS.iter().any(|w| lower.contains(w)) {
        return QueryIntent::Temporal;
    }
    if PROCEDURAL_VERBS.contains(&first_word) {
        return QueryIntent::Procedural;
    }
    QueryIntent::General
}

/// A weight bias for the four retrieval voices, for one intent.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct IntentWeights {
    pub vector: f64,
    pub keyword: f64,
    pub importance: f64,
    pub temporal: f64,
}

/// The design's per-intent biases.
///
/// * A **temporal** query leans on the keyword list (the FTS index
///   holds dates) and away from the vector list (an embedding does
///   not capture "last Tuesday").
/// * A **procedural** query leans on the vector list (a how-to
///   resembles its answer semantically, not lexically).
/// * A **preference** query leans on importance (a preference is a
///   durable fact, and importance captures that).
/// * A **question** and **general** query use the neutral weights.
pub fn intent_weights(intent: QueryIntent) -> IntentWeights {
    match intent {
        QueryIntent::Temporal => IntentWeights {
            vector: 0.6,
            keyword: 1.5,
            importance: 1.0,
            temporal: 1.0,
        },
        QueryIntent::Procedural => IntentWeights {
            vector: 1.3,
            keyword: 1.0,
            importance: 1.0,
            temporal: 1.0,
        },
        QueryIntent::Preference => IntentWeights {
            vector: 1.0,
            keyword: 1.0,
            importance: 1.5,
            temporal: 1.0,
        },
        QueryIntent::Question | QueryIntent::General => IntentWeights {
            vector: 1.0,
            keyword: 1.0,
            importance: 1.0,
            temporal: 1.0,
        },
    }
}

/// The design's confidence formula for a matched intent:
/// `min(0.3 + 0.15 * matches, 1.0)`.
///
/// `matches` is the number of cue hits the classifier found. A query
/// with no cue returns `0.3` — the floor, not zero, because even an
/// uncued query leans slightly toward its classification over a
/// uniform prior.
pub fn intent_confidence(matches: u32) -> f64 {
    (0.3 + 0.15 * matches as f64).min(1.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn list(ids: &[&str]) -> Vec<String> {
        ids.iter().map(|s| s.to_string()).collect()
    }

    // ---- RRF ---------------------------------------------------------

    #[test]
    fn rrf_of_one_list_preserves_order() {
        let fused = reciprocal_rank_fusion(&[list(&["a", "b", "c"])]);
        let ids: Vec<&str> = fused.iter().map(|(id, _)| id.as_str()).collect();
        assert_eq!(ids, vec!["a", "b", "c"]);
    }

    #[test]
    fn rrf_rewards_agreement_across_lists() {
        // "b" is second in both lists; "a" is first in one, absent
        // from the other. Agreement should lift "b" above "a".
        let fused = reciprocal_rank_fusion(&[list(&["a", "b"]), list(&["c", "b"])]);
        let ids: Vec<&str> = fused.iter().map(|(id, _)| id.as_str()).collect();
        assert_eq!(ids[0], "b", "agreement wins: {ids:?}");
    }

    #[test]
    fn rrf_scores_decrease_with_rank() {
        let fused = reciprocal_rank_fusion(&[list(&["a", "b", "c"])]);
        assert!(fused[0].1 > fused[1].1);
        assert!(fused[1].1 > fused[2].1);
    }

    #[test]
    fn rrf_of_no_lists_is_empty() {
        assert!(reciprocal_rank_fusion(&[]).is_empty());
    }

    #[test]
    fn rrf_handles_an_empty_list_among_lists() {
        let fused = reciprocal_rank_fusion(&[list(&["a"]), vec![]]);
        assert_eq!(fused.len(), 1);
        assert_eq!(fused[0].0, "a");
    }

    #[test]
    fn rrf_uses_the_constant() {
        // The first item of a list scores exactly 1/(K+1).
        let fused = reciprocal_rank_fusion(&[list(&["only"])]);
        let expected = 1.0 / (RRF_K + 1.0);
        assert!((fused[0].1 - expected).abs() < 1e-12);
    }

    // ---- MMR ---------------------------------------------------------

    fn jaccard(a: &str, b: &str) -> f64 {
        if a == b {
            1.0
        } else {
            0.0
        }
    }

    #[test]
    fn mmr_with_lambda_one_is_pure_relevance() {
        let c = vec![
            ("a".to_string(), 0.5),
            ("b".to_string(), 0.9),
            ("c".to_string(), 0.7),
        ];
        let out = mmr_rerank(&c, jaccard, 1.0, None);
        let ids: Vec<&str> = out.iter().map(|(id, _)| id.as_str()).collect();
        assert_eq!(ids, vec!["b", "c", "a"]);
    }

    #[test]
    fn mmr_with_lambda_zero_is_pure_novelty() {
        // With λ=0, the first pick is arbitrary (all similarities are
        // 0 before any pick); after picking "a", everything else is
        // novel, so all are returned.
        let c = vec![("a".to_string(), 1.0), ("b".to_string(), 0.9)];
        let out = mmr_rerank(&c, jaccard, 0.0, None);
        assert_eq!(out.len(), 2);
    }

    #[test]
    fn mmr_penalizes_a_duplicate() {
        // "a" and "a2" are identical to the similarity function;
        // "a2" has slightly higher relevance but is a duplicate of
        // "a". With λ=0.7 the distinct "b" should be picked before
        // "a2".
        let sim = |x: &str, y: &str| -> f64 {
            let x = x.trim_end_matches('2');
            let y = y.trim_end_matches('2');
            if x == y { 1.0 } else { 0.0 }
        };
        let c = vec![
            ("a".to_string(), 0.9),
            ("a2".to_string(), 0.95),
            ("b".to_string(), 0.6),
        ];
        let out = mmr_rerank(&c, sim, MMR_LAMBDA, None);
        let ids: Vec<&str> = out.iter().map(|(id, _)| id.as_str()).collect();
        // First pick is the highest relevance, "a2".
        assert_eq!(ids[0], "a2");
        // "b" is novel; "a" is a duplicate of the picked "a2".
        assert_eq!(ids[1], "b", "novelty beats a duplicate: {ids:?}");
    }

    #[test]
    fn mmr_limit_caps_the_output() {
        let c = vec![
            ("a".to_string(), 0.9),
            ("b".to_string(), 0.8),
            ("c".to_string(), 0.7),
        ];
        let out = mmr_rerank(&c, jaccard, MMR_LAMBDA, Some(2));
        assert_eq!(out.len(), 2);
    }

    #[test]
    fn mmr_of_an_empty_candidate_set_is_empty() {
        let out = mmr_rerank(&[], jaccard, MMR_LAMBDA, None);
        assert!(out.is_empty());
    }

    // ---- intent ------------------------------------------------------

    #[test]
    fn a_trailing_question_mark_is_a_question() {
        assert_eq!(classify_intent("what is this?"), QueryIntent::Question);
    }

    #[test]
    fn a_leading_question_word_is_a_question() {
        assert_eq!(classify_intent("how does it work"), QueryIntent::Question);
    }

    #[test]
    fn a_preference_word_marks_a_preference() {
        assert_eq!(classify_intent("i prefer tabs"), QueryIntent::Preference);
    }

    #[test]
    fn a_temporal_word_marks_temporal() {
        assert_eq!(
            classify_intent("when did we last change this"),
            QueryIntent::Question,
            "a leading 'when' is a question first",
        );
        assert_eq!(
            classify_intent("the change from yesterday"),
            QueryIntent::Temporal,
        );
    }

    #[test]
    fn a_leading_imperative_is_procedural() {
        assert_eq!(classify_intent("add a test"), QueryIntent::Procedural);
    }

    #[test]
    fn no_cue_is_general() {
        assert_eq!(classify_intent("the parser module"), QueryIntent::General);
    }

    #[test]
    fn preference_beats_procedural_when_both_appear() {
        // "always add" has a procedural verb *and* a preference word.
        assert_eq!(
            classify_intent("always add a header"),
            QueryIntent::Preference,
        );
    }

    // ---- intent weights ----------------------------------------------

    #[test]
    fn general_weights_are_neutral() {
        let w = intent_weights(QueryIntent::General);
        assert_eq!(w.vector, 1.0);
        assert_eq!(w.keyword, 1.0);
        assert_eq!(w.importance, 1.0);
    }

    #[test]
    fn temporal_leans_on_keyword() {
        let w = intent_weights(QueryIntent::Temporal);
        assert!(w.keyword > w.vector);
    }

    #[test]
    fn procedural_leans_on_vector() {
        let w = intent_weights(QueryIntent::Procedural);
        assert!(w.vector > w.keyword);
    }

    #[test]
    fn preference_leans_on_importance() {
        let w = intent_weights(QueryIntent::Preference);
        assert!(w.importance > w.vector);
    }

    #[test]
    fn confidence_floor_is_point_three() {
        assert!((intent_confidence(0) - 0.3).abs() < 1e-9);
    }

    #[test]
    fn confidence_grows_and_caps() {
        assert!((intent_confidence(1) - 0.45).abs() < 1e-9);
        assert!((intent_confidence(100) - 1.0).abs() < 1e-9);
    }
}
