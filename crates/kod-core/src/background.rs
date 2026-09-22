//! Background read-only jobs (P6).
//!
//! Cross-model review, background evals, and docs generation run
//! alongside the interactive session. The design's core safety
//! property is that a background job **cannot** mutate the
//! workspace, and this is enforced at the tool layer, not by a
//! prompt:
//!
//! 1. The child engine is created with `preset = read-only`.
//! 2. The tool registry is filtered to a read-only whitelist.
//! 3. The child's `ToolContext` has `write_files: false` and
//!    `execute_commands: false` regardless of the preset.
//!
//! A job that asks for a tool outside the whitelist gets a
//! `PolicyDenied` result; the job logs the attempt and continues.
//! Failing closed is the correct response — a job that tries to
//! write has a bug.
//!
//! See `docs/design/p2-p5-p6.md` § P6 for the full design.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Instant;

use dashmap::DashMap;
use tokio::sync::Semaphore;

/// Tools a background job may call. Deliberately read-only.
///
/// A tool that reads the filesystem, the git index, or the LSP is
/// allowed. A tool that writes, executes a command, or opens a
/// network connection is not. The list is small and explicit so an
/// audit can confirm it at a glance.
pub const READ_ONLY_TOOLS: &[&str] = &[
    "read_file",
    "grep",
    "search_files",
    "file_info",
    "list_files",
    "git_status",
    "git_diff",
    "lsp",
    "check",
];

/// Whether a tool name is on the read-only whitelist.
pub fn is_read_only(tool: &str) -> bool {
    READ_ONLY_TOOLS.contains(&tool)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct JobId(pub u64);

impl std::fmt::Display for JobId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "job-{}", self.0)
    }
}

/// What a job is doing. The `Review` variant carries the turn it is
/// reviewing and the endpoint picked for the cross-model pass; the
/// others carry their subject path.
#[derive(Debug, Clone)]
pub enum JobKind {
    /// Cross-model review of a completed turn.
    Review {
        subject: crate::trace::TurnId,
        endpoint: kod_provider::ModelRef,
    },
    /// An eval run against a recorded transcript.
    Eval { transcript: PathBuf },
    /// Generate prose documentation for a module.
    Docs { path: PathBuf },
}

impl JobKind {
    /// Short label for the `/jobs` surface.
    pub fn label(&self) -> String {
        match self {
            JobKind::Review { subject, endpoint } => {
                format!("review turn {subject} on {}", endpoint.display())
            }
            JobKind::Eval { transcript } => {
                format!("eval {}", transcript.display())
            }
            JobKind::Docs { path } => {
                format!("docs {}", path.display())
            }
        }
    }
}

/// The state of one job.
#[derive(Debug, Clone)]
pub struct JobState {
    pub kind: JobKind,
    pub started_at: Instant,
    pub status: JobStatus,
}

#[derive(Debug, Clone)]
pub enum JobStatus {
    /// The job is running.
    Running,
    /// The job produced a summary.
    Completed { summary: String },
    /// The job failed.
    Failed { error: String },
}

/// The runner. Holds every job's state and a semaphore that caps
/// concurrent background agents.
pub struct BackgroundJobRunner {
    jobs: DashMap<JobId, JobState>,
    /// Concurrency cap. A background job holds a permit for its
    /// whole run; a job that cannot acquire one queues (the caller
    /// awaits the semaphore).
    sem: Arc<Semaphore>,
    /// Monotonic id counter.
    next_id: std::sync::atomic::AtomicU64,
    /// Configured cap, for the `/jobs` surface.
    max_concurrent: usize,
}

impl Default for BackgroundJobRunner {
    fn default() -> Self {
        Self::new(2)
    }
}

impl BackgroundJobRunner {
    /// A runner with a concurrency cap. `2` is the default: two
    /// background jobs can share the CPU and endpoint budget
    /// without starving the interactive session.
    pub fn new(max_concurrent: usize) -> Self {
        let n = max_concurrent.max(1);
        Self {
            jobs: DashMap::new(),
            sem: Arc::new(Semaphore::new(n)),
            next_id: std::sync::atomic::AtomicU64::new(1),
            max_concurrent: n,
        }
    }

    pub fn max_concurrent(&self) -> usize {
        self.max_concurrent
    }

