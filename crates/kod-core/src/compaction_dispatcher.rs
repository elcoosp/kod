//! Ordered compaction dispatcher (borrow from oh-my-pi, delta §4.1).
//!
//! # The design
//!
//! Compaction is a *preference list with fail-over*, not a hardcoded
//! ladder. The dispatcher holds an ordered list of
//! [`CompactionMethod`]s; on each request it asks each method, in
//! order, whether it can reduce the context. The first method that
//! produces a non-empty plan wins. A method that reports
//! [`MethodOutcome::Unavailable`] or [`MethodOutcome::Failed`] is
//! skipped with a user-visible notice; the dispatcher falls through.
//!
//! The default order, from the design note:
//!
//! ```text
//! [remote, snapcompact, handoff, shake, soft]
//! ```
//!
//! `remote` (provider-native compaction, Anthropic
//! `compact-2026-01-12`), `snapcompact` (bitmap-frame imaging), and
//! `handoff` (one-shot handoff document) are not implemented in this
//! workspace yet — they land as stub methods returning
//! `Unavailable`, so the ladder shape is correct and a future pass
//! that implements any of them slots in without a dispatcher change.
//!
//! `shake` is real: [`ShakeMethod`] wraps [`crate::shake::plan_shake`].
//!
//! `soft` (local LLM summarize) is stubbed as well — the summarizer
//! pipeline is a separate slice.
//!
//! # Plans, not applications
//!
//! [`CompactionDispatcher::compact`] returns a [`CompactionPlan`] —
//! a description of the transcript mutations to make — not a mutated
//! transcript. That matches the shape of [`crate::shake::plan_shake`]
//! and [`crate::prune::plan_prune`]: pure functions returning action
//! lists, testable without an engine. The caller applies the plan
//! through the same code path it already uses for either primitive.
//!
//! # Threshold
//!
//! [`should_compact`] answers the caller's other question — should we
//! even try? — with the doc's reserve rule: `reserve = max(15% of
//! window, 16_384)`, so a compaction fires when
//! `used_tokens + reserve > window_tokens`. The `used_tokens`
//! argument is the caller's provider-anchored count where one exists
//! (see [`crate::context_gauge::ContextGauge`]) and a char-arithmetic
//! estimate where it does not.
//!
//! # What this is NOT
//!
//! * Not a scheduler. It runs when the caller asks it to; the caller
//!   decides the cadence (post-turn maintenance, an explicit
//!   `/compact`, a threshold crossing).
//! * Not a policy engine. Each method owns its own rules — shake has
//!   its `protect_tokens` and cache-warm guard, prune has its
//!   supersede index. The dispatcher does not second-guess them.
//! * Not the compaction decision. `crate::compaction::decide` answers
//!   "should we compact?" from observed token usage. `should_compact`
//!   here is the simpler threshold check that pairs with a preference
//!   ladder; both exist for callers at different abstraction levels.

use async_trait::async_trait;
use kod_types::ChatMessage;

use crate::prune::PrunePlan;
use crate::shake::{ShakeConfig, ShakePlan, plan_shake};

/// The doc's reserve rule: `max(15% of window, 16_384)`.
pub fn resolve_reserve(window_tokens: u64) -> u64 {
    let fifteen_pct = window_tokens.saturating_mul(15) / 100;
    fifteen_pct.max(16_384)
}

/// Should a compaction be attempted at this size?
///
/// `used_tokens + reserve > window_tokens` — the doc's rule. A zero
/// window returns `false`; refusing to compact on an unknown window
/// is the safe reading (the caller learns the real number from the
/// provider on the next call).
pub fn should_compact(used_tokens: u64, window_tokens: u64) -> bool {
    if window_tokens == 0 {
        return false;
    }
    let reserve = resolve_reserve(window_tokens);
    used_tokens.saturating_add(reserve) > window_tokens
}

/// The keep-recent default the doc names.
pub const DEFAULT_KEEP_RECENT_TOKENS: u64 = 20_000;

/// The context a method reasons about. Borrows from the caller — the
/// dispatcher does not own the transcript.
///
/// # `Send + Sync` on the closure
///
/// The `#[async_trait]`-generated futures hold a `&CompactionContext`
/// across an `.await`, which requires the context to be `Sync`. A
/// trait object `dyn Fn(usize) -> u64` is not automatically `Sync`
/// (the auto-trait is erased by `dyn`), so the bound is written
/// explicitly. `Send` is added for symmetry: a `Sync` closure is
/// always usable from a `Send` future, but the bound spells out the
/// requirement rather than relying on the reader to reason it out.
/// A provider handle the LLM-calling methods (`handoff`, `soft`) use
/// to make their one call. Carried in [`CompactionContext`] rather
/// than stored on the method because the provider is installed on the
/// engine *after* the dispatcher is constructed (`set_registry` runs
/// later than `KodEngine::new`), and because a method is a `Box<dyn
/// CompactionMethod>` shared across the dispatcher's lifetime while
/// the provider may change.
///
/// `None` when no provider is installed; the methods that need one
/// report [`MethodOutcome::Unavailable`] and the dispatcher falls
/// through.
#[derive(Clone)]
pub struct ProviderHandle {
    pub provider: std::sync::Arc<dyn kod_provider::LlmProvider>,
    pub options: kod_provider::GenerationOptions,
    /// The model to name in a request built from this handle. The
    /// `native_compact` call needs a `CompletionRequest`, and a
    /// `CompletionRequest` names a `ModelRef` — the provider knows
    /// its own default model, but the *engine* is the one that knows
    /// which endpoint this turn routed to.
    pub model: kod_provider::ModelRef,
}

impl std::fmt::Debug for ProviderHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ProviderHandle")
            .field("provider", &self.provider.name())
            .field("model", &self.model.display())
            .field("options", &self.options)
            .finish()
    }
}

pub struct CompactionContext<'a> {
    /// The transcript, in send order.
    pub transcript: &'a [ChatMessage],
    /// The model's context window in tokens.
    pub window_tokens: u64,
    /// Per-message suffix estimator, matching the shape `plan_prune`
    /// and `plan_shake` already take: given a message index, the
    /// estimated token count of everything strictly after it.
    pub suffix_tokens_after: &'a (dyn Fn(usize) -> u64 + Send + Sync),
    /// Whether the provider's prefix cache is still warm. Passed
    /// through to the methods that respect it.
    pub prefix_is_warm: bool,
    /// The provider handle for methods that make an LLM call
    /// (`handoff`, `soft`). `None` when no provider is installed.
    pub provider: Option<ProviderHandle>,
}

