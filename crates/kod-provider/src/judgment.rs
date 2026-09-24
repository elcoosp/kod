//! Judgment framework (borrow from oh-my-pi, delta §9.5).
//!
//! # What this is
//!
//! A batch of typed questions answered by an LLM in a single call.
//! Rather than parse one prompt per question, the framework renders
//! every question into a **byte-stable** system prompt, puts the
//! state in a separate user message, asks for one `id: label` line
//! per question, and parses the reply. The byte-stable prompt is a
//! prompt-cache target; a caller that asks the same question set
//! across turns pays for it once.
//!
//! # Three question kinds
//!
//! * [`Question::YesNo`] — a binary judgement. Labels are always
//!   `yes` and `no`; the caller supplies a rubric describing what
//!   each means.
//! * [`Question::Choice`] — one of N named alternatives. Each
//!   criterion carries a label and a description.
//! * [`Question::Score`] — one level on an ordered rubric. Levels
//!   are given weakest-to-strongest; the label that matches is the
//!   model's score.
//!
//! # Parsing
//!
//! The reply is scanned line-by-line. Each `id: label` line is
//! matched to a question by id; the label part is matched against
//! the question's allowed labels using *earliest whole-word match,
//! longest label wins tie*. That rule exists for label pairs like
//! `high` and `xhigh`: `xhigh` contains `high` as a substring, but
//! whole-word matching means only `xhigh` matches `xhigh`, and
//! `high` matches `high` but not `xhigh`. The "longest wins" tie-
//! break handles the case where both labels happen to match the
//! same position, which can happen with a caller that includes
//! overlapping labels.
//!
//! # Retry
//!
//! A single retry fires when the first reply leaves any question
//! unanswered. The retry appends a strict reminder to the user
//! message. The design note's original plan was to escalate to a
//! `submit_judgment` tool with a forced `toolChoice` — kod's
//! `GenerationOptions` has no `forced_tool_choice` field today and
//! neither provider honors one, so the retry leans on instruction
//! alone. When that field lands, the retry gains a second leg.
//!
//! # What this is NOT
//!
//! * Not a chat interface. The framework is one-shot: state in,
//!   answers out. Multi-turn judgement is a caller concern.
//! * Not a scorer. "Is this answer high quality?" is a caller's
//!   question to encode; the framework just gets the label back.
//! * Not deterministic on the model's side. Two calls can produce
//!   different labels; the framework's own rendering and parsing
//!   are deterministic (byte-stable prompts, exact-match
//!   parsing), which is what makes the retry, cache, and tests
//!   all work.

use std::collections::HashMap;
use std::sync::Arc;

use crate::request::{CompletionRequest, ModelRef, SystemPrompt};
use crate::traits::{GenerationOptions, LlmProvider};
use crate::types::GenerationResponse;
use kod_error::KodError;

/// Tunables for one judgment call.
#[derive(Debug, Clone, Copy)]
pub struct JudgmentOptions {
    /// Maximum tokens for the reply. The design note's number:
    /// 4096, chosen to exceed Anthropic's 1024-token minimum
    /// thinking budget even when reasoning is "disabled".
    pub max_tokens: usize,
    /// Sampling temperature. 0.0 — judgement should be
    /// deterministic given the same state and questions.
    pub temperature: f32,
    /// How many format-correction retries on a partial parse.
    /// Default 1; a caller that wants aggressive correction sets
    /// higher, one that trusts the model sets 0.
    pub max_format_retries: u32,
}

impl Default for JudgmentOptions {
    fn default() -> Self {
        Self {
            max_tokens: 4096,
            temperature: 0.0,
            max_format_retries: 1,
        }
    }
}

/// One question to ask the judge.
///
/// The `id` is a single token (no whitespace). It is how the model
/// refers to the question in its reply and how the caller reads the
/// answer back. Validated at render time.
#[derive(Debug, Clone)]
pub enum Question {
    /// A yes/no question.
    YesNo {
        id: String,
        text: String,
        /// Descriptions of what `yes` and `no` mean. The model
        /// reads these to decide.
        rubric: (String, String),
    },
    /// A choice among named alternatives.
    Choice {
        id: String,
        text: String,
        /// `(label, description)`. Order preserved for rendering.
        criteria: Vec<(String, String)>,
    },
    /// A score on an ordered rubric.
    Score {
        id: String,
        text: String,
        /// `(label, description)`, ordered weakest to strongest.
        levels: Vec<(String, String)>,
    },
}

impl Question {
    /// The question's id.
    pub fn id(&self) -> &str {
        match self {
            Self::YesNo { id, .. }
            | Self::Choice { id, .. }
            | Self::Score { id, .. } => id,
        }
    }