    /// Allocate a fresh job id.
    pub fn allocate_id(&self) -> JobId {
        JobId(
            self.next_id
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed),
        )
    }

    /// Register a job as running. Called by the spawner before the
    /// job's task starts.
    pub fn register(&self, id: JobId, kind: JobKind) {
        self.jobs.insert(
            id,
            JobState {
                kind,
                started_at: Instant::now(),
                status: JobStatus::Running,
            },
        );
    }

    /// Mark a job completed with a summary.
    pub fn complete(&self, id: JobId, summary: String) {
        if let Some(mut state) = self.jobs.get_mut(&id) {
            state.status = JobStatus::Completed { summary };
        }
    }

    /// Mark a job failed.
    pub fn fail(&self, id: JobId, error: String) {
        if let Some(mut state) = self.jobs.get_mut(&id) {
            state.status = JobStatus::Failed { error };
        }
    }

    /// Snapshot of every job, newest first, for the `/jobs` surface.
    pub fn snapshot(&self) -> Vec<(JobId, JobState)> {
        let mut out: Vec<(JobId, JobState)> = self
            .jobs
            .iter()
            .map(|e| (*e.key(), e.value().clone()))
            .collect();
        out.sort_by(|a, b| b.0.cmp(&a.0));
        out
    }

    /// Jobs currently running.
    pub fn running_count(&self) -> usize {
        self.jobs
            .iter()
            .filter(|e| matches!(e.value().status, JobStatus::Running))
            .count()
    }

    /// Acquire a permit for a job. Awaits when the cap is reached.
    pub async fn acquire_permit(&self) -> tokio::sync::OwnedSemaphorePermit {
        self.sem
            .clone()
            .acquire_owned()
            .await
            .expect("semaphore never closed")
    }

    /// Forget jobs whose state is terminal and older than the
    /// retention window. Called by the `/jobs` surface on read, so
    /// the map does not grow without bound.
    pub fn prune_old(&self, max_age: std::time::Duration) {
        let now = Instant::now();
        self.jobs.retain(|_, state| {
            matches!(state.status, JobStatus::Running)
                || now.duration_since(state.started_at) < max_age
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn read_only_whitelist_is_explicit() {
        assert!(is_read_only("read_file"));
        assert!(is_read_only("grep"));
        assert!(is_read_only("git_status"));
        assert!(!is_read_only("write_file"));
        assert!(!is_read_only("execute_command"));
        assert!(!is_read_only("patch_file"));
        assert!(!is_read_only("git_commit"));
    }

    #[test]
    fn fresh_runner_has_no_jobs() {
        let r = BackgroundJobRunner::default();
        assert_eq!(r.running_count(), 0);
        assert!(r.snapshot().is_empty());
    }

    #[test]
    fn register_makes_a_job_running() {
        let r = BackgroundJobRunner::default();
        let id = r.allocate_id();
        r.register(
            id,
            JobKind::Eval {
                transcript: PathBuf::from("/tmp/x"),
            },
        );
        assert_eq!(r.running_count(), 1);
    }

    #[test]
    fn complete_transitions_to_terminal() {
        let r = BackgroundJobRunner::default();
        let id = r.allocate_id();
        r.register(
            id,
            JobKind::Eval {
                transcript: PathBuf::from("/tmp/x"),
            },
        );
        r.complete(id, "ok".to_string());
        assert_eq!(r.running_count(), 0);
        let snap = r.snapshot();
        assert!(matches!(
            snap[0].1.status,
            JobStatus::Completed { .. }
        ));
    }

    #[test]
    fn fail_transitions_to_terminal() {
        let r = BackgroundJobRunner::default();
        let id = r.allocate_id();
        r.register(
            id,
            JobKind::Eval {
                transcript: PathBuf::from("/tmp/x"),
            },
        );
        r.fail(id, "boom".to_string());
        assert_eq!(r.running_count(), 0);
    }

    #[tokio::test]
    async fn semaphore_serializes_when_cap_is_one() {
        let r = BackgroundJobRunner::new(1);
        let p1 = r.acquire_permit().await;
        // Second acquire would block; try_acquire confirms.
        let try_p = r.sem.clone().try_acquire_owned();
        assert!(try_p.is_err(), "cap 1 should prevent a second permit");
        drop(p1);
        // After release, a new permit is available.
        let _p2 = r.acquire_permit().await;
    }

    #[test]
    fn prune_removes_terminal_jobs_past_retention() {
        let r = BackgroundJobRunner::default();
        let id = r.allocate_id();
        r.register(
            id,
            JobKind::Eval {
                transcript: PathBuf::from("/tmp/x"),
            },
        );
        r.complete(id, "done".to_string());
        // Prune with zero retention removes terminal jobs.
        r.prune_old(std::time::Duration::from_millis(0));
        assert!(r.snapshot().is_empty());
    }

    #[test]
    fn ids_are_monotonic() {
        let r = BackgroundJobRunner::default();
        let a = r.allocate_id();
        let b = r.allocate_id();
        assert!(b.0 > a.0);
    }

    #[test]
    fn job_labels_are_descriptive() {
        let j = JobKind::Eval {
            transcript: PathBuf::from("/tmp/t.jsonl"),
        };
        assert!(j.label().contains("eval"));
        assert!(j.label().contains("t.jsonl"));
    }
}
