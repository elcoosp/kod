//! Cleanse loop: file-sticky streaming dispatch (borrow from
//! oh-my-pi, delta §11.9).
//!
//! # The problem
//!
//! A lint-repair fleet runs several workers against one repository.
//! If two workers edit the same file, their edits interleave and
//! neither lands cleanly. The design's rule: **two workers never edit
//! the same file** — a file is *sticky* to the first worker that takes
//! it, and every diagnostic for that file routes to that worker until
//! it releases.
//!
//! # The shape
//!
//! * `pending: file → diagnostics` — the backlog.
//! * `owned: file → owner` — who holds each file, whether they have an
//!   in-flight send, and whether they have released it.
//!
//! [`CleanseScheduler::take_batch`] hands a worker the files it may
//! own that are not already owned by someone else, up to a batch
//! budget. A follow-up for a file already owned by the same worker is
//! *single-flight*: it joins the worker's next batch rather than
//! starting a second send.
//!
//! # The batch budget
//!
//! `total_weight / max_agents` (the design's default 32 at the cap) so
//! one free worker cannot swallow the whole backlog while others idle.
//!
//! # What this is NOT
//!
//! * Not the worker. It decides which files a worker may take; running
//!   the model over them is the caller's job.
//! * Not the verifier. [`CleanseScheduler::verify`] reports whether
//!   anything is left; a real `verify()` that re-lints the repo is the
//!   caller's.

use std::collections::HashMap;

/// One diagnostic for a file. The scheduler does not interpret it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Diagnostic {
    pub line: u32,
    pub message: String,
}

/// A file's identity — the canonical path, as a string.
pub type FileKey = String;

/// The design's default batch budget.
pub const DEFAULT_BATCH_BUDGET: usize = 32;

/// A worker's claim on a file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OwnerEntry {
    pub worker: String,
    /// Diagnostics currently held for this file.
    pub held: usize,
    /// Whether a send for this file is in flight.
    pub sending: bool,
    /// Set when the worker has released the file; a released file is
    /// eligible for another worker.
    pub released: bool,
}

/// The verdict of a `verify` pass.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CleanseVerdict {
    /// Nothing pending and nothing owned: done.
    Clean,
    /// Files remain — either owned and in flight, or pending.
    Stalled,
}

/// The scheduler.
#[derive(Debug, Default)]
pub struct CleanseScheduler {
    pending: HashMap<FileKey, Vec<Diagnostic>>,
    owned: HashMap<FileKey, OwnerEntry>,
    max_agents: usize,
    batch_budget: Option<usize>,
}

impl CleanseScheduler {
    pub fn new(max_agents: usize) -> Self {
        Self {
            pending: HashMap::new(),
            owned: HashMap::new(),
            max_agents: max_agents.max(1),
            batch_budget: None,
        }
    }

    /// Override the batch budget. The default is
    /// [`DEFAULT_BATCH_BUDGET`] clamped to `total / max_agents`.
    pub fn with_batch_budget(mut self, budget: usize) -> Self {
        self.batch_budget = Some(budget);
        self
    }

    /// Add diagnostics for a file to the backlog.
    ///
    /// A file already owned by a worker joins that worker's next batch
    /// (single-flight): the diagnostics are appended to `pending`, and
    /// the owner's `held` count grows. A released file is re-claimable.
    pub fn enqueue(&mut self, file: impl Into<FileKey>, diags: Vec<Diagnostic>) {
        if diags.is_empty() {
            return;
        }
        let file = file.into();
        let n = diags.len();
        self.pending.entry(file.clone()).or_default().extend(diags);
        // A released file with new diagnostics is a fresh claim: drop
        // the ownership so any worker can take it. The previous shape
        // set `released = false`, which made the file *less* claimable
        // — `take_batch`'s fresh filter admits only an unowned or
        // released file, so flipping the flag hid the work.
        let released = self.owned.get(&file).map(|o| o.released).unwrap_or(false);
        if released {
            self.owned.remove(&file);
        } else if let Some(owner) = self.owned.get_mut(&file) {
            // A still-owned file's follow-up joins the owner's next
            // batch (single-flight): the diagnostics accumulate and
            // the held count grows.
            owner.held += n;
        }
    }

    /// The effective batch budget: the override, or
    /// `total_weight / max_agents` with the default as a floor.
    fn budget(&self) -> usize {
        if let Some(b) = self.batch_budget {
            return b;
        }
        let total: usize = self.pending.values().map(|v| v.len()).sum();
        let share = total / self.max_agents;
        share.clamp(1, DEFAULT_BATCH_BUDGET)
    }

