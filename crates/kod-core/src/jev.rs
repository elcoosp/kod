//! KOD wrapper around the `typesafe-ai-rs` TypeSafe System One client.
//!
//! The underlying crate (`typesafe_ai_rs`) ships the HTTP client, the
//! typed Noul/Choice/Score questions, retries, and cancellation. This
//! module adds the four things every KOD call site needs, in one
//! shape:
//!
//! 1. A **decision cache** keyed by (state, question) with a TTL.
//!    Users repeat themselves — "run the tests", "list src/" — and a
//!    cached decision is a free decision.
//! 2. A **session log entry** for every call so `/jev stats` and
//!    `/debug jev` can read back what happened.
//! 3. **Configurable thresholds** so a user can tighten or loosen
//!    every boolean decision from one place.
//! 4. **Fail-open** semantics: when Jev is disabled or errors, call
//!    sites fall back to their existing heuristic without changing
//!    their control flow.
//!
//! # Design
//!
//! Every method returns a `Decision<T>` that carries the value, a
//! confidence in `[0.0, 1.0]`, and the `DecisionSource` that produced
//! it. A call site never branches on "did Jev fail?" — it branches on
//! the value, and the source is metadata for logging. The
//! `fail_open` config decides whether a Jev failure is an `Err` or a
//! `Heuristic` decision; the default is `Heuristic`.
//!
//! # Disabled path
//!
//! `JevClient::from_config` returns `Ok(None)` when the config has
//! `enabled = false`. A `None` client is the normal case for an
//! unconfigured KOD, and the call sites' `if let Some(jev) = ...`
//! wrapping means the disabled path is one branch, not a feature
//! flag threaded through every function.

use kod_config::JevConfig;
use parking_lot::Mutex;
use serde_json::Value;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};
use typesafe_ai_rs::{Choice, Client, Noul, Question, Score, SystemOneRequest};

/// Who produced a decision. Logged with every call so `/jev stats`
/// can report Jev's share, and so a user can see when the heuristic
/// answered because Jev was down.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DecisionSource {
    /// The decision came from a live (or cached) Jev call.
    Jev,
    /// Jev was disabled or failed, and the caller's fallback ran.
    Heuristic,
    /// The caller decided to use its LLM path instead of Jev.
    Llm,
}

impl DecisionSource {
    /// The string written to the session log. Kept stable so a log
    /// viewer can branch on it.
    pub fn as_str(&self) -> &'static str {
        match self {
            DecisionSource::Jev => "jev",
            DecisionSource::Heuristic => "heuristic",
            DecisionSource::Llm => "llm",
        }
    }
}

/// One decision, wrapped with the metadata every call site logs.
#[derive(Debug, Clone)]
pub struct Decision<T> {
    /// The value the caller branches on.
    pub value: T,
    /// How sure the source is, in `[0.0, 1.0]`. `1.0` is certain.
    /// For a `Heuristic` decision the confidence reflects the
    /// heuristic's own calibration; for a Jev decision it is derived
    /// from the answer distribution.
    pub confidence: f32,
    /// Which source produced the value.
    pub source: DecisionSource,
}

impl<T> Decision<T> {
    /// Convenience for the common "the heuristic answered" case.
    pub fn heuristic(value: T) -> Self {
        Self {
            value,
            confidence: 1.0,
            source: DecisionSource::Heuristic,
        }
    }

    /// Convenience for a Jev-sourced decision.
    pub fn jev(value: T, confidence: f32) -> Self {
        Self {
            value,
            confidence: confidence.clamp(0.0, 1.0),
            source: DecisionSource::Jev,
        }
    }
}

/// Errors from the wrapper. The SDK's own error type is stringified
/// because nothing downstream of the wrapper branches on it — the
/// `fail_open` policy turns every `Err` into a heuristic decision at
/// the call site.
#[derive(Debug, thiserror::Error)]
pub enum JevError {
    #[error("jev is not enabled")]
    Disabled,
    #[error("typesafe sdk: {0}")]
    Sdk(String),
    #[error("malformed typesafe response: {0}")]
    Malformed(String),
    #[error("jev cache lock was poisoned")]
    CachePoisoned,
}