/// What a method produced.
pub enum MethodOutcome {
    /// A plan to apply. Non-empty by construction — a method that
    /// finds nothing to do returns `NoChange`.
    Plan(CompactionPlan),
    /// The method ran and found nothing to change. The dispatcher
    /// falls through to the next method: this is not a failure, it is
    /// "no eligible work at this size".
    NoChange,
    /// The method is not applicable right now — a provider that does
    /// not support native compaction, a feature flag that is off, a
    /// summary pipeline that is not yet wired. Carries a short
    /// reason for the user notice.
    Unavailable(String),
    /// The method failed. Carries a short error for the user notice.
    /// The dispatcher falls through; a method that fails does not
    /// block the ladder.
    Failed(String),
}

/// A compaction plan: what the caller should do to the transcript.
///
/// The variants name the plan shapes that exist in the workspace.
/// A future method adds a variant.
#[derive(Debug, Clone)]
pub enum CompactionPlan {
    /// Region-elision actions (shake).
    Shake(ShakePlan),
    /// Whole-result blanking (supersede pruning).
    Prune(PrunePlan),
    /// A summary string that replaces the first `covers_through + 1`
    /// messages.
    Summary {
        covers_through: usize,
        text: String,
    },
    /// Delta §4.5: rasterize the older half of the transcript into a
    /// PNG and attach it as an image on the next request. The
    /// transcript's older half is *replaced* by a short marker
    /// message ("rendered as frame; see attachment") so the local
    /// view shrinks too.
    ///
    /// `png_base64` is the base64-encoded PNG (no data-URL prefix —
    /// the wire layer adds the framing). `source_lines` is the count
    /// of original lines that went into the frame, for the marker
    /// text and for telemetry.
    Image {
        covers_through: usize,
        png_base64: String,
        source_lines: usize,
    },
    /// A summary produced by provider-native compaction. Same drain
    /// and replace shape as [`Self::Summary`], plus an opaque
    /// `encrypted_content` token the provider expects replayed on the
    /// next request so its server-side KV cache is reused.
    ///
    /// Separate from `Summary` rather than a `Option<String>` field
    /// on it: a plan that carries a native block has different
    /// downstream behaviour (the wire layer prepends the block),
    /// and making that a distinct variant means the apply step is
    /// explicit about which one it is looking at.
    NativeSummary {
        covers_through: usize,
        text: String,
        encrypted_content: String,
    },
}

impl CompactionPlan {
    pub fn is_empty(&self) -> bool {
        match self {
            Self::Shake(p) => p.is_empty(),
            Self::Prune(p) => p.is_empty(),
            Self::Summary { text, .. } => text.trim().is_empty(),
            Self::NativeSummary { text, .. } => text.trim().is_empty(),
            Self::Image { png_base64, .. } => png_base64.trim().is_empty(),
        }
    }
}

/// One rung of the compaction ladder.
#[async_trait]
pub trait CompactionMethod: Send + Sync {
    /// Short name for logs and the user notice.
    fn name(&self) -> &'static str;

    /// A cheap check: could this method plausibly do anything?
    ///
    /// Called before [`Self::run`]. Returning `false` skips the
    /// method without a user notice — availability is expected to be
    /// false when it is (a provider that does not support a feature),
    /// not a failure to report.
    fn available(&self, ctx: &CompactionContext<'_>) -> bool;

    /// Run the method. Async because `remote` and `soft` make
    /// provider calls in a future pass; shake and prune are pure.
    async fn run(&self, ctx: &CompactionContext<'_>) -> MethodOutcome;
}

/// What the dispatcher concluded.
#[derive(Debug)]
pub struct DispatchOutcome {
    /// The name of the method that produced a plan, if any.
    pub method: Option<&'static str>,
    /// The plan to apply, if any.
    pub plan: Option<CompactionPlan>,
    /// Human-readable notices from methods that were skipped. One
    /// per method that reported `Unavailable` or `Failed`, in order.
    pub notices: Vec<String>,
}

impl DispatchOutcome {
    /// Whether a plan was produced.
    pub fn has_plan(&self) -> bool {
        self.plan.is_some()
    }
}

/// The dispatcher: an ordered list of methods.
pub struct CompactionDispatcher {
    methods: Vec<Box<dyn CompactionMethod>>,
}

impl CompactionDispatcher {
    /// Build with an explicit method list, in preference order.
    pub fn new(methods: Vec<Box<dyn CompactionMethod>>) -> Self {
        Self { methods }
    }

    /// Build the doc's default ladder: `[remote, snapcompact,
    /// handoff, shake, soft]`. `remote`, `snapcompact`, `handoff`,
    /// and `soft` land as stubs; `shake` is the real rung.
    pub fn default_ladder(shake_config: ShakeConfig) -> Self {
        Self::new(vec![
            Box::new(RemoteMethod::stub()),
            Box::new(SnapcompactMethod::stub()),
            Box::new(HandoffMethod::stub()),
            Box::new(ShakeMethod::new(shake_config)),
            Box::new(SoftMethod::stub()),
        ])
    }

    /// The number of methods registered.
    pub fn len(&self) -> usize {
        self.methods.len()
    }

    pub fn is_empty(&self) -> bool {
        self.methods.is_empty()
    }

    /// The registered methods' names, in preference order.
    pub fn method_names(&self) -> Vec<&'static str> {
        self.methods.iter().map(|m| m.name()).collect()
    }

    /// Run the ladder.
    ///
    /// For each method: `available` is checked first, then `run` is
    /// awaited. The first `Plan` wins and the ladder stops. A
    /// `NoChange` moves on silently; an `Unavailable` or `Failed`
    /// moves on with a notice.
    pub async fn compact(&self, ctx: &CompactionContext<'_>) -> DispatchOutcome {
        let mut notices = Vec::new();
        for method in &self.methods {
            if !method.available(ctx) {
                continue;
            }
            match method.run(ctx).await {
                MethodOutcome::Plan(plan) => {
                    if plan.is_empty() {
                        // A method that returned an empty plan is
                        // treated as `NoChange`; the invariant
                        // "Plan is non-empty" is enforced here.
                        continue;
                    }
                    return DispatchOutcome {
                        method: Some(method.name()),
                        plan: Some(plan),
                        notices,
                    };
                }
                MethodOutcome::NoChange => continue,
                MethodOutcome::Unavailable(reason) => {
                    notices.push(format!("`{}` unavailable: {reason}", method.name()));
                    continue;
                }
                MethodOutcome::Failed(err) => {
                    notices.push(format!("`{}` failed: {err}; falling through", method.name()));
                    continue;
                }
            }
        }
        DispatchOutcome {
            method: None,
            plan: None,
            notices,
        }
    }
}

// ---------------------------------------------------------------------------
// Real methods
// ---------------------------------------------------------------------------

/// The shake rung: region elision inside user/assistant messages, and
/// whole-body elision of oversized tool results.
pub struct ShakeMethod {
    config: ShakeConfig,
}

impl ShakeMethod {
    pub fn new(config: ShakeConfig) -> Self {
        Self { config }
    }
}

#[async_trait]
impl CompactionMethod for ShakeMethod {
    fn name(&self) -> &'static str {
        "shake"
    }

