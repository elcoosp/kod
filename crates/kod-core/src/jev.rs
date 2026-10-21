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

/// Abstraction over the Jev client for testing and dependency
/// injection (P5.6 follow-up). `JevClient` implements it; a test can
/// provide a scripted impl that returns deterministic verdicts.
///
/// Every method that existing call sites use is on the trait. The
/// signatures match the inherent methods on `JevClient`, so a call
/// site that reads `engine.jev_client()?` and calls `.evaluate_*`
/// works unchanged whether the engine holds a real client or a
/// test double.
#[async_trait::async_trait]
pub trait JevDecider: Send + Sync {
    fn thresholds(&self) -> &kod_config::JevThresholds;
    fn config(&self) -> &kod_config::JevConfig;
    fn reasoning_timeout(&self) -> std::time::Duration;
    fn clear_cache(&self);
    fn cache_len(&self) -> usize;
    fn cache_enabled(&self) -> bool;

    async fn evaluate_yes_no(
        &self,
        state: &Value,
        question: &str,
    ) -> Result<Decision<bool>, JevError>;

    async fn evaluate_yes_no_batch(
        &self,
        state: &Value,
        questions: &[(String, String)],
    ) -> Result<Vec<(String, f32)>, JevError>;

    async fn evaluate_score(
        &self,
        state: &Value,
        question: &str,
        levels: &[&str],
    ) -> Result<Decision<String>, JevError>;

    async fn evaluate_choice(
        &self,
        state: &Value,
        question: &str,
        options: &[&str],
    ) -> Result<Decision<String>, JevError>;

    /// Rebuild this decider with a new threshold set. Used by
    /// `/jev tune set` to hot-swap the effective thresholds without
    /// restarting the engine.
    fn with_thresholds(
        &self,
        new: kod_config::JevThresholds,
    ) -> Result<std::sync::Arc<dyn JevDecider>, JevError>;
}

#[async_trait::async_trait]
impl JevDecider for JevClient {
    fn thresholds(&self) -> &kod_config::JevThresholds {
        JevClient::thresholds(self)
    }
    fn config(&self) -> &kod_config::JevConfig {
        JevClient::config(self)
    }
    fn reasoning_timeout(&self) -> std::time::Duration {
        JevClient::reasoning_timeout(self)
    }
    fn clear_cache(&self) {
        JevClient::clear_cache(self)
    }
    fn cache_len(&self) -> usize {
        JevClient::cache_len(self)
    }
    fn cache_enabled(&self) -> bool {
        JevClient::cache_enabled(self)
    }
    async fn evaluate_yes_no(
        &self,
        state: &Value,
        question: &str,
    ) -> Result<Decision<bool>, JevError> {
        JevClient::evaluate_yes_no(self, state, question).await
    }
    async fn evaluate_yes_no_batch(
        &self,
        state: &Value,
        questions: &[(String, String)],
    ) -> Result<Vec<(String, f32)>, JevError> {
        JevClient::evaluate_yes_no_batch(self, state, questions).await
    }
    async fn evaluate_score(
        &self,
        state: &Value,
        question: &str,
        levels: &[&str],
    ) -> Result<Decision<String>, JevError> {
        JevClient::evaluate_score(self, state, question, levels).await
    }
    async fn evaluate_choice(
        &self,
        state: &Value,
        question: &str,
        options: &[&str],
    ) -> Result<Decision<String>, JevError> {
        JevClient::evaluate_choice(self, state, question, options).await
    }
    fn with_thresholds(
        &self,
        new: kod_config::JevThresholds,
    ) -> Result<std::sync::Arc<dyn JevDecider>, JevError> {
        let mut cfg = JevClient::config(self).clone();
        cfg.thresholds = new;
        cfg.thresholds.clamp();
        let client = JevClient::from_config(&cfg)?.ok_or(JevError::Disabled)?;
        Ok(std::sync::Arc::new(client))
    }
}

impl JevClient {
    /// The state value as it goes on the wire: with absolute paths
    /// hashed when `[jev] redact_paths = true`. Read on every
    /// `evaluate_*` call so a config reload takes effect without a
    /// rebuild.
    fn wire_state(&self, state: &Value) -> Value {
        redact_state_paths(state, self.config.redact_paths)
    }

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
            let trimmed = u.trim();
            if !trimmed.is_empty() {
                builder = builder.base_url(trimmed);
            }
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