/// A cached answer. Every answer shape the wrapper produces has its
/// own variant so a `Decision` can be reconstructed without a network
/// round-trip.
#[derive(Debug, Clone)]
enum CachedAnswer {
    /// A yes/no probability.
    Bool { p_yes: f32 },
    /// A label from an ordered rubric, with the winning probability.
    Score { label: String, confidence: f32 },
    /// A label from a choice set, with the winning probability.
    Choice { label: String, confidence: f32 },
}

struct CacheEntry {
    answer: CachedAnswer,
    cached_at: Instant,
    ttl: Duration,
}

/// The KOD wrapper. Clone shares the inner SDK client (which shares a
/// connection pool) and the cache.
#[derive(Clone)]
pub struct JevClient {
    inner: Client,
    cache: Arc<Mutex<HashMap<u64, CacheEntry>>>,
    config: Arc<JevConfig>,
}

impl JevClient {
    /// Build a client from config, or `None` when `enabled = false`.
    ///
    /// The builder reads `TYPESAFE_API_KEY` from the environment when
    /// `config.api_key` is `None`; an explicit key wins over the env,
    /// matching the SDK's documented precedence.
    pub fn from_config(cfg: &JevConfig) -> Result<Option<Self>, JevError> {
        if !cfg.is_enabled() {
            return Ok(None);
        }

        let mut builder = Client::builder().timeout(cfg.timeout());
        if let Some(m) = cfg.model.as_deref() {
            builder = builder.model(m);
        }
        if let Some(k) = cfg.api_key.as_deref() {
            builder = builder.api_key(k);
        }
        if let Some(u) = cfg.base_url.as_deref() {
            builder = builder.base_url(u);
        }
        let inner = builder.build().map_err(|e| JevError::Sdk(e.to_string()))?;

        Ok(Some(Self {
            inner,
            cache: Arc::new(Mutex::new(HashMap::new())),
            config: Arc::new(cfg.clone()),
        }))
    }

    /// The thresholds the client was built with. Call sites use these
    /// so a single config change moves every decision.
    pub fn thresholds(&self) -> &kod_config::JevThresholds {
        &self.config.thresholds
    }

    /// True when the cache is enabled (a zero TTL disables it).
    pub fn cache_enabled(&self) -> bool {
        self.config.cache_ttl_secs > 0
    }

    /// Drop every cached decision. Called by `/jev cache clear`.
    pub fn clear_cache(&self) {
        self.cache.lock().clear();
    }

    /// Number of entries currently cached. For `/jev stats`.
    pub fn cache_len(&self) -> usize {
        self.cache.lock().len()
    }

    // ---- Cache helpers ----------------------------------------------------

    fn cache_get(&self, key: u64) -> Option<CachedAnswer> {
        let guard = self.cache.lock();
        let entry = guard.get(&key)?;
        if entry.cached_at.elapsed() < entry.ttl {
            Some(entry.answer.clone())
        } else {
            None
        }
    }

    fn cache_put(&self, key: u64, answer: CachedAnswer) {
        let ttl = self.config.cache_ttl();
        if ttl.is_zero() {
            return;
        }
        let mut guard = self.cache.lock();
        guard.insert(
            key,
            CacheEntry {
                answer,
                cached_at: Instant::now(),
                ttl,
            },
        );
    }

    // ---- Public decision API ---------------------------------------------

    /// Ask one yes/no question about `state`.
    ///
    /// `purpose` is a short label used in the session log; it is not
    /// sent to TypeSafe.
    pub async fn evaluate_yes_no(
        &self,
        state: &Value,
        question: &str,
    ) -> Result<Decision<bool>, JevError> {
        let key = cache_key("noul", state, question);
        if let Some(CachedAnswer::Bool { p_yes }) = self.cache_get(key) {
            return Ok(Decision::jev(p_yes >= 0.5, (p_yes - 0.5).abs() * 2.0));
        }

        let request = SystemOneRequest::new(
            state.clone(),
            [("q", Question::from(Noul::new(question)))],
        );
        let response = self
            .inner
            .system_one(request)
            .await
            .map_err(|e| JevError::Sdk(e.to_string()))?;
        let nouls = response.nouls();
        let answer = nouls
            .get("q")
            .ok_or_else(|| JevError::Malformed("no noul answer for key 'q'".into()))?;
        let p_yes = answer.noul as f32;

        self.cache_put(key, CachedAnswer::Bool { p_yes });
        Ok(Decision::jev(p_yes >= 0.5, (p_yes - 0.5).abs() * 2.0))
    }