    fn available(&self, ctx: &CompactionContext<'_>) -> bool {
        // Shake is a pure function; nothing ever makes it unavailable.
        // An empty transcript short-circuits the work but is not a
        // reason to skip the method — it returns `NoChange` cleanly.
        let _ = ctx;
        true
    }

    async fn run(&self, ctx: &CompactionContext<'_>) -> MethodOutcome {
        let estimator = ctx.suffix_tokens_after;
        let plan = plan_shake(
            ctx.transcript,
            &self.config,
            |i| estimator(i),
            ctx.prefix_is_warm,
        );
        if plan.is_empty() {
            MethodOutcome::NoChange
        } else {
            MethodOutcome::Plan(CompactionPlan::Shake(plan))
        }
    }
}

/// The prune rung. Not in the doc's default ladder (prune and shake
/// operate at different granularities), but frequently a caller's
/// lowest-cost option: blanking a superseded result is cheaper than
/// eliding a region.
pub struct PruneMethod {
    config: crate::prune::PruneConfig,
}

impl PruneMethod {
    pub fn new(config: crate::prune::PruneConfig) -> Self {
        Self { config }
    }
}

#[async_trait]
impl CompactionMethod for PruneMethod {
    fn name(&self) -> &'static str {
        "prune"
    }

    fn available(&self, _ctx: &CompactionContext<'_>) -> bool {
        true
    }

    async fn run(&self, ctx: &CompactionContext<'_>) -> MethodOutcome {
        let estimator = ctx.suffix_tokens_after;
        let plan = crate::prune::plan_prune(
            ctx.transcript,
            &self.config,
            |i| estimator(i),
            ctx.prefix_is_warm,
        );
        if plan.is_empty() {
            MethodOutcome::NoChange
        } else {
            MethodOutcome::Plan(CompactionPlan::Prune(plan))
        }
    }
}

// ---------------------------------------------------------------------------
// Stub methods
// ---------------------------------------------------------------------------
//
// Each stub exists so the ladder shape is correct and a future pass
// can implement the method without touching the dispatcher. The
// `Unavailable` reason names what is missing, so a caller that logs
// the notices sees an accurate picture of which rungs are
// operational.

/// Provider-native compaction: the API summarizes the prompt and
/// returns a block the client replays.
///
/// # What is wired
///
/// The provider is asked to compact via [`LlmProvider::native_compact`].
/// When it returns a `NativeCompaction`, the plain-text `summary` is
/// returned as a `CompactionPlan::Summary` — the older half of the
/// transcript is replaced by the summary in kod's view.
///
/// # What is not wired (yet)
///
/// The encrypted block itself is not stored and not replayed. The
/// server-side compaction is therefore a one-shot: the summary
/// replaces the older half locally, but the *next* request does not
/// carry the encrypted block, so the server re-reads the newer
/// transcript without the pre-computed KV reuse the block would
/// allow. The local reduction is still real, and the summary is
/// what kod would send anyway. Replay is the follow-up commit: the
/// wire layer already has `prepend_compaction_block`, and the
/// storage is a per-transcript `String` map on the engine.
///
/// # When this is not available
///
/// * No provider in the context.
/// * `capabilities().native_compaction == false`.
/// * Fewer than [`MIN_REMOTE_MESSAGES`] messages.
///
/// All three produce `Unavailable`, not `Failed`, and the dispatcher
/// falls through without a user notice. A provider that does not
/// implement the feature is not a failure.
pub struct RemoteMethod;

/// Below this, the compaction call has nothing meaningful to
/// summarize. Matches the handoff method's floor, for the same
/// reason: one LLM round trip costs more than the tokens saved.
pub const MIN_REMOTE_MESSAGES: usize = 4;

impl RemoteMethod {
    pub fn new() -> Self {
        Self
    }

    /// Alias, preserved for the stub-era registration.
    pub fn stub() -> Self {
        Self::new()
    }
}

impl Default for RemoteMethod {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl CompactionMethod for RemoteMethod {
    fn name(&self) -> &'static str {
        "remote"
    }

    fn available(&self, ctx: &CompactionContext<'_>) -> bool {
        match ctx.provider.as_ref() {
            Some(h) => {
                h.provider.capabilities().native_compaction
                    && ctx.transcript.len() >= MIN_REMOTE_MESSAGES
            }
            None => false,
        }
    }

