//! Per-tool quotas (Tier 2.5).
//!
//! Counts tool invocations per turn and per session. A quota's
//! soft threshold (80 %) surfaces a warning to the model; the hard
//! cap refuses the call with a descriptive error. Both per-turn and
//! per-session counters are tracked in one place so the call site has
//! a single predicate to ask.

use kod_config::ToolQuota;
use parking_lot::Mutex;
use std::collections::HashMap;

/// Live counters. Clone shares state; the engine holds one.
#[derive(Clone)]
pub struct ToolCounts {
    inner: std::sync::Arc<Mutex<ToolCountsInner>>,
}

struct ToolCountsInner {
    /// Calls this turn, by tool name.
    per_turn: HashMap<String, usize>,
    /// Calls this session, by tool name.
    per_session: HashMap<String, usize>,
    /// Same-command-string runs this turn. Only `execute_command`
    /// increments this. Keyed by tool name → command → count.
    per_command: HashMap<(String, String), usize>,
}

impl Default for ToolCounts {
    fn default() -> Self {
        Self::new()
    }
}

impl ToolCounts {
    pub fn new() -> Self {
        Self {
            inner: std::sync::Arc::new(Mutex::new(ToolCountsInner {
                per_turn: HashMap::new(),
                per_session: HashMap::new(),
                per_command: HashMap::new(),
            })),
        }
    }

    /// Record a dispatch, before the tool runs.
    pub fn record(&self, tool: &str, command: Option<&str>) {
        let mut g = self.inner.lock();
        *g.per_turn.entry(tool.to_string()).or_insert(0) += 1;
        *g.per_session.entry(tool.to_string()).or_insert(0) += 1;
        if let Some(c) = command {
            *g.per_command.entry((tool.to_string(), c.to_string())).or_insert(0) += 1;
        }
    }

    /// Reset per-turn counters. Called at the top of every user turn.
    pub fn begin_turn(&self) {
        let mut g = self.inner.lock();
        g.per_turn.clear();
        g.per_command.clear();
    }

    /// Reset everything. Called by `/limits reset`.
    pub fn reset(&self) {
        let mut g = self.inner.lock();
        g.per_turn.clear();
        g.per_session.clear();
        g.per_command.clear();
    }

    pub fn per_turn(&self, tool: &str) -> usize {
        self.inner
            .lock()
            .per_turn
            .get(tool)
            .copied()
            .unwrap_or(0)
    }

    pub fn per_session(&self, tool: &str) -> usize {
        self.inner
            .lock()
            .per_session
            .get(tool)
            .copied()
            .unwrap_or(0)
    }

    pub fn per_command(&self, tool: &str, command: &str) -> usize {
        self.inner
            .lock()
            .per_command
            .get(&(tool.to_string(), command.to_string()))
            .copied()
            .unwrap_or(0)
    }

    /// A snapshot of every counter, for `/limits`.
    pub fn snapshot(&self) -> Vec<(String, usize, usize)> {
        let g = self.inner.lock();
        let mut names: Vec<String> = g.per_session.keys().cloned().collect();
        names.sort();
        names
            .into_iter()
            .map(|n| {
                let t = g.per_turn.get(&n).copied().unwrap_or(0);
                let s = g.per_session.get(&n).copied().unwrap_or(0);
                (n, t, s)
            })
            .collect()
    }
}

/// The outcome of checking a quota before dispatch.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum QuotaVerdict {
    /// Within every limit.
    Ok,
    /// Approaching the cap; the tool should still run.
    Soft { reason: String },
    /// At or over a hard cap; the tool must not run.
    Hard { reason: String },
}