    /// The question text, as shown to the model.
    pub fn text(&self) -> &str {
        match self {
            Self::YesNo { text, .. }
            | Self::Choice { text, .. }
            | Self::Score { text, .. } => text,
        }
    }

    /// Every label the model may answer with, in rendering order.
    ///
    /// `YesNo` always yields `["yes", "no"]`. `Choice` and `Score`
    /// yield their labels verbatim.
    pub fn labels(&self) -> Vec<&str> {
        match self {
            Self::YesNo { .. } => vec!["yes", "no"],
            Self::Choice { criteria, .. } => {
                criteria.iter().map(|(l, _)| l.as_str()).collect()
            }
            Self::Score { levels, .. } => {
                levels.iter().map(|(l, _)| l.as_str()).collect()
            }
        }
    }

    /// Render this question's block for the system prompt.
    fn render_block(&self) -> String {
        let mut out = String::new();
        out.push('[');
        out.push_str(self.id());
        out.push_str("] ");
        out.push_str(self.text());
        out.push('\n');

        let labels = self.labels();
        out.push_str("  labels: ");
        for (i, l) in labels.iter().enumerate() {
            if i > 0 {
                out.push_str(" | ");
            }
            out.push_str(l);
        }
        out.push('\n');

        match self {
            Self::YesNo { rubric, .. } => {
                out.push_str("  yes: ");
                out.push_str(&rubric.0);
                out.push('\n');
                out.push_str("  no: ");
                out.push_str(&rubric.1);
                out.push('\n');
            }
            Self::Choice { criteria, .. } => {
                for (label, desc) in criteria {
                    out.push_str("  ");
                    out.push_str(label);
                    out.push_str(": ");
                    out.push_str(desc);
                    out.push('\n');
                }
            }
            Self::Score { levels, .. } => {
                for (label, desc) in levels {
                    out.push_str("  ");
                    out.push_str(label);
                    out.push_str(": ");
                    out.push_str(desc);
                    out.push('\n');
                }
            }
        }
        out
    }

    /// Check the question is well-formed enough to render and match.
    ///
    /// * id is non-empty and has no whitespace (a single token).
    /// * text is non-empty.
    /// * the question yields at least one label.
    /// * every label is non-empty, has no whitespace, and is unique
    ///   within the question.
    fn validate(&self) -> Result<(), JudgmentError> {
        let id = self.id();
        if id.is_empty() {
            return Err(JudgmentError::Malformed(
                "question id must not be empty".to_string(),
            ));
        }
        if id.chars().any(char::is_whitespace) {
            return Err(JudgmentError::Malformed(format!(
                "question id {id:?} must be a single token (no whitespace)"
            )));
        }
        if self.text().trim().is_empty() {
            return Err(JudgmentError::Malformed(format!(
                "question {id:?} has empty text"
            )));
        }
        let labels = self.labels();
        if labels.is_empty() {
            return Err(JudgmentError::Malformed(format!(
                "question {id:?} has no labels"
            )));
        }
        let mut seen = std::collections::HashSet::new();
        for l in &labels {
            if l.is_empty() {
                return Err(JudgmentError::Malformed(format!(
                    "question {id:?} has an empty label"
                )));
            }
            if l.chars().any(char::is_whitespace) {
                return Err(JudgmentError::Malformed(format!(
                    "question {id:?} label {l:?} must be a single token"
                )));
            }
            if !seen.insert(*l) {
                return Err(JudgmentError::Malformed(format!(
                    "question {id:?} has duplicate label {l:?}"
                )));
            }
        }
        Ok(())
    }
}

/// Render the byte-stable system prompt for a question set.
///
/// "Byte-stable" means: given the same `questions`, this function
/// returns the same `String` bytes every time. That property is what
/// lets a provider with prefix caching (OpenAI-compatible automatic,
/// Anthropic explicit) bill the preamble once per session rather
/// than once per call.
///
/// Fails on an empty question set or a malformed question.
pub fn render_system_prompt(questions: &[Question]) -> Result<String, JudgmentError> {
    if questions.is_empty() {
        return Err(JudgmentError::Malformed(
            "at least one question is required".to_string(),
        ));
    }
    for q in questions {
        q.validate()?;
    }
    let mut seen_ids = std::collections::HashSet::new();
    for q in questions {
        if !seen_ids.insert(q.id()) {
            return Err(JudgmentError::Malformed(format!(
                "duplicate question id {:?}",
                q.id()
            )));
        }
    }

    let mut out = String::new();
    out.push_str(
        "You answer a batch of judgment questions about a state. For \
         each question listed below, reply with exactly one line:\n\
         \n\
         \x20 <id>: <label>\n\
         \n\
         Choose <label> from the labels listed for that question, \
         spelled exactly as shown. Do not add prose, preamble, or \
         explanation.\n\
         \n\
         Questions:\n\
         \n",
    );
    for q in questions {
        out.push_str(&q.render_block());
        out.push('\n');
    }
    Ok(out)
}

