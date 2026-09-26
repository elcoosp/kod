//! Mental models: curated, prompt-cache-frozen summaries (borrow from
//! oh-my-pi, delta §12.7).
//!
//! # What a mental model is
//!
//! A named, long-lived summary — "User Preferences", "Project
//! Conventions", "Project Decisions" — that is built once from a
//! memory query and then injected into every prompt for the life of a
//! session.
//!
//! # Why it is frozen
//!
//! The whole reason a mental model is cheap is that it occupies a
//! stable byte range in the system prompt. If the block is
//! re-rendered mid-session — because a new memory matched its
//! source query — the bytes after it shift, and the provider's prefix
//! cache is invalidated for every turn. The design's rule: render at
//! session start, **freeze for the transcript**, reload at a
//! transcript boundary (a `/clear`, a new session), never mid-turn.
//!
//! # Seed, then freeze
//!
//! A [`MentalModel`] is a *seed*: its id, name, the query that fills
//! it, its scopes, its token budget, and whether it refreshes after
//! consolidation. Seeding is create-only — a second seed with the
//! same id is ignored, so a session that reloads config does not
//! change an existing model's definition.
//!
//! # What this is NOT
//!
//! * Not the query. The seed names a `source_query`; running it
//!   against the store is the manager's job.
//! * Not the cache. This module tracks the frozen rendering and its
//!   generation; the engine is what places it in the prompt.

use std::collections::HashMap;

/// When a model reloads its contents from the store.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum RefreshTrigger {
    /// Only at a transcript boundary (a new session). The default —
    /// the frozen block survives the whole session.
    #[default]
    SessionStart,
    /// Also after a memory consolidation pass, at the *next*
    /// transcript boundary. The reload is still deferred to a
    /// boundary so the mid-session bytes stay stable.
    AfterConsolidation,
}

/// A model's seed: its identity and how to fill it.
#[derive(Debug, Clone)]
pub struct MentalModelSeed {
    pub id: String,
    pub name: String,
    /// The memory query that fills the model.
    pub source_query: String,
    /// Optional scope tags (a project key, a memory type) that
    /// narrow the query.
    pub scopes: Vec<String>,
    /// Token budget for the rendered block. The design uses 600 for
    /// preference models, 800 for decision models.
    pub max_tokens: usize,
    pub trigger: RefreshTrigger,
}

/// A filled model: the seed plus its rendered text and generation.
#[derive(Debug, Clone)]
pub struct MentalModel {
    pub seed: MentalModelSeed,
    /// The rendered block. `None` until the first fill.
    pub rendered: Option<String>,
    /// Bumped on every fill. A caller comparing generations can see
    /// whether a reload happened.
    pub generation: u64,
}

impl MentalModel {
    fn from_seed(seed: MentalModelSeed) -> Self {
        Self {
            seed,
            rendered: None,
            generation: 0,
        }
    }

    /// Whether the model has been filled.
    pub fn is_rendered(&self) -> bool {
        self.rendered.is_some()
    }

    /// The rendered block, or `None` before the first fill.
    pub fn block(&self) -> Option<&str> {
        self.rendered.as_deref()
    }

    /// Install a rendered block. Bumps the generation. The caller is
    /// responsible for only calling this at a transcript boundary.
    pub fn fill(&mut self, text: String) {
        self.rendered = Some(text);
        self.generation += 1;
    }
}

/// The session's models, keyed by id.
///
/// Frozen by construction: `fill` is the only mutation, and the
/// caller only calls it at a boundary. `render_block` concatenates
/// every rendered model into one prompt section, in id order, so the
/// bytes are stable across turns.
#[derive(Debug, Default)]
pub struct MentalModels {
    models: HashMap<String, MentalModel>,
    /// The transcript generation this set was rendered for. A caller
    /// that crosses a boundary bumps it and re-fills.
    transcript_generation: u64,
}

impl MentalModels {
    pub fn new() -> Self {
        Self::default()
    }

    /// Seed a model. Create-only: a second seed with the same id is
    /// ignored, so reloading config mid-session cannot change an
    /// existing model's definition — which would move the frozen
    /// bytes.
    pub fn seed(&mut self, seed: MentalModelSeed) -> bool {
        if self.models.contains_key(&seed.id) {
            return false;
        }
        self.models
            .insert(seed.id.clone(), MentalModel::from_seed(seed));
        true
    }

    /// The ids of every seeded model, sorted.
    pub fn ids(&self) -> Vec<String> {
        let mut v: Vec<String> = self.models.keys().cloned().collect();
        v.sort();
        v
    }

    /// One model by id.
    pub fn get(&self, id: &str) -> Option<&MentalModel> {
        self.models.get(id)
    }

    /// One model by id, mutably — for `fill`.
    pub fn get_mut(&mut self, id: &str) -> Option<&mut MentalModel> {
        self.models.get_mut(id)
    }

    /// The current transcript generation.
    pub fn transcript_generation(&self) -> u64 {
        self.transcript_generation
    }

    /// Cross a transcript boundary. Bumps the generation; the caller
    /// then re-fills every model whose trigger fires. The frozen
    /// blocks are *not* cleared — a model that is not re-filled keeps
    /// its previous block, which is still valid for the new session
    /// (a stale-but-stable summary beats an empty one).
    pub fn begin_transcript(&mut self) -> u64 {
        self.transcript_generation += 1;
        self.transcript_generation
    }

    /// Every model whose trigger says it should refresh at a
    /// boundary.
    pub fn refresh_ids(&self) -> Vec<String> {
        let mut v: Vec<String> = self
            .models
            .values()
            .filter(|m| {
                matches!(
                    m.seed.trigger,
                    RefreshTrigger::SessionStart | RefreshTrigger::AfterConsolidation,
                )
            })
            .map(|m| m.seed.id.clone())
            .collect();
        v.sort();
        v
    }