    async fn run(&self, ctx: &CompactionContext<'_>) -> MethodOutcome {
        let Some(handle) = ctx.provider.as_ref() else {
            return MethodOutcome::Unavailable(
                "no provider installed; provider-native compaction needs one".to_string(),
            );
        };
        if !handle.provider.capabilities().native_compaction {
            return MethodOutcome::Unavailable(
                "provider does not implement native compaction".to_string(),
            );
        }
        if ctx.transcript.len() < MIN_REMOTE_MESSAGES {
            return MethodOutcome::Unavailable(format!(
                "transcript has {} messages; native compaction needs at least {}",
                ctx.transcript.len(),
                MIN_REMOTE_MESSAGES,
            ));
        }

        // Build the completion request the same shape `complete`
        // takes. Empty system prompt: the compaction request's job
        // is the transcript, and the server-side summarizer is
        // instructed by the API itself. No tools: a summarization
        // request that could call a tool would be a bug.
        let mut req = kod_provider::request::CompletionRequest::new(handle.model.clone());
        req.system = kod_provider::request::SystemPrompt::new();
        req.messages = ctx.transcript.to_vec();
        req.tools = Vec::new();
        req.options = handle.options.clone();
        // No transcript marker: this is not a turn, it is a
        // summarization. A `cache_control` marker on the transcript
        // would be silently wrong.
        req.cache_transcript = false;

        match handle.provider.native_compact(&req).await {
            Ok(Some(compaction)) => {
                // The server compacted whatever it saw; in kod's
                // view the whole transcript is covered. The apply
                // step clamps to the live length, so `len - 1` here
                // means "everything that was sent".
                let covers_through = ctx.transcript.len().saturating_sub(1);
                MethodOutcome::Plan(CompactionPlan::NativeSummary {
                    covers_through,
                    text: compaction.summary,
                    encrypted_content: compaction.encrypted_content,
                })
            }
            Ok(None) => MethodOutcome::NoChange,
            Err(e) => MethodOutcome::Failed(format!(
                "provider-native compaction failed: {e}",
            )),
        }
    }
}

/// Bitmap-frame imaging (`snapcompact`): rasterize the older half of
/// the transcript into a PNG a vision model reads for far fewer
/// tokens than the raw text.
///
/// # Availability
///
/// Requires a provider whose `capabilities().vision` is true. A
/// text-only provider would receive an image block it cannot
/// interpret — worse than the text it replaced.
///
/// # Savings margin
///
/// The design's `SAVINGS_MARGIN = 0.9`: the rasterized frame must be
/// at least 10% cheaper in tokens than the text it replaces, or the
/// method declines. The frame token estimate is the design's
/// `FRAME_TOKEN_ESTIMATE`; the text estimate is the transcript's
/// char count over the 4-chars-per-token convention. Below the
/// margin the method returns `NoChange` and the dispatcher falls
/// through.
///
/// # What this renders
///
/// The older half (`len / 2`), matching the handoff method's split.
/// A single frame per the plan shape: a transcript past the canvas
/// limit produces `NoChange` rather than a truncated frame — the
/// rasterizer refuses too-large input and the method honours that.
pub struct SnapcompactMethod;

/// The design's per-frame token estimate. A 1568×1568 PNG of text
/// bills at roughly this many tokens regardless of how much text it
/// holds, which is what makes the compression real.
pub const FRAME_TOKEN_ESTIMATE: u64 = 5_024;

/// The design's savings margin. Rasterizing is only a win when the
/// frame is meaningfully cheaper than the text.
pub const SAVINGS_MARGIN: f64 = 0.9;

impl SnapcompactMethod {
    pub fn new() -> Self {
        Self
    }

    /// Alias, preserved for the stub-era registration.
    pub fn stub() -> Self {
        Self::new()
    }
}

impl Default for SnapcompactMethod {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl CompactionMethod for SnapcompactMethod {
    fn name(&self) -> &'static str {
        "snapcompact"
    }

    fn available(&self, ctx: &CompactionContext<'_>) -> bool {
        match ctx.provider.as_ref() {
            Some(h) => {
                h.provider.capabilities().vision
                    && ctx.transcript.len() >= MIN_REMOTE_MESSAGES
            }
            None => false,
        }
    }

    async fn run(&self, ctx: &CompactionContext<'_>) -> MethodOutcome {
        let Some(handle) = ctx.provider.as_ref() else {
            return MethodOutcome::Unavailable(
                "no provider installed; snapcompact needs a vision provider".to_string(),
            );
        };
        if !handle.provider.capabilities().vision {
            return MethodOutcome::Unavailable(
                "provider does not have the vision capability".to_string(),
            );
        }
        if ctx.transcript.len() < MIN_REMOTE_MESSAGES {
            return MethodOutcome::Unavailable(format!(
                "transcript has {} messages; snapcompact needs at least {}",
                ctx.transcript.len(),
                MIN_REMOTE_MESSAGES,
            ));
        }

        // Older half, same split as handoff.
        let split = ctx.transcript.len() / 2;
        let dropped = &ctx.transcript[..split];
        let text: String = dropped
            .iter()
            .map(|m| m.render_text())
            .collect::<Vec<_>>()
            .join("\n");

        // Savings margin. The text token estimate is the transcript
        // convention (chars / 4); the frame estimate is the design's
        // constant. Below the margin the frame is not worth it.
        let text_tokens = (text.len() as u64) / 4;
        if text_tokens == 0 {
            return MethodOutcome::NoChange;
        }
        let frame_estimate = FRAME_TOKEN_ESTIMATE;
        let ratio = frame_estimate as f64 / text_tokens as f64;
        if ratio >= SAVINGS_MARGIN {
            return MethodOutcome::NoChange;
        }

        match crate::snapcompact::rasterize_to_png(&text) {
            Ok(png) => {
                let source_lines = text.lines().count();
                let base64 = crate::snapcompact::base64_encode(&png);
                MethodOutcome::Plan(CompactionPlan::Image {
                    covers_through: split.saturating_sub(1),
                    png_base64: base64,
                    source_lines,
                })
            }
            Err(crate::snapcompact::RasterizeError::Empty) => MethodOutcome::NoChange,
            Err(e) => MethodOutcome::Failed(format!("snapcompact rasterize failed: {e}")),
        }
    }
}

/// The design's `handoff` rung: one-shot handoff document *as* the
/// compaction summary.
///
/// # What a handoff document is
///
/// Not a summary. A summary narrates what happened; a handoff
/// document *instructs the next agent*. The difference matters when
/// the compaction is happening so the session can keep going: the
/// model reading the handoff needs to know what to do next, what has
/// already been tried, and what the current state is — not a
/// paragraph of prose about the previous ten turns.
///
/// The prompt asks for that shape explicitly. The output is used
/// verbatim as the replacement text for the older half of the
/// transcript: `CompactionPlan::Summary` with `covers_through` at the
/// split point.
///
/// # Why the first half
///
/// The same split the engine's existing summary path uses: the
/// *newer* half stays (it is the working set), the *older* half is
/// replaced. A `handoff` on the newer half would throw away the
/// context the model most needs.
///
/// # When this is not available
///
/// * No provider in the context (`None`) — the engine has not
///   installed a registry yet, or the session is offline.
/// * Fewer than [`MIN_HANDOFF_MESSAGES`] messages — a transcript
///   with a couple of turns does not have anything worth a handoff
///   call.
pub struct HandoffMethod;