/// Render the user message: state as XML fields plus a short cue.
///
/// The state is a slice of `(field_name, field_value)` pairs. XML
/// special characters in either are escaped, so a state that itself
/// contains `<` or `&` does not produce malformed XML for the model
/// to trip on.
pub fn render_user_message(state: &[(&str, &str)], questions: &[Question]) -> String {
    let mut out = String::from("<state>\n");
    for (name, value) in state {
        out.push_str("  <field name=\"");
        out.push_str(&escape_xml(name));
        out.push_str("\">");
        out.push_str(&escape_xml(value));
        out.push_str("</field>\n");
    }
    out.push_str("</state>\n\n");
    out.push_str("Answer each question on one line:\n");
    for q in questions {
        out.push_str("  ");
        out.push_str(q.id());
        out.push_str(": <label>\n");
    }
    out
}

fn escape_xml(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '&' => out.push_str("&amp;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&apos;"),
            other => out.push(other),
        }
    }
    out
}

/// The parsed answers from one reply: id → label.
///
/// A question whose answer did not parse is simply absent from the
/// map. A caller that requires every answer checks
/// [`Answers::missing`].
#[derive(Debug, Clone, Default)]
pub struct Answers {
    values: HashMap<String, String>,
}

impl Answers {
    /// The answer to `id`, if any.
    pub fn get(&self, id: &str) -> Option<&str> {
        self.values.get(id).map(String::as_str)
    }

    /// How many questions got an answer.
    pub fn len(&self) -> usize {
        self.values.len()
    }

    pub fn is_empty(&self) -> bool {
        self.values.is_empty()
    }

    /// Every question id that did not get a parseable answer, in the
    /// order the questions were supplied.
    ///
    /// The returned `&str`s borrow from `questions`, not from
    /// `self` — the ids come from the question list, not from the
    /// answers map. The named lifetime makes that explicit and lets
    /// the caller keep the returned vector alive independently of
    /// the `Answers`.
    pub fn missing<'a>(&self, questions: &'a [Question]) -> Vec<&'a str> {
        questions
            .iter()
            .filter(|q| !self.values.contains_key(q.id()))
            .map(Question::id)
            .collect()
    }

    /// The raw map, for a caller that wants to iterate.
    pub fn iter(&self) -> impl Iterator<Item = (&str, &str)> {
        self.values.iter().map(|(k, v)| (k.as_str(), v.as_str()))
    }
}

/// Parse a reply against the question set.
///
/// Line-based. Each line is scanned for `<id>: <label>`; the id is
/// the trimmed text before the first `:`, the label part is the
/// trimmed text after. Unknown ids are ignored (a model that adds
/// prose with a colon in it is not penalized). For each known id,
/// the label part is matched against the question's allowed labels
/// with earliest-whole-word, longest-wins-tie.
///
/// Deterministic. A partial parse is not an error — the caller
/// checks [`Answers::missing`] and decides whether to retry.
pub fn parse_response(text: &str, questions: &[Question]) -> Answers {
    let mut by_id: HashMap<&str, &Question> = HashMap::new();
    for q in questions {
        by_id.insert(q.id(), q);
    }

    let mut out = Answers::default();
    for line in text.lines() {
        let Some(colon) = line.find(':') else { continue };
        let id = line[..colon].trim();
        let rest = line[colon + 1..].trim();
        let Some(q) = by_id.get(id) else { continue };
        if out.values.contains_key(id) {
            // First occurrence wins. A model that repeats a line
            // (a stop-sequence stumble, a mid-stream restart) does
            // not overwrite the answer it already gave.
            continue;
        }
        if let Some(label) = best_label_match(rest, &q.labels()) {
            out.values.insert(id.to_string(), label);
        }
    }
    out
}

/// The label that best matches `haystack`: earliest whole-word
/// occurrence; on a position tie, the longer label.
fn best_label_match(haystack: &str, labels: &[&str]) -> Option<String> {
    let mut best: Option<(usize, &str)> = None;
    for label in labels {
        if let Some(pos) = earliest_whole_word(haystack, label) {
            match best {
                None => best = Some((pos, label)),
                Some((bp, bl)) => {
                    if pos < bp || (pos == bp && label.len() > bl.len()) {
                        best = Some((pos, label));
                    }
                }
            }
        }
    }
    best.map(|(_, l)| l.to_string())
}

