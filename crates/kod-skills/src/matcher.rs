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

    /// Construct with a caller-supplied minimum score.
    pub fn with_threshold(min_score: f32) -> Self {
        Self {
            skills: RwLock::new(HashMap::new()),
            max_results: 3,
            min_score,
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

    /// Replace the matcher's contents with exactly `skills`,
    /// discarding whatever was there before.
    ///
    /// Used by hot reload: a file-system event means the on-disk
    /// skill set changed, and the matcher rebuilds from a fresh
    /// directory read. Add/remove-one-at-a-time would need the event
    /// to carry the change kind (Created / Modified / Removed) plus
    /// the skill's name, and even then two events from one edit (a
    /// truncate and a write, common on some editors) would need
    /// coalescing. A full replace is O(n) directory reads — cheap for
    /// a skill set — and cannot drift.
    pub async fn replace_all(&self, skills: Vec<kod_types::Skill>) {
        let mut map = self.skills.write().await;
        map.clear();
        for skill in skills {
            map.insert(skill.metadata.name.clone(), skill);
        }
    }

    /// Remove a skill from the matcher
    pub async fn remove_skill(&self, name: &str) {
        self.skills.write().await.remove(name);
    }

    /// Names of all loaded skills (for `/skills` listing).
    pub async fn skill_names(&self) -> Vec<String> {
        let skills = self.skills.read().await;
        let mut names: Vec<String> = skills.keys().cloned().collect();
        names.sort();
        names
    }

    /// Names + descriptions of all loaded skills (for `/skills` listing).
    pub async fn skill_details(&self) -> Vec<(String, String)> {
        let skills = self.skills.read().await;
        let mut out: Vec<(String, String)> = skills
            .values()
            .map(|s| (s.metadata.name.clone(), s.metadata.description.clone()))
            .collect();
        out.sort_by(|a, b| a.0.cmp(&b.0));
        out
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

        // 4. Name matching (weighty: a directly named skill should win).
        //
        // Minimum length guards against short queries ("hi", "ui", "o")
        // matching every skill whose name happens to contain those letters
        // ("this-tool", "history-writer", "hint-helper").
        let name_lower = skill.metadata.name.to_lowercase();
        const MIN_NAME_MATCH_CHARS: usize = 3;
        let name_hit = name_lower == query
            || (name_lower.contains(query) && query.len() >= MIN_NAME_MATCH_CHARS)
            || (query.contains(&name_lower) && name_lower.len() >= MIN_NAME_MATCH_CHARS);
        if name_hit {
            score += 0.9;
            reasons.push(MatchReason::NameMatch {
                name: skill.metadata.name.clone(),
            });
        }

        // 5. Description keyword overlap: the skill's description talks
        // about the query's words (e.g. "design a landing page" finds a
        // skill described as "UI/UX design"). Capped so a wordy
        // description can't outrank trigger/tag matches. Both directions:
        // a description word in the query ("design"), or a query word
        // inside the description ("interface" ⊂ "interfaces").
        let desc_lower = skill.metadata.description.to_lowercase();
        let mut desc_hits = 0;
        let mut seen: Vec<&str> = Vec::new();
        for word in query.split(|c: char| !c.is_alphanumeric()) {
            if word.len() > 3 && desc_lower.contains(word) && !seen.contains(&word) {
                seen.push(word);
                desc_hits += 1;
            }
        }
        for word in desc_lower.split(|c: char| !c.is_alphanumeric()) {
            if word.len() > 4 && query.contains(word) && !seen.contains(&word) {
                seen.push(word);
                desc_hits += 1;
            }
        }
        if desc_hits > 0 {
            // Weak lift only: enough to surface a genuinely related skill when
            // nothing stronger matches, but never enough to outrank an explicit
            // name or trigger hit on its own (≤ 0.2 vs ≥ 0.9).
            let desc_score = (0.10 * desc_hits as f32).min(0.20);
            score += desc_score;
            reasons.push(MatchReason::SemanticSimilarity { score: desc_score });
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

    #[tokio::test]
    async fn test_description_overlap_alone_is_not_enough() {
        let matcher = SkillMatcher::new();
        let mut skill = create_skill("ui-ux-designer", Vec::new());
        skill.metadata.description =
            "Design beautiful user interfaces and experiences for applications".to_string();
        matcher.add_skill(skill).await;

        // No trigger/tag/capability/name overlap — description words alone
        // no longer surface a skill (capped at 0.2 < 0.3 threshold) so that
        // generic word sharing doesn't pollute the matched set.
        let results = matcher
            .find_relevant_skills("help me design the interface")
            .await;
        assert_eq!(results.len(), 0);
    }

    #[tokio::test]
    async fn test_name_match_dominates_description_noise() {
        // Three skills: one explicitly named in the query, two only sharing a
        // single description word. The named one wins big; the others stay
        // below threshold so they don't pollute [skills] used.
        let matcher = SkillMatcher::new();
        let make = |name: &str, desc: &str| {
            let mut s = create_skill(name, Vec::new());
            s.metadata.description = desc.to_string();
            s
        };
        matcher
            .add_skill(make("ui-ux-designer", "Design beautiful user interfaces"))
            .await;
        matcher
            .add_skill(make("spec-writer", "Write design specs"))
            .await;
        matcher
            .add_skill(make("marketing-ideas", "Creative marketing copy"))
            .await;
        let results = matcher
            .find_relevant_skills("instructions for ui-ux-designer")
            .await;
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].skill.metadata.name, "ui-ux-designer");
    }

    #[tokio::test]
    async fn test_short_query_does_not_match_by_name_substring() {
        // Regression: with the old substring rule, a 2-char query like
        // "hi" matched every skill whose name contained those letters
        // ("this-tool", "history-writer"). Those still score through
        // tags/triggers/description below threshold, but must NOT get
        // the 0.9 name-match bonus.
        let matcher = SkillMatcher::new();
        matcher
            .add_skill(create_skill("this-tool", Vec::new()))
            .await;
        matcher
            .add_skill(create_skill("history-writer", Vec::new()))
            .await;
        matcher
            .add_skill(create_skill("hint-helper", Vec::new()))
            .await;

        let results = matcher.find_relevant_skills("hi").await;
        assert!(
            results.is_empty(),
            "short query produced name matches: {:?}",
            results
                .iter()
                .map(|m| m.skill.metadata.name.as_str())
                .collect::<Vec<_>>()
        );

        // A query long enough to be meaningful still matches by name.
        let results = matcher.find_relevant_skills("use this-tool").await;
        assert!(!results.is_empty(), "expected a match for 'this-tool'");
    }

    #[tokio::test]
    async fn test_replace_all_swaps_contents() {
        let matcher = SkillMatcher::new();
        matcher.add_skill(create_skill("first", vec!["one"])).await;
        matcher.add_skill(create_skill("second", vec!["two"])).await;
        matcher
            .add_skill(create_skill("third", vec!["three"]))
            .await;
        assert_eq!(matcher.count().await, 3);

        // replace_all with a smaller set: everything from before is
        // gone, only the new set is visible.
        matcher
            .replace_all(vec![create_skill("fresh", vec!["new"])])
            .await;
        assert_eq!(matcher.count().await, 1);
        let names = matcher.skill_names().await;
        assert_eq!(names, vec!["fresh".to_string()]);

        // Matching uses only the new set.
        let hits = matcher.find_relevant_skills("new").await;
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].skill.metadata.name, "fresh");

        // The old name does not match anything.
        let hits = matcher.find_relevant_skills("one").await;
        assert!(hits.is_empty());
    }

    #[tokio::test]
    async fn test_replace_all_with_empty_clears() {
        let matcher = SkillMatcher::new();
        matcher.add_skill(create_skill("a", vec!["x"])).await;
        assert_eq!(matcher.count().await, 1);
        matcher.replace_all(Vec::new()).await;
        assert_eq!(matcher.count().await, 0);
    }
}