/// The smallest transcript the handoff method will act on. Below
/// this, a one-call LLM handoff costs more than the tokens it saves.
pub const MIN_HANDOFF_MESSAGES: usize = 4;

impl HandoffMethod {
    pub fn new() -> Self {
        Self
    }

    /// Alias for [`Self::new`], preserved for the stub-registration
    /// shape the dispatcher's constructor used before the real
    /// implementation landed.
    pub fn stub() -> Self {
        Self::new()
    }
}

impl Default for HandoffMethod {
    fn default() -> Self {
        Self::new()
    }
}

/// Build the handoff prompt from a slice of the transcript.
///
/// Distinct from `build_summary_prompt` in the engine: the summary
/// prompt asks for four labelled sections of *history*; the handoff
/// prompt asks for a *briefing document* a next agent could pick up
/// from. Same input, different output shape.
fn build_handoff_prompt(dropped: &[ChatMessage]) -> String {
    const CAP: usize = 24_000;
    let mut body = String::new();
    for m in dropped {
        let line = m.render_text();
        if body.len() + line.len() + 1 > CAP {
            body.push_str("\n[...earlier messages truncated for the handoff call...]\n");
            break;
        }
        body.push_str(&line);
        body.push('\n');
    }
    format!(
        "You are handing this session off to another agent who will \
         continue the work. Write a briefing document with these \
         sections:\n\n\
         ## Objective\n\
         What the session is trying to accomplish, in one or two \
         sentences.\n\n\
         ## What has been done\n\
         Concrete changes made, with file paths and function names. \
         Past tense, itemized. Skip narration.\n\n\
         ## Current state\n\
         What works, what is broken, what is mid-change. Be specific \
         about the boundary — the next agent needs to know what to \
         trust.\n\n\
         ## What to do next\n\
         The immediate next step, if one is clear. If not, what needs \
         investigation.\n\n\
         ## Constraints and preferences\n\
         Anything the user stated that must persist: conventions, \
         choices that were settled, things to avoid.\n\n\
         Write only the document. Do not restate the excerpt; \
         extract from it. If a section has nothing, write `(none)` \
         rather than inventing content.\n\n\
         ## Excerpt\n\n{body}",
    )
}

#[async_trait]
impl CompactionMethod for HandoffMethod {
    fn name(&self) -> &'static str {
        "handoff"
    }

    fn available(&self, ctx: &CompactionContext<'_>) -> bool {
        ctx.provider.is_some() && ctx.transcript.len() >= MIN_HANDOFF_MESSAGES
    }

    async fn run(&self, ctx: &CompactionContext<'_>) -> MethodOutcome {
        let Some(handle) = ctx.provider.as_ref() else {
            return MethodOutcome::Unavailable(
                "no provider installed; handoff needs one LLM call".to_string(),
            );
        };
        if ctx.transcript.len() < MIN_HANDOFF_MESSAGES {
            return MethodOutcome::Unavailable(format!(
                "transcript has {} messages; handoff needs at least {}",
                ctx.transcript.len(),
                MIN_HANDOFF_MESSAGES,
            ));
        }

        // The older half is what gets replaced. The split preserves
        // the working set (newer messages) unchanged, which is the
        // whole point of doing a handoff rather than a full summary.
        let split = ctx.transcript.len() / 2;
        let dropped = &ctx.transcript[..split];
        let prompt = build_handoff_prompt(dropped);

        match handle.provider.generate(&prompt, &handle.options).await {
            Ok(text) if !text.trim().is_empty() => MethodOutcome::Plan(CompactionPlan::Summary {
                covers_through: split - 1,
                text: text.trim().to_string(),
            }),
            Ok(_) => MethodOutcome::Failed(
                "handoff call returned empty; falling through to the next method".to_string(),
            ),
            Err(e) => MethodOutcome::Failed(format!(
                "handoff call failed: {e}; falling through to the next method",
            )),
        }
    }
}

/// Local LLM summarize (the `soft` rung).
pub struct SoftMethod;

impl SoftMethod {
    pub fn stub() -> Self {
        Self
    }
}

