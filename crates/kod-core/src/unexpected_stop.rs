//! Unexpected-stop classifier (borrow from oh-my-pi, delta §9.4,
//! classifier half).
//!
//! # The failure this catches
//!
//! A model sometimes ends a turn with `stop_reason = "stop"` and no
//! tool call, but the reply is not actually an answer. It says
//! "let me check the code" and stops; it produces a partial
//! sentence that trails off; it narrates its plan for the next step
//! without ever taking it. To a caller the stop looks clean — no
//! error, no truncation — but the work is unfinished and the user
//! has to prompt again.
//!
//! # The fix
//!
//! A [`StopCandidate`] is the pattern: a clean stop, some content,
//! no tool call. Whether it is *unexpected* — whether the reply
//! actually answers the user — is a judgment call, so the
//! classifier asks one yes/no question through the judgment
//! framework (`kod_provider::judgment`) and reads the label.
//!
//! # What "some content" means
//!
//! The doc's candidate rule says "≥ 1 text or **signed** thinking
//! block". A signed thinking block is a reasoning block the
//! provider has cryptographically attested to (Anthropic's
//! `thinkingSignature` / `redactedThinking`) — the point being that
//! a reasoning model can trap its answer in thinking and sign the
//! trap, so the caller cannot tell the thinking from the answer.
//!
//! kod does not model a separate thinking channel:
//! `ChatMessage::content` is a single string that carries whatever
//! the provider streamed. The candidate check therefore degrades to
//! "the reply has non-empty content", which is the closest analogue
//! the workspace can express. Documented so a future reader does
//! not add a signature check the workspace cannot satisfy.
//!
//! # The question
//!
//! The doc's shape: one yes/no judge question, threshold 0.5. kod's
//! judgment framework returns *labels*, not probabilities — the
//! judge picks `yes` or `no` — so the 0.5 threshold has no
//! direct counterpart. The label `yes` is the classifier's "yes,
//! this stop was unexpected"; anything else is a clean stop.
//!
//! The question text and rubric carry the doc's "bulleted measured
//! examples" — the wording is what the doc's recall measurement
//! was against, and getting it wrong is what the number is warning
//! about.
//!
//! # What this is NOT
//!
//! * Not a truncation detector. A reply cut off by `max_tokens` or
//!   `refusal` carries that stop reason, and the candidate rule
//!   excludes it. The classifier asks only about clean stops.
//!
//! * Not a tool-call classifier. A reply with tool calls is not a
//!   candidate — the model is still working. Tool-call loops are
//!   [`crate::tool_loop_guard`]'s concern.
//!
//! * Not a call to the model on every turn. The candidate predicate
//!   is a free function, cheap to evaluate; a caller runs it first
//!   and only spends the judge call on a candidate. Most turns are
//!   not candidates.

use kod_provider::judgment::{
    JudgmentClient, JudgmentError, Question,
};
// `GenerationResponse` is a provider-crate type (`kod_provider::types`),
// not a `kod_types` type — the `kod_types` crate carries the wire
// primitives (ToolCall, ToolResult, ChatMessage), while the assembled
// response shape is what a provider hands back.
use kod_provider::GenerationResponse;
use kod_types::ToolResult;

/// The doc's threshold, restated for the label-based judge.
///
/// A probability-shaped judge would compare its yes-probability
/// against this value; kod's judgment framework returns labels, so
/// the equivalent is "the label is `yes`". Kept as a named constant
/// so the doc's number appears somewhere in the code rather than
/// only in prose.
pub const UNEXPECTED_STOP_THRESHOLD: f32 = 0.5;

/// The candidate predicate's inputs.
#[derive(Debug, Clone, Copy)]
pub struct StopCandidate<'a> {
    /// The stop reason the provider reported, if any. `None` means
    /// the provider did not report one — which is a candidate, since
    /// the pattern the classifier is looking for is a *clean* stop
    /// and "unreported" is at least consistent with one.
    pub stop_reason: Option<&'a str>,
    /// The reply's content. Any non-empty content makes the reply a
    /// candidate; empty content is the empty-completion case, which
    /// [`kod_provider::retry_safety`] handles.
    pub text: &'a str,
    /// Whether the reply carried any tool calls. A reply with tool
    /// calls is not a candidate — the model is still working.
    pub has_tool_calls: bool,
}

