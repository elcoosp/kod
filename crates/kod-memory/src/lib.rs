//! Memory system for multi-layer storage (short-term, long-term, episodic).
//!
//! This crate provides different memory backends with a unified interface
//! for storing and retrieving context.
//!
//! # Example
//!
//! ```rust,no_run
//! use kod_memory::MemoryManager;
//! use kod_types::MemoryType;
//! use std::path::PathBuf;
//!
//! # async fn example() {
//! let manager = MemoryManager::new(PathBuf::from("/tmp/memory.redb"), 100).unwrap();
//! manager.store(MemoryType::LongTerm, "User prefers dark mode").await.unwrap();
//! let context = manager.retrieve_context("dark mode").await.unwrap();
//! # }
//! ```

pub mod context;
pub mod episodic;
pub mod long_term;
pub mod manager;
pub mod short_term;

pub use context::{Context, ContextBuilder};
pub use episodic::EpisodicMemory;
pub use long_term::LongTermMemory;
pub use manager::MemoryManager;
pub use short_term::ShortTermMemory;