/// Earliest byte offset at which `needle` appears as a whole word.
///
/// Whole-word means: the character immediately before is not a word
/// character (`alphanumeric` or `_`) and the character immediately
/// after is not a word character. `high` matches `very high` but
/// not `xhigh` or `higher`.
fn earliest_whole_word(haystack: &str, needle: &str) -> Option<usize> {
    if needle.is_empty() {
        return None;
    }
    let needle_len = needle.len();
    for (i, _) in haystack.char_indices() {
        if !haystack[i..].starts_with(needle) {
            continue;
        }
        let before_ok = if i == 0 {
            true
        } else {
            haystack[..i]
                .chars()
                .next_back()
                .map(|c| !is_word_char(c))
                .unwrap_or(true)
        };
        let after = i + needle_len;
        let after_ok = if after >= haystack.len() {
            true
        } else {
            haystack[after..]
                .chars()
                .next()
                .map(|c| !is_word_char(c))
                .unwrap_or(true)
        };
        if before_ok && after_ok {
            return Some(i);
        }
    }
    None
}

fn is_word_char(c: char) -> bool {
    c.is_alphanumeric() || c == '_'
}

/// Why a judgment call failed.
#[derive(Debug)]
pub enum JudgmentError {
    /// The provider call failed.
    Provider(KodError),
    /// A question set or question was structurally invalid.
    Malformed(String),
    /// Every retry produced a reply whose parse left one or more
    /// questions unanswered.
    Unparseable {
        /// The raw reply from the last attempt, for the caller to log
        /// or inspect.
        raw: String,
        /// Which question ids were missing.
        missing: Vec<String>,
    },
}

impl std::fmt::Display for JudgmentError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Provider(e) => write!(f, "provider error: {e}"),
            Self::Malformed(m) => write!(f, "malformed judgment: {m}"),
            Self::Unparseable { missing, .. } => write!(
                f,
                "unparseable judgment: {} question(s) unanswered: {missing:?}",
                missing.len(),
            ),
        }
    }
}

impl std::error::Error for JudgmentError {}

impl From<KodError> for JudgmentError {
    fn from(e: KodError) -> Self {
        Self::Provider(e)
    }
}

/// The async client. Wraps a provider and a model, does the call,
/// retries the format correction when needed.
///
/// Cheap to clone via `Arc` if a caller wants to share one across
/// tasks. The provider is behind `Arc<dyn LlmProvider>` — the same
/// shape the registry already hands out — so constructing one from a
/// registry entry is a single clone.
pub struct JudgmentClient {
    provider: Arc<dyn LlmProvider>,
    model: ModelRef,
    options: JudgmentOptions,
}

impl JudgmentClient {
    pub fn new(
        provider: Arc<dyn LlmProvider>,
        model: ModelRef,
        options: JudgmentOptions,
    ) -> Self {
        Self {
            provider,
            model,
            options,
        }
    }