/// Check the quota for a tool. `quota` is the resolved
/// `ToolQuota` (explicit or default entry); `None` means no quota.
pub fn check(
    counts: &ToolCounts,
    tool: &str,
    quota: Option<&ToolQuota>,
    command: Option<&str>,
) -> QuotaVerdict {
    let Some(q) = quota else {
        return QuotaVerdict::Ok;
    };
    let t = counts.per_turn(tool);
    let s = counts.per_session(tool);

    if q.per_session > 0 && s >= q.per_session {
        return QuotaVerdict::Hard {
            reason: format!(
                "tool `{tool}` has used all {} of its per-session quota",
                q.per_session,
            ),
        };
    }
    if q.per_turn > 0 && t >= q.per_turn {
        return QuotaVerdict::Hard {
            reason: format!(
                "tool `{tool}` has used all {} of its per-turn quota",
                q.per_turn,
            ),
        };
    }
    if let Some(cmd) = command
        && q.per_command > 0
    {
        let pc = counts.per_command(tool, cmd);
        if pc >= q.per_command {
            return QuotaVerdict::Hard {
                reason: format!(
                    "tool `{tool}` has run the same command {pc} times this turn (cap {})",
                    q.per_command,
                ),
            };
        }
    }

    // Soft threshold at 80 % of either cap.
    if q.per_session > 0 && s * 100 >= q.per_session * 80 {
        return QuotaVerdict::Soft {
            reason: format!(
                "`{tool}` has used {s}/{} of its session quota",
                q.per_session,
            ),
        };
    }
    if q.per_turn > 0 && t * 100 >= q.per_turn * 80 {
        return QuotaVerdict::Soft {
            reason: format!(
                "`{tool}` has used {t}/{} of its per-turn quota",
                q.per_turn,
            ),
        };
    }
    QuotaVerdict::Ok
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn record_accumulates_per_turn_and_session() {
        let c = ToolCounts::new();
        c.record("grep", None);
        c.record("grep", None);
        assert_eq!(c.per_turn("grep"), 2);
        assert_eq!(c.per_session("grep"), 2);
    }

    #[test]
    fn begin_turn_resets_turn_only() {
        let c = ToolCounts::new();
        c.record("grep", None);
        c.begin_turn();
        assert_eq!(c.per_turn("grep"), 0);
        assert_eq!(c.per_session("grep"), 1);
    }

    #[test]
    fn reset_clears_everything() {
        let c = ToolCounts::new();
        c.record("grep", None);
        c.record("read_file", None);
        c.reset();
        assert_eq!(c.per_turn("grep"), 0);
        assert_eq!(c.per_session("grep"), 0);
    }

    #[test]
    fn per_command_counts_same_command_only() {
        let c = ToolCounts::new();
        c.record("execute_command", Some("cargo test"));
        c.record("execute_command", Some("cargo test"));
        c.record("execute_command", Some("cargo build"));
        assert_eq!(c.per_command("execute_command", "cargo test"), 2);
        assert_eq!(c.per_command("execute_command", "cargo build"), 1);
    }

    #[test]
    fn no_quota_means_ok() {
        let c = ToolCounts::new();
        assert_eq!(check(&c, "grep", None, None), QuotaVerdict::Ok);
    }

    #[test]
    fn soft_then_hard_on_per_turn() {
        // 80% of 10 = 8, so after 8 calls the verdict is Soft; after
        // 10 it is Hard.
        let c = ToolCounts::new();
        let q = ToolQuota { per_turn: 10, per_session: 0, per_command: 0 };
        for _ in 0..8 {
            c.record("grep", None);
        }
        assert!(matches!(check(&c, "grep", Some(&q), None), QuotaVerdict::Soft { .. }));
        c.record("grep", None);
        c.record("grep", None);
        assert!(matches!(check(&c, "grep", Some(&q), None), QuotaVerdict::Hard { .. }));
    }

    #[test]
    fn small_caps_skip_the_soft_window() {
        // A cap of 2 has no reachable 80%..99% window: 1 is below
        // soft, 2 is hard.
        let c = ToolCounts::new();
        let q = ToolQuota { per_turn: 2, per_session: 0, per_command: 0 };
        c.record("grep", None);
        assert_eq!(check(&c, "grep", Some(&q), None), QuotaVerdict::Ok);
        c.record("grep", None);
        assert!(matches!(check(&c, "grep", Some(&q), None), QuotaVerdict::Hard { .. }));
    }

    #[test]
    fn hard_cap_on_per_session_wins_over_per_turn() {
        let c = ToolCounts::new();
        let q = ToolQuota { per_turn: 100, per_session: 1, per_command: 0 };
        c.record("grep", None);
        let v = check(&c, "grep", Some(&q), None);
        assert!(matches!(v, QuotaVerdict::Hard { .. }));
    }

    #[test]
    fn per_command_cap() {
        let c = ToolCounts::new();
        let q = ToolQuota { per_turn: 0, per_session: 0, per_command: 2 };
        c.record("execute_command", Some("ls"));
        c.record("execute_command", Some("ls"));
        let v = check(&c, "execute_command", Some(&q), Some("ls"));
        assert!(matches!(v, QuotaVerdict::Hard { .. }));
        // A different command still has room.
        let ok = check(&c, "execute_command", Some(&q), Some("pwd"));
        assert_eq!(ok, QuotaVerdict::Ok);
    }

    #[test]
    fn soft_fires_at_80_percent() {
        let c = ToolCounts::new();
        let q = ToolQuota { per_turn: 10, per_session: 100, per_command: 0 };
        for _ in 0..8 {
            c.record("grep", None);
        }
        assert!(matches!(check(&c, "grep", Some(&q), None), QuotaVerdict::Soft { .. }));
    }

    #[test]
    fn disabled_quota_means_ok() {
        let c = ToolCounts::new();
        let q = ToolQuota { per_turn: 0, per_session: 0, per_command: 0 };
        for _ in 0..100 {
            c.record("grep", None);
        }
        assert_eq!(check(&c, "grep", Some(&q), None), QuotaVerdict::Ok);
    }

    #[test]
    fn snapshot_lists_all_counters() {
        let c = ToolCounts::new();
        c.record("grep", None);
        c.record("read_file", None);
        c.record("grep", None);
        let snap = c.snapshot();
        assert_eq!(snap.len(), 2);
        let grep = snap.iter().find(|(n, _, _)| n == "grep").unwrap();
        assert_eq!(grep.1, 2);
        assert_eq!(grep.2, 2);
    }
}
