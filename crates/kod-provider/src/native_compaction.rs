//! Provider-native compaction (borrow from oh-my-pi, delta §4.4).
//!
//! # What this is
//!
//! Anthropic's `compact-2026-01-12` beta lets the *API* summarize a
//! prompt instead of the client. The request carries
//! `pause_after_compaction: true`; the response carries a
//! `compaction` content block whose `encrypted_content` field is an
//! opaque token the client replays on the next call. Anthropic drops
//! every message before that token, so the effective transcript the
//! server sees is the summary plus whatever the client sends after.
//!
//! The two halves matter differently:
//!
//! * `encrypted_content` is the *contract*. Only the API can produce
//!   or consume it, and replaying it verbatim is what makes the
//!   compaction free — the server reuses its own KV cache and does
//!   not re-read the prefix.
//! * `summary` is the *fallback*. When the session later routes to a
//!   provider that cannot consume the encrypted block (OpenAI, a
//!   local model, a different Anthropic endpoint), the plain text is
//!   what the next provider sees.
//!
//! # Why the type lives here
//!
//! The wire-level functions that build and parse the compaction
//! request live in the provider crate that speaks the protocol. This
//! type does not: it is the shape the engine stores and hands back,
//! and the trait method that produces it is on the shared
//! [`crate::LlmProvider`] surface. A caller that has only
//! `kod-provider` in scope can hold one without pulling the Anthropic
//! crate in.

/// A parsed compaction block from a provider response.
///
/// Both fields are `String` — the encrypted form is opaque base64 and
/// the summary is prose. Neither is interpreted by kod; both are
/// carried.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NativeCompaction {
    /// Opaque token the provider expects prepended to the next
    /// request's first user message. Its content is not kod's
    /// business; its *presence* is what tells the provider to drop
    /// the messages before it.
    pub encrypted_content: String,
    /// The human-readable summary text. The same content the server
    /// would use internally. Used as the compaction summary when the
    /// next request routes to a provider that cannot consume the
    /// encrypted block.
    pub summary: String,
}
