//! Skills system for markdown-based skill management.
//!
//! This crate handles loading, parsing, matching, and hot-reloading
//! of skill files from ~/.kod/skills/
//!
//! # Example
//!
//! ```rust,no_run
//! use kod_skills::{SkillLoader, SkillMatcher};
//!
//! # async fn example() {
//! let mut loader = SkillLoader::new("/skills");
//! let skills = loader.load_all().await.unwrap();
//!
//! let matcher = SkillMatcher::new();
//! for skill in skills {
//!     matcher.add_skill(skill).await;
//! }
//!
//! let matches = matcher.find_relevant_skills("refactor rust code").await;
//! # }
//! ```

pub mod loader;
pub mod matcher;
pub mod parser;
pub mod watcher;

pub use loader::{SkillLoader, load_from_dirs};
pub use matcher::SkillMatcher;
pub use parser::SkillParser;
pub use watcher::{SkillWatcher, WatchEvent};