#[cfg(test)]
mod coverage_match_scoring {
    //! Pins the ranking shape a caller relies on: a name hit
    //! outranks a trigger hit, a trigger hit outranks a tag hit, a
    //! tag hit outranks a capability hit, and a capability hit
    //! outranks pure description overlap. A regression that changed
    //! these ratios would silently reroute every prompt that
    //! mentions a skill name.
    use super::*;
    use kod_types::{SkillId, SkillMetadata};
    use std::path::PathBuf;

    fn skill(name: &str, desc: &str, triggers: &[&str], tags: &[&str], caps: &[&str]) -> Skill {
        Skill {
            id: SkillId::new(),
            metadata: SkillMetadata {
                name: name.into(),
                description: desc.into(),
                version: "1.0.0".into(),
                author: None,
                category: "test".into(),
                tags: tags.iter().map(|s| s.to_string()).collect(),
                capabilities: caps.iter().map(|s| s.to_string()).collect(),
                requirements: vec![],
                triggers: triggers.iter().map(|s| s.to_string()).collect(),
            },
            instructions: String::new(),
            examples: vec![],
            constraints: None,
            content: String::new(),
            path: PathBuf::new(),
        }
    }

    #[tokio::test]
    async fn name_hit_outranks_trigger_hit() {
        let m = SkillMatcher::new();
        m.add_skill(skill("rust-refactoring", "", &[], &[], &[]))
            .await;
        m.add_skill(skill("other", "", &["rust-refactoring"], &[], &[]))
            .await;
        let results = m.find_relevant_skills("rust-refactoring").await;
        assert!(!results.is_empty(), "expected at least one match");
        assert_eq!(results[0].skill.metadata.name, "rust-refactoring");
    }

