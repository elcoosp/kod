//! Memory system for multi-layer storage (short-term, long-term, episodic).
//!
//! This crate provides different memory backends with a unified interface
//! for storing and retrieving context.
//!

pub mod context;
pub mod long_term;
pub mod manager;
pub mod short_term;

pub use context::{Context, ContextBuilder};
pub use long_term::LongTermMemory;
pub use manager::{CompactionReport, MemoryManager};
pub use short_term::ShortTermMemory;
