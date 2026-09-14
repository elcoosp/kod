//! Memory system for multi-layer storage (short-term, long-term, episodic).
//!
//! This crate provides different memory backends with a unified interface
//! for storing and retrieving context.
//!
//! # Current state of the episodic layer
//!
//! `EpisodicMemory` exposes a cosine-similarity `find_similar` API, but
//! the manager does not compute real embeddings yet — `MemoryManager::store`
//! writes episodic entries with an empty `embedding` vec. Until a real
//! embedding model is wired in (fastembed is in the workspace deps but
//! unused in this crate), context retrieval goes through keyword matching
//! in `MemoryManager::retrieve_context`. Treat the embedding-based API as
//! a placeholder rather than a working semantic search.
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
