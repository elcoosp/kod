//! Command blast-radius classification.
//!
//! Stage one of a two-stage design: a cheap deterministic high-recall
//! classifier that classifies a shell command **by blast radius, not
//! by command name**, and a model-facing reflection gate for the
//! ambiguous middle. It is defense in depth, not a sandbox — the OS
//! sandbox catches escape, the policy engine catches project intent,
//! and this catches "that command will delete the user's home
//! directory."
//!
//! The classifier never refuses to run something it does not
//! understand; it escalates. An unrecognised wrapper, a `$VAR` it
//! cannot resolve lexically, a glob over a protected directory — each
//! of those produces `Confirm`, not `Safe`. The failure mode of a
//! too-clever classifier is a deleted filesystem; the failure mode of
//! a too-cautious one is an extra confirmation prompt.

pub mod classify;
pub mod paths;

pub use classify::{GateOutcome, Justification, RiskAssessment, RiskFinding, RiskLevel, assess};
pub use paths::{PathDanger, RiskContext};