    /// Take a batch for `worker`: the files this worker may edit.
    ///
    /// A file is taken when it has pending diagnostics and is either
    /// unowned or owned by this worker and not currently sending. The
    /// worker's own files are taken first, so a follow-up joins the
    /// in-flight work rather than starting fresh.
    ///
    /// Returns `None` when the worker has nothing to do.
    pub fn take_batch(&mut self, worker: &str) -> Option<Vec<(FileKey, Vec<Diagnostic>)>> {
        let budget = self.budget();
        let mut claimed: Vec<FileKey> = Vec::new();

        // Workers' own files first.
        for (file, owner) in &self.owned {
            if claimed.len() >= budget {
                break;
            }
            // F2h-13: the documented single-flight check — a file
            // with a send already in flight must not join a second
            // batch.
            if owner.worker == worker
                && !owner.released
                && !owner.sending
                && self.pending.contains_key(file)
            {
                claimed.push(file.clone());
            }
        }
        // Then unowned / released files, in a stable order.
        let mut fresh: Vec<FileKey> = self
            .pending
            .keys()
            .filter(|f| {
                !claimed.contains(f) && self.owned.get(*f).map(|o| o.released).unwrap_or(true)
            })
            .cloned()
            .collect();
        fresh.sort();
        for file in fresh {
            if claimed.len() >= budget {
                break;
            }
            claimed.push(file);
        }

        if claimed.is_empty() {
            return None;
        }

        let mut batch = Vec::with_capacity(claimed.len());
        for file in claimed {
            let diags = self.pending.remove(&file).unwrap_or_default();
            let held = diags.len();
            self.owned.insert(
                file.clone(),
                OwnerEntry {
                    worker: worker.to_string(),
                    held,
                    sending: true,
                    released: false,
                },
            );
            batch.push((file, diags));
        }
        Some(batch)
    }

    /// Mark a worker's send complete. The file stays owned (sticky)
    /// until the worker releases it; `held` is cleared.
    pub fn complete_send(&mut self, worker: &str, file: &str) {
        if let Some(owner) = self.owned.get_mut(file)
            && owner.worker == worker
        {
            owner.sending = false;
            owner.held = 0;
        }
    }

    /// Release a file so another worker may claim it. Called when a
    /// worker has fixed its diagnostics and verified the file.
    pub fn release(&mut self, worker: &str, file: &str) {
        if let Some(owner) = self.owned.get_mut(file)
            && owner.worker == worker
        {
            owner.released = true;
            owner.sending = false;
        }
    }

    /// The verdict: clean when nothing is pending and no file is
    /// owned-and-unreleased.
    pub fn verify(&self) -> CleanseVerdict {
        let pending_any = self.pending.values().any(|v| !v.is_empty());
        let owned_any = self.owned.values().any(|o| !o.released);
        if pending_any || owned_any {
            CleanseVerdict::Stalled
        } else {
            CleanseVerdict::Clean
        }
    }

    /// How many files are pending.
    pub fn pending_files(&self) -> usize {
        self.pending.keys().count()
    }