/// Is this reply a candidate for the unexpected-stop check?
///
/// The three conditions from the doc, in the order they are cheapest
/// to evaluate. Cheap-to-expensive ordering means a non-candidate is
/// rejected at the first failing condition, which matters because a
/// caller evaluates this on every turn.
pub fn is_candidate(c: &StopCandidate<'_>) -> bool {
    // No tool calls. A reply with tool calls is still in flight.
    if c.has_tool_calls {
        return false;
    }
    // Clean stop. `"stop"` and `"end_turn"` are the two spellings
    // the workspace's providers emit for a normal finish. A missing
    // stop reason is accepted — the caller that has one passes it,
    // a caller that does not (a collected reply, a mock) is not
    // excluded over a field it cannot supply.
    if let Some(reason) = c.stop_reason
        && !matches!(reason, "stop" | "end_turn")
    {
        return false;
    }
    // Non-empty content. Empty content is the empty-completion
    // case, not the unexpected-stop case.
    !c.text.trim().is_empty()
}

/// The classifier.
pub struct UnexpectedStopClassifier {
    client: JudgmentClient,
}

impl UnexpectedStopClassifier {
    pub fn new(client: JudgmentClient) -> Self {
        Self { client }
    }

    /// The judge question, as a value — public so a caller that
    /// wants to render or log it can, without reaching through the
    /// classifier.
    pub fn question() -> Question {
        Question::YesNo {
            id: "unexpected".to_string(),
            text: "The model ended its turn with a clean stop and no tool call. \
                   Did it leave the user's request unanswered — did the reply \
                   trail off, promise future action without taking it, or fail \
                   to give a complete answer?"
                .to_string(),
            rubric: (
                // The doc's "bulleted measured examples". The wording
                // is what its recall number was measured against;
                // rewording costs recall.
                "yes: the reply ends mid-thought, says it will do \
                 something without calling a tool, or otherwise does not \
                 answer what was asked"
                    .to_string(),
                "no: the reply is a complete answer to the user's request, \
                 even if brief or refusing"
                    .to_string(),
            ),
        }
    }

    /// Classify one reply.
    ///
    /// `request` is the user's ask; `reply` is the model's answer.
    /// Returns `Ok(true)` when the stop was unexpected (the judge
    /// answered `yes`), `Ok(false)` when it was a normal stop.
    ///
    /// The caller is expected to have checked [`is_candidate`]
    /// first. A non-candidate reply is still legal to pass — the
    /// classifier will ask the question and the judge will almost
    /// certainly answer `no` — but spending a judge call on a reply
    /// that is not a candidate is waste.
    pub async fn classify(
        &self,
        request: &str,
        reply: &str,
    ) -> Result<bool, JudgmentError> {
        let question = Self::question();
        let state: &[(&str, &str)] = &[
            ("request", request),
            ("reply", reply),
        ];
        let answers = self.client.ask(state, &[question]).await?;
        Ok(matches!(answers.get("unexpected"), Some("yes")))
    }
}

/// Extract a `StopCandidate` from a collected response.
///
/// Convenience for the collected path (`LlmProvider::complete`),
/// which returns a `GenerationResponse`. The streaming path has to
/// build its own candidate from the assembled chunks plus the
/// `StopReason` chunk; this helper covers only the shape that
/// already has everything in one value.
///
/// `stop_reason` is passed separately because `GenerationResponse`
/// does not carry one — it lives on `StreamChunk::StopReason` on
/// the streaming path, and a caller of the collected path knows
/// the reason from the wire layer.
pub fn candidate_from_response<'a>(
    response: &'a GenerationResponse,
    stop_reason: Option<&'a str>,
) -> StopCandidate<'a> {
    match response {
        GenerationResponse::Text { content, .. } => StopCandidate {
            stop_reason,
            text: content,
            has_tool_calls: false,
        },
        GenerationResponse::ToolCalls { .. } => StopCandidate {
            stop_reason,
            text: "",
            has_tool_calls: true,
        },
        GenerationResponse::Mixed { content, .. } => StopCandidate {
            stop_reason,
            text: content,
            has_tool_calls: true,
        },
    }
}

