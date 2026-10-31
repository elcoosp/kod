#![doc = include_str!("../README.md")]
#![warn(missing_docs)]
pub mod error;

pub use error::KodError;
/// The workspace-wide result type.
pub type Result<T> = std::result::Result<T, KodError>;
