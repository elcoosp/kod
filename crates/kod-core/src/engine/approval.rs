//! Approval-request and tool-filter types.
//!
//! Extracted from `engine/mod.rs`. These are the data shapes the
//! engine hands to the TUI/CLI/serve layers when a tool needs
//! approval, plus the `ToolFilterState` that decides when to re-run
//! the Jev tool-inventory filter within a session.

use super::*;


/// The serialized shape of an approval request. Sent as JSON inside an
/// [`tool_approval_marker`] chunk, decoded by the consumer.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ApprovalRequest {
    /// The tool the model wants to run.
    pub tool_name: String,
    /// The arguments the model passed. Included so a consumer that
    /// wants a specific view (the CLI prints the JSON verbatim) does
    /// not have to re-invoke anything.
    pub arguments: serde_json::Value,
    /// A unified diff of the intended change, when one could be
    /// computed. `None` when the target does not exist yet (a
    /// create) or the file is binary.
    pub diff: Option<String>,
    /// The user-facing summary a plain-text consumer can print
    /// without decoding `diff`.
    pub summary: String,
    /// The engine's internal approval id. Present in items of an
    /// [`ApprovalBatch`]; `None` on the legacy single-item marker
    /// (which carries the id out-of-band as
    /// `\0kod-approval:<id>:<json>`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<u64>,
}


/// A single round's worth of approvals, emitted together so a
/// consumer can present them as one batch.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ApprovalBatch {
    pub items: Vec<ApprovalRequest>,
}


/// What the consumer decides.
#[derive(Debug, Clone, PartialEq)]
pub enum ApprovalDecision {
    /// Run the call as the model proposed it.
    Approve,
    /// Run the call with these arguments substituted for the model's
    /// (Tier 2.3). The approval overlay's "edit" action sends this.
    ApproveWith {
        arguments: serde_json::Value,
    },
    Deny,
    /// Same as `Deny` in this version; the variant exists so that
    /// adding a "remember my choice" set later does not change the
    /// wire format.
    DenyAlways,
}


impl ApprovalDecision {
    /// True for both `Approve` and `ApproveWith`.
    pub fn is_approve(&self) -> bool {
        matches!(
            self,
            ApprovalDecision::Approve | ApprovalDecision::ApproveWith { .. },
        )
    }
}


/// Hysteresis state for the per-turn Jev tool-category filter (P0).
///
/// The tool list sits in the cached prefix (Anthropic caches the
/// ordered request stream up to the system marker, and tools precede
/// system on the wire), so every time Jev flips a category the whole
/// cached prefix — tools, system, repo map — goes cold. The
/// registry's own tools-by-name sort exists to keep the prefix
/// byte-stable; the per-turn filter was working against that.
///
/// This state records the last committed category set and the
/// classification that produced it. A refilter is allowed only when
/// the classified task signature differs from the committed one *and*
/// the committed set has survived `min_stable_turns` — enough turns
/// to have paid back the cache write it cost.
#[derive(Debug, Clone)]
pub(crate) struct ToolFilterState {
    /// Categories the committed filter kept. A refilter replaces this.
    pub enabled_categories: std::collections::HashSet<kod_types::ToolCategory>,
    /// The task signature the committed filter was computed for.
    pub committed_signature: String,
    /// Turns elapsed since the last commit. Incremented on every
    /// `filter_tool_definitions_with_hysteresis` call; reset to zero
    /// on commit.
    pub turns_since_change: u64,
    /// How many consecutive turns the committed set must survive
    /// before a different signature is allowed to replace it. Three
    /// is a design choice: two is too eager (a single mis-classified
    /// turn flips the set), five is too slow (a real task change
    /// waits half a minute on a chatty session).
    pub min_stable_turns: u64,
    /// One-shot gate: set when a commit changed the enabled set.
    /// The next `build_grounded_request` for this key consumes it and
    /// clears the transcript cache breakpoint for that single
    /// request. The round that first sees a changed prefix should not
    /// pay Anthropic's 1.25x cache-write premium for a prefix that
    /// may not survive the next round either.
    pub suppress_marker_once: bool,
}


impl ToolFilterState {
    /// A fresh state with no committed signature: the first call
    /// always refilters, since there is nothing to be stable against.
    pub(crate) fn fresh() -> Self {
        Self {
            enabled_categories: std::collections::HashSet::new(),
            committed_signature: String::new(),
            turns_since_change: 0,
            min_stable_turns: 3,
            suppress_marker_once: false,
        }
    }

    /// Whether a refilter is allowed for `task_sig`.
    ///
    /// Returns `false` when the signature is unchanged (the committed
    /// set is still correct) or when it changed but the committed
    /// set has not yet seasoned.
    pub(crate) fn should_refilter(&self, task_sig: &str) -> bool {
        if task_sig == self.committed_signature {
            return false;
        }
        self.turns_since_change >= self.min_stable_turns
    }

    /// Record that this turn passed. Called on every filter attempt,
    /// so `turns_since_change` tracks wall-clock turns rather than
    /// filter attempts.
    pub(crate) fn tick(&mut self) {
        self.turns_since_change = self.turns_since_change.saturating_add(1);
    }

    /// Commit a new signature and category set. Resets the stable
    /// counter so the *next* change must season again.
    pub(crate) fn commit(
        &mut self,
        task_sig: String,
        cats: std::collections::HashSet<kod_types::ToolCategory>,
    ) {
        self.committed_signature = task_sig;
        self.enabled_categories = cats;
        self.turns_since_change = 0;
    }
}

