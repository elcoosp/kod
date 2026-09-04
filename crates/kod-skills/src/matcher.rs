//! Skill matcher - finds relevant skills based on user input.
//!
//! Uses pattern matching (triggers, tags, capabilities) to score
//! and rank skills by relevance.

use kod_types::{MatchReason, Skill, SkillMatch};
use std::collections::HashMap;
use tokio::sync::RwLock;

/// Matches user queries to relevant skills
pub struct SkillMatcher {
    skills: RwLock<HashMap<String, Skill>>,
    max_results: usize,
    min_score: f32,
}

impl Default for SkillMatcher {
    fn default() -> Self {
        Self::new()
    }
}

impl SkillMatcher {
    pub fn new() -> Self {
        Self {
            skills: RwLock::new(HashMap::new()),
            max_results: 3,
            min_score: 0.3,
        }
    }

    /// Set maximum number of results to return
    pub fn set_max_results(&mut self, max: usize) {
        self.max_results = max;
    }

    /// Set minimum score threshold
    pub fn set_min_score(&mut self, min: f32) {
        self.min_score = min;
    }

    /// Add a skill to the matcher
    pub async fn add_skill(&self, skill: Skill) {
        self.skills
            .write()
            .await
            .insert(skill.metadata.name.clone(), skill);
    }

    /// Remove a skill from the matcher
    pub async fn remove_skill(&self, name: &str) {
        self.skills.write().await.remove(name);
    }

    /// Find skills relevant to the given query
    pub async fn find_relevant_skills(&self, query: &str) -> Vec<SkillMatch> {
        let query_lower = query.to_lowercase();
        let skills = self.skills.read().await;

        let mut matches: Vec<SkillMatch> = Vec::new();

        for skill in skills.values() {
            let (score, reasons) = self.score_skill(skill, &query_lower);

            if score >= self.min_score {
                matches.push(SkillMatch {
                    skill: skill.clone(),
                    score,
                    match_reasons: reasons,
                });
            }
        }

        // Sort by score (descending)
        matches.sort_by(|a, b| {
            b.score
                .partial_cmp(&a.score)
                .unwrap_or(std::cmp::Ordering::Equal)
        });

        // Limit results
        matches.into_iter().take(self.max_results).collect()
    }

    /// Score a single skill against a query
    fn score_skill(&self, skill: &Skill, query: &str) -> (f32, Vec<MatchReason>) {
        let mut score: f32 = 0.0;
        let mut reasons = Vec::new();

        // 1. Trigger matching (highest weight: 0.8)
        for trigger in &skill.metadata.triggers {
            let trigger_lower = trigger.to_lowercase();
            if query.contains(&trigger_lower) {
                score += 0.8;
                reasons.push(MatchReason::TriggerMatch {
                    trigger: trigger.clone(),
                });
            }
        }

        // 2. Tag matching (medium weight: 0.4)
        for tag in &skill.metadata.tags {
            let tag_lower = tag.to_lowercase();
            if query.contains(&tag_lower) || tag_lower.contains(query) {
                score += 0.4;
                reasons.push(MatchReason::TagMatch { tag: tag.clone() });
            }
        }

        // 3. Capability matching (lower weight: 0.3)
        for capability in &skill.metadata.capabilities {
            let cap_lower = capability.to_lowercase();
            if query.contains(&cap_lower) {
                score += 0.3;
                reasons.push(MatchReason::CapabilityMatch {
                    capability: capability.clone(),
                });
            }
        }

        // 4. Name matching (if query is similar to skill name)
        let name_lower = skill.metadata.name.to_lowercase();
        if query.contains(&name_lower) || name_lower.contains(query) {
            score += 0.5;
        }

        // Normalize score to 0.0 - 1.0
        score = score.min(1.0);

        (score, reasons)
    }

    /// Get all skills in the matcher
    pub async fn get_all_skills(&self) -> Vec<Skill> {
        self.skills.read().await.values().cloned().collect()
    }

    /// Get number of skills
    pub async fn count(&self) -> usize {
        self.skills.read().await.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use kod_types::{SkillId, SkillMetadata};
    use std::path::PathBuf;

    fn create_skill(name: &str, triggers: Vec<&str>) -> Skill {
        Skill {
            id: SkillId::new(),
            metadata: SkillMetadata {
                name: name.to_string(),
                description: "Test".to_string(),
                version: "1.0.0".to_string(),
                author: None,
                category: "test".to_string(),
                tags: Vec::new(),
                capabilities: Vec::new(),
                requirements: Vec::new(),
                triggers: triggers.into_iter().map(String::from).collect(),
            },
            instructions: "Test".to_string(),
            examples: Vec::new(),
            constraints: None,
            content: String::new(),
            path: PathBuf::new(),
        }
    }

    #[tokio::test]
    async fn test_basic_matching() {
        let matcher = SkillMatcher::new();
        matcher
            .add_skill(create_skill("test", vec!["test trigger"]))
            .await;

        let results = matcher.find_relevant_skills("test trigger").await;
        assert_eq!(results.len(), 1);
    }
}