    #[tokio::test]
    async fn trigger_hit_outranks_tag_hit() {
        let m = SkillMatcher::new();
        m.add_skill(skill("by-trigger", "", &["special phrase"], &[], &[]))
            .await;
        m.add_skill(skill("by-tag", "", &[], &["special"], &[]))
            .await;
        // The query contains the exact trigger phrase; both skills
        // have some signal, but the exact-trigger match must lead.
        let results = m.find_relevant_skills("use special phrase here").await;
        assert!(!results.is_empty(), "expected a match");
        assert_eq!(results[0].skill.metadata.name, "by-trigger");
    }

    #[tokio::test]
    async fn case_insensitive_trigger_match() {
        // Matching lowercases both sides. A trigger written with
        // capitals must fire on a lowercase query and vice versa.
        let m = SkillMatcher::new();
        m.add_skill(skill("x", "", &["Refactor Rust"], &[], &[]))
            .await;
        let results = m.find_relevant_skills("please refactor rust now").await;
        assert_eq!(results.len(), 1);
    }

    #[tokio::test]
    async fn no_signal_skill_is_filtered() {
        // A skill whose every field is empty returns no match for a
        // query that has nothing in common with its name. The
        // threshold filter is what keeps a huge, unrelated skill
        // library from polluting the prompt.
        let m = SkillMatcher::new();
        m.add_skill(skill("unrelated", "", &[], &[], &[])).await;
        let results = m.find_relevant_skills("completely different subject").await;
        assert!(results.is_empty());
    }

    #[tokio::test]
    async fn results_are_sorted_by_score_descending() {
        let m = SkillMatcher::new();
        // Three skills with decreasing signal.
        m.add_skill(skill("target", "", &["target"], &[], &[]))
            .await;
        m.add_skill(skill("second", "", &[], &["target"], &[]))
            .await;
        m.add_skill(skill("third", "", &[], &[], &["target"])).await;
        let results = m.find_relevant_skills("target").await;
        assert!(results.len() >= 2, "expected at least 2: {results:?}");
        for w in results.windows(2) {
            assert!(
                w[0].score >= w[1].score,
                "scores not sorted: {} before {}",
                w[0].score,
                w[1].score,
            );
        }
    }

    #[tokio::test]
    async fn max_results_caps_the_output() {
        // `max_results` defaults to 3. A query matching 10 skills
        // returns 3 — that cap is what bounds prompt cost.
        let m = SkillMatcher::new();
        for i in 0..10 {
            m.add_skill(skill(
                &format!("s{i}"),
                "matching description about matching",
                &["matching"],
                &[],
                &[],
            ))
            .await;
        }
        let results = m.find_relevant_skills("matching").await;
        assert!(results.len() <= 3, "cap ignored: got {}", results.len());
    }

    #[tokio::test]
    async fn with_threshold_controls_the_minimum() {
        // A higher threshold rejects weak matches; a lower one
        // accepts them. The knob is the caller's only lever on the
        // precision/recall tradeoff, so both directions must work.
        let strict = SkillMatcher::with_threshold(0.99);
        strict
            .add_skill(skill("x", "some description", &[], &[], &[]))
            .await;
        let r = strict.find_relevant_skills("some description").await;
        // Description-only overlap is capped at 0.2, so nothing
        // reaches 0.99.
        assert!(r.is_empty(), "strict should reject: {r:?}");

        let loose = SkillMatcher::with_threshold(0.05);
        loose
            .add_skill(skill("x", "some description", &[], &[], &[]))
            .await;
        let r = loose.find_relevant_skills("some description").await;
        assert!(!r.is_empty(), "loose should accept: {r:?}");
    }

    #[tokio::test]
    async fn removing_a_skill_makes_it_unmatchable() {
        let m = SkillMatcher::new();
        m.add_skill(skill("gone", "", &["unique-trigger"], &[], &[]))
            .await;
        assert_eq!(m.find_relevant_skills("unique-trigger").await.len(), 1);
        m.remove_skill("gone").await;
        assert!(m.find_relevant_skills("unique-trigger").await.is_empty());
    }

    #[tokio::test]
    async fn adding_a_skill_under_an_existing_name_replaces_it() {
        // The matcher keys skills by name. Registering a second
        // skill under the same name must overwrite, not append:
        // otherwise a hot-reloaded skill produces two matches for
        // one name and the prompt carries the instructions twice.
        let m = SkillMatcher::new();
        m.add_skill(skill("same", "old", &["old-trigger"], &[], &[]))
            .await;
        m.add_skill(skill("same", "new", &["new-trigger"], &[], &[]))
            .await;
        assert_eq!(m.count().await, 1);
        assert!(m.find_relevant_skills("old-trigger").await.is_empty());
        assert_eq!(m.find_relevant_skills("new-trigger").await.len(), 1);
    }
}
