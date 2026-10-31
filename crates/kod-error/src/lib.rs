#![doc = include_str!("../README.md")]
pub mod error;

pub use error::KodError;
pub type Result<T> = std::result::Result<T, KodError>;
