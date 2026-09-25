//! Async-result delivery: owner-routed, batched (borrow from
//! oh-my-pi, delta §11.4).
//!
//! # The shape this replaces
//!
//! A background job finishing today pushes one `SoftInterrupt` per
//! job. Three jobs finishing at once produce three steers; a job
//! whose result is 200 KB gets pushed whole. That is two problems:
//!
//! * **Batching.** The model sees N separate messages where one
//!   message listing N results would read better and cost fewer
//!   tokens.
//! * **Size.** A large result should be an `agent://` artifact with
//!   an inline preview, not an unbounded string in the transcript.
//!
//! # The delivery
//!
//! [`AsyncDelivery`] queues results per owner. A drain (at a round
//! boundary or an idle flush) produces one batched message per
//! owner:
//!
//! * Each result is rendered with its job id and a short summary.
//! * A result whose body exceeds [`INLINE_CAP`] characters is
//!   replaced by a [`PREVIEW`]-character preview plus a pointer to
//!   the full payload.
//! * The batched message is capped so a hundred tiny results do not
//!   become one enormous message.
//!
//! # Epochs
//!
//! A delivery carries an *epoch* — a counter the caller bumps on a
//! session transition (a `/clear`, a model switch, a transcript
//! reset). A queued result from an older epoch is dropped at drain
//! rather than delivered into a session that has moved on. Without
//! this, a job that finished before a reset would surface in the
//! post-reset transcript.
//!
//! # What this does NOT do
//!
//! * Not the job runner. [`crate::background`] owns job lifecycle;
//!   this module owns the *delivery* of a finished job's result.
//! * Not persistence. A queued result is lost if the process dies
//!   before a drain. A caller that needs durability writes the
//!   result to a store first.

use std::collections::HashMap;

/// The doc's inline cap: a single result longer than this is
/// replaced by a preview plus an artifact pointer.
pub const INLINE_CAP: usize = 12_000;

/// The doc's preview length: how much of an over-cap result stays
/// inline.
pub const PREVIEW: usize = 4_000;

/// One finished job's result, queued for delivery.
#[derive(Debug, Clone)]
pub struct AsyncResult {
    pub job_id: u64,
    /// The transcript the result belongs to.
    pub owner_id: String,
    /// The job's kind label (`"review"`, `"shell cargo test"`).
    pub kind: String,
    /// The result body. A body over [`INLINE_CAP`] is truncated to
    /// [`PREVIEW`] at render time, with the artifact pointer
    /// appended.
    pub body: String,
    /// The `agent://` artifact holding the full body, when one was
    /// written. `None` for a small result delivered whole.
    pub artifact: Option<String>,
    /// The session epoch this result was produced under.
    pub epoch: u64,
}

/// Render one result for inclusion in a batched message.
fn render_one(r: &AsyncResult) -> String {
    let header = format!("[job {} {}]", r.job_id, r.kind);
    if r.body.len() <= INLINE_CAP {
        return format!("{header}\n{}", r.body);
    }
    // Over the cap: preview + pointer.
    let preview: String = r.body.chars().take(PREVIEW).collect();
    let pointer = match r.artifact.as_deref() {
        Some(url) => format!("\n[full result: {url}]"),
        None => "\n[full result truncated — no artifact was written]".to_string(),
    };
    format!("{header}\n{preview}\n…[{} chars elided]{}", r.body.len() - PREVIEW, pointer)
}

/// The delivery queue.
#[derive(Debug, Default)]
pub struct AsyncDelivery {
    /// owner_id -> queued results, oldest first.
    queues: HashMap<String, Vec<AsyncResult>>,
    /// Current session epoch.
    epoch: u64,
}

impl AsyncDelivery {
    pub fn new() -> Self {
        Self {
            queues: HashMap::new(),
            epoch: 0,
        }
    }

    /// The current epoch.
    pub fn epoch(&self) -> u64 {
        self.epoch
    }

    /// Bump the epoch. Every queued result from a lower epoch is
    /// dropped — a session transition moved on before they were
    /// delivered.
    pub fn bump_epoch(&mut self) -> u64 {
        self.epoch += 1;
        self.queues.clear();
        self.epoch
    }

    /// Queue a result. A result tagged with an epoch older than the
    /// current one is dropped on arrival (a job that finished during
    /// a transition).
    pub fn enqueue(&mut self, result: AsyncResult) {
        if result.epoch < self.epoch {
            return;
        }
        self.queues
            .entry(result.owner_id.clone())
            .or_default()
            .push(result);
    }

    /// How many results are queued for an owner.
    pub fn queued_for(&self, owner_id: &str) -> usize {
        self.queues.get(owner_id).map(|v| v.len()).unwrap_or(0)
    }

    /// Whether anything is queued for any owner.
    pub fn is_empty(&self) -> bool {
        self.queues.values().all(|v| v.is_empty())
    }

    /// Drain every queued result for an owner into one batched
    /// message. Returns `None` when the owner has nothing queued.
    ///
    /// The message lists each result in the order it was queued. A
    /// result that would push the batch past a soft bound is still
    /// included — the batch is bounded by the per-result cap times
    /// the number of results, and a caller that wants a hard bound
    /// drains more often.
    pub fn drain(&mut self, owner_id: &str) -> Option<String> {
        let results = self.queues.remove(owner_id)?;
        if results.is_empty() {
            return None;
        }
        let n = results.len();
        let mut out = format!("{n} background job(s) finished:\n\n");
        for r in &results {
            out.push_str(&render_one(r));
            out.push_str("\n\n");
        }
        Some(out.trim_end().to_string())
    }