    /// Ask `questions` about `state`. Returns the parsed answers.
    ///
    /// Every question is expected to be answered on the first
    /// attempt; if any is missing, the call retries up to
    /// `options.max_format_retries` times with a stricter reminder.
    /// Exhausting the retries without a complete parse is
    /// [`JudgmentError::Unparseable`].
    pub async fn ask(
        &self,
        state: &[(&str, &str)],
        questions: &[Question],
    ) -> Result<Answers, JudgmentError> {
        let system = render_system_prompt(questions)?;
        let mut user = render_user_message(state, questions);

        let mut attempts_left = self.options.max_format_retries.saturating_add(1);
        let mut last_raw = String::new();
        let mut last_missing: Vec<String> = Vec::new();

        while attempts_left > 0 {
            attempts_left -= 1;

            let mut req = CompletionRequest::new(self.model.clone());
            req.system = SystemPrompt::new().with(system.clone(), true);
            req.messages = vec![kod_types::ChatMessage::text(
                kod_types::MessageId::new(),
                kod_types::MessageRole::User,
                user.clone(),
                time::OffsetDateTime::now_utc(),
            )];
            req.options = GenerationOptions {
                max_tokens: Some(self.options.max_tokens),
                temperature: Some(self.options.temperature),
                ..Default::default()
            };

            let response = self
                .provider
                .complete(&req)
                .await
                .map_err(JudgmentError::Provider)?;

            let text = match response {
                GenerationResponse::Text { content, .. } => content,
                GenerationResponse::ToolCalls { .. } => {
                    // The judge returned a tool call instead of text.
                    // Forced-tool-choice isn't wired yet, so this
                    // means the model misbehaved; treat the reply as
                    // empty and retry with the strict reminder.
                    String::new()
                }
                GenerationResponse::Mixed { content, .. } => content,
            };

            let answers = parse_response(&text, questions);
            let missing = answers.missing(questions);

            if missing.is_empty() {
                return Ok(answers);
            }

            last_raw = text;
            last_missing = missing.iter().map(|s| s.to_string()).collect();

            // Strict reminder for the next attempt. Rewrites the user
            // message with the same state but a sharper instruction.
            if attempts_left > 0 {
                user = format!(
                    "{user}\n\nYour previous reply did not answer every \
                     question. Reply with exactly one line per question, \
                     of the form `id: label`. Questions left unanswered: {}.\n",
                    last_missing.join(", "),
                );
            }
        }

        Err(JudgmentError::Unparseable {
            raw: last_raw,
            missing: last_missing,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use kod_error::Result as KodResult;
    use kod_types::ToolDefinition;
    use std::sync::Mutex;

    // -----------------------------------------------------------------
    // Test fixtures
    // -----------------------------------------------------------------

    fn yes_no(id: &str) -> Question {
        Question::YesNo {
            id: id.to_string(),
            text: format!("Is {id} true?"),
            rubric: ("it is true".to_string(), "it is not true".to_string()),
        }
    }

    fn choice_abcd(id: &str) -> Question {
        Question::Choice {
            id: id.to_string(),
            text: format!("Which option for {id}?"),
            criteria: vec![
                ("a".to_string(), "option a".to_string()),
                ("b".to_string(), "option b".to_string()),
                ("c".to_string(), "option c".to_string()),
                ("d".to_string(), "option d".to_string()),
            ],
        }
    }

    fn score_effort(id: &str) -> Question {
        Question::Score {
            id: id.to_string(),
            text: "How much effort?".to_string(),
            levels: vec![
                ("low".to_string(), "a little".to_string()),
                ("high".to_string(), "a lot".to_string()),
                ("xhigh".to_string(), "the most".to_string()),
            ],
        }
    }

    /// A provider whose every reply is scripted, consumed in order.
    /// The last reply repeats when the queue drains. Captures the
    /// rendered system prompt for byte-stability assertions.
    struct ScriptedJudge {
        replies: Mutex<Vec<String>>,
        system_prompts: Mutex<Vec<String>>,
    }

    impl ScriptedJudge {
        fn new(replies: Vec<String>) -> Self {
            assert!(!replies.is_empty(), "need at least one scripted reply");
            Self {
                replies: Mutex::new(replies),
                system_prompts: Mutex::new(Vec::new()),
            }
        }
    }

    #[async_trait]
    impl LlmProvider for ScriptedJudge {
        fn name(&self) -> &str {
            "scripted-judge"
        }
        async fn list_models(&self) -> KodResult<Vec<crate::ModelInfo>> {
            Ok(vec![])
        }
        async fn generate(
            &self,
            _prompt: &str,
            _options: &GenerationOptions,
        ) -> KodResult<String> {
            Ok(String::new())
        }
        async fn generate_with_tools(
            &self,
            _prompt: &str,
            _tools: &[ToolDefinition],
            _options: &GenerationOptions,
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
        ) -> std::pin::Pin<
            Box<dyn futures::Stream<Item = KodResult<crate::StreamChunk>> + Send + '_>,
        > {
            Box::pin(futures::stream::empty())
        }
        async fn complete(
            &self,
            req: &CompletionRequest,
        ) -> KodResult<GenerationResponse> {
            self.system_prompts
                .lock()
                .unwrap()
                .push(req.system.render_text());
            let mut q = self.replies.lock().unwrap();
            let reply = if q.len() > 1 {
                q.remove(0)
            } else {
                q[0].clone()
            };
            Ok(GenerationResponse::Text {
                content: reply,
                usage: None,
            })
        }
    }

    // -----------------------------------------------------------------
    // System prompt rendering
    // -----------------------------------------------------------------

    #[test]
    fn the_system_prompt_is_byte_stable() {
        // Two calls with the same question set must produce the same
        // bytes. This is the property the provider's prefix cache
        // depends on.
        let qs = vec![yes_no("stuck"), choice_abcd("direction")];
        let a = render_system_prompt(&qs).unwrap();
        let b = render_system_prompt(&qs).unwrap();
        assert_eq!(a, b);
    }

    #[test]
    fn the_system_prompt_carries_every_question() {
        let qs = vec![yes_no("a"), choice_abcd("b"), score_effort("c")];
        let s = render_system_prompt(&qs).unwrap();
        assert!(s.contains("[a]"));
        assert!(s.contains("[b]"));
        assert!(s.contains("[c]"));
        assert!(s.contains("labels: yes | no"));
        assert!(s.contains("labels: a | b | c | d"));
        assert!(s.contains("labels: low | high | xhigh"));
    }

    #[test]
    fn an_empty_question_set_is_rejected() {
        let e = render_system_prompt(&[]).unwrap_err();
        assert!(matches!(e, JudgmentError::Malformed(_)));
    }

    #[test]
    fn a_whitespace_id_is_rejected() {
        let bad = Question::YesNo {
            id: "not a token".to_string(),
            text: "x".to_string(),
            rubric: ("y".to_string(), "n".to_string()),
        };
        assert!(render_system_prompt(&[bad]).is_err());
    }

    #[test]
    fn duplicate_ids_are_rejected() {
        let qs = vec![yes_no("x"), yes_no("x")];
        assert!(render_system_prompt(&qs).is_err());
    }

    #[test]
    fn a_question_with_no_labels_is_rejected() {
        let bad = Question::Choice {
            id: "x".to_string(),
            text: "y".to_string(),
            criteria: vec![],
        };
        assert!(render_system_prompt(&[bad]).is_err());
    }

    #[test]
    fn a_duplicate_label_is_rejected() {
        let bad = Question::Choice {
            id: "x".to_string(),
            text: "y".to_string(),
            criteria: vec![
                ("a".to_string(), "one".to_string()),
                ("a".to_string(), "two".to_string()),
            ],
        };
        assert!(render_system_prompt(&[bad]).is_err());
    }

    // -----------------------------------------------------------------
    // User message rendering
    // -----------------------------------------------------------------

    #[test]
    fn the_user_message_wraps_state_in_xml() {
        let qs = vec![yes_no("stuck")];
        let m = render_user_message(&[("prompt", "hello"), ("n", "5")], &qs);
        assert!(m.contains("<state>"));
        assert!(m.contains("<field name=\"prompt\">hello</field>"));
        assert!(m.contains("<field name=\"n\">5</field>"));
        assert!(m.contains("stuck: <label>"));
    }

    #[test]
    fn xml_special_chars_in_state_are_escaped() {
        let qs = vec![yes_no("q")];
        let m = render_user_message(&[("code", "a < b & c > d")], &qs);
        assert!(m.contains("a &lt; b &amp; c &gt; d"));
        assert!(!m.contains("a < b & c > d"));
    }

    // -----------------------------------------------------------------
    // Parsing
    // -----------------------------------------------------------------

    #[test]
    fn a_canonical_reply_parses() {
        let qs = vec![yes_no("stuck"), choice_abcd("dir")];
        let reply = "stuck: yes\ndir: b\n";
        let a = parse_response(reply, &qs);
        assert_eq!(a.get("stuck"), Some("yes"));
        assert_eq!(a.get("dir"), Some("b"));
        assert!(a.missing(&qs).is_empty());
    }

    #[test]
    fn prose_lines_are_ignored() {
        let qs = vec![yes_no("stuck")];
        let reply = "Here is my answer.\n\nstuck: yes\n\nLet me know.";
        let a = parse_response(reply, &qs);
        assert_eq!(a.get("stuck"), Some("yes"));
    }

    #[test]
    fn unknown_ids_are_ignored() {
        let qs = vec![yes_no("stuck")];
        let reply = "other: yes\nstuck: no\n";
        let a = parse_response(reply, &qs);
        assert_eq!(a.get("stuck"), Some("no"));
        assert!(a.get("other").is_none());
    }

    #[test]
    fn an_unparseable_label_leaves_the_question_missing() {
        let qs = vec![yes_no("stuck")];
        let reply = "stuck: maybe\n";
        let a = parse_response(reply, &qs);
        assert!(a.get("stuck").is_none());
        assert_eq!(a.missing(&qs), vec!["stuck"]);
    }

    #[test]
    fn whole_word_matching_does_not_match_substrings() {
        // `high` must not match inside `xhigh`.
        let qs = vec![score_effort("e")];
        let reply = "e: xhigh\n";
        let a = parse_response(reply, &qs);
        assert_eq!(a.get("e"), Some("xhigh"));
    }

    #[test]
    fn the_doc_specific_case_xhigh_beats_high() {
        // The design note's example: `xhigh` and `high` in the same
        // question. `xhigh` contains `high` as a substring but not
        // as a whole word, so a reply of `xhigh` parses as `xhigh`
        // (not `high`) and a reply of `high` parses as `high`.
        let qs = vec![score_effort("e")];
        assert_eq!(
            parse_response("e: xhigh", &qs).get("e"),
            Some("xhigh"),
        );
        assert_eq!(
            parse_response("e: high", &qs).get("e"),
            Some("high"),
        );
    }

    #[test]
    fn the_longer_label_wins_a_position_tie() {
        // Contrived but complete: two labels that match at the same
        // position, one a prefix of the other. The rule from the doc
        // is that the longer label wins.
        let q = Question::Choice {
            id: "q".to_string(),
            text: "?".to_string(),
            criteria: vec![
                ("foo".to_string(), "a".to_string()),
                ("foobar".to_string(), "b".to_string()),
            ],
        };
        // `foobar` contains `foo` at position 0 too, but
        // whole-word-boundary rejects `foo` because it is followed
        // by `b` (a word char). The match is unambiguously
        // `foobar`. To exercise the tie-break directly, construct a
        // haystack where both labels appear as whole words at the
        // same offset — impossible with these labels since one is a
        // prefix of the other. Instead verify the more relevant
        // property: with both labels present, the reply picks the
        // one that appears as a whole word.
        let a = parse_response("q: foo", &[q.clone()]);
        assert_eq!(a.get("q"), Some("foo"));
        let b = parse_response("q: foobar", &[q]);
        assert_eq!(b.get("q"), Some("foobar"));
    }

    #[test]
    fn the_earliest_whole_word_match_wins() {
        // Two labels both appear; the earlier one is chosen.
        let q = Question::Choice {
            id: "q".to_string(),
            text: "?".to_string(),
            criteria: vec![
                ("alpha".to_string(), "a".to_string()),
                ("beta".to_string(), "b".to_string()),
            ],
        };
        let a = parse_response("q: beta then alpha", &[q.clone()]);
        assert_eq!(a.get("q"), Some("beta"));
        let b = parse_response("q: alpha then beta", &[q]);
        assert_eq!(b.get("q"), Some("alpha"));
    }

    #[test]
    fn first_occurrence_of_a_line_wins() {
        let qs = vec![yes_no("q")];
        let reply = "q: yes\nq: no\n";
        let a = parse_response(reply, &qs);
        assert_eq!(a.get("q"), Some("yes"));
    }

    #[test]
    fn whitespace_around_id_and_label_is_trimmed() {
        let qs = vec![yes_no("q")];
        let a = parse_response("   q   :   yes   ", &qs);
        assert_eq!(a.get("q"), Some("yes"));
    }

    #[test]
    fn a_label_at_end_of_line_matches() {
        let qs = vec![yes_no("q")];
        assert_eq!(parse_response("q: yes", &qs).get("q"), Some("yes"));
    }

    #[test]
    fn a_label_followed_by_punctuation_matches() {
        // A model that adds a full stop: `q: yes.`. Whole-word
        // boundary check treats `.` as a non-word character, so
        // `yes` matches.
        let qs = vec![yes_no("q")];
        assert_eq!(parse_response("q: yes.", &qs).get("q"), Some("yes"));
    }

    #[test]
    fn a_label_attached_to_letters_does_not_match() {
        // `yesterday` contains `yes` but not as a whole word.
        let qs = vec![yes_no("q")];
        assert!(parse_response("q: yesterday", &qs).get("q").is_none());
    }

    #[test]
    fn answers_iterates_every_parsed_id() {
        let qs = vec![yes_no("a"), choice_abcd("b")];
        let a = parse_response("a: yes\nb: c", &qs);
        let mut collected: Vec<(&str, &str)> = a.iter().collect();
        collected.sort();
        assert_eq!(collected, vec![("a", "yes"), ("b", "c")]);
    }

    // -----------------------------------------------------------------
    // Async client
    // -----------------------------------------------------------------

    #[tokio::test]
    async fn ask_returns_the_parsed_answers_on_a_clean_reply() {
        let provider = Arc::new(ScriptedJudge::new(vec![
            "stuck: no\ndir: a\n".to_string(),
        ]));
        let client = JudgmentClient::new(
            provider,
            ModelRef::new("judge", "test"),
            JudgmentOptions::default(),
        );
        let qs = vec![yes_no("stuck"), choice_abcd("dir")];
        let a = client.ask(&[("prompt", "hello")], &qs).await.unwrap();
        assert_eq!(a.get("stuck"), Some("no"));
        assert_eq!(a.get("dir"), Some("a"));
    }

    #[tokio::test]
    async fn ask_retries_when_a_question_is_missing() {
        // First reply omits `dir`; second reply answers both.
        let provider = Arc::new(ScriptedJudge::new(vec![
            "stuck: yes\n".to_string(),
            "stuck: yes\ndir: c\n".to_string(),
        ]));
        let client = JudgmentClient::new(
            provider,
            ModelRef::new("judge", "test"),
            JudgmentOptions::default(),
        );
        let qs = vec![yes_no("stuck"), choice_abcd("dir")];
        let a = client.ask(&[("prompt", "hello")], &qs).await.unwrap();
        assert_eq!(a.get("dir"), Some("c"));
    }

    #[tokio::test]
    async fn ask_gives_up_after_the_retry_budget() {
        // Every reply omits the second question. The retry budget is
        // 1 by default, so 2 total attempts, then Unparseable.
        let provider = Arc::new(ScriptedJudge::new(vec![
            "stuck: yes\n".to_string(),
        ]));
        let client = JudgmentClient::new(
            provider,
            ModelRef::new("judge", "test"),
            JudgmentOptions::default(),
        );
        let qs = vec![yes_no("stuck"), choice_abcd("dir")];
        let e = client.ask(&[("prompt", "hello")], &qs).await.unwrap_err();
        match e {
            JudgmentError::Unparseable { missing, .. } => {
                assert_eq!(missing, vec!["dir".to_string()]);
            }
            other => panic!("expected Unparseable, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn ask_with_zero_retries_gives_up_after_one_attempt() {
        let provider = Arc::new(ScriptedJudge::new(vec!["stuck: yes\n".to_string()]));
        let client = JudgmentClient::new(
            provider,
            ModelRef::new("judge", "test"),
            JudgmentOptions {
                max_format_retries: 0,
                ..JudgmentOptions::default()
            },
        );
        let qs = vec![yes_no("stuck"), yes_no("other")];
        let r = client.ask(&[("prompt", "x")], &qs).await;
        assert!(matches!(r, Err(JudgmentError::Unparseable { .. })));
    }

    #[tokio::test]
    async fn the_system_prompt_is_identical_across_retries() {
        // The prefix must be byte-identical: the retry only changes
        // the *user* message. This is what lets the provider cache
        // the system prefix across the two attempts.
        let provider = Arc::new(ScriptedJudge::new(vec![
            "stuck: yes\n".to_string(),           // misses `dir`
            "stuck: yes\ndir: d\n".to_string(),   // answers both
        ]));
        let captured = provider.clone();
        let client = JudgmentClient::new(
            provider,
            ModelRef::new("judge", "test"),
            JudgmentOptions::default(),
        );
        let qs = vec![yes_no("stuck"), choice_abcd("dir")];
        let _ = client.ask(&[("prompt", "hi")], &qs).await.unwrap();

        let prompts = captured.system_prompts.lock().unwrap().clone();
        assert_eq!(prompts.len(), 2, "one prompt per attempt");
        assert_eq!(
            prompts[0], prompts[1],
            "the system prompt must be byte-identical across attempts",
        );
    }

    #[tokio::test]
    async fn a_provider_error_is_wrapped_as_judgment_error() {
        struct AlwaysFails;
        #[async_trait]
        impl LlmProvider for AlwaysFails {
            fn name(&self) -> &str {
                "fails"
            }
            async fn list_models(&self) -> KodResult<Vec<crate::ModelInfo>> {
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
                Err(KodError::Provider("simulated failure".into()))
            }
            fn stream(
                &self,
                _p: &str,
                _o: &GenerationOptions,
            ) -> std::pin::Pin<
                Box<
                    dyn futures::Stream<Item = KodResult<crate::StreamChunk>>
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
                Err(KodError::Provider("simulated failure".into()))
            }
        }
        let client = JudgmentClient::new(
            Arc::new(AlwaysFails),
            ModelRef::new("judge", "test"),
            JudgmentOptions::default(),
        );
        let qs = vec![yes_no("q")];
        let e = client.ask(&[], &qs).await.unwrap_err();
        assert!(matches!(e, JudgmentError::Provider(_)));
    }

    #[tokio::test]
    async fn the_request_options_carry_the_judgment_settings() {
        // A provider that captures the request options and asserts
        // they match `JudgmentOptions`.
        struct CaptureOptions {
            captured: Mutex<Vec<GenerationOptions>>,
        }
        #[async_trait]
        impl LlmProvider for CaptureOptions {
            fn name(&self) -> &str {
                "capture"
            }
            async fn list_models(&self) -> KodResult<Vec<crate::ModelInfo>> {
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
            ) -> std::pin::Pin<
                Box<
                    dyn futures::Stream<Item = KodResult<crate::StreamChunk>>
                        + Send
                        + '_,
                >,
            > {
                Box::pin(futures::stream::empty())
            }
            async fn complete(
                &self,
                req: &CompletionRequest,
            ) -> KodResult<GenerationResponse> {
                self.captured.lock().unwrap().push(req.options.clone());
                Ok(GenerationResponse::Text {
                    content: "q: yes\n".to_string(),
                    usage: None,
                })
            }
        }
        let cap = Arc::new(CaptureOptions {
            captured: Mutex::new(Vec::new()),
        });
        let client = JudgmentClient::new(
            cap.clone(),
            ModelRef::new("judge", "test"),
            JudgmentOptions {
                max_tokens: 999,
                temperature: 0.0,
                max_format_retries: 0,
            },
        );
        let qs = vec![yes_no("q")];
        let _ = client.ask(&[], &qs).await.unwrap();
        let captured = cap.captured.lock().unwrap().clone();
        assert_eq!(captured.len(), 1);
        assert_eq!(captured[0].max_tokens, Some(999));
        assert_eq!(captured[0].temperature, Some(0.0));
    }

    // -----------------------------------------------------------------
    // Defaults
    // -----------------------------------------------------------------

    #[test]
    fn the_documented_defaults_hold() {
        let o = JudgmentOptions::default();
        assert_eq!(o.max_tokens, 4096);
        assert_eq!(o.temperature, 0.0);
        assert_eq!(o.max_format_retries, 1);
    }
}
