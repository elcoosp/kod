//! Jev (TypeSafe AI System One) configuration.
//!
//! The `[jev]` section of the KOD config controls the optional
//! TypeSafe AI integration. Every field has a safe default that
//! keeps the integration disabled until the user opts in — an
//! unconfigured KOD runs identically to one built before Jev existed.
//!
//! The wrapper in `kod-core` reads this struct and turns it into a
//! concrete client. Nothing in this module performs I/O; it is pure
//! data so `kod config export` and the `/jev` command can print the
//! effective settings.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::time::Duration;

/// The whole `[jev]` block. All fields have defaults, so a config
/// that omits the block entirely still produces a usable (disabled)
/// `JevConfig`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct JevConfig {
    /// Master switch. When false, every call site falls back to the
    /// pre-Jev heuristic without making a network call. Default
    /// false: Jev is opt-in.
    pub enabled: bool,
    /// Explicit API key. When `None`, the client reads
    /// `TYPESAFE_API_KEY` from the environment. An explicit value
    /// wins over the environment.
    ///
    /// H-S4: never serialized in the clear. `kod config show-merged`
    /// and `kod config export` are common enough that a plaintext
    /// key in the terminal scrollback (or an exported file) is the
    /// pre-fix behaviour; `serialize_redacted` emits a stable
    /// `"[redacted]"` marker instead. Deserialization still accepts a
    /// literal key so an existing config that carried one keeps
    /// working.
    #[serde(serialize_with = "serialize_redacted_option")]
    pub api_key: Option<String>,
    /// Override for the TypeSafe API root.
    ///
    /// Resolution order (highest precedence first):
    ///
    /// 1. `[jev] base_url` — this field. Example: `base_url =
    ///    "https://api.typesafe.ai"` for the public endpoint, or a
    ///    self-hosted gateway like `"http://localhost:8080"` or a
    ///    Vercel AI Gateway URL.
    /// 2. `TYPESAFE_BASE_URL` environment variable (read by the
    ///    SDK).
    /// 3. The SDK default, `https://api.typesafe.ai`.
    ///
    /// Whitespace-only values are treated as unset so a placeholder
    /// in a config file does not produce an invalid request.
    pub base_url: Option<String>,
    /// Model alias. When `None`, the client uses `jev-latest`.
    pub model: Option<String>,
    /// How long a cached decision stays valid, in seconds. A cached
    /// decision is free — this is the knob that trades memory for
    /// network calls.
    pub cache_ttl_secs: u64,
    /// Per-request timeout in milliseconds. The SDK default is ten
    /// seconds; KOD's interactive round needs a much tighter budget
    /// because the decision gates a user-visible action.
    pub timeout_ms: u64,
    /// When true (the default), a Jev failure is caught and the
    /// caller runs its heuristic. When false, the error propagates
    /// and the caller decides what to do. The default is the safe
    /// choice: Jev is an optimisation, not a hard dependency.
    pub fail_open: bool,
    /// When true, absolute paths in the state are hashed before the
    /// state is sent to TypeSafe. Trade-off: Jev's judgment gets a
    /// little weaker, and the user keeps the paths on their machine.
    pub redact_paths: bool,
    /// Seconds the TUI will hide reasoning-classified text before it
    /// forces the buffer to render as prose (P1.4 safety valve). A
    /// value of 0 disables the valve — the classifier is trusted
    /// absolutely, which is a choice worth making only on a model
    /// with well-calibrated classification behaviour.
    pub reasoning_timeout_secs: u64,
    /// Confidence thresholds for every boolean Jev decision. One
    /// table so a user can tighten or loosen the whole integration.
    pub thresholds: JevThresholds,
    /// Per-round model routing. Keys are the round kinds the engine
    /// scores (`planning`, `tool_execution`, `synthesis`, `summary`),
    /// values are endpoint names from `[llm]`. Empty means "no
    /// per-round routing".
    pub round_routing: HashMap<String, String>,
}

impl Default for JevConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            api_key: None,
            base_url: None,
            model: None,
            cache_ttl_secs: 300,
            timeout_ms: 800,
            fail_open: true,
            redact_paths: false,
            reasoning_timeout_secs: 20,
            thresholds: JevThresholds::default(),
            round_routing: HashMap::new(),
        }
    }
}

impl JevConfig {
    /// True when the integration is switched on. Call sites check
    /// this before doing anything else so the disabled path never
    /// touches the network.
    pub fn is_enabled(&self) -> bool {
        self.enabled
    }

    /// The per-request timeout, floored at 50 ms so a typo cannot
    /// produce a zero-length timeout that fails on every call.
    pub fn timeout(&self) -> Duration {
        Duration::from_millis(self.timeout_ms.max(50))
    }

    /// The reasoning-timeout as a `Duration`. Zero means the valve
    /// is disabled and the classifier is trusted absolutely.
    pub fn reasoning_timeout(&self) -> Duration {
        Duration::from_secs(self.reasoning_timeout_secs)
    }

    /// The cache TTL as a `Duration`. A TTL of zero disables the
    /// cache entirely (every lookup misses).
    pub fn cache_ttl(&self) -> Duration {
        Duration::from_secs(self.cache_ttl_secs)
    }

