//! kod-core-quality: codebase analysis, cleanup, and quality gates.
//! Extracted from kod-core so the quality tools can be worked on
//! independently of the agent engine.

#![allow(clippy::all)]

pub mod auto_thinking;
pub mod preflight;
pub mod prune;
pub mod repomap;
pub mod shake;
pub mod snapcompact;
pub mod transcript_coherence;
pub mod worktree;
pub mod worktree_isolation_ownership;