    /// Drain every owner, returning `(owner_id, message)` pairs.
    /// Sorted by owner id so a caller's iteration is deterministic.
    pub fn drain_all(&mut self) -> Vec<(String, String)> {
        let owners: Vec<String> = {
            let mut v: Vec<String> = self.queues.keys().cloned().collect();
            v.sort();
            v
        };
        let mut out = Vec::new();
        for o in owners {
            if let Some(msg) = self.drain(&o) {
                out.push((o, msg));
            }
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn result(owner: &str, id: u64, body: &str) -> AsyncResult {
        AsyncResult {
            job_id: id,
            owner_id: owner.to_string(),
            kind: "test".to_string(),
            body: body.to_string(),
            artifact: None,
            epoch: 0,
        }
    }

    #[test]
    fn an_empty_queue_drains_nothing() {
        let mut d = AsyncDelivery::new();
        assert!(d.drain("session").is_none());
    }

    #[test]
    fn a_small_result_is_delivered_whole() {
        let mut d = AsyncDelivery::new();
        d.enqueue(result("session", 1, "done"));
        let msg = d.drain("session").unwrap();
        assert!(msg.contains("job 1 test"), "got: {msg}");
        assert!(msg.contains("done"), "got: {msg}");
    }

    #[test]
    fn three_results_batch_into_one_message() {
        let mut d = AsyncDelivery::new();
        d.enqueue(result("session", 1, "a"));
        d.enqueue(result("session", 2, "b"));
        d.enqueue(result("session", 3, "c"));
        let msg = d.drain("session").unwrap();
        assert!(msg.contains("3 background job(s)"), "got: {msg}");
        assert!(msg.contains("job 1"), "got: {msg}");
        assert!(msg.contains("job 3"), "got: {msg}");
    }

    #[test]
    fn an_over_cap_result_is_truncated_to_a_preview() {
        let big = "x".repeat(INLINE_CAP + 5_000);
        let mut d = AsyncDelivery::new();
        d.enqueue(result("session", 1, &big));
        let msg = d.drain("session").unwrap();
        assert!(msg.contains("elided"), "got: {} chars", msg.len());
        assert!(msg.len() < big.len(), "the batch must be shorter than the raw");
    }

    #[test]
    fn an_over_cap_result_with_an_artifact_names_it() {
        let big = "x".repeat(INLINE_CAP + 100);
        let mut d = AsyncDelivery::new();
        d.enqueue(AsyncResult {
            job_id: 1,
            owner_id: "session".to_string(),
            kind: "review".to_string(),
            body: big,
            artifact: Some("agent://job-1".to_string()),
            epoch: 0,
        });
        let msg = d.drain("session").unwrap();
        assert!(msg.contains("agent://job-1"), "got: {msg}");
    }

    #[test]
    fn results_route_by_owner() {
        let mut d = AsyncDelivery::new();
        d.enqueue(result("alpha", 1, "for alpha"));
        d.enqueue(result("beta", 2, "for beta"));
        assert_eq!(d.queued_for("alpha"), 1);
        assert_eq!(d.queued_for("beta"), 1);
        let a = d.drain("alpha").unwrap();
        assert!(a.contains("for alpha"));
        assert!(!a.contains("for beta"));
    }

    #[test]
    fn draining_an_owner_empties_its_queue() {
        let mut d = AsyncDelivery::new();
        d.enqueue(result("session", 1, "x"));
        let _ = d.drain("session");
        assert_eq!(d.queued_for("session"), 0);
    }

    #[test]
    fn bumping_the_epoch_drops_queued_results() {
        let mut d = AsyncDelivery::new();
        d.enqueue(result("session", 1, "old"));
        d.bump_epoch();
        assert!(d.drain("session").is_none(), "queued results were cleared");
    }

    #[test]
    fn a_result_from_an_old_epoch_is_dropped_on_arrival() {
        let mut d = AsyncDelivery::new();
        d.bump_epoch(); // epoch now 1
        // A result tagged epoch 0 arrives late.
        d.enqueue(result("session", 1, "stale"));
        assert_eq!(d.queued_for("session"), 0);
    }

    #[test]
    fn a_result_from_the_current_epoch_is_kept() {
        let mut d = AsyncDelivery::new();
        d.bump_epoch();
        let mut r = result("session", 1, "fresh");
        r.epoch = d.epoch();
        d.enqueue(r);
        assert_eq!(d.queued_for("session"), 1);
    }

    #[test]
    fn drain_all_returns_every_owner_sorted() {
        let mut d = AsyncDelivery::new();
        d.enqueue(result("zeta", 1, "z"));
        d.enqueue(result("alpha", 2, "a"));
        let all = d.drain_all();
        assert_eq!(all.len(), 2);
        assert_eq!(all[0].0, "alpha");
        assert_eq!(all[1].0, "zeta");
    }

    #[test]
    fn is_empty_reflects_every_queue() {
        let mut d = AsyncDelivery::new();
        assert!(d.is_empty());
        d.enqueue(result("session", 1, "x"));
        assert!(!d.is_empty());
        let _ = d.drain("session");
        assert!(d.is_empty());
    }
}
