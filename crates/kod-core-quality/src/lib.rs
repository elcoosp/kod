//! kod-core-quality: codebase analysis, cleanup, and quality gates.
//! Extracted from kod-core so the quality tools can be worked on
//! independently of the agent engine.

#![allow(clippy::all)]

pub mod repomap;
pub mod prune;
pub mod shake;
pub mod worktree;
pub mod worktree_isolation_ownership;
pub mod preflight;
pub mod transcript_coherence;
pub mod auto_thinking;
pub mod snapcompact;