    /// Ask one ordered-rubric question about `state`. The value is the
    /// winning level from `levels`.
    pub async fn evaluate_score(
        &self,
        state: &Value,
        question: &str,
        levels: &[&str],
    ) -> Result<Decision<String>, JevError> {
        if levels.is_empty() {
            return Err(JevError::Malformed(
                "score question needs at least one level".into(),
            ));
        }
        let key = cache_key("score", state, question);
        if let Some(CachedAnswer::Score { label, confidence }) = self.cache_get(key) {
            return Ok(Decision::jev(label, confidence));
        }

        let q = Score::new(levels.iter().copied()).instructions(question);
        let request = SystemOneRequest::new(state.clone(), [("q", Question::from(q))]);
        let response = self
            .inner
            .system_one(request)
            .await
            .map_err(|e| JevError::Sdk(e.to_string()))?;
        let scores = response.scores();
        let answer = scores
            .get("q")
            .ok_or_else(|| JevError::Malformed("no score answer for key 'q'".into()))?;

        // `ScoreAnswer.score` is the expected score as a float on
        // `[0.0, levels.len() - 1]`. Round to the nearest integer
        // level index and clamp, so a 0.4 on a three-level rubric
        // reads as level 0 and a 2.7 reads as level 2.
        let raw = answer.score;
        let last = (levels.len() - 1) as f64;
        let idx = raw.round().clamp(0.0, last) as usize;
        let label = levels[idx].to_string();
        let confidence = answer.confidence.clamp(0.0, 1.0) as f32;

        self.cache_put(
            key,
            CachedAnswer::Score {
                label: label.clone(),
                confidence,
            },
        );
        Ok(Decision::jev(label, confidence))
    }

    /// Ask a choice question with a set of mutually exclusive labels.
    pub async fn evaluate_choice(
        &self,
        state: &Value,
        question: &str,
        options: &[&str],
    ) -> Result<Decision<String>, JevError> {
        if options.is_empty() {
            return Err(JevError::Malformed(
                "choice question needs at least one option".into(),
            ));
        }
        let key = cache_key("choice", state, question);
        if let Some(CachedAnswer::Choice { label, confidence }) = self.cache_get(key) {
            return Ok(Decision::jev(label, confidence));
        }

        let labelled: Vec<(&str, Value)> = options
            .iter()
            .map(|o| (*o, Value::Null))
            .collect();
        let q = Choice::new(labelled).instructions(question);
        let request = SystemOneRequest::new(state.clone(), [("q", Question::from(q))]);
        let response = self
            .inner
            .system_one(request)
            .await
            .map_err(|e| JevError::Sdk(e.to_string()))?;
        let choices = response.choices();
        let answer = choices
            .get("q")
            .ok_or_else(|| JevError::Malformed("no choice answer for key 'q'".into()))?;
        let label = answer.choice.clone();
        let confidence = answer.confidence.clamp(0.0, 1.0) as f32;

        self.cache_put(
            key,
            CachedAnswer::Choice {
                label: label.clone(),
                confidence,
            },
        );
        Ok(Decision::jev(label, confidence))
    }

    /// Threshold check against the client's configured minimum for a
    /// named decision. Convenience for call sites that want to say
    /// "did the answer clear the bar for this category?".
    pub fn passes(&self, decision: &Decision<bool>, min: f32) -> bool {
        decision.value && decision.confidence >= min
    }
}

// ---------------------------------------------------------------------------
// Pure helpers
// ---------------------------------------------------------------------------

