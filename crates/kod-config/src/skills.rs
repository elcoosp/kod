//! Skills system configuration.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
/// Skills subsystem configuration.
#[serde(default)]
pub struct SkillsConfig {
    /// Directory to load skills from. Uses the default when None.
    pub skills_dir: Option<String>,
    /// Reload skill files when they change on disk.
    pub enable_hot_reload: bool,
    /// Maximum number of skills composed into a single query.
    pub max_skills_per_query: usize,
    /// Minimum matcher score for a skill to apply.
    pub match_threshold: f32,
}

impl Default for SkillsConfig {
    fn default() -> Self {
        Self {
            skills_dir: None,
            enable_hot_reload: true,
            max_skills_per_query: 3,
            // H-S11: 0.3 matches the matcher's calibrated scale (tag
            // 0.4, capability 0.3, trigger 0.8, name 0.9). A 0.7
            // threshold would make tag-only / capability-only matches
            // silently unreachable.
            match_threshold: 0.3,
        }
    }
}

impl SkillsConfig {
    /// Clamp `match_threshold` into `[0.0, 1.0]`, replacing NaN with
    /// the default. Called by `KodConfig::load_default` so a typo
    /// (`70` for 70 %) silently matches everything instead of
    /// silently matching nothing.
    pub fn validate(&mut self) {
        let default = SkillsConfig::default().match_threshold;
        if !self.match_threshold.is_finite() {
            self.match_threshold = default;
            return;
        }
        let clamped = self.match_threshold.clamp(0.0, 1.0);
        if clamped.to_bits() != self.match_threshold.to_bits() {
            self.match_threshold = clamped;
        }
        // Floor max_skills_per_query at 1; a zero silently disables
        // the feature with no log line.
        if self.max_skills_per_query == 0 {
            self.max_skills_per_query = 1;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_default_skills_config() {
        let config = SkillsConfig::default();
        assert!(config.enable_hot_reload);
        assert_eq!(config.max_skills_per_query, 3);
    }
}

#[cfg(test)]
mod coverage_threshold_validation {
    //! H-S11 regression suite. The default `0.7` contradicted the
    //! matcher's calibrated 0.3 scale, and there was no clamp — a
    //! user who wrote `70` (percent, not fraction) silently got zero
    //! matches. Both are now caught at config load.
    use super::*;

    #[test]
    fn default_matches_the_matcher_scale() {
        // Tied to `SkillMatcher::new`'s min_score of 0.3. If either
        // side changes, the other should follow.
        assert!((SkillsConfig::default().match_threshold - 0.3).abs() < f32::EPSILON);
    }

    #[test]
    fn nan_threshold_is_replaced_by_default() {
        let mut c = SkillsConfig {
            match_threshold: f32::NAN,
            ..Default::default()
        };
        c.validate();
        assert!(c.match_threshold.is_finite());
        assert!((c.match_threshold - 0.3).abs() < f32::EPSILON);
    }

    #[test]
    fn threshold_above_one_is_clamped() {
        let mut c = SkillsConfig {
            match_threshold: 70.0,
            ..Default::default()
        };
        c.validate();
        assert_eq!(
            f64::from(c.match_threshold).to_bits(),
            f64::from(1.0_f32).to_bits()
        );
    }

    #[test]
    fn negative_threshold_is_clamped_to_zero() {
        let mut c = SkillsConfig {
            match_threshold: -1.0,
            ..Default::default()
        };
        c.validate();
        assert_eq!(
            f64::from(c.match_threshold).to_bits(),
            f64::from(0.0_f32).to_bits()
        );
    }

    #[test]
    fn a_valid_threshold_is_untouched() {
        let mut c = SkillsConfig {
            match_threshold: 0.55,
            ..Default::default()
        };
        c.validate();
        assert!((c.match_threshold - 0.55).abs() < f32::EPSILON);
    }

    #[test]
    fn zero_max_skills_becomes_one() {
        let mut c = SkillsConfig {
            max_skills_per_query: 0,
            ..Default::default()
        };
        c.validate();
        assert_eq!(c.max_skills_per_query, 1);
    }
}
