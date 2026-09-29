//! Memory system for multi-layer storage (short-term and long-term).
//!
//! This crate provides different memory backends with a unified interface
//! for storing and retrieving context.
//!

pub mod context;
pub mod embedding;
pub mod extract;
pub mod fusion;
pub mod hygiene;
pub mod long_term;
pub mod manager;
pub mod mental_models;
pub mod retention;
pub mod retrieval;
pub mod sharpshooter;
pub mod short_term;
pub mod stopwords;
pub mod tier;
pub mod vector_index;
pub mod veracity;

pub use embedding::{EmbeddingClient, NoEmbedder, OllamaEmbedder, OpenAIEmbedder};
pub use extract::{ExtractedFact, FactKind, extract};
pub use long_term::LongTermMemory;
pub use manager::{CompactionReport, ConsolidationReport, MemoryManager};
pub use retrieval::{HybridScorer, QueryTerms, recency, redistribute};
pub use short_term::ShortTermMemory;
pub use vector_index::VectorIndex;