    /// The TUI's reasoning-hide timeout (P1.4).
    pub fn reasoning_timeout(&self) -> std::time::Duration {
        self.config.reasoning_timeout()
    }

    /// The full config the client was built from. Call sites
    /// that read `round_routing` or any non-threshold field go
    /// through here rather than plumbing a second config
    /// handle.
    pub fn config(&self) -> &kod_config::JevConfig {
        &self.config
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
        // F2e-5: bound + sweep — `cache_get` ignored expired entries
        // but never removed them, so the map grew monotonically.
        guard.retain(|_, e| e.cached_at.elapsed() < e.ttl);
        if guard.len() >= 4096 {
            guard.clear();
        }
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
            self.wire_state(state),
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

    /// Ask several yes/no questions about one state in a single
    /// round-trip. Returns `(label, p_yes)` pairs in the order the
    /// labels were supplied. An empty label list returns an empty
    /// vec without a network call.
    pub async fn evaluate_yes_no_batch(
        &self,
        state: &Value,
        questions: &[(String, String)],
    ) -> Result<Vec<(String, f32)>, JevError> {
        if questions.is_empty() {
            return Ok(Vec::new());
        }

        // Cache: one hash of the whole batch, so a repeated batch is
        // one lookup, not N.
        let mut cache_buf = String::from("batch|");
        if let Ok(s) = serde_json::to_string(state) {
            cache_buf.push_str(&s);
        }
        for (k, q) in questions {
            cache_buf.push('|');
            cache_buf.push_str(k);
            cache_buf.push('=');
            cache_buf.push_str(q);
        }
        let cache_hit_key = fnv1a_64(cache_buf.as_bytes());
        // A dedicated cache slot per batch; store the answers as a
        // JSON array under a synthetic "score" entry.
        if let Some(CachedAnswer::Choice { label, .. }) = self.cache_get(cache_hit_key) {
            // We stored the serialized answers in `label`; decode.
            if let Ok(v) = serde_json::from_str::<Vec<(String, f32)>>(&label) {
                return Ok(v);
            }
        }

        let mut pairs: Vec<(String, Question)> = Vec::with_capacity(questions.len());
        for (k, q) in questions {
            pairs.push((k.clone(), Question::from(Noul::new(q.clone()))));
        }
        let request = SystemOneRequest::new(self.wire_state(state), pairs);
        let response = self
            .inner
            .system_one(request)
            .await
            .map_err(|e| JevError::Sdk(e.to_string()))?;
        let nouls = response.nouls();
        let mut out = Vec::with_capacity(questions.len());
        for (k, _) in questions {
            let answer = nouls
                .get(k.as_str())
                .ok_or_else(|| JevError::Malformed(format!("no noul answer for key '{k}'")))?;
            out.push((k.clone(), answer.noul as f32));
        }

        // Persist the whole batch so the next identical call is free.
        if let Ok(serialized) = serde_json::to_string(&out) {
            self.cache_put(
                cache_hit_key,
                CachedAnswer::Choice {
                    label: serialized,
                    confidence: 1.0,
                },
            );
        }
        Ok(out)
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
        let request = SystemOneRequest::new(self.wire_state(state), [("q", Question::from(q))]);
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

        let labelled: Vec<(&str, Value)> = options.iter().map(|o| (*o, Value::Null)).collect();
        let q = Choice::new(labelled).instructions(question);
        let request = SystemOneRequest::new(self.wire_state(state), [("q", Question::from(q))]);
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

/// Redact absolute filesystem paths in a state value when
/// `[jev] redact_paths = true`: each path is replaced by a stable
/// `path-<hash>` token (same path → same token, so Jev's judgment
/// still sees distinct locations; the user keeps the real paths on
/// their machine). A no-op when the config leaves the flag off.
///
/// A path is a whitespace-delimited token that starts with `/` (a
/// POSIX absolute path) or a Windows drive (`C:\`). The scan is
/// deliberately conservative: a token that is not path-shaped is
/// left verbatim, so a Jev question about `/regex/` syntax is not
/// mangled.
fn redact_state_paths(value: &Value, enabled: bool) -> Value {
    if !enabled {
        return value.clone();
    }
    fn redact_token(tok: &str) -> String {
        let is_abs = tok.starts_with('/')
            || (tok.len() >= 2
                && tok.as_bytes()[1] == b':'
                && tok.as_bytes()[0].is_ascii_alphabetic());
        if !is_abs {
            return tok.to_string();
        }
        let hash = fnv1a_64(tok.as_bytes());
        format!("path-{hash:016x}")
    }
    fn walk(v: &Value) -> Value {
        match v {
            Value::String(s) => {
                // Rebuild the string, replacing path-shaped
                // whitespace-delimited tokens.
                let mut out = String::with_capacity(s.len());
                let mut rest = s.as_str();
                while !rest.is_empty() {
                    match rest.find(|c: char| c.is_whitespace()) {
                        Some(i) => {
                            out.push_str(&redact_token(&rest[..i]));
                            // Preserve the whitespace char(s).
                            let ws_end = rest[i..]
                                .find(|c: char| !c.is_whitespace())
                                .map(|j| i + j)
                                .unwrap_or(rest.len());
                            out.push_str(&rest[i..ws_end]);
                            rest = &rest[ws_end..];
                        }
                        None => {
                            out.push_str(&redact_token(rest));
                            rest = "";
                        }
                    }
                }
                Value::String(out)
            }
            Value::Array(a) => Value::Array(a.iter().map(walk).collect()),
            Value::Object(o) => {
                Value::Object(o.iter().map(|(k, v)| (k.clone(), walk(v))).collect())
            }
            other => other.clone(),
        }
    }
    walk(value)
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

/// A [`JevDecider`] wrapper that obfuscates registered secrets in the
/// state before delegating to the inner decider.
///
/// The Jev outbound path sends transcript previews to a remote
/// judgment endpoint. The engine's provider path scrubs secrets with
/// the vault (`agent_loop.rs`), but the Jev advisor built its state
/// with raw text and passed it straight to the client — so a
/// registered secret in a preview reached TypeSafe in the clear. The
/// vault's `redact_paths` knob (see `JevClient::wire_state`) covers
/// only paths; this covers the secret *values*.
///
/// Wrapping at the trait is the single choke point: every
/// `evaluate_*` call on every one of the ~30 advisor sites passes
/// through here. Installed by [`crate::engine`] when a Jev client is
/// set; the vault slot is shared, so a vault installed after the
/// client is still seen.
pub struct RedactingJevDecider {
    inner: std::sync::Arc<dyn JevDecider>,
    vault: std::sync::Arc<
        tokio::sync::RwLock<Option<std::sync::Arc<kod_types::secret_placeholder::SecretVault>>>,
    >,
}

impl RedactingJevDecider {
    pub fn new(
        inner: std::sync::Arc<dyn JevDecider>,
        vault: std::sync::Arc<
            tokio::sync::RwLock<Option<std::sync::Arc<kod_types::secret_placeholder::SecretVault>>>,
        >,
    ) -> Self {
        Self { inner, vault }
    }

    /// Obfuscate every string in `state` with the installed vault.
    /// A no-op when no vault is installed. `obfuscate` is the same
    /// reversible substitution the provider path uses, so the model
    /// sees the placeholder and the engine can reverse it — the
    /// judgment endpoint sees neither the raw secret nor a
    /// one-way redaction it cannot reason about.
    async fn scrub(&self, state: &Value) -> Value {
        let vault = self.vault.read().await.clone();
        let Some(vault) = vault else {
            return state.clone();
        };
        if vault.is_empty() {
            return state.clone();
        }
        fn walk(v: &Value, vault: &kod_types::secret_placeholder::SecretVault) -> Value {
            match v {
                Value::String(s) => Value::String(vault.obfuscate(s)),
                Value::Array(a) => Value::Array(a.iter().map(|x| walk(x, vault)).collect()),
                Value::Object(o) => {
                    Value::Object(o.iter().map(|(k, x)| (k.clone(), walk(x, vault))).collect())
                }
                other => other.clone(),
            }
        }
        walk(state, &vault)
    }
}

#[async_trait::async_trait]
impl JevDecider for RedactingJevDecider {
    fn thresholds(&self) -> &kod_config::JevThresholds {
        self.inner.thresholds()
    }
    fn config(&self) -> &kod_config::JevConfig {
        self.inner.config()
    }
    fn reasoning_timeout(&self) -> std::time::Duration {
        self.inner.reasoning_timeout()
    }
    fn clear_cache(&self) {
        self.inner.clear_cache();
    }
    fn cache_len(&self) -> usize {
        self.inner.cache_len()
    }
    fn cache_enabled(&self) -> bool {
        self.inner.cache_enabled()
    }

    async fn evaluate_yes_no(
        &self,
        state: &Value,
        question: &str,
    ) -> Result<Decision<bool>, JevError> {
        self.inner
            .evaluate_yes_no(&self.scrub(state).await, question)
            .await
    }
    async fn evaluate_yes_no_batch(
        &self,
        state: &Value,
        questions: &[(String, String)],
    ) -> Result<Vec<(String, f32)>, JevError> {
        self.inner
            .evaluate_yes_no_batch(&self.scrub(state).await, questions)
            .await
    }
    async fn evaluate_score(
        &self,
        state: &Value,
        question: &str,
        levels: &[&str],
    ) -> Result<Decision<String>, JevError> {
        self.inner
            .evaluate_score(&self.scrub(state).await, question, levels)
            .await
    }
    async fn evaluate_choice(
        &self,
        state: &Value,
        question: &str,
        options: &[&str],
    ) -> Result<Decision<String>, JevError> {
        self.inner
            .evaluate_choice(&self.scrub(state).await, question, options)
            .await
    }

    fn with_thresholds(
        &self,
        new: kod_config::JevThresholds,
    ) -> Result<std::sync::Arc<dyn JevDecider>, JevError> {
        let inner = self.inner.with_thresholds(new)?;
        Ok(std::sync::Arc::new(RedactingJevDecider {
            inner,
            vault: std::sync::Arc::clone(&self.vault),
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn redact_state_paths_hashes_absolute_paths() {
        let state = serde_json::json!({
            "text": "read /home/alice/.ssh/id_rsa and /etc/passwd please",
        });
        let out = redact_state_paths(&state, true);
        let text = out["text"].as_str().unwrap();
        assert!(!text.contains("/home/alice"), "path leaked: {text}");
        assert!(!text.contains("/etc/passwd"), "path leaked: {text}");
        assert!(text.contains("path-"), "expected a hashed token: {text}");
        // Same path, same token (stable); different paths differ.
        let a = redact_state_paths(&serde_json::json!({"t": "/a/b"}), true);
        let b = redact_state_paths(&serde_json::json!({"t": "/a/b"}), true);
        let c = redact_state_paths(&serde_json::json!({"t": "/a/c"}), true);
        assert_eq!(a, b, "same path must hash the same");
        assert_ne!(a, c, "different paths must differ");
    }

    #[test]
    fn redact_state_paths_is_a_noop_when_disabled() {
        let state = serde_json::json!({"text": "read /etc/passwd"});
        let out = redact_state_paths(&state, false);
        assert_eq!(out, state, "disabled must return the input unchanged");
    }

    #[test]
    fn redact_state_paths_leaves_non_paths_alone() {
        let state = serde_json::json!({"text": "is foo/bar a regex or a path?"});
        let out = redact_state_paths(&state, true);
        // `foo/bar` is relative, not absolute, so it is untouched.
        assert!(out["text"].as_str().unwrap().contains("foo/bar"));
    }

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

    // --- RedactingJevDecider (the Jev outbound choke point) ---

    /// Records the state it receives, so a test can assert what the
    /// decorator forwarded.
    struct RecordingDecider {
        cfg: JevConfig,
        seen: parking_lot::Mutex<Vec<Value>>,
    }
    #[async_trait::async_trait]
    impl JevDecider for RecordingDecider {
        fn thresholds(&self) -> &kod_config::JevThresholds {
            &self.cfg.thresholds
        }
        fn config(&self) -> &kod_config::JevConfig {
            &self.cfg
        }
        fn reasoning_timeout(&self) -> std::time::Duration {
            std::time::Duration::from_secs(1)
        }
        fn clear_cache(&self) {}
        fn cache_len(&self) -> usize {
            0
        }
        fn cache_enabled(&self) -> bool {
            false
        }
        async fn evaluate_yes_no(
            &self,
            state: &Value,
            _q: &str,
        ) -> std::result::Result<Decision<bool>, JevError> {
            self.seen.lock().push(state.clone());
            Ok(Decision::jev(true, 1.0))
        }
        async fn evaluate_yes_no_batch(
            &self,
            state: &Value,
            qs: &[(String, String)],
        ) -> std::result::Result<Vec<(String, f32)>, JevError> {
            self.seen.lock().push(state.clone());
            Ok(qs.iter().map(|(k, _)| (k.clone(), 1.0)).collect())
        }
        async fn evaluate_score(
            &self,
            state: &Value,
            _q: &str,
            _levels: &[&str],
        ) -> std::result::Result<Decision<String>, JevError> {
            self.seen.lock().push(state.clone());
            Ok(Decision::jev("x".to_string(), 1.0))
        }
        async fn evaluate_choice(
            &self,
            state: &Value,
            _q: &str,
            _opts: &[&str],
        ) -> std::result::Result<Decision<String>, JevError> {
            self.seen.lock().push(state.clone());
            Ok(Decision::jev("x".to_string(), 1.0))
        }
        fn with_thresholds(
            &self,
            _new: kod_config::JevThresholds,
        ) -> std::result::Result<std::sync::Arc<dyn JevDecider>, JevError> {
            Err(JevError::Disabled)
        }
    }

    #[tokio::test]
    async fn redacting_decorator_scrubs_secrets_in_state() {
        let vault = std::sync::Arc::new(kod_types::secret_placeholder::SecretVault::with_key(
            [7u8; 32],
        ));
        let placeholder = vault.register("super-secret-value-1234");
        let slot = std::sync::Arc::new(tokio::sync::RwLock::new(Some(vault)));
        let inner = std::sync::Arc::new(RecordingDecider {
            cfg: JevConfig::default(),
            seen: parking_lot::Mutex::new(Vec::new()),
        });
        let dec = RedactingJevDecider::new(inner.clone(), slot);
        let state = serde_json::json!({
            "text": "the key super-secret-value-1234 is in /home/a/.env",
        });
        dec.evaluate_yes_no(&state, "q").await.unwrap();
        let seen = inner.seen.lock();
        let got = seen.last().unwrap().to_string();
        assert!(
            !got.contains("super-secret-value-1234"),
            "raw secret reached the decider: {got}"
        );
        assert!(
            got.contains(&placeholder),
            "expected the placeholder in the forwarded state: {got}"
        );
    }

    #[tokio::test]
    async fn redacting_decorator_is_a_noop_without_a_vault() {
        let slot = std::sync::Arc::new(tokio::sync::RwLock::new(None));
        let inner = std::sync::Arc::new(RecordingDecider {
            cfg: JevConfig::default(),
            seen: parking_lot::Mutex::new(Vec::new()),
        });
        let dec = RedactingJevDecider::new(inner.clone(), slot);
        let state = serde_json::json!({"text": "nothing secret here"});
        dec.evaluate_yes_no(&state, "q").await.unwrap();
        let seen = inner.seen.lock();
        assert_eq!(
            &seen[0], &state,
            "no vault must forward the state unchanged"
        );
    }

    #[test]
    fn disabled_config_yields_none_client() {
        let cfg = JevConfig::default();
        assert!(!cfg.enabled);
        let got = JevClient::from_config(&cfg).expect("disabled path must not error");
        assert!(got.is_none());
    }
}

#[cfg(test)]
mod jev_redaction_proptests {
    //! Properties for the Jev state redaction and cache key.
    //!
    //! `redact_state_paths` hashes absolute-path-shaped tokens before
    //! the state goes to the external Jev service. The existing three
    //! examples cover one path, the disabled branch, and a non-path.
    //! They do not cover the property that matters: **no absolute path
    //! survives the walk** — including at the edge of a string, in
    //! nested arrays/objects, with Windows drive syntax, or with
    //! punctuation attached.
    //!
    //! `cache_key`'s separator bytes (`0`) prevent prefix collisions:
    //! `("a", state, "bc")` must not equal `("ab", state, "c")`. The
    //! existing tests check "differs for different kind/state/question"
    //! but never two calls whose concatenation would collide without
    //! the separators.
    use super::*;
    use proptest::prelude::*;
    use serde_json::{Value, json};

    /// A Unix absolute path built from a random suffix. Long enough
    /// to be distinctive (so a false positive cannot accidentally
    /// contain it), short enough to keep the test fast.
    fn unix_path() -> impl Strategy<Value = String> {
        "[a-z]{8,12}".prop_map(|s| format!("/srv/data/{s}"))
    }

    /// A Windows absolute path: one letter, colon, backslash, then
    /// two segments. The `is_abs` check is `as_bytes()[1] == b':'`
    /// and `as_bytes()[0].is_ascii_alphabetic()`.
    fn windows_path() -> impl Strategy<Value = String> {
        "[a-z]{8,12}".prop_map(|s| format!("C:\\Users\\{s}\\file.txt"))
    }

    /// A short safe token that will never look like a path.
    fn safe_word() -> impl Strategy<Value = String> {
        "[a-z]{1,10}".prop_map(|s| format!("w{s}"))
    }

    proptest! {
        /// **The security invariant, Unix form.** A `Value::String`
        /// carrying a single absolute path, walked through
        /// `redact_state_paths`, must not contain the literal path
        /// bytes anywhere in its redacted output. Covers the case
        /// where the path is the *entire* string — the tokenizer's
        /// `None` branch, which the existing one-path example does
        /// not exercise.
        #[test]
        fn prop_unix_path_never_survives(path in unix_path()) {
            let input = json!(path);
            let out = redact_state_paths(&input, true);
            let out_str = out.as_str().expect("string");
            prop_assert!(
                !out_str.contains(&path),
                "absolute path survived: input={:?} output={:?}",
                path, out_str,
            );
            prop_assert!(
                out_str.starts_with("path-"),
                "path was not redacted to a hash marker: {:?}", out_str,
            );
        }

        /// **The security invariant, Windows form.** The `is_abs`
        /// check treats `C:` as absolute; a walk that misses it leaks
        /// a Windows path.
        #[test]
        fn prop_windows_path_never_survives(path in windows_path()) {
            let input = json!(path);
            let out = redact_state_paths(&input, true);
            let out_str = out.as_str().expect("string");
            prop_assert!(
                !out_str.contains(&path),
                "Windows path survived: input={:?} output={:?}",
                path, out_str,
            );
        }

        /// **Nested structures.** The walk recurses through arrays
        /// and objects. A leak in the recursion is a leak in a real
        /// request body — the state sent to Jev is an object tree.
        #[test]
        fn prop_nested_paths_are_all_redacted(
            p1 in unix_path(),
            p2 in unix_path(),
            s1 in safe_word(),
        ) {
            let input = json!({
                "outer": {
                    "inner": [s1.clone(), p1.clone(), p2.clone()],
                    "second": {"deep": p2.clone()},
                },
                "safe": s1.clone(),
            });
            let out = redact_state_paths(&input, true);
            let serialized = serde_json::to_string(&out).expect("serialize");
            prop_assert!(!serialized.contains(&p1), "p1 leaked: {}", serialized);
            prop_assert!(!serialized.contains(&p2), "p2 leaked: {}", serialized);
            // Safe word survives — redaction is not "delete everything".
            prop_assert!(serialized.contains(&s1), "safe word lost: {}", serialized);
        }

        /// **Whitespace-preservation.** The `walk` string branch has
        /// three sub-branches (whitespace found / not found / trailing
        /// whitespace). Round-trip: the count of whitespace characters
        /// is unchanged, and the literal paths are gone.
        #[test]
        fn prop_whitespace_shape_preserved(
            p in unix_path(),
            ws in "[ \\t]{1,3}",
        ) {
            let input_str = format!("{p}{ws}tail");
            let input = json!(input_str);
            let out = redact_state_paths(&input, true);
            let out_str = out.as_str().expect("string");
            let in_ws = input_str.chars().filter(|c| c.is_whitespace()).count();
            let out_ws = out_str.chars().filter(|c| c.is_whitespace()).count();
            prop_assert_eq!(in_ws, out_ws, "whitespace count changed");
            prop_assert!(!out_str.contains(&p), "path survived");
        }

        /// **Disabled is a true no-op.** The `enabled=false` branch
        /// must return the input unchanged, byte for byte. The
        /// existing test uses a single fixed value; this exercises
        /// arbitrary strings.
        #[test]
        fn prop_disabled_is_identity(prefix in "[a-z ]{0,10}", p in unix_path()) {
            let input = json!(format!("{prefix}{p}"));
            let out = redact_state_paths(&input, false);
            prop_assert_eq!(out, input);
        }

        /// **cache_key is a deterministic function of its inputs.**
        /// Same `(kind, state, question)` → same key, always.
        #[test]
        fn prop_cache_key_deterministic(
            kind in "[a-z_]{1,10}",
            state_text in "[a-zA-Z0-9 ]{0,40}",
            question in "[a-zA-Z0-9 ?]{0,40}",
        ) {
            let state = json!({"text": state_text});
            let k1 = cache_key(&kind, &state, &question);
            let k2 = cache_key(&kind, &state, &question);
            prop_assert_eq!(k1, k2);
        }

        /// **The separator-byte property.** `("a", s, "bc")` must
        /// differ from `("ab", s, "c")`. Without the `0` separators
        /// the concatenation would be identical and the cache would
        /// return the wrong decision. This is the shape the three
        /// existing cache-key tests never exercise — they vary one
        /// field at a time, never in a prefix-colliding way.
        #[test]
        fn prop_cache_key_separators_prevent_prefix_collision(
            state_text in "[a-z]{0,20}",
        ) {
            let state = json!({"text": state_text});
            let k1 = cache_key("a", &state, "bc");
            let k2 = cache_key("ab", &state, "c");
            prop_assert_ne!(
                k1, k2,
                "prefix-collision: ('a', s, 'bc') and ('ab', s, 'c') produced the same key",
            );
            // Same for kind/question position.
            let k3 = cache_key("a", &state, "b c");
            let k4 = cache_key("a b", &state, "c");
            prop_assert_ne!(k3, k4, "space-vs-kind prefix collision");
        }

        /// Different state → different key. The existing example
        /// uses `{"a":1}` vs `{"a":2}`; this varies the state tree.
        #[test]
        fn prop_cache_key_varies_with_state(
            a in "[a-z]{1,8}",
            b in "[a-z]{1,8}",
        ) {
            prop_assume!(a != b);
            let s1 = json!({"text": a});
            let s2 = json!({"text": b});
            prop_assert_ne!(
                cache_key("kind", &s1, "q"),
                cache_key("kind", &s2, "q"),
                "different state, same key",
            );
        }
    }

    /// Concrete pin: the `None` branch (no whitespace) is what fires
    /// when the whole string is one path. The example test covers
    /// the multi-token case; this pins the single-token case.
    #[test]
    fn whole_string_path_is_redacted() {
        let p = "/home/user/secret";
        let out = redact_state_paths(&json!(p), true);
        let s = out.as_str().expect("string");
        assert!(!s.contains(p), "single-token path survived: {s:?}");
        assert!(s.starts_with("path-"));
    }

    /// Concrete pin: a path with punctuation attached. `redact_token`
    /// only checks the *first* byte; `/home/user/x:` starts with `/`
    /// and is redacted as a whole, so the punctuation goes into the
    /// hash and does not appear in the output. That is the current
    /// behaviour; the test makes it visible.
    #[test]
    fn path_with_trailing_punctuation_is_redacted() {
        let p = "/home/user/x:";
        let out = redact_state_paths(&json!(p), true);
        let s = out.as_str().expect("string");
        assert!(
            !s.contains("/home/user/x"),
            "trailing-punctuation path survived: {s:?}"
        );
    }

    /// Concrete pin: leading whitespace is preserved, the path after
    /// it is redacted. The `find` loop with `Some(0)` whitespace
    /// starts by pushing the (empty) token before the first
    /// whitespace — a subtle branch.
    #[test]
    fn leading_whitespace_preserved() {
        let out = redact_state_paths(&json!("  /etc/passwd"), true);
        let s = out.as_str().expect("string");
        assert!(s.starts_with("  "), "leading whitespace lost: {s:?}");
        assert!(!s.contains("/etc/passwd"), "path survived: {s:?}");
    }
}
