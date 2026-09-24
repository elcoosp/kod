//! Per-prompt effort classification (borrow from oh-my-pi, delta §9.6).
//!
//! # What this is
//!
//! When a caller configures `effort = "auto"`, the effort a request
//! asks for is chosen per-turn by a small classifier: a trivial edit
//! gets `low`, a refactor gets `high`, a novel problem gets `xhigh`.
//! The classification is one judgment call — a `Choice` question over
//! the user's input — so it costs one round-trip and produces a
//! deterministic answer.
//!
//! # Two ladders, two questions
//!
//! The doc names two classifier shapes:
//!
//! * **Full ladder.** A `Choice` question with every effort level as
//!   a criterion. Used with a reliable judge model (the `judge` role
//!   chain).
//! * **Three buckets.** A coarser `Choice` question — `trivial`,
//!   `moderate`, `hard` — mapped onto `low`, `medium`, `xhigh`. The
//!   doc notes that three-class is more reliable than four-way
//!   ordinal on a sub-2B model, which is the practical model size
//!   for a latency-sensitive per-turn classifier.
//!
//! Both modes use the same [`JudgmentClient`] and the same
//! [`AutoThinking::classify`] entry point; a mode enum selects which
//! question to render.
//!
//! # Ceiling and clamping
//!
//! The classifier's raw answer is filtered through two pure
//! functions before it reaches the request:
//!
//! * [`clamp_to_supported`] — a model that only supports `[low, high]`
//!   cannot be asked for `xhigh`. The answer drops to the highest
//!   supported level at or below it.
//! * [`ceiling_for_auto`] — auto never returns the top of the model's
//!   ladder. `Max` on a model whose top is `Max` requires an explicit
//!   `effort = "max"` in the config; an auto-classified turn lands at
//!   `xhigh` at most, so a user who has not opted into minute-long
//!   turns is not surprised by one.
//!
//! The two compose: classify → clamp to the model's supported set →
//! apply the auto ceiling. On a model that supports only `[low,
//! medium]`, auto never returns more than `medium`, and the ceiling
//! cannot push below `low`.
//!
//! # What this is NOT
//!
//! * Not a replacement for the offline keyword classifier. A caller
//!   that has no judge configured falls back to whatever it already
//!   had; this module is the LLM-judge variant the doc names.
//! * Not a cache. Two calls with the same input may classify to
//!   different levels if the judge is non-deterministic.
//! * Not a scheduler. It answers "what effort?" — the router that
//!   acts on the answer is a separate concern.
//!
//! # Fallback on an unparseable judge reply
//!
//! A judge that replies with a label the question did not offer —
//! a hallucination, a stray prose line, a strong model that
//! answers with an effort-tier name the caller's ladder did not
//! include — is treated as a neutral vote (`Medium`), not an
//! error. Failing every turn on a judge hiccup would make a
//! per-turn classifier useless. Only real transport failures
//! (`JudgmentError::Provider`) propagate.

use kod_provider::judgment::{JudgmentClient, JudgmentError, Question};
use kod_types::effort::EffortLevel;

/// Which classifier shape to use.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ClassifierMode {
    /// Full ladder: a `Choice` question over every level the model
    /// supports. Used with a capable judge model.
    #[default]
    FullLadder,
    /// Three buckets: `trivial | moderate | hard`, mapped to `low |
    /// medium | xhigh`. Used with a small judge model, where three
    /// classes are more reliable than seven.
    ThreeBucket,
}

/// The auto-thinking classifier.
pub struct AutoThinking {
    client: JudgmentClient,
    mode: ClassifierMode,
}

impl AutoThinking {
    pub fn new(client: JudgmentClient) -> Self {
        Self {
            client,
            mode: ClassifierMode::default(),
        }
    }

    pub fn with_mode(mut self, mode: ClassifierMode) -> Self {
        self.mode = mode;
        self
    }

    pub fn mode(&self) -> ClassifierMode {
        self.mode
    }

