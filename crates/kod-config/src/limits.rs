//! Session cost and token limits (Tier 1.2 / 2.5).
//!
//! Every cap is opt-in: `0` disables it. An unconfigured KOD runs
//! identically to one that predates this block.

use serde::{Deserialize, Serialize};

/// What to do when a cap is hit.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum OnExhausted {
    /// Pause and prompt the user (TUI only). Non-interactive callers
    /// treat this as `Stop`.
    #[default]
    Ask,
    /// End the turn cleanly. The partial reply is kept.
    Stop,
    /// Log a warning and keep going. Audit-run mode.
    Continue,
}

/// A per-tool quota (Tier 2.5). `0` disables that limit.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct ToolQuota {
    pub per_turn: usize,
    pub per_session: usize,
    /// For `execute_command`: how many times the same command string
    /// may run per turn.
    pub per_command: usize,
}

/// The `[limits]` block.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct LimitsConfig {
    /// USD. 0 disables.
    pub max_cost_usd_per_session: f64,
    /// USD. 0 disables.
    pub max_cost_usd_per_turn: f64,
    /// 0 disables.
    pub max_input_tokens_per_turn: usize,
    /// 0 disables.
    pub max_output_tokens_per_turn: usize,
    /// What to do when a cap is hit.
    pub on_exhausted: OnExhausted,
    /// Warn when spend crosses this fraction of a cap. 0 disables.
    pub soft_warn_at: f64,
    /// Per-tool quotas (Tier 2.5). Keyed by tool name; `"default"`
    /// applies to any tool not otherwise listed.
    pub tools: std::collections::BTreeMap<String, ToolQuota>,
}

impl Default for LimitsConfig {
    fn default() -> Self {
        Self {
            max_cost_usd_per_session: 0.0,
            max_cost_usd_per_turn: 0.0,
            max_input_tokens_per_turn: 0,
            max_output_tokens_per_turn: 0,
            on_exhausted: OnExhausted::Ask,
            soft_warn_at: 0.5,
            tools: std::collections::BTreeMap::new(),
        }
    }
}

impl LimitsConfig {
    pub fn clamp(&mut self) {
        if !self.soft_warn_at.is_finite() {
            self.soft_warn_at = 0.5;
        }
        self.soft_warn_at = self.soft_warn_at.clamp(0.0, 1.0);
        if !self.max_cost_usd_per_session.is_finite() || self.max_cost_usd_per_session < 0.0 {
            self.max_cost_usd_per_session = 0.0;
        }
        if !self.max_cost_usd_per_turn.is_finite() || self.max_cost_usd_per_turn < 0.0 {
            self.max_cost_usd_per_turn = 0.0;
        }
    }

    pub fn any_active(&self) -> bool {
        self.max_cost_usd_per_session > 0.0
            || self.max_cost_usd_per_turn > 0.0
            || self.max_input_tokens_per_turn > 0
            || self.max_output_tokens_per_turn > 0
            || !self.tools.is_empty()
    }

    /// Resolve the quota for a tool name — explicit entry, else the
    /// `default` entry, else none.
    pub fn quota_for(&self, tool: &str) -> Option<&ToolQuota> {
        self.tools
            .get(tool)
            .or_else(|| self.tools.get("default"))
            .filter(|q| q.per_turn > 0 || q.per_session > 0 || q.per_command > 0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_has_every_cap_disabled() {
        let c = LimitsConfig::default();
        assert_eq!(c.max_cost_usd_per_session, 0.0);
        assert_eq!(c.max_cost_usd_per_turn, 0.0);
        assert_eq!(c.on_exhausted, OnExhausted::Ask);
        assert_eq!(c.soft_warn_at, 0.5);
        assert!(!c.any_active());
    }

    #[test]
    fn empty_toml_uses_defaults() {
        let c: LimitsConfig = toml::from_str("").unwrap();
        assert!(!c.any_active());
    }

    #[test]
    fn caps_parse_independently() {
        let s = r#"
max_cost_usd_per_session = 5.0
max_cost_usd_per_turn = 0.5
soft_warn_at = 0.75
on_exhausted = "stop"
"#;
        let c: LimitsConfig = toml::from_str(s).unwrap();
        assert_eq!(c.max_cost_usd_per_session, 5.0);
        assert_eq!(c.max_cost_usd_per_turn, 0.5);
        assert_eq!(c.soft_warn_at, 0.75);
        assert_eq!(c.on_exhausted, OnExhausted::Stop);
        assert!(c.any_active());
    }

    #[test]
    fn on_exhausted_round_trips_every_variant() {
        #[derive(Serialize, Deserialize)]
        struct W {
            v: OnExhausted,
        }
        for variant in [OnExhausted::Ask, OnExhausted::Stop, OnExhausted::Continue] {
            let w = W { v: variant };
            let s = toml::to_string(&w).unwrap();
            let back: W = toml::from_str(&s).unwrap();
            assert_eq!(back.v, variant);
        }
    }

    #[test]
    fn clamp_handles_bad_values() {
        let mut c = LimitsConfig {
            max_cost_usd_per_session: -1.0,
            max_cost_usd_per_turn: f64::NAN,
            max_input_tokens_per_turn: 0,
            max_output_tokens_per_turn: 0,
            on_exhausted: OnExhausted::Ask,
            soft_warn_at: 2.0,
            tools: Default::default(),
        };
        c.clamp();
        assert_eq!(c.max_cost_usd_per_session, 0.0);
        assert_eq!(c.max_cost_usd_per_turn, 0.0);
        assert_eq!(c.soft_warn_at, 1.0);
    }

    #[test]
    fn tool_quota_resolves_explicit_then_default() {
        let mut c = LimitsConfig::default();
        c.tools.insert(
            "grep".to_string(),
            ToolQuota {
                per_turn: 20,
                per_session: 200,
                per_command: 0,
            },
        );
        c.tools.insert(
            "default".to_string(),
            ToolQuota {
                per_turn: 50,
                per_session: 1000,
                per_command: 0,
            },
        );
        assert_eq!(c.quota_for("grep").unwrap().per_turn, 20);
        assert_eq!(c.quota_for("read_file").unwrap().per_turn, 50);
    }

    #[test]
    fn tool_quota_disabled_when_all_zero() {
        let mut c = LimitsConfig::default();
        c.tools.insert(
            "noop".to_string(),
            ToolQuota {
                per_turn: 0,
                per_session: 0,
                per_command: 0,
            },
        );
        assert!(c.quota_for("noop").is_none());
    }
}