    /// The endpoint name configured for a round kind, if any.
    pub fn endpoint_for_round(&self, kind: &str) -> Option<&str> {
        self.round_routing.get(kind).map(String::as_str)
    }
}

/// Thresholds for every Jev boolean decision. A probability at or
/// above the threshold counts as "yes". Values are in `[0.0, 1.0]`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct JevThresholds {
    pub task_classify_min: f32,
    pub tool_filter_min: f32,
    pub early_termination_min: f32,
    pub auto_approve_min: f32,
    pub memory_filter_min: f32,
    pub ambiguity_min: f32,
}

impl Default for JevThresholds {
    fn default() -> Self {
        Self {
            task_classify_min: 0.6,
            tool_filter_min: 0.7,
            early_termination_min: 0.9,
            auto_approve_min: 0.95,
            memory_filter_min: 0.7,
            ambiguity_min: 0.85,
        }
    }
}

impl JevThresholds {
    /// Names accepted by `/jev tune set`. Stable strings — the
    /// command's docs and the config file's keys must agree.
    pub const NAMES: &'static [&'static str] = &[
        "task_classify_min",
        "tool_filter_min",
        "early_termination_min",
        "auto_approve_min",
        "memory_filter_min",
        "ambiguity_min",
    ];

    /// Read a threshold by name. `None` for an unknown name.
    pub fn get(&self, name: &str) -> Option<f32> {
        Some(match name {
            "task_classify_min" => self.task_classify_min,
            "tool_filter_min" => self.tool_filter_min,
            "early_termination_min" => self.early_termination_min,
            "auto_approve_min" => self.auto_approve_min,
            "memory_filter_min" => self.memory_filter_min,
            "ambiguity_min" => self.ambiguity_min,
            _ => return None,
        })
    }

    /// Set a threshold by name. Returns `false` for an unknown name.
    /// The value is clamped into `[0.0, 1.0]` and NaN is coerced to
    /// `0.5` so a bad input cannot make every decision fail.
    pub fn set(&mut self, name: &str, value: f32) -> bool {
        let v = if !value.is_finite() {
            0.5
        } else {
            value.clamp(0.0, 1.0)
        };
        match name {
            "task_classify_min" => self.task_classify_min = v,
            "tool_filter_min" => self.tool_filter_min = v,
            "early_termination_min" => self.early_termination_min = v,
            "auto_approve_min" => self.auto_approve_min = v,
            "memory_filter_min" => self.memory_filter_min = v,
            "ambiguity_min" => self.ambiguity_min = v,
            _ => return false,
        }
        true
    }

    /// Clamp every threshold into `[0.0, 1.0]` so a bad config does
    /// not make every decision fail (or succeed).
    pub fn clamp(&mut self) {
        fn c(x: &mut f32) {
            if !x.is_finite() {
                *x = 0.5;
            } else {
                *x = x.clamp(0.0, 1.0);
            }
        }
        c(&mut self.task_classify_min);
        c(&mut self.tool_filter_min);
        c(&mut self.early_termination_min);
        c(&mut self.auto_approve_min);
        c(&mut self.memory_filter_min);
        c(&mut self.ambiguity_min);
    }
}

/// Serialize an `Option<String>` as a redacted marker. Both
/// `Some(_)` and `None` render as `None` for a missing key, and
/// `Some("[redacted]")` for a set one — the value itself is never
/// emitted. This is the one hook that keeps a Jev key out of
/// `kod config show-merged` / `export` output and any log line that
/// prints the config.
fn serialize_redacted_option<S>(value: &Option<String>, ser: S) -> Result<S::Ok, S::Error>
where
    S: serde::Serializer,
{
    use serde::Serialize;
    match value {
        Some(_) => Some("[redacted]").serialize(ser),
        None => Option::<String>::None.serialize(ser),
    }
}

#[cfg(test)]
#[allow(clippy::field_reassign_with_default)]
mod tests {
    use super::*;

    #[test]
    fn default_is_disabled_and_fail_open() {
        let c = JevConfig::default();
        assert!(!c.enabled);
        assert!(c.fail_open);
        assert_eq!(c.cache_ttl_secs, 300);
        assert_eq!(c.timeout_ms, 800);
        assert!(c.round_routing.is_empty());
    }

    #[test]
    fn thresholds_match_the_documented_values() {
        let t = JevThresholds::default();
        assert!((t.task_classify_min - 0.6).abs() < 1e-6);
        assert!((t.tool_filter_min - 0.7).abs() < 1e-6);
        assert!((t.early_termination_min - 0.9).abs() < 1e-6);
        assert!((t.auto_approve_min - 0.95).abs() < 1e-6);
        assert!((t.memory_filter_min - 0.7).abs() < 1e-6);
        assert!((t.ambiguity_min - 0.85).abs() < 1e-6);
    }

    #[test]
    fn empty_toml_table_uses_all_defaults() {
        let parsed: JevConfig = toml::from_str("").unwrap();
        assert!(!parsed.enabled);
        assert!(parsed.fail_open);
        assert_eq!(parsed.cache_ttl_secs, 300);
    }

