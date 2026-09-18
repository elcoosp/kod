//! Memory system for multi-layer storage (short-term and long-term).
//!
//! This crate provides different memory backends with a unified interface
//! for storing and retrieving context.
//!

pub mod context;
pub mod embedding;
pub mod extract;
pub mod long_term;
pub mod manager;
pub mod retrieval;
pub mod short_term;
pub mod stopwords;
pub mod vector_index;

pub use embedding::{EmbeddingClient, NoEmbedder, OllamaEmbedder, OpenAIEmbedder};
pub use extract::{ExtractedFact, FactKind, extract};
pub use long_term::LongTermMemory;
pub use manager::{CompactionReport, ConsolidationReport, MemoryManager};
pub use retrieval::{HybridScorer, QueryTerms, recency, redistribute};
pub use short_term::ShortTermMemory;
pub use vector_index::VectorIndex;