#[async_trait]
impl CompactionMethod for SoftMethod {
    fn name(&self) -> &'static str {
        "soft"
    }
    fn available(&self, _ctx: &CompactionContext<'_>) -> bool {
        true
    }
    async fn run(&self, _ctx: &CompactionContext<'_>) -> MethodOutcome {
        MethodOutcome::Unavailable(
            "soft summarize is not yet wired into the engine".to_string(),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::prune::SUPERSEDED_PLACEHOLDER;
    use kod_types::{MessageId, MessageRole, ToolCall};
    use std::sync::Arc;
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use time::OffsetDateTime;

    fn user(content: &str) -> ChatMessage {
        ChatMessage::text(
            MessageId::new(),
            MessageRole::User,
            content,
            OffsetDateTime::now_utc(),
        )
    }

    fn assistant_with_read(call_id: &str, path: &str) -> ChatMessage {
        let mut m = ChatMessage::text(
            MessageId::new(),
            MessageRole::Assistant,
            "",
            OffsetDateTime::now_utc(),
        );
        m.tool_calls.push(ToolCall {
            id: Some(call_id.to_string()),
            tool_name: "read_file".to_string(),
            arguments: serde_json::json!({"path": path}),
        });
        m
    }

    fn tool_result(call_id: &str, body: &str) -> ChatMessage {
        let mut m = ChatMessage::text(
            MessageId::new(),
            MessageRole::Tool,
            body,
            OffsetDateTime::now_utc(),
        );
        m.tool_call_id = Some(call_id.to_string());
        m
    }

    fn big_suffix() -> impl Fn(usize) -> u64 {
        |_| 1_000_000
    }

    fn ctx<'a>(
        transcript: &'a [ChatMessage],
        estimator: &'a (dyn Fn(usize) -> u64 + Send + Sync),
    ) -> CompactionContext<'a> {
        CompactionContext {
            transcript,
            window_tokens: 200_000,
            suffix_tokens_after: estimator,
            prefix_is_warm: false,
            // Tests that exercise shake/prune do not need a provider.
            // A test that exercises `handoff` supplies one.
            provider: None,
        }
    }

    // ---- Threshold math -------------------------------------------------

    #[test]
    fn reserve_is_15_percent_or_16_384_whichever_is_larger() {
        // Small window: the floor wins.
        assert_eq!(resolve_reserve(50_000), 16_384);
        // Large window: the percentage wins.
        assert_eq!(resolve_reserve(200_000), 30_000);
    }

    #[test]
    fn should_compact_is_false_on_a_small_context() {
        assert!(!should_compact(100_000, 200_000));
    }

    #[test]
    fn should_compact_is_true_when_the_reserve_would_be_breached() {
        // 200k - 30k = 170k threshold. 171k crosses it.
        assert!(should_compact(171_000, 200_000));
    }

    #[test]
    fn should_compact_is_false_on_a_zero_window() {
        // No window known: refuse to compact. The caller learns the
        // real number on the next provider call.
        assert!(!should_compact(u64::MAX, 0));
    }

    // ---- Dispatcher ordering and fall-through ---------------------------

    /// A method whose behavior is scripted per call, and which
    /// records the order it was invoked in.
    struct ScriptedMethod {
        name: &'static str,
        available: bool,
        outcome: Mutex<Option<MethodOutcome>>,
    }

    impl ScriptedMethod {
        fn new(name: &'static str, outcome: MethodOutcome) -> Self {
            Self {
                name,
                available: true,
                outcome: Mutex::new(Some(outcome)),
            }
        }
        fn unavailable_flag(mut self) -> Self {
            self.available = false;
            self
        }
    }

    #[async_trait]
    impl CompactionMethod for ScriptedMethod {
        fn name(&self) -> &'static str {
            self.name
        }
        fn available(&self, _ctx: &CompactionContext<'_>) -> bool {
            self.available
        }
        async fn run(&self, _ctx: &CompactionContext<'_>) -> MethodOutcome {
            let mut g = self.outcome.lock().unwrap();
            g.take().unwrap_or(MethodOutcome::NoChange)
        }
    }

    #[tokio::test]
    async fn the_first_plan_wins() {
        let d = CompactionDispatcher::new(vec![
            Box::new(ScriptedMethod::new(
                "first",
                MethodOutcome::Plan(CompactionPlan::Summary {
                    covers_through: 0,
                    text: "summary".to_string(),
                }),
            )),
            Box::new(ScriptedMethod::new(
                "second",
                MethodOutcome::Plan(CompactionPlan::Summary {
                    covers_through: 0,
                    text: "other".to_string(),
                }),
            )),
        ]);
        let t: Vec<ChatMessage> = Vec::new();
        let est = big_suffix();
        let out = d.compact(&ctx(&t, &est)).await;
        assert_eq!(out.method, Some("first"));
    }

    #[tokio::test]
    async fn no_change_falls_through_silently() {
        let d = CompactionDispatcher::new(vec![
            Box::new(ScriptedMethod::new("first", MethodOutcome::NoChange)),
            Box::new(ScriptedMethod::new(
                "second",
                MethodOutcome::Plan(CompactionPlan::Summary {
                    covers_through: 0,
                    text: "s".to_string(),
                }),
            )),
        ]);
        let t: Vec<ChatMessage> = Vec::new();
        let est = big_suffix();
        let out = d.compact(&ctx(&t, &est)).await;
        assert_eq!(out.method, Some("second"));
        assert!(
            out.notices.is_empty(),
            "no-change is not a notice; got {:?}",
            out.notices,
        );
    }

    #[tokio::test]
    async fn unavailable_falls_through_with_a_notice() {
        let d = CompactionDispatcher::new(vec![
            Box::new(ScriptedMethod::new(
                "first",
                MethodOutcome::Unavailable("not wired".into()),
            )),
            Box::new(ScriptedMethod::new(
                "second",
                MethodOutcome::Plan(CompactionPlan::Summary {
                    covers_through: 0,
                    text: "s".to_string(),
                }),
            )),
        ]);
        let t: Vec<ChatMessage> = Vec::new();
        let est = big_suffix();
        let out = d.compact(&ctx(&t, &est)).await;
        assert_eq!(out.method, Some("second"));
        assert_eq!(out.notices.len(), 1);
        assert_eq!(out.notices.len(), 1);
        assert!(out.notices[0].contains("first"));
        assert!(out.notices[0].contains("not wired"));
    }

    #[tokio::test]
    async fn failed_falls_through_with_a_notice() {
        let d = CompactionDispatcher::new(vec![
            Box::new(ScriptedMethod::new(
                "first",
                MethodOutcome::Failed("timeout".into()),
            )),
            Box::new(ScriptedMethod::new(
                "second",
                MethodOutcome::Plan(CompactionPlan::Summary {
                    covers_through: 0,
                    text: "s".to_string(),
                }),
            )),
        ]);
        let t: Vec<ChatMessage> = Vec::new();
        let est = big_suffix();
        let out = d.compact(&ctx(&t, &est)).await;
        assert_eq!(out.method, Some("second"));
        assert_eq!(out.notices.len(), 1);
        assert_eq!(out.notices.len(), 1);
        assert!(out.notices[0].contains("first"));
        assert!(out.notices[0].contains("falling through"));
    }

    #[tokio::test]
    async fn unavailable_flag_skips_silently() {
        let d = CompactionDispatcher::new(vec![
            Box::new(
                ScriptedMethod::new("first", MethodOutcome::NoChange).unavailable_flag(),
            ),
            Box::new(ScriptedMethod::new(
                "second",
                MethodOutcome::Plan(CompactionPlan::Summary {
                    covers_through: 0,
                    text: "s".to_string(),
                }),
            )),
        ]);
        let t: Vec<ChatMessage> = Vec::new();
        let est = big_suffix();
        let out = d.compact(&ctx(&t, &est)).await;
        assert_eq!(out.method, Some("second"));
        assert!(
            out.notices.is_empty(),
            "an unavailable=false skip is not a notice: {:?}",
            out.notices,
        );
    }

    #[tokio::test]
    async fn no_method_produces_a_plan() {
        let d = CompactionDispatcher::new(vec![
            Box::new(ScriptedMethod::new("first", MethodOutcome::NoChange)),
            Box::new(ScriptedMethod::new("second", MethodOutcome::NoChange)),
        ]);
        let t: Vec<ChatMessage> = Vec::new();
        let est = big_suffix();
        let out = d.compact(&ctx(&t, &est)).await;
        assert!(!out.has_plan());
        assert!(out.method.is_none());
    }

    #[tokio::test]
    async fn an_empty_plan_from_a_method_is_treated_as_no_change() {
        // A method that returns `Plan(empty)` is buggy but not fatal:
        // the dispatcher normalizes it to "no work" and moves on.
        let d = CompactionDispatcher::new(vec![
            Box::new(ScriptedMethod::new(
                "first",
                MethodOutcome::Plan(CompactionPlan::Prune(crate::prune::PrunePlan::default())),
            )),
            Box::new(ScriptedMethod::new(
                "second",
                MethodOutcome::Plan(CompactionPlan::Summary {
                    covers_through: 0,
                    text: "s".to_string(),
                }),
            )),
        ]);
        let t: Vec<ChatMessage> = Vec::new();
        let est = big_suffix();
        let out = d.compact(&ctx(&t, &est)).await;
        assert_eq!(out.method, Some("second"));
    }

    // ---- Default ladder shape ------------------------------------------

    #[test]
    fn the_default_ladder_matches_the_doc_order() {
        let d = CompactionDispatcher::default_ladder(ShakeConfig::default());
        assert_eq!(
            d.method_names(),
            vec!["remote", "snapcompact", "handoff", "shake", "soft"],
        );
    }

    // ---- ShakeMethod end-to-end ----------------------------------------

    #[tokio::test]
    async fn the_shake_method_produces_a_plan_for_a_large_fence() {
        let big = "x".repeat(100_000);
        let content = format!("```\n{big}\n```");
        let transcript = vec![user(&content)];

        let method = ShakeMethod::new(ShakeConfig {
            protect_tokens: 0,
            minimum_savings: 0,
            ..ShakeConfig::default()
        });
        let est = big_suffix();
        let c = ctx(&transcript, &est);
        assert!(method.available(&c));
        match method.run(&c).await {
            MethodOutcome::Plan(CompactionPlan::Shake(p)) => {
                assert_eq!(p.len(), 1);
            }
            other => panic!("expected a shake plan, got {other:?}", other = variant_name(&other)),
        }
    }

    #[tokio::test]
    async fn the_shake_method_returns_no_change_on_plain_prose() {
        let transcript = vec![user("just prose, no fences")];
        let method = ShakeMethod::new(ShakeConfig::default());
        let est = big_suffix();
        let c = ctx(&transcript, &est);
        assert!(matches!(
            method.run(&c).await,
            MethodOutcome::NoChange,
        ));
    }

    // ---- PruneMethod end-to-end ----------------------------------------

    #[tokio::test]
    async fn the_prune_method_produces_a_plan_for_a_superseded_read() {
        let big = "y".repeat(4_000);
        let transcript = vec![
            assistant_with_read("c1", "a.rs"),
            tool_result("c1", &big),
            assistant_with_read("c2", "a.rs"),
            tool_result("c2", "fresh"),
        ];
        let method = PruneMethod::new(crate::prune::PruneConfig {
            minimum_savings: 0,
            ..Default::default()
        });
        let est = big_suffix();
        let c = ctx(&transcript, &est);
        assert!(method.available(&c));
        match method.run(&c).await {
            MethodOutcome::Plan(CompactionPlan::Prune(p)) => {
                assert_eq!(p.len(), 1);
                // The placeholder matches the workspace's constant.
                assert!(matches!(
                    p.actions[0].1,
                    crate::prune::PruneAction::Blank { .. }
                ));
                let _ = SUPERSEDED_PLACEHOLDER;
            }
            other => panic!("expected a prune plan, got {}", variant_name(&other)),
        }
    }

    #[tokio::test]
    async fn the_prune_method_returns_no_change_on_a_fresh_transcript() {
        let transcript = vec![user("no reads here")];
        let method = PruneMethod::new(crate::prune::PruneConfig::default());
        let est = big_suffix();
        let c = ctx(&transcript, &est);
        assert!(matches!(
            method.run(&c).await,
            MethodOutcome::NoChange,
        ));
    }

    // ---- Stub availability ----------------------------------------------

    #[tokio::test]
    async fn every_remaining_stub_reports_unavailable_with_a_named_reason() {
        // Only `soft` is still a stub. `remote`, `handoff`, and
        // `snapcompact` are real methods now; their availability and
        // run semantics are exercised separately (see
        // `real_methods_are_unavailable_without_a_provider` below and
        // the snapcompact tests in `snapcompact.rs`).
        let transcript: Vec<ChatMessage> = Vec::new();
        let est = big_suffix();
        let c = ctx(&transcript, &est);

        let soft = SoftMethod::stub();
        assert!(soft.available(&c));
        match soft.run(&c).await {
            MethodOutcome::Unavailable(reason) => {
                assert!(reason.contains("soft"), "got: {reason}");
            }
            other => panic!("soft should be unavailable, got {}", variant_name(&other)),
        }
    }

    #[tokio::test]
    async fn real_methods_are_unavailable_without_a_provider() {
        // `remote`, `handoff`, and `snapcompact` are real methods
        // whose `available()` gates on a provider handle. A context
        // with no provider must report `Unavailable` from each, and
        // the reason must name the missing provider rather than the
        // missing wire support the stub-era messages referenced.
        let transcript: Vec<ChatMessage> = Vec::new();
        let est = big_suffix();
        let c = ctx(&transcript, &est);

        let snap = SnapcompactMethod::new();
        assert!(
            !snap.available(&c),
            "snapcompact without a provider is not available",
        );
        match snap.run(&c).await {
            MethodOutcome::Unavailable(reason) => {
                assert!(
                    reason.contains("provider"),
                    "snapcompact reason should name the missing provider: {reason}",
                );
            }
            other => panic!(
                "snapcompact should be unavailable, got {}",
                variant_name(&other),
            ),
        }

        let remote = RemoteMethod::new();
        assert!(
            !remote.available(&c),
            "remote without a provider is not available",
        );
        match remote.run(&c).await {
            MethodOutcome::Unavailable(reason) => {
                assert!(
                    reason.contains("provider"),
                    "remote reason should name the missing provider: {reason}",
                );
            }
            other => panic!("remote should be unavailable, got {}", variant_name(&other)),
        }

        let handoff = HandoffMethod::new();
        assert!(
            !handoff.available(&c),
            "handoff without a provider is not available",
        );
        match handoff.run(&c).await {
            MethodOutcome::Unavailable(reason) => {
                assert!(
                    reason.contains("provider"),
                    "handoff reason should name the missing provider: {reason}",
                );
            }
            other => panic!("handoff should be unavailable, got {}", variant_name(&other)),
        }
    }

    #[tokio::test]
    async fn the_default_ladder_falls_through_to_shake() {
        // The ladder is [remote, snapcompact, handoff, shake, soft].
        // The dispatcher stops at the first plan; shake produces one.
        //
        // Every rung before shake is now a real method whose
        // `available()` gates on a provider handle — `remote`,
        // `snapcompact` (also needs vision), and `handoff`. This
        // test builds a context with no provider, so all three are
        // skipped *without a notice* (availability, not failure).
        //
        // `soft` is behind shake and never runs.
        //
        // Net: no notices before shake.
        let big = "x".repeat(100_000);
        let content = format!("```\n{big}\n```");
        let transcript = vec![user(&content)];
        let est = big_suffix();

        let d = CompactionDispatcher::default_ladder(ShakeConfig {
            protect_tokens: 0,
            minimum_savings: 0,
            ..ShakeConfig::default()
        });
        let out = d.compact(&ctx(&transcript, &est)).await;
        assert_eq!(out.method, Some("shake"));
        assert!(matches!(out.plan, Some(CompactionPlan::Shake(_))));
        assert_eq!(
            out.notices.len(),
            0,
            "every rung before shake gates on a provider now \
             (remote, snapcompact, handoff), so none produces a \
             notice when the context has none; got: {:?}",
            out.notices,
        );
        assert!(
            !out.notices.iter().any(|n| n.contains("remote")),
            "remote without a provider is unavailable, not failed — no \
             notice: {:?}",
            out.notices,
        );
        assert!(
            !out.notices.iter().any(|n| n.contains("snapcompact")),
            "snapcompact without a vision provider is unavailable — no \
             notice: {:?}",
            out.notices,
        );
        assert!(
            !out.notices.iter().any(|n| n.contains("handoff")),
            "an unavailable (not failed) method does not produce a \
             notice: {:?}",
            out.notices,
        );
        assert!(
            !out.notices.iter().any(|n| n.contains("`soft`")),
            "soft is behind shake and must not have run: {:?}",
            out.notices,
        );
    }

    // ---- Helpers --------------------------------------------------------

    /// A short name for a `MethodOutcome` variant, for `panic!`
    /// messages. `Debug` on `MethodOutcome` is not derived (the
    /// `Plan` variant holds `CompactionPlan`, whose fields are
    /// large), so this helper spells the variant out.
    fn variant_name(o: &MethodOutcome) -> &'static str {
        match o {
            MethodOutcome::Plan(_) => "Plan",
            MethodOutcome::NoChange => "NoChange",
            MethodOutcome::Unavailable(_) => "Unavailable",
            MethodOutcome::Failed(_) => "Failed",
        }
    }

    /// A method whose `run` counts invocations. The counter is an
    /// `Arc<AtomicUsize>` shared with the test, so the count remains
    /// observable after the method has moved into `Box<dyn ...>`.
    struct CountingMethod {
        name: &'static str,
        calls: Arc<AtomicUsize>,
    }

    impl CountingMethod {
        fn new(name: &'static str) -> (Self, Arc<AtomicUsize>) {
            let calls = Arc::new(AtomicUsize::new(0));
            (
                Self {
                    name,
                    calls: Arc::clone(&calls),
                },
                calls,
            )
        }
    }

    #[async_trait]
    impl CompactionMethod for CountingMethod {
        fn name(&self) -> &'static str {
            self.name
        }
        fn available(&self, _ctx: &CompactionContext<'_>) -> bool {
            true
        }
        async fn run(&self, _ctx: &CompactionContext<'_>) -> MethodOutcome {
            self.calls.fetch_add(1, Ordering::SeqCst);
            MethodOutcome::NoChange
        }
    }

    #[tokio::test]
    async fn the_ladder_stops_at_the_first_plan() {
        // A method that returns a plan sits in front of two counting
        // methods. If the dispatcher stopped at the first plan (the
        // intended behavior), neither counter increments.
        let (stop, stop_count) = CountingMethod::new("stop");
        let (never, never_count) = CountingMethod::new("never");
        let d = CompactionDispatcher::new(vec![
            Box::new(ScriptedMethod::new(
                "first",
                MethodOutcome::Plan(CompactionPlan::Summary {
                    covers_through: 0,
                    text: "s".to_string(),
                }),
            )),
            Box::new(stop),
            Box::new(never),
        ]);
        let t: Vec<ChatMessage> = Vec::new();
        let est = big_suffix();
        let out = d.compact(&ctx(&t, &est)).await;
        assert_eq!(out.method, Some("first"));
        assert_eq!(
            stop_count.load(Ordering::SeqCst),
            0,
            "the second method must not run after the first produces a plan",
        );
        assert_eq!(
            never_count.load(Ordering::SeqCst),
            0,
            "the third method must not run either",
        );
    }

    #[tokio::test]
    async fn the_ladder_runs_every_method_when_none_produce_a_plan() {
        // Counterpart: with three NoChange methods, every one runs.
        // Pins that the early-stop behavior above is not a bug that
        // accidentally skips methods.
        let (a, a_count) = CountingMethod::new("a");
        let (b, b_count) = CountingMethod::new("b");
        let (c, c_count) = CountingMethod::new("c");
        let d = CompactionDispatcher::new(vec![
            Box::new(a),
            Box::new(b),
            Box::new(c),
        ]);
        let t: Vec<ChatMessage> = Vec::new();
        let est = big_suffix();
        let out = d.compact(&ctx(&t, &est)).await;
        assert!(!out.has_plan());
        assert_eq!(a_count.load(Ordering::SeqCst), 1);
        assert_eq!(b_count.load(Ordering::SeqCst), 1);
        assert_eq!(c_count.load(Ordering::SeqCst), 1);
    }
}