    /// Classify `input` into an effort level.
    ///
    /// The returned level is the judge's answer, clamped to
    /// `supported` and capped by the auto ceiling.
    ///
    /// # Unparseable replies fall back to `Medium`
    ///
    /// A judge that replies with a label the question did not offer
    /// — a hallucination, or a model that knows the workspace's
    /// effort vocabulary from training and answers with a tier the
    /// caller did not include — is treated as a neutral vote, not
    /// an error. Failing every turn on a judge hiccup would make a
    /// per-turn classifier useless; the workspace's own convention
    /// (`EffortLevel::parse` returns `Medium` for unknown labels,
    /// `EffortLevel::default()` is `Medium`) points the same way.
    ///
    /// `JudgmentError::Provider` — the network is down, the
    /// credentials are wrong — still propagates. That is a real
    /// failure the caller has to know about; an unparseable label
    /// is the judge expressing an opinion outside the offered
    /// vocabulary, which is exactly the ambiguity `Medium` exists
    /// to resolve.
    pub async fn classify(
        &self,
        input: &str,
        supported: &[EffortLevel],
    ) -> Result<EffortLevel, JudgmentError> {
        let raw = match self.mode {
            ClassifierMode::FullLadder => {
                match self.classify_full_ladder(input, supported).await {
                    Ok(l) => l,
                    Err(JudgmentError::Unparseable { .. }) => EffortLevel::Medium,
                    Err(e) => return Err(e),
                }
            }
            ClassifierMode::ThreeBucket => {
                match self.classify_three_bucket(input).await {
                    Ok(l) => l,
                    Err(JudgmentError::Unparseable { .. }) => EffortLevel::Medium,
                    Err(e) => return Err(e),
                }
            }
        };
        let clamped = clamp_to_supported(raw, supported);
        Ok(ceiling_for_auto(clamped, supported))
    }

    async fn classify_full_ladder(
        &self,
        input: &str,
        supported: &[EffortLevel],
    ) -> Result<EffortLevel, JudgmentError> {
        // The question's criteria are exactly the model's supported
        // ladder — a judge asked to choose between unsupported levels
        // would produce an answer the clamp would then drop, wasting
        // the call's information.
        let ladder = if supported.is_empty() {
            vec![EffortLevel::Medium]
        } else {
            supported.to_vec()
        };
        let criteria: Vec<(String, String)> = ladder
            .iter()
            .map(|l| (l.as_str().to_string(), ladder_criterion(*l).to_string()))
            .collect();
        let question = Question::Choice {
            id: "effort".to_string(),
            text: "How much reasoning effort does this request need?".to_string(),
            criteria,
        };
        let state: &[(&str, &str)] = &[("input", input)];
        let answers = self.client.ask(state, &[question]).await?;
        Ok(parse_effort_label(
            answers.get("effort").unwrap_or("medium"),
        ))
    }

    async fn classify_three_bucket(
        &self,
        input: &str,
    ) -> Result<EffortLevel, JudgmentError> {
        let question = Question::Choice {
            id: "bucket".to_string(),
            text: "How much reasoning effort does this request need?".to_string(),
            criteria: vec![
                (
                    "trivial".to_string(),
                    "a rename, a typo, a one-line edit, or a question \
                     with a known answer"
                        .to_string(),
                ),
                (
                    "moderate".to_string(),
                    "a small feature, a targeted fix, or a question that \
                     needs reading a few files"
                        .to_string(),
                ),
                (
                    "hard".to_string(),
                    "a design decision, a multi-file refactor, a bug with \
                     no reproduction, or an irreversible operation"
                        .to_string(),
                ),
            ],
        };
        let state: &[(&str, &str)] = &[("input", input)];
        let answers = self.client.ask(state, &[question]).await?;
        let raw = answers.get("bucket").unwrap_or("moderate");
        Ok(match raw {
            "trivial" => EffortLevel::Low,
            "hard" => EffortLevel::Xhigh,
            _ => EffortLevel::Medium,
        })
    }
}

/// The human-readable criterion for one effort level.
fn ladder_criterion(level: EffortLevel) -> &'static str {
    match level {
        EffortLevel::None => "no reasoning wanted — a direct factual answer",
        EffortLevel::Minimal => {
            "almost none — a short chain on a known problem"
        }
        EffortLevel::Low => "a rename, a typo, a one-line edit",
        EffortLevel::Medium => {
            "a small feature, a targeted fix, or a short lookup"
        }
        EffortLevel::High => {
            "a multi-file change, a design decision, or a debugging session"
        }
        EffortLevel::Xhigh => {
            "a hard problem where correctness matters more than latency — \
             a bug with no reproduction, an irreversible operation, or a \
             live cutover"
        }
        EffortLevel::Max => {
            "the hardest problems — correctness and completeness matter \
             above all, minutes of silence are expected"
        }
    }
}

