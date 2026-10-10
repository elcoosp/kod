//! Integration tests for `kod-types`.
//!
//! Two kinds:
//!
//! * `source_lints` — workspace-wide scans for shapes that were bugs
//!   in this codebase's history (a fixed temp filename, an unchecked
//!   `as u16` on a length, a `&str` byte slice without a
//!   boundary check). These fail *before* a future commit
//!   re-introduces the shape, not on a specific line.
//! * `string_safety` — property tests that exercise the string
//!   helpers with generated inputs, so a new bug on a shape the
//!   hand-enumerated tests do not cover has a chance of being caught
//!   by the same commit that adds it.

mod source_lints;
mod string_safety;
