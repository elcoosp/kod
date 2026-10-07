//! Engine-level tuning constants.
//!
//! Extracted from `engine/mod.rs`. Re-exported from `engine` so bare
//! names in sibling modules are unchanged.

/// Max agentic tool rounds per `process()` call before forcing a
/// summary. A single agentic pass typically uses 3–15 rounds for a
/// non-trivial task; 40 is a generous safety margin that catches a
/// runaway loop (a small model that keeps re-calling `read_file` on
/// the same path, unable to recognize it is done) well before the
/// user has waited minutes for nothing.
pub(crate) const MAX_TOOL_ROUNDS: usize = 40;
/// H-RL1: how many times the engine may sleep out a provider rate-limit
/// window and re-drive the same request within one turn (streaming and
/// collected paths share the cap). A second full window inside one turn
/// means the provider's quota is not coming back soon enough to be
/// worth parking the session again — the error surfaces instead.
pub(crate) const MAX_RATE_LIMIT_RETRIES: u32 = 2;
/// How many times the engine may sleep out a provider *overload*
/// (`server_busy`, ~10 min cooldown) and re-drive the same request
/// within one turn. Higher than [`MAX_RATE_LIMIT_RETRIES`]: overload
/// clears faster than a send-frequency quota, so two extra waits are
/// still worth parking the session for.
pub(crate) const MAX_SERVER_BUSY_RETRIES: u32 = 4;
/// Delta §4.5: the minimum token count a tool result must have
/// before the inline-imaging pass considers rasterizing it.
pub(crate) const MIN_INLINE_IMAGE_TOKENS: u64 = 3_000;
/// Notice appended to the conversation when the tool loop hits
/// [`MAX_TOOL_ROUNDS`] without a text-only reply. The loop calls the
/// provider one more time afterwards to request a summary; this note
/// is what steers that summary toward "what got done and what
/// remains" instead of "the model answers as if nothing unusual
/// happened." Without it the last tool-result block is the only
/// context for the summary, and a small model tends to summarize
/// that one result rather than the whole run.
pub(crate) const TOOL_ROUNDS_EXHAUSTED_NOTE: &str = "\n\n[tool-round limit reached — no further tool calls will run this turn. \
     Summarize what has been done so far and what remains.]";
/// Max turns of the `/goal` loop before it stops and reports progress.
pub(crate) const MAX_GOAL_TURNS: usize = 6;
/// How long a write approval waits for an answer before defaulting to
/// deny. A dialog nobody answers — the user closed the terminal, walked
/// away, or a script that cannot answer ran unattended — must not hang
/// the tool loop forever. Denying is the safe default: the file is not
/// written, the model sees the denial, and the user can re-run with
/// `tools.confirm_writes = false` to skip the prompt entirely.
pub(crate) const AWAIT_APPROVAL_SECS: u64 = 120;
/// Streaming chunk count between Jev early-termination checks
/// (P1.2). Every check is a Jev round-trip; running one per
/// chunk would double the stream's wall time on a fast
/// provider. Five is the empirical sweet spot: enough
/// coverage that we rarely miss a completion, few enough
/// that the network cost stays under 10% of streaming time.
pub(crate) const EARLY_TERM_CHECK_EVERY_CHUNKS: usize = 5;
/// Minimum accumulated response length (in chars) before an
/// early-termination check runs. A model that has emitted
/// fewer than this many characters has not yet said anything
/// a completion check could meaningfully judge. 400 chars ≈
/// 100 tokens — the same floor the design document cites.
pub(crate) const EARLY_TERM_MIN_CHARS: usize = 400;
/// Probability at or above which Jev is considered certain
/// the response is complete (P1.2). Below `[jev.thresholds]
/// .early_termination_min`, the stream continues. The default
/// matches `JevThresholds::default().early_termination_min`.
pub(crate) const EARLY_TERM_DEFAULT_MIN: f32 = 0.9;
/// Cap the remembered transcript: last turns, each truncated, total render
/// capped so history can never blow the context window on its own.
pub(crate) const MAX_HISTORY_TURNS: usize = 40;
/// Per-turn cap. A single turn can hold a code snippet, an error trace,
/// or a tool-result excerpt without being chopped. Was 1500, which was
/// smaller than a typical `read_file` output — every turn past the first
/// got truncated.
pub(crate) const MAX_TURN_CHARS: usize = 4_000;
/// Default total rendered-history budget, in chars. ~8k tokens at the
/// rough 4-chars-per-token approximation, which fits comfortably
/// alongside the prompt scaffolding (identity, environment, tool
/// inventory, skill inventory, user request) even on an 8k-context
/// model. Larger models should raise this via
/// [`KodEngine::set_history_budget`]; the TUI and CLI derive the value
/// from `LlmConfig::context_window` at startup.
pub const DEFAULT_HISTORY_CHAR_BUDGET: usize = 32_000;
/// Floor for a caller-supplied budget. Below this, history is so short
/// that the model effectively has no memory of past turns — that is
/// worse than the small-window default it is trying to protect, so we
/// clamp instead of silently dropping every turn.
pub(crate) const MIN_HISTORY_CHAR_BUDGET: usize = 4_000;
/// Default transcript key: the interactive session. Public methods
/// without an explicit key operate on this. Swarm agents use a
/// `swarm:<agent-id>` key so concurrent agents do not interleave their
/// turns into one shared history.
/// §7.3: how long a memory entry stays out of the prompt after being
/// injected once. 45 minutes: long enough that a working session does
/// not repeat itself, short enough that a fact is refreshed before it
/// is forgotten.
pub(crate) const MEMORY_INJECTION_TTL_MS: u64 = 45 * 60 * 1000;
pub(crate) const DEFAULT_TRANSCRIPT_KEY: &str = "";