    /// The concatenated block for the prompt: each rendered model
    /// under its name, in id order. Empty when nothing is rendered.
    ///
    /// The bytes are stable for a given set of rendered blocks, so
    /// the prompt prefix that contains them does not churn.
    pub fn render_block(&self) -> String {
        let mut out = String::new();
        for id in self.ids() {
            let Some(m) = self.models.get(&id) else { continue };
            let Some(text) = m.block() else { continue };
            if text.trim().is_empty() {
                continue;
            }
            out.push_str(&format!("### {}\n\n{}\n\n", m.seed.name, text.trim_end()));
        }
        out.trim_end().to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn seed(id: &str, name: &str) -> MentalModelSeed {
        MentalModelSeed {
            id: id.to_string(),
            name: name.to_string(),
            source_query: format!("{id} query"),
            scopes: vec![],
            max_tokens: 600,
            trigger: RefreshTrigger::SessionStart,
        }
    }

    #[test]
    fn seeding_a_model_adds_it() {
        let mut m = MentalModels::new();
        assert!(m.seed(seed("prefs", "User Preferences")));
        assert_eq!(m.ids(), vec!["prefs".to_string()]);
    }

    #[test]
    fn seeding_twice_is_create_only() {
        let mut m = MentalModels::new();
        assert!(m.seed(seed("prefs", "User Preferences")));
        // A second seed with the same id, different name.
        let mut second = seed("prefs", "Changed Name");
        second.max_tokens = 9999;
        assert!(!m.seed(second), "the second seed must be refused");
        // The original definition survives.
        assert_eq!(m.get("prefs").unwrap().seed.name, "User Preferences");
        assert_eq!(m.get("prefs").unwrap().seed.max_tokens, 600);
    }

    #[test]
    fn a_fresh_model_is_not_rendered() {
        let mut m = MentalModels::new();
        m.seed(seed("prefs", "P"));
        assert!(!m.get("prefs").unwrap().is_rendered());
        assert!(m.get("prefs").unwrap().block().is_none());
    }

    #[test]
    fn filling_sets_the_block_and_bumps_the_generation() {
        let mut m = MentalModels::new();
        m.seed(seed("prefs", "P"));
        m.get_mut("prefs").unwrap().fill("tabs over spaces".to_string());
        assert_eq!(m.get("prefs").unwrap().generation, 1);
        assert!(m.get("prefs").unwrap().is_rendered());
    }

    #[test]
    fn render_block_concatenates_rendered_models() {
        let mut m = MentalModels::new();
        m.seed(seed("a", "Alpha"));
        m.seed(seed("b", "Beta"));
        m.get_mut("a").unwrap().fill("a content".to_string());
        m.get_mut("b").unwrap().fill("b content".to_string());
        let block = m.render_block();
        assert!(block.contains("### Alpha"), "got: {block}");
        assert!(block.contains("a content"), "got: {block}");
        assert!(block.contains("### Beta"), "got: {block}");
        assert!(block.contains("b content"), "got: {block}");
    }

    #[test]
    fn render_block_skips_unrendered_models() {
        let mut m = MentalModels::new();
        m.seed(seed("a", "Alpha"));
        m.seed(seed("b", "Beta"));
        m.get_mut("a").unwrap().fill("only a".to_string());
        let block = m.render_block();
        assert!(block.contains("only a"), "got: {block}");
        assert!(!block.contains("Beta"), "got: {block}");
    }

    #[test]
    fn render_block_is_empty_when_nothing_is_filled() {
        let mut m = MentalModels::new();
        m.seed(seed("a", "Alpha"));
        assert_eq!(m.render_block(), "");
    }

    #[test]
    fn render_block_is_stable_across_calls() {
        let mut m = MentalModels::new();
        m.seed(seed("a", "Alpha"));
        m.get_mut("a").unwrap().fill("stable".to_string());
        assert_eq!(m.render_block(), m.render_block());
    }

    #[test]
    fn render_block_orders_by_id_not_insertion() {
        let mut m = MentalModels::new();
        m.seed(seed("z", "Zeta"));
        m.seed(seed("a", "Alpha"));
        m.get_mut("z").unwrap().fill("z".to_string());
        m.get_mut("a").unwrap().fill("a".to_string());
        let block = m.render_block();
        let a_pos = block.find("### Alpha").unwrap();
        let z_pos = block.find("### Zeta").unwrap();
        assert!(a_pos < z_pos, "id order must be stable, got: {block}");
    }

    #[test]
    fn crossing_a_boundary_bumps_the_generation() {
        let mut m = MentalModels::new();
        assert_eq!(m.transcript_generation(), 0);
        assert_eq!(m.begin_transcript(), 1);
        assert_eq!(m.begin_transcript(), 2);
    }

    #[test]
    fn crossing_a_boundary_keeps_the_previous_block() {
        // A stale-but-stable summary beats an empty one.
        let mut m = MentalModels::new();
        m.seed(seed("a", "Alpha"));
        m.get_mut("a").unwrap().fill("kept".to_string());
        m.begin_transcript();
        assert!(m.render_block().contains("kept"));
    }

    #[test]
    fn refresh_ids_lists_every_model() {
        let mut m = MentalModels::new();
        m.seed(seed("a", "A"));
        m.seed(seed("b", "B"));
        assert_eq!(m.refresh_ids(), vec!["a".to_string(), "b".to_string()]);
    }

    #[test]
    fn render_block_skips_a_whitespace_only_block() {
        let mut m = MentalModels::new();
        m.seed(seed("a", "Alpha"));
        m.get_mut("a").unwrap().fill("   \n  ".to_string());
        assert_eq!(m.render_block(), "");
    }
}
