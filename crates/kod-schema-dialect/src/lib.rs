//! Schema dialects: make a JSON Schema acceptable to a provider that
//! dislikes parts of it.
//!
//! A tool schema is written once and sent to every provider. The
//! providers do not agree on JSON Schema: one rejects `const`, another
//! chokes on `oneOf`, a gateway flattens nested `anyOf` inconsistently
//! and answers a 400 that names the offending keyword. The failure is
//! per-turn and per-tool, so a schema that trips one endpoint makes
//! every request to that endpoint fail until someone notices.
//!
//! Three layers, in the notebook's shape:
//!
//! 1. **Prevention** ([`sanitize`]) — rewrite a schema into the subset
//!    a provider accepts before sending it. Renames first, then
//!    structural transforms, then removal of keywords the provider
//!    does not support and that are safe to drop.
//! 2. **Recovery** — parse a provider's rejection to learn *which*
//!    construct it refused, so the retry drops exactly that.
//! 3. **Memory** — remember a learned rejection per endpoint, so the
//!    cost is one wasted round trip ever rather than one per request.
//!
//! This module is layer 1 and the data types layers 2 and 3 need. It
//! does no I/O: a caller supplies the [`DialectSpec`], gets back a
//! sanitized schema and a list of what changed.

pub mod recovery;
pub mod sanitize;

pub use recovery::{Rejection, RetryPlan, classify_rejection};
pub use sanitize::{AppliedTransform, DialectSpec, KeywordRole, role_of, sanitize};

/// The dialect a spec describes, for logs and the quirks file.
pub use sanitize::spec_for_provider;