    /// The owner of a file, if any.
    pub fn owner_of(&self, file: &str) -> Option<&OwnerEntry> {
        self.owned.get(file)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn diag(line: u32) -> Diagnostic {
        Diagnostic {
            line,
            message: format!("lint at {line}"),
        }
    }

    #[test]
    fn an_empty_scheduler_is_clean() {
        assert_eq!(CleanseScheduler::new(2).verify(), CleanseVerdict::Clean);
    }

    #[test]
    fn enqueuing_makes_it_stalled() {
        let mut s = CleanseScheduler::new(2);
        s.enqueue("a.rs", vec![diag(1)]);
        assert_eq!(s.verify(), CleanseVerdict::Stalled);
    }

    #[test]
    fn take_batch_claims_the_file() {
        let mut s = CleanseScheduler::new(2);
        s.enqueue("a.rs", vec![diag(1)]);
        let b = s.take_batch("w1").unwrap();
        assert_eq!(b.len(), 1);
        assert_eq!(b[0].0, "a.rs");
        assert_eq!(b[0].1.len(), 1);
        assert_eq!(s.owner_of("a.rs").unwrap().worker, "w1");
    }

    #[test]
    fn a_second_worker_cannot_take_an_owned_file() {
        let mut s = CleanseScheduler::new(2);
        s.enqueue("a.rs", vec![diag(1)]);
        let _ = s.take_batch("w1").unwrap();
        // w1 still holds it (not released), so w2 gets nothing.
        assert!(s.take_batch("w2").is_none());
    }

    #[test]
    fn a_released_file_is_claimable_again() {
        let mut s = CleanseScheduler::new(2);
        s.enqueue("a.rs", vec![diag(1)]);
        let _ = s.take_batch("w1").unwrap();
        s.complete_send("w1", "a.rs");
        s.release("w1", "a.rs");
        // New diagnostics for the same file, and the release lets
        // another worker claim.
        s.enqueue("a.rs", vec![diag(2)]);
        let b = s.take_batch("w2").unwrap();
        assert_eq!(b[0].0, "a.rs");
        assert_eq!(s.owner_of("a.rs").unwrap().worker, "w2");
    }

    #[test]
    fn a_follow_up_joins_the_same_worker() {
        let mut s = CleanseScheduler::new(2);
        s.enqueue("a.rs", vec![diag(1)]);
        let _ = s.take_batch("w1").unwrap();
        s.complete_send("w1", "a.rs");
        // New diagnostics while w1 still owns the file.
        s.enqueue("a.rs", vec![diag(2), diag(3)]);
        let b = s.take_batch("w1").unwrap();
        assert_eq!(b[0].0, "a.rs");
        assert_eq!(b[0].1.len(), 2, "the follow-up joins w1's next batch");
    }

    #[test]
    fn the_batch_budget_limits_one_worker() {
        let mut s = CleanseScheduler::new(1).with_batch_budget(2);
        for i in 0..10 {
            s.enqueue(format!("f{i}.rs"), vec![diag(1)]);
        }
        let b = s.take_batch("w1").unwrap();
        assert_eq!(b.len(), 2, "one worker takes at most the budget");
    }

    #[test]
    fn the_default_budget_shares_across_agents() {
        let mut s = CleanseScheduler::new(4);
        for i in 0..100 {
            s.enqueue(format!("f{i}.rs"), vec![diag(1)]);
        }
        // total 100 / 4 agents = 25, under the 32 default.
        let b = s.take_batch("w1").unwrap();
        assert_eq!(b.len(), 25, "a quarter of the backlog");
    }

    #[test]
    fn the_default_budget_is_capped_at_32() {
        let mut s = CleanseScheduler::new(1);
        for i in 0..100 {
            s.enqueue(format!("f{i}.rs"), vec![diag(1)]);
        }
        // 100 / 1 = 100, capped at 32.
        let b = s.take_batch("w1").unwrap();
        assert_eq!(b.len(), 32);
    }

    #[test]
    fn a_worker_takes_its_own_files_first() {
        let mut s = CleanseScheduler::new(2).with_batch_budget(2);
        s.enqueue("owned.rs", vec![diag(1)]);
        let _ = s.take_batch("w1").unwrap();
        s.complete_send("w1", "owned.rs");
        // Two more files arrive.
        s.enqueue("a.rs", vec![diag(1)]);
        s.enqueue("b.rs", vec![diag(1)]);
        s.enqueue("owned.rs", vec![diag(2)]);
        let b = s.take_batch("w1").unwrap();
        assert_eq!(b[0].0, "owned.rs", "own file first");
        assert_eq!(b.len(), 2);
    }

    #[test]
    fn verify_is_clean_after_every_file_releases() {
        let mut s = CleanseScheduler::new(2);
        s.enqueue("a.rs", vec![diag(1)]);
        s.enqueue("b.rs", vec![diag(1)]);
        // The default budget shares the backlog across agents, so with
        // two files and two agents one `take_batch` yields one file.
        // Drain until the scheduler has nothing left, which is what a
        // worker loop does.
        while let Some(batch) = s.take_batch("w1") {
            for (f, _) in &batch {
                s.complete_send("w1", f);
                s.release("w1", f);
            }
        }
        assert_eq!(s.verify(), CleanseVerdict::Clean);
    }

    #[test]
    fn verify_stalls_while_a_file_is_held() {
        let mut s = CleanseScheduler::new(2);
        s.enqueue("a.rs", vec![diag(1)]);
        let _ = s.take_batch("w1").unwrap();
        assert_eq!(s.verify(), CleanseVerdict::Stalled);
    }

    #[test]
    fn enqueuing_an_empty_diag_list_is_a_no_op() {
        let mut s = CleanseScheduler::new(2);
        s.enqueue("a.rs", vec![]);
        assert_eq!(s.pending_files(), 0);
    }

    #[test]
    fn take_batch_on_an_empty_scheduler_is_none() {
        assert!(CleanseScheduler::new(2).take_batch("w1").is_none());
    }

    #[test]
    fn release_by_a_non_owner_is_a_no_op() {
        let mut s = CleanseScheduler::new(2);
        s.enqueue("a.rs", vec![diag(1)]);
        let _ = s.take_batch("w1").unwrap();
        s.release("w2", "a.rs");
        // w1 still owns it.
        assert_eq!(s.owner_of("a.rs").unwrap().worker, "w1");
        assert!(!s.owner_of("a.rs").unwrap().released);
    }
}
