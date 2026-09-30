//! Delta §7.3: tree-sitter parsing with a process-global parse cache.
//!
//! Two things live here:
//!
//! * [`lang::Lang`] — the nine grammars kod bundles, with the
//!   extension table its repo map already uses.
//! * [`parse_cache`] — an LRU of parsed `tree_sitter::Tree`s keyed on
//!   `(xxh3(source), source.len(), Lang)` with a byte-for-byte check
//!   on a hash hit. See the module docs for the collision reasoning.
//!
//! # Why a crate
//!
//! Each grammar compiles a C parser. Keeping them out of `kod-core`
//! means a consumer that does not need AST parsing (the CLI's doctor
//! path, a test binary) does not pay for nine `cc` invocations. The
//! crate is also the intended home for §7.1's hash-edit parse check
//! and §7.4's semantic search.
//!
//! # What this is not
//!
//! Not a symbol extractor yet — that is `repomap`'s job today and a
//! follow-up here. This crate ships the parse layer; the cache is the
//! §7.3 deliverable.

pub mod lang;
pub mod parse_cache;
pub mod rust;

pub use lang::Lang;
pub use parse_cache::{ParseCache, global};
pub use rust::AstSymbol;