    #[test]
    fn full_config_round_trips_through_toml() {
        let mut c = JevConfig::default();
        c.enabled = true;
        c.model = Some("jev-latest".into());
        c.round_routing.insert("planning".into(), "cloud".into());
        let s = toml::to_string(&c).unwrap();
        let back: JevConfig = toml::from_str(&s).unwrap();
        assert!(back.enabled);
        assert_eq!(back.model.as_deref(), Some("jev-latest"));
        assert_eq!(back.endpoint_for_round("planning"), Some("cloud"));
    }

    #[test]
    fn timeout_is_floored_at_50ms() {
        let mut c = JevConfig::default();
        c.timeout_ms = 0;
        assert_eq!(c.timeout(), Duration::from_millis(50));
    }

    #[test]
    fn every_field_parses_independently() {
        let s = r#"
            enabled = true
            cache_ttl_secs = 60
            timeout_ms = 250
            fail_open = false
            redact_paths = true
        "#;
        let c: JevConfig = toml::from_str(s).unwrap();
        assert!(c.enabled);
        assert_eq!(c.cache_ttl_secs, 60);
        assert_eq!(c.timeout_ms, 250);
        assert!(!c.fail_open);
        assert!(c.redact_paths);
    }

    #[test]
    fn threshold_get_round_trips_every_name() {
        let t = JevThresholds::default();
        for name in JevThresholds::NAMES {
            assert!(t.get(name).is_some(), "get({name}) returned None");
        }
        assert!(t.get("nope").is_none());
    }

    #[test]
    fn threshold_set_rejects_unknown_names() {
        let mut t = JevThresholds::default();
        assert!(!t.set("nope", 0.5));
    }

    #[test]
    fn threshold_set_clamps_and_stores() {
        let mut t = JevThresholds::default();
        assert!(t.set("task_classify_min", 1.5));
        assert_eq!(t.task_classify_min, 1.0);
        assert!(t.set("task_classify_min", -1.0));
        assert_eq!(t.task_classify_min, 0.0);
        assert!(t.set("tool_filter_min", 0.55));
        assert!((t.tool_filter_min - 0.55).abs() < 1e-6);
    }

    #[test]
    fn threshold_set_handles_nan() {
        let mut t = JevThresholds::default();
        t.set("ambiguity_min", f32::NAN);
        assert_eq!(t.ambiguity_min, 0.5);
    }

    #[test]
    fn threshold_clamp_handles_out_of_range_values() {
        let mut t = JevThresholds {
            task_classify_min: -1.0,
            tool_filter_min: 2.0,
            early_termination_min: f32::NAN,
            auto_approve_min: 0.5,
            memory_filter_min: 0.7,
            ambiguity_min: 0.85,
        };
        t.clamp();
        assert_eq!(t.task_classify_min, 0.0);
        assert_eq!(t.tool_filter_min, 1.0);
        assert_eq!(t.early_termination_min, 0.5);
    }

    #[test]
    fn reasoning_timeout_default_is_twenty_seconds() {
        let c = JevConfig::default();
        assert_eq!(c.reasoning_timeout_secs, 20);
        assert_eq!(c.reasoning_timeout(), Duration::from_secs(20));
    }

    #[test]
    #[allow(clippy::field_reassign_with_default)]
    fn reasoning_timeout_zero_disables_the_valve() {
        let mut c = JevConfig::default();
        c.reasoning_timeout_secs = 0;
        assert_eq!(c.reasoning_timeout(), Duration::from_secs(0));
    }

    #[test]
    #[allow(clippy::field_reassign_with_default)]
    fn zero_cache_ttl_disables_the_cache() {
        let mut c = JevConfig::default();
        c.cache_ttl_secs = 0;
        assert_eq!(c.cache_ttl(), Duration::from_secs(0));
    }

    #[test]
    #[allow(clippy::field_reassign_with_default)]
    fn api_key_is_redacted_in_serialized_form() {
        // H-S4: the whole point of the redaction hook. A config
        // that carries `api_key = "sk-live-..."` must serialize with
        // the marker, never the value.
        let cfg = JevConfig {
            api_key: Some("sk-live-do-not-leak".to_string()),
            ..Default::default()
        };
        let json = serde_json::to_string(&cfg).unwrap();
        assert!(
            !json.contains("sk-live-do-not-leak"),
            "api_key value leaked: {json}",
        );
        assert!(json.contains("[redacted]"), "marker missing: {json}");
    }

    #[test]
    #[allow(clippy::field_reassign_with_default)]
    fn api_key_none_serializes_as_null() {
        let cfg = JevConfig::default();
        let v: serde_json::Value = serde_json::to_value(&cfg).unwrap();
        assert!(v["api_key"].is_null());
    }

    #[test]
    fn api_key_round_trips_through_deserialization() {
        // Deserialize still accepts a real key from a config file;
        // the redaction only applies on the way out.
        let json = r#"{"api_key":"sk-live-real"}"#;
        let cfg: JevConfig = serde_json::from_str(json).unwrap();
        assert_eq!(cfg.api_key.as_deref(), Some("sk-live-real"));
    }
}