/// Parse an effort label back to an `EffortLevel`.
fn parse_effort_label(label: &str) -> EffortLevel {
    EffortLevel::parse(label)
}

/// Clamp `level` to the highest value at or below it in `supported`.
///
/// `supported` is ascending. An empty list is treated as
/// `[Medium]`.
pub fn clamp_to_supported(level: EffortLevel, supported: &[EffortLevel]) -> EffortLevel {
    if supported.is_empty() {
        return EffortLevel::Medium;
    }
    let mut best: Option<EffortLevel> = None;
    for &s in supported {
        if s <= level {
            best = Some(match best {
                Some(b) if b >= s => b,
                _ => s,
            });
        }
    }
    best.unwrap_or_else(|| {
        supported
            .iter()
            .copied()
            .min()
            .unwrap_or(EffortLevel::Medium)
    })
}

/// Apply the auto ceiling: `level` is at most one tier below the top
/// of `supported`.
///
/// "One tier" means one step in `supported`'s own ordering, not one
/// step in the full `EffortLevel` enum — a model whose top is `High`
/// ceilings at `Medium`, not at `Low`.
pub fn ceiling_for_auto(level: EffortLevel, supported: &[EffortLevel]) -> EffortLevel {
    if supported.is_empty() {
        return EffortLevel::Medium;
    }
    let mut sorted: Vec<EffortLevel> = supported.to_vec();
    sorted.sort();
    sorted.dedup();

    let ceiling = match sorted.len() {
        1 => sorted[0],
        _ => sorted[sorted.len() - 2],
    };
    if level > ceiling { ceiling } else { level }
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use kod_error::{KodError, Result as KodResult};
    use kod_provider::judgment::JudgmentOptions;
    use kod_provider::request::CompletionRequest;
    use kod_provider::{
        GenerationOptions, GenerationResponse, LlmProvider, ModelInfo, ModelRef,
        StreamChunk,
    };
    use kod_types::ToolDefinition;
    use std::pin::Pin;
    use std::sync::Arc;

    struct FixedJudge {
        reply: String,
    }

    impl FixedJudge {
        fn new(reply: impl Into<String>) -> Self {
            Self { reply: reply.into() }
        }
    }

    #[async_trait]
    impl LlmProvider for FixedJudge {
        fn name(&self) -> &str {
            "fixed-judge"
        }
        async fn list_models(&self) -> KodResult<Vec<ModelInfo>> {
            Ok(vec![])
        }
        async fn generate(
            &self,
            _p: &str,
            _o: &GenerationOptions,
        ) -> KodResult<String> {
            Ok(String::new())
        }
        async fn generate_with_tools(
            &self,
            _p: &str,
            _t: &[ToolDefinition],
            _o: &GenerationOptions,
        ) -> KodResult<GenerationResponse> {
            Ok(GenerationResponse::Text {
                content: String::new(),
                usage: None,
            })
        }
        fn stream(
            &self,
            _p: &str,
            _o: &GenerationOptions,
        ) -> Pin<Box<dyn futures::Stream<Item = KodResult<StreamChunk>> + Send + '_>>
        {
            Box::pin(futures::stream::empty())
        }
        async fn complete(
            &self,
            _req: &CompletionRequest,
        ) -> KodResult<GenerationResponse> {
            Ok(GenerationResponse::Text {
                content: self.reply.clone(),
                usage: None,
            })
        }
    }

    fn classifier(reply: &str, mode: ClassifierMode) -> AutoThinking {
        let provider: Arc<dyn LlmProvider> = Arc::new(FixedJudge::new(reply));
        let client = JudgmentClient::new(
            provider,
            ModelRef::new("judge", "test"),
            JudgmentOptions::default(),
        );
        AutoThinking::new(client).with_mode(mode)
    }

    // ---- clamp_to_supported -------------------------------------------

    #[test]
    fn clamp_returns_the_same_level_when_supported() {
        let supported = [
            EffortLevel::Low,
            EffortLevel::Medium,
            EffortLevel::High,
            EffortLevel::Xhigh,
            EffortLevel::Max,
        ];
        assert_eq!(
            clamp_to_supported(EffortLevel::High, &supported),
            EffortLevel::High,
        );
    }

    #[test]
    fn clamp_drops_to_the_next_supported_below() {
        let supported = [EffortLevel::Low, EffortLevel::Medium, EffortLevel::Xhigh];
        assert_eq!(
            clamp_to_supported(EffortLevel::High, &supported),
            EffortLevel::Medium,
        );
    }

    #[test]
    fn clamp_returns_the_lowest_supported_when_level_is_under_the_floor() {
        let supported = [EffortLevel::Medium, EffortLevel::High];
        assert_eq!(
            clamp_to_supported(EffortLevel::None, &supported),
            EffortLevel::Medium,
        );
    }

    #[test]
    fn clamp_returns_the_highest_supported_when_level_is_over_the_ceiling() {
        let supported = [EffortLevel::Low, EffortLevel::Medium];
        assert_eq!(
            clamp_to_supported(EffortLevel::Max, &supported),
            EffortLevel::Medium,
        );
    }

    #[test]
    fn clamp_with_empty_supported_returns_medium() {
        assert_eq!(
            clamp_to_supported(EffortLevel::Max, &[]),
            EffortLevel::Medium,
        );
    }

    // ---- ceiling_for_auto ---------------------------------------------

    #[test]
    fn ceiling_drops_to_one_below_the_top() {
        let supported = [
            EffortLevel::Low,
            EffortLevel::Medium,
            EffortLevel::High,
            EffortLevel::Max,
        ];
        assert_eq!(
            ceiling_for_auto(EffortLevel::Max, &supported),
            EffortLevel::High,
        );
    }

    #[test]
    fn ceiling_leaves_a_lower_level_alone() {
        let supported = [EffortLevel::Low, EffortLevel::Medium, EffortLevel::Max];
        assert_eq!(
            ceiling_for_auto(EffortLevel::Low, &supported),
            EffortLevel::Low,
        );
    }

    #[test]
    fn ceiling_with_one_supported_level_returns_that_level() {
        let supported = [EffortLevel::Medium];
        assert_eq!(
            ceiling_for_auto(EffortLevel::Max, &supported),
            EffortLevel::Medium,
        );
    }

    #[test]
    fn ceiling_with_empty_supported_returns_medium() {
        assert_eq!(ceiling_for_auto(EffortLevel::Max, &[]), EffortLevel::Medium);
    }

    #[test]
    fn ceiling_is_one_tier_below_top_on_the_models_own_ladder() {
        let supported = [EffortLevel::Low, EffortLevel::Medium, EffortLevel::High];
        assert_eq!(
            ceiling_for_auto(EffortLevel::High, &supported),
            EffortLevel::Medium,
        );
    }

    // ---- ladder_criterion ---------------------------------------------

    #[test]
    fn low_criterion_mentions_the_doc_examples() {
        let s = ladder_criterion(EffortLevel::Low);
        assert!(s.contains("rename"));
        assert!(s.contains("typo"));
        assert!(s.contains("one-line edit"));
    }

    #[test]
    fn xhigh_criterion_mentions_the_doc_examples() {
        let s = ladder_criterion(EffortLevel::Xhigh);
        assert!(s.contains("no reproduction") || s.contains("no-repro"));
        assert!(s.contains("irreversible"));
    }

    #[test]
    fn every_level_has_a_criterion() {
        for l in [
            EffortLevel::None,
            EffortLevel::Minimal,
            EffortLevel::Low,
            EffortLevel::Medium,
            EffortLevel::High,
            EffortLevel::Xhigh,
            EffortLevel::Max,
        ] {
            let s = ladder_criterion(l);
            assert!(!s.is_empty(), "criterion empty for {l:?}");
        }
    }

    // ---- Async classify -----------------------------------------------

    #[tokio::test]
    async fn full_ladder_classification_returns_the_judges_choice() {
        let c = classifier("effort: high\n", ClassifierMode::FullLadder);
        let supported = [
            EffortLevel::Low,
            EffortLevel::Medium,
            EffortLevel::High,
            EffortLevel::Max,
        ];
        let e = c.classify("refactor the parser", &supported).await.unwrap();
        assert_eq!(e, EffortLevel::High);
    }

    #[tokio::test]
    async fn full_ladder_respects_the_auto_ceiling() {
        let c = classifier("effort: max\n", ClassifierMode::FullLadder);
        let supported = [
            EffortLevel::Low,
            EffortLevel::Medium,
            EffortLevel::High,
            EffortLevel::Max,
        ];
        let e = c.classify("anything", &supported).await.unwrap();
        assert_eq!(e, EffortLevel::High);
    }

    #[tokio::test]
    async fn full_ladder_composes_with_the_auto_ceiling() {
        // The full-ladder path cannot exercise clamp: the question
        // is built *from* `supported`, so the judge is only ever
        // offered labels the model accepts. What it can exercise is
        // the ceiling: the judge answers `high`, the model supports
        // `[Low, Medium, High]`, and the auto ceiling drops `High`
        // to `Medium` because the top of this model's ladder is
        // `High`.
        //
        // (Clamp has its own tests, both as pure functions and — more
        // importantly — through the three-bucket path, where the
        // three buckets map to levels that may not all be in the
        // model's ladder.)
        let c = classifier("effort: high\n", ClassifierMode::FullLadder);
        let supported = [EffortLevel::Low, EffortLevel::Medium, EffortLevel::High];
        let e = c.classify("anything", &supported).await.unwrap();
        assert_eq!(e, EffortLevel::Medium);
    }

    #[tokio::test]
    async fn full_ladder_cannot_request_a_level_the_model_does_not_support() {
        // The full-ladder question is *built* from `supported`, so a
        // judge cannot vote for an unsupported level through this
        // path — the label was never on the ballot. A judge that
        // nevertheless replies with an unoffered label (a strong
        // model that knows the workspace's effort vocabulary from
        // training) is handled by `classify`'s unparseable-to-Medium
        // fallback, and the neutral answer is what the caller sees.
        let c = classifier("effort: xhigh\n", ClassifierMode::FullLadder);
        let supported = [EffortLevel::Low, EffortLevel::Medium, EffortLevel::High];
        let e = c.classify("anything", &supported).await.unwrap();
        assert_eq!(e, EffortLevel::Medium);
    }

    #[tokio::test]
    async fn three_bucket_trivial_maps_to_low() {
        let c = classifier("bucket: trivial\n", ClassifierMode::ThreeBucket);
        let supported = [
            EffortLevel::Low,
            EffortLevel::Medium,
            EffortLevel::High,
            EffortLevel::Max,
        ];
        let e = c.classify("rename x to y", &supported).await.unwrap();
        assert_eq!(e, EffortLevel::Low);
    }

    #[tokio::test]
    async fn three_bucket_moderate_maps_to_medium() {
        let c = classifier("bucket: moderate\n", ClassifierMode::ThreeBucket);
        let supported = [
            EffortLevel::Low,
            EffortLevel::Medium,
            EffortLevel::High,
        ];
        let e = c.classify("fix the bug", &supported).await.unwrap();
        assert_eq!(e, EffortLevel::Medium);
    }

    #[tokio::test]
    async fn three_bucket_hard_maps_to_xhigh_then_ceilinged() {
        let c = classifier("bucket: hard\n", ClassifierMode::ThreeBucket);
        let supported = [
            EffortLevel::Low,
            EffortLevel::Medium,
            EffortLevel::High,
            EffortLevel::Max,
        ];
        let e = c.classify("redesign auth", &supported).await.unwrap();
        assert_eq!(e, EffortLevel::High);
    }

    #[tokio::test]
    async fn three_bucket_hard_on_a_narrow_ladder_clamps_and_ceils() {
        let c = classifier("bucket: hard\n", ClassifierMode::ThreeBucket);
        let supported = [EffortLevel::Low, EffortLevel::Medium, EffortLevel::High];
        let e = c.classify("redesign auth", &supported).await.unwrap();
        assert_eq!(e, EffortLevel::Medium);
    }

    #[tokio::test]
    async fn an_unparseable_reply_falls_back_to_medium() {
        // The judge replied with prose and named no label. `classify`
        // catches the framework's `Unparseable` and returns the
        // neutral default rather than erroring — see the method's
        // doc for why.
        let c = classifier("I am not sure.\n", ClassifierMode::FullLadder);
        let e = c.classify("x", &[EffortLevel::Medium]).await.unwrap();
        assert_eq!(e, EffortLevel::Medium);
    }

    #[tokio::test]
    async fn an_unparseable_three_bucket_reply_falls_back_to_medium() {
        // Same fallback on the three-bucket path.
        let c = classifier("I am not sure.\n", ClassifierMode::ThreeBucket);
        let supported = [EffortLevel::Low, EffortLevel::Medium, EffortLevel::High];
        let e = c.classify("x", &supported).await.unwrap();
        assert_eq!(e, EffortLevel::Medium);
    }

    #[tokio::test]
    async fn a_foreign_label_in_three_bucket_falls_back_to_medium() {
        // The judge answers with an effort-tier name (`xhigh`) when
        // the three-bucket question offered `trivial | moderate |
        // hard`. Unparseable, falls back to Medium.
        let c = classifier("bucket: xhigh\n", ClassifierMode::ThreeBucket);
        let supported = [
            EffortLevel::Low,
            EffortLevel::Medium,
            EffortLevel::High,
            EffortLevel::Max,
        ];
        let e = c.classify("x", &supported).await.unwrap();
        assert_eq!(e, EffortLevel::Medium);
    }

    #[tokio::test]
    async fn a_provider_error_is_propagated() {
        struct AlwaysFails;
        #[async_trait]
        impl LlmProvider for AlwaysFails {
            fn name(&self) -> &str {
                "fails"
            }
            async fn list_models(&self) -> KodResult<Vec<ModelInfo>> {
                Ok(vec![])
            }
            async fn generate(
                &self,
                _p: &str,
                _o: &GenerationOptions,
            ) -> KodResult<String> {
                Ok(String::new())
            }
            async fn generate_with_tools(
                &self,
                _p: &str,
                _t: &[ToolDefinition],
                _o: &GenerationOptions,
            ) -> KodResult<GenerationResponse> {
                Err(KodError::Provider("simulated".into()))
            }
            fn stream(
                &self,
                _p: &str,
                _o: &GenerationOptions,
            ) -> Pin<
                Box<
                    dyn futures::Stream<Item = KodResult<StreamChunk>>
                        + Send
                        + '_,
                >,
            > {
                Box::pin(futures::stream::empty())
            }
            async fn complete(
                &self,
                _req: &CompletionRequest,
            ) -> KodResult<GenerationResponse> {
                Err(KodError::Provider("simulated".into()))
            }
        }
        let provider: Arc<dyn LlmProvider> = Arc::new(AlwaysFails);
        let client = JudgmentClient::new(
            provider,
            ModelRef::new("judge", "test"),
            JudgmentOptions::default(),
        );
        let c = AutoThinking::new(client);
        let r = c.classify("x", &[EffortLevel::Medium]).await;
        assert!(r.is_err());
    }

    #[tokio::test]
    async fn an_empty_supported_ladder_collapses_to_medium() {
        // Empty `supported` is treated as `[Medium]`, so the question
        // offers exactly one label. A judge that answers `medium`
        // parses cleanly, and clamp+ceiling on a one-element ladder
        // both return that element.
        let c = classifier("effort: medium\n", ClassifierMode::FullLadder);
        let e = c.classify("anything", &[]).await.unwrap();
        assert_eq!(e, EffortLevel::Medium);
    }

    #[tokio::test]
    async fn an_empty_supported_ladder_with_a_foreign_answer_still_collapses() {
        // Same ladder, but the judge ignores the one offered label
        // and answers something else. The unparseable-to-Medium
        // fallback kicks in, and the one-element ladder keeps it
        // there.
        let c = classifier("effort: max\n", ClassifierMode::FullLadder);
        let e = c.classify("anything", &[]).await.unwrap();
        assert_eq!(e, EffortLevel::Medium);
    }

    // ---- Mode selection -----------------------------------------------

    #[test]
    fn default_mode_is_full_ladder() {
        let provider: Arc<dyn LlmProvider> = Arc::new(FixedJudge::new("x"));
        let client = JudgmentClient::new(
            provider,
            ModelRef::new("j", "m"),
            JudgmentOptions::default(),
        );
        let c = AutoThinking::new(client);
        assert_eq!(c.mode(), ClassifierMode::FullLadder);
    }

    #[test]
    fn the_mode_is_settable() {
        let provider: Arc<dyn LlmProvider> = Arc::new(FixedJudge::new("x"));
        let client = JudgmentClient::new(
            provider,
            ModelRef::new("j", "m"),
            JudgmentOptions::default(),
        );
        let c = AutoThinking::new(client).with_mode(ClassifierMode::ThreeBucket);
        assert_eq!(c.mode(), ClassifierMode::ThreeBucket);
    }

    // ---- Label parsing ------------------------------------------------

    #[test]
    fn label_parsing_matches_the_workspace_convention() {
        assert_eq!(parse_effort_label("max"), EffortLevel::Max);
        assert_eq!(parse_effort_label("xhigh"), EffortLevel::Xhigh);
        assert_eq!(parse_effort_label("X-HIGH"), EffortLevel::Xhigh);
        assert_eq!(parse_effort_label("unknown"), EffortLevel::Medium);
    }
}