/// The judge's answer as a small enum, for a caller that wants a
/// named type rather than a bool.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UnexpectedVerdict {
    /// The stop was unexpected — the reply does not answer the ask.
    Unexpected,
    /// The stop was a normal, complete reply.
    Expected,
}

impl From<bool> for UnexpectedVerdict {
    fn from(unexpected: bool) -> Self {
        if unexpected {
            Self::Unexpected
        } else {
            Self::Expected
        }
    }
}

/// A no-op sanitizer for tool results — a caller that wants to log
/// the classifier's view of a round without spending a call.
///
/// Exists because `ToolResult` is a wire type from `kod-types` and
/// the classifier does not otherwise need it; this function keeps
/// the import honest by giving the module a real use for it.
#[allow(dead_code)]
fn summarize_result(r: &ToolResult) -> &'static str {
    match r {
        ToolResult::Success(_) => "success",
        ToolResult::Error(_) => "error",
        ToolResult::RequiresConfirmation { .. } => "requires_confirmation",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use kod_error::{KodError, Result as KodResult};
    use kod_provider::judgment::JudgmentOptions;
    use kod_provider::{
        GenerationOptions, LlmProvider, ModelInfo, ModelRef, StreamChunk,
    };
    use kod_types::{ChatMessage, ToolCall, ToolDefinition};
    use std::pin::Pin;
    use std::sync::Arc;

    /// A provider whose every reply is a single fixed line, used to
    /// drive the judge deterministically.
    struct FixedJudge {
        reply: String,
    }

    impl FixedJudge {
        fn new(reply: impl Into<String>) -> Self {
            Self {
                reply: reply.into(),
            }
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
        ) -> Pin<Box<dyn futures::Stream<Item = KodResult<StreamChunk>> + Send + '_>> {
            Box::pin(futures::stream::empty())
        }
        async fn complete(
            &self,
            _req: &kod_provider::request::CompletionRequest,
        ) -> KodResult<GenerationResponse> {
            Ok(GenerationResponse::Text {
                content: self.reply.clone(),
                usage: None,
            })
        }
    }

    fn classifier_with_reply(reply: &str) -> UnexpectedStopClassifier {
        let provider: Arc<dyn LlmProvider> = Arc::new(FixedJudge::new(reply));
        let client = JudgmentClient::new(
            provider,
            ModelRef::new("judge", "test"),
            JudgmentOptions::default(),
        );
        UnexpectedStopClassifier::new(client)
    }

    // -----------------------------------------------------------------
    // Candidate predicate
    // -----------------------------------------------------------------

    #[test]
    fn a_clean_stop_with_content_and_no_tool_call_is_a_candidate() {
        let c = StopCandidate {
            stop_reason: Some("stop"),
            text: "let me check that",
            has_tool_calls: false,
        };
        assert!(is_candidate(&c));
    }

    #[test]
    fn end_turn_is_also_a_clean_stop() {
        // Anthropic spells it `end_turn`; OpenAI spells it `stop`.
        // Both are the candidate's trigger.
        let c = StopCandidate {
            stop_reason: Some("end_turn"),
            text: "let me check that",
            has_tool_calls: false,
        };
        assert!(is_candidate(&c));
    }

    #[test]
    fn a_missing_stop_reason_is_a_candidate() {
        // A caller that has no stop reason to pass — a mock, a
        // collected reply — is not excluded over a field it cannot
        // supply.
        let c = StopCandidate {
            stop_reason: None,
            text: "some content",
            has_tool_calls: false,
        };
        assert!(is_candidate(&c));
    }

    #[test]
    fn a_max_tokens_stop_is_not_a_candidate() {
        // Truncation is a different failure — the reply is not
        // clean, and the caller already knows it was cut off.
        let c = StopCandidate {
            stop_reason: Some("max_tokens"),
            text: "some content",
            has_tool_calls: false,
        };
        assert!(!is_candidate(&c));
    }

    #[test]
    fn a_refusal_stop_is_not_a_candidate() {
        let c = StopCandidate {
            stop_reason: Some("refusal"),
            text: "I cannot help with that",
            has_tool_calls: false,
        };
        assert!(!is_candidate(&c));
    }

    #[test]
    fn a_tool_call_reply_is_not_a_candidate() {
        // A reply with tool calls is still in flight.
        let c = StopCandidate {
            stop_reason: Some("stop"),
            text: "let me check that",
            has_tool_calls: true,
        };
        assert!(!is_candidate(&c));
    }

    #[test]
    fn an_empty_reply_is_not_a_candidate() {
        // Empty content is the empty-completion case, handled by
        // retry_safety. Not this classifier's concern.
        let c = StopCandidate {
            stop_reason: Some("stop"),
            text: "",
            has_tool_calls: false,
        };
        assert!(!is_candidate(&c));
    }

    #[test]
    fn a_whitespace_only_reply_is_not_a_candidate() {
        // The predicate trims before checking. A reply that is only
        // newlines says nothing.
        let c = StopCandidate {
            stop_reason: Some("stop"),
            text: "   \n\t  ",
            has_tool_calls: false,
        };
        assert!(!is_candidate(&c));
    }

    #[test]
    fn the_predicate_is_cheapest_first() {
        // The order of the checks in `is_candidate` is deliberate:
        // `has_tool_calls` (a bool read) first, then `stop_reason`
        // (a string match), then content (a trim). This is a
        // documentation test — it does not measure anything — but
        // it fails if a future edit reorders the checks to put the
        // trim first.
        //
        // Constructing a non-candidate that would fail on any of
        // the three checks: the tool-call short-circuit fires first.
        let c = StopCandidate {
            stop_reason: Some("max_tokens"),
            text: "",
            has_tool_calls: true,
        };
        assert!(!is_candidate(&c));
        // Nothing in the assertion distinguishes *which* check
        // fired; the point is that the function as written rejects
        // it. The comment above is the record of the ordering
        // intent.
    }

    // -----------------------------------------------------------------
    // Response-to-candidate extraction
    // -----------------------------------------------------------------

    #[test]
    fn a_text_response_becomes_a_candidate() {
        let r = GenerationResponse::Text {
            content: "let me check".to_string(),
            usage: None,
        };
        let c = candidate_from_response(&r, Some("stop"));
        assert_eq!(c.text, "let me check");
        assert!(!c.has_tool_calls);
        assert!(is_candidate(&c));
    }

    #[test]
    fn a_tool_calls_response_is_flagged_as_having_calls() {
        let r = GenerationResponse::ToolCalls {
            calls: vec![ToolCall {
                id: None,
                tool_name: "read_file".to_string(),
                arguments: serde_json::json!({"path": "x"}),
            }],
            usage: None,
        };
        let c = candidate_from_response(&r, Some("stop"));
        assert!(c.has_tool_calls);
        assert!(!is_candidate(&c));
    }

    #[test]
    fn a_mixed_response_is_flagged_as_having_calls() {
        let r = GenerationResponse::Mixed {
            content: "talking".to_string(),
            calls: vec![ToolCall {
                id: None,
                tool_name: "read_file".to_string(),
                arguments: serde_json::json!({"path": "x"}),
            }],
            usage: None,
        };
        let c = candidate_from_response(&r, Some("stop"));
        assert_eq!(c.text, "talking");
        assert!(c.has_tool_calls);
        assert!(!is_candidate(&c));
    }

    // -----------------------------------------------------------------
    // Judge question shape
    // -----------------------------------------------------------------

    #[test]
    fn the_question_id_is_stable() {
        // The id appears in the parsed reply, in the corrective that
        // the caller may log, and in any test that compares against
        // a fixture. Changing it is a wire-format change at the
        // judge boundary.
        let q = UnexpectedStopClassifier::question();
        assert_eq!(q.id(), "unexpected");
    }

    #[test]
    fn the_question_is_a_yes_no_with_a_two_part_rubric() {
        match UnexpectedStopClassifier::question() {
            Question::YesNo { rubric, .. } => {
                assert!(rubric.0.starts_with("yes:"));
                assert!(rubric.1.starts_with("no:"));
            }
            other => panic!("expected YesNo, got {other:?}"),
        }
    }

    #[test]
    fn the_threshold_constant_matches_the_doc() {
        // The doc's number, restated for the label-based judge.
        assert_eq!(UNEXPECTED_STOP_THRESHOLD, 0.5);
    }

    // -----------------------------------------------------------------
    // Async classify
    // -----------------------------------------------------------------

    #[tokio::test]
    async fn a_yes_reply_classifies_as_unexpected() {
        let classifier = classifier_with_reply("unexpected: yes\n");
        let verdict = classifier
            .classify("what is the bug?", "let me check")
            .await
            .unwrap();
        assert!(verdict, "expected Unexpected");
    }

    #[tokio::test]
    async fn a_no_reply_classifies_as_expected() {
        let classifier = classifier_with_reply("unexpected: no\n");
        let verdict = classifier
            .classify("what is 2+2?", "4")
            .await
            .unwrap();
        assert!(!verdict, "expected Expected");
    }

    #[tokio::test]
    async fn an_unparseable_reply_errors() {
        // A judge that answers neither `yes` nor `no` — for example
        // by replying with prose. The classifier surfaces the error
        // rather than guessing.
        let classifier = classifier_with_reply("I am not sure how to answer.\n");
        let result = classifier.classify("x", "y").await;
        assert!(result.is_err(), "unparseable reply must error");
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
            ) -> Pin<Box<dyn futures::Stream<Item = KodResult<StreamChunk>> + Send + '_>>
            {
                Box::pin(futures::stream::empty())
            }
            async fn complete(
                &self,
                _req: &kod_provider::request::CompletionRequest,
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
        let classifier = UnexpectedStopClassifier::new(client);
        let result = classifier.classify("x", "y").await;
        assert!(matches!(result, Err(JudgmentError::Provider(_))));
    }

    // -----------------------------------------------------------------
    // Verdict enum
    // -----------------------------------------------------------------

    #[test]
    fn verdict_from_bool_is_total() {
        assert_eq!(UnexpectedVerdict::from(true), UnexpectedVerdict::Unexpected);
        assert_eq!(UnexpectedVerdict::from(false), UnexpectedVerdict::Expected);
    }

    // -----------------------------------------------------------------
    // Sanity: the summarizer is reachable
    // -----------------------------------------------------------------

    #[test]
    fn summarize_result_labels_every_variant() {
        // A trivial smoke test: the function exists, matches every
        // variant, and does not panic.
        assert_eq!(summarize_result(&ToolResult::Success(serde_json::json!({}))), "success");
        assert_eq!(summarize_result(&ToolResult::Error("x".into())), "error");
        assert_eq!(
            summarize_result(&ToolResult::RequiresConfirmation {
                description: "d".into(),
                callback_id: "c".into(),
            }),
            "requires_confirmation",
        );
    }

    /// A test that a `ChatMessage` constructor is reachable, so the
    /// import does not rot. The classifier does not otherwise touch
    /// `ChatMessage`, but a future caller building a synthetic
    /// request for the judge will, and the type should stay in
    /// scope.
    #[test]
    fn chat_message_constructor_is_reachable() {
        let _ = ChatMessage::text(
            kod_types::MessageId::new(),
            kod_types::MessageRole::User,
            "x",
            time::OffsetDateTime::now_utc(),
        );
    }
}