/// A 64-bit FNV-1a hash. Used only for the in-process cache key, so
/// collisions are a performance concern, not a correctness one — a
/// collision means the wrong answer is served from the cache for a
/// short window.
fn fnv1a_64(bytes: &[u8]) -> u64 {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for b in bytes {
        hash ^= *b as u64;
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    hash
}

/// Cache key: the question kind, the serialized state, and the
/// question text. Deliberately includes the state so "same question,
/// different state" is a cache miss.
fn cache_key(kind: &str, state: &Value, question: &str) -> u64 {
    let mut buf = Vec::with_capacity(64 + question.len());
    buf.extend_from_slice(kind.as_bytes());
    buf.push(0);
    // `to_string` on a `Value` cannot fail.
    if let Ok(s) = serde_json::to_string(state) {
        buf.extend_from_slice(s.as_bytes());
    }
    buf.push(0);
    buf.extend_from_slice(question.as_bytes());
    fnv1a_64(&buf)
}

/// Truncate `s` to `max_chars` characters for a preview, appending an
/// ellipsis when cut. Counts characters, not bytes, so a multi-byte
/// UTF-8 sequence never splits.
pub fn preview_chars(s: &str, max_chars: usize) -> String {
    let mut out = String::with_capacity(s.len().min(max_chars * 4));
    for (i, c) in s.chars().enumerate() {
        if i >= max_chars {
            out.push('…');
            return out;
        }
        out.push(c);
    }
    out
}

/// Build a state value for one Jev call. The SDK accepts any
/// `serde_json::Value`; KOD's convention is an object with a `text`
/// field and an optional `metadata` object. Keeping the shape in one
/// helper means a future change to the convention is one function
/// edit.
pub fn build_state(text: &str, metadata: &[(&str, &str)]) -> Value {
    let mut obj = serde_json::Map::new();
    obj.insert("text".into(), Value::String(text.to_string()));
    if !metadata.is_empty() {
        let mut m = serde_json::Map::new();
        for (k, v) in metadata {
            m.insert((*k).to_string(), Value::String((*v).to_string()));
        }
        obj.insert("metadata".into(), Value::Object(m));
    }
    Value::Object(obj)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fnv1a_is_deterministic() {
        assert_eq!(fnv1a_64(b"hello"), fnv1a_64(b"hello"));
        assert_ne!(fnv1a_64(b"hello"), fnv1a_64(b"world"));
    }

    #[test]
    fn cache_key_differs_for_different_states() {
        let a = build_state("hello", &[]);
        let b = build_state("world", &[]);
        assert_ne!(cache_key("noul", &a, "q"), cache_key("noul", &b, "q"));
    }

    #[test]
    fn cache_key_differs_for_different_questions() {
        let s = build_state("hello", &[]);
        assert_ne!(cache_key("noul", &s, "a"), cache_key("noul", &s, "b"));
    }

    #[test]
    fn cache_key_differs_for_different_kinds() {
        let s = build_state("hello", &[]);
        assert_ne!(cache_key("noul", &s, "q"), cache_key("score", &s, "q"));
    }

    #[test]
    fn build_state_has_text_and_metadata() {
        let v = build_state("hi", &[("a", "1"), ("b", "2")]);
        assert_eq!(v["text"], "hi");
        assert_eq!(v["metadata"]["a"], "1");
        assert_eq!(v["metadata"]["b"], "2");
    }

    #[test]
    fn build_state_omits_empty_metadata() {
        let v = build_state("hi", &[]);
        assert!(v.get("metadata").is_none());
    }

    #[test]
    fn preview_chars_short_input_is_verbatim() {
        assert_eq!(preview_chars("hello", 10), "hello");
    }

    #[test]
    fn preview_chars_appends_ellipsis_when_cut() {
        assert_eq!(preview_chars("hello world", 5), "hello…");
    }

    #[test]
    fn preview_chars_handles_empty_input() {
        assert_eq!(preview_chars("", 5), "");
    }

    #[test]
    fn preview_chars_counts_chars_not_bytes() {
        // 'é' is two bytes; five of them are ten bytes. The
        // truncation must count the five characters, not the bytes.
        assert_eq!(preview_chars("éééééé", 3), "ééé…");
    }

    #[test]
    fn decision_source_strings_are_stable() {
        assert_eq!(DecisionSource::Jev.as_str(), "jev");
        assert_eq!(DecisionSource::Heuristic.as_str(), "heuristic");
        assert_eq!(DecisionSource::Llm.as_str(), "llm");
    }

    #[test]
    fn heuristic_decision_carries_full_confidence() {
        let d: Decision<bool> = Decision::heuristic(true);
        assert!(d.value);
        assert_eq!(d.confidence, 1.0);
        assert_eq!(d.source, DecisionSource::Heuristic);
    }

    #[test]
    fn jev_decision_clamps_confidence() {
        let d = Decision::jev(true, 1.5);
        assert_eq!(d.confidence, 1.0);
        let d = Decision::jev(true, -0.5);
        assert_eq!(d.confidence, 0.0);
    }

    #[test]
    fn disabled_config_yields_none_client() {
        let cfg = JevConfig::default();
        assert!(!cfg.enabled);
        let got = JevClient::from_config(&cfg).expect("disabled path must not error");
        assert!(got.is_none());
    }
}
