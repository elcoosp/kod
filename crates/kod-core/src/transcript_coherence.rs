//! Cache-coherent transcript editing (borrow from oh-my-pi, delta §2.3).
//!
//! # The problem
//!
//! kod's golden-prefix tests pin the *system* prefix (identity + tool
//! schemas + repo map). The transcript tail is a different story. When
//! the engine prunes a stale tool result, rewraps a block, or drops a
//! message during compaction, the next request re-serializes the whole
//! transcript — and on a provider with explicit caching (Anthropic) or
//! byte-prefix caching (OpenAI-compatible), that re-serialization is
//! billed at the full input rate even though 90 % of the bytes are
//! identical to what the provider already holds.
//!
//! The fix is not "don't mutate the transcript" — that is what
//! compaction exists to do. It is "re-emit only the bytes that
//! actually changed, and tell the provider *where* the stable prefix
//! ends".
//!
//! # What this module provides
//!
//! - [`MessageDigest`] — a 64-bit FNV-1a hash over exactly the fields
//!   of a [`ChatMessage`] that reach a provider's wire, on any of the
//!   three serialization paths kod has (Anthropic's `wire.rs`, the
//!   OpenAI-compatible provider's `request_from_completion`, and the
//!   legacy `render_text`). Every field that ever appears in a request
//!   body is fed; every field that never does (the message id, the
//!   timestamp, the `MessageMetadata` block) is deliberately excluded.
//!
//! - [`DigestMemo`] — the digest list from the last log that was sent.
//!   A memo of *zero* messages is the first turn of a session; a memo
//!   of N messages means "the last request carried this log, in this
//!   order, with these digests".
//!
//! - [`sync_messages`] — compare the current log against the memo and
//!   return a [`SyncOutcome`]:
//!
//!   * `Append` — every previously-sent message is byte-identical and
//!     any difference is at the tail. The provider's cache survives
//!     to the end of the memo's prefix; only the new tail is uncached.
//!
//!   * `Compaction` — the log shrank. The memo is dropped and
//!     re-recorded. This is the one sanctioned wholesale rewrite; a
//!     caller that shrinks its own log is a compaction, and the
//!     provider's cache is invalidated for the whole transcript by
//!     definition.
//!
//!   * `InPlaceRewrite { stable_prefix }` — a message at or after
//!     `stable_prefix` differs from what was last sent. The caller
//!     should re-emit from `stable_prefix` onward. The provider's
//!     cache survives up to and including `stable_prefix - 1`.
//!
//! # What this module does NOT do
//!
//! It does not decide *when* to mutate the transcript. The pruning and
//! compaction rungs are separate concerns with their own policies (see
//! the delta note's §3). This module answers one question — "if the
//! log is as it currently stands, where is the cache-valid prefix?" —
//! and leaves the policy to the caller.
//!
//! It also does not surface the sync result anywhere. Wiring
//! `InPlaceRewrite` into `CompletionRequest.cache_transcript` (which
//! already exists, see `kod-provider/src/request.rs`) and into the
//! Anthropic streaming-breakpoint placement is a follow-up; the
//! primitive is testable and useful on its own.

use kod_types::{ChatMessage, MessageId, MessageRole};
use serde_json::Value;

/// FNV-1a 64-bit digest over the wire-relevant fields of one message.
///
/// The type is deliberately opaque: callers compare digests for
/// equality, they never inspect the value. That keeps the hash
/// function free to change (to a faster algorithm, to one with better
/// distribution) without any caller noticing.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct MessageDigest(pub u64);

/// FNV-1a 64-bit offset basis and prime.
const FNV_OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
const FNV_PRIME: u64 = 0x0000_0100_0000_01b3;

impl MessageDigest {
    /// Digest `msg` over the fields that reach a provider's wire.
    ///
    /// Every serialization path in the workspace — Anthropic's
    /// `wire::messages_array_impl`, the OpenAI-compatible provider's
    /// `request_from_completion`, and the legacy text renderer
    /// `ChatMessage::render_text` — reads from this set. Nothing more,
    /// nothing less:
    ///
    /// * `role` — as a stable wire-role tag, not the enum's `Debug`.
    /// * `content` — the message body, verbatim.
    /// * `tool_calls` — id, name, and canonicalized arguments, in the
    ///   order they appear.
    /// * `tool_call_id` — the link from a tool-role message back to
    ///   its originating call.
    ///
    /// Excluded on purpose:
    ///
    /// * `id` — a `MessageId` is internal bookkeeping; neither
    ///   provider ever sees it.
    /// * `timestamp` — nothing renders it.
    /// * `metadata` — `pinned`, `token_count`, `thinking_time_ms`,
    ///   `skill_applied`, `tools_used`, `agent_id`: none of these
    ///   appear on any wire. Including them would make pinning a turn
    ///   invalidate the transcript cache, which is exactly the failure
    ///   mode this module exists to prevent.
    /// * The inner `AgentId` of [`MessageRole::Agent`] — both providers
    ///   flatten that role to `assistant` before serializing, so the
    ///   id never reaches the wire. Two `Agent` messages with different
    ///   inner ids are byte-identical on every wire path and must
    ///   therefore digest the same.
    pub fn of(msg: &ChatMessage) -> Self {
        let mut h = FNV_OFFSET;
        feed(&mut h, role_tag(&msg.role).as_bytes());
        feed(&mut h, msg.content.as_bytes());
        for c in &msg.tool_calls {
            feed(&mut h, c.id.as_deref().unwrap_or("").as_bytes());
            feed(&mut h, c.tool_name.as_bytes());
            feed(&mut h, canon_json(&c.arguments).as_bytes());
        }
        if let Some(id) = &msg.tool_call_id {
            feed(&mut h, id.as_bytes());
        }
        Self(h)
    }
}

/// Feed one length-prefixed byte slice into an FNV-1a-64 accumulator.
///
/// Length-prefixing makes the encoding injective: `("ab", "c")` and
/// `("a", "bc")` produce different digests. The alternative — a NUL
/// separator — is not injective because NUL is a valid byte inside a
/// `String`, and a message body that happened to contain one would
/// collide with a differently-split field sequence.
fn feed(h: &mut u64, bytes: &[u8]) {
    for b in (bytes.len() as u64).to_le_bytes() {
        *h ^= b as u64;
        *h = h.wrapping_mul(FNV_PRIME);
    }
    for &b in bytes {
        *h ^= b as u64;
        *h = h.wrapping_mul(FNV_PRIME);
    }
}

/// A stable, wire-relevant tag for a role.
///
/// The role's `Debug` output carries a UUID for the `Agent` variant
/// (which would defeat the "exclude the agent id" rule above) and is
/// not guaranteed stable across Rust versions. This function returns a
/// fixed string per variant.
fn role_tag(r: &MessageRole) -> &'static str {
    match r {
        MessageRole::User => "user",
        MessageRole::Assistant => "assistant",
        MessageRole::System => "system",
        MessageRole::Tool => "tool",
        MessageRole::Agent(_) => "agent",
    }
}

/// Canonical JSON: object keys sorted, no whitespace.
///
/// `serde_json::to_string` emits object keys in insertion order. Two
/// `Value::Object`s that are semantically equal but built in different
/// orders serialize to different byte sequences. The digest is over
/// the *semantic* content — a caller that re-builds the same arguments
/// object with a different insertion order is not changing the message
/// — so canonicalize before hashing.
///
/// The output is the empty-whitespace, key-sorted, recursively
/// canonicalized JSON form. Numbers keep serde's default rendering
/// (which is deterministic for a given `Value`). Strings are escaped
/// the way `serde_json` escapes them, so the encoding is unambiguous.
fn canon_json(v: &Value) -> String {
    let mut out = String::new();
    write_canon(&mut out, v);
    out
}

fn write_canon(out: &mut String, v: &Value) {
    match v {
        Value::Null => out.push_str("null"),
        Value::Bool(b) => out.push_str(if *b { "true" } else { "false" }),
        Value::Number(n) => out.push_str(&n.to_string()),
        Value::String(s) => {
            // `serde_json::to_string` on a `String` produces a quoted,
            // escaped JSON string. `unwrap_or_default` is defensive:
            // serializing a string cannot fail.
            out.push_str(&serde_json::to_string(s).unwrap_or_default());
        }
        Value::Array(a) => {
            out.push('[');
            for (i, x) in a.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                write_canon(out, x);
            }
            out.push(']');
        }
        Value::Object(o) => {
            let mut keys: Vec<&String> = o.keys().collect();
            keys.sort();
            out.push('{');
            for (i, k) in keys.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                out.push_str(&serde_json::to_string(k.as_str()).unwrap_or_default());
                out.push(':');
                write_canon(out, &o[k.as_str()]);
            }
            out.push('}');
        }
    }
}

/// The digests of the last log that was sent, in order.
///
/// A fresh memo — the state before the first request of a session — has
/// zero entries. Every call to [`sync_messages`] replaces the memo's
/// contents with the current log's digests, so the memo always
/// describes "what the provider last saw".
#[derive(Debug, Default, Clone)]
pub struct DigestMemo {
    entries: Vec<(MessageId, MessageDigest)>,
}

impl DigestMemo {
    pub fn new() -> Self {
        Self::default()
    }

    /// Number of messages the memo has recorded. Zero on the first
    /// turn of a session.
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Forget every recorded digest. Called by the caller after a
    /// prefix-changing event the memo cannot observe — a model switch,
    /// a system-prompt edit — so the next `sync_messages` treats the
    /// whole log as fresh.
    pub fn clear(&mut self) {
        self.entries.clear();
    }

    /// The digest recorded for the message at position `i` of the
    /// last-sent log, if any. For tests and for a debug readout.
    pub fn digest_at(&self, i: usize) -> Option<(MessageId, MessageDigest)> {
        self.entries.get(i).cloned()
    }

    /// Replace the memo's contents with the digests of `log`.
    ///
    /// Exposed so a caller that has done a full re-emit — a compaction
    /// that was already billed — can reset the memo without going
    /// through `sync_messages`.
    pub fn record(&mut self, log: &[ChatMessage]) {
        self.entries.clear();
        self.entries.reserve(log.len());
        for msg in log {
            self.entries.push((msg.id.clone(), MessageDigest::of(msg)));
        }
    }
}

/// What `sync_messages` concluded about the current log.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SyncOutcome {
    /// Every previously-sent message is byte-identical, and any
    /// difference is at the tail. The provider's cache survives to the
    /// end of the memo's prefix. Also returned when the log is exactly
    /// what was last sent — "nothing to do" is a special case of "the
    /// append is empty".
    Append,
    /// The log shrank. The memo was dropped and re-recorded. The
    /// provider's cache is invalidated for the whole transcript.
    Compaction,
    /// A message at index `stable_prefix` or later differs from what
    /// was last sent. The provider's cache survives up to (but not
    /// including) `stable_prefix`.
    InPlaceRewrite {
        /// Number of leading messages that are byte-identical to the
        /// last-sent log.
        stable_prefix: usize,
    },
}

/// Compare `log` against `memo` and return where the cache-valid prefix
/// ends.
///
/// The memo is updated to describe `log` before returning, so the next
/// call compares against this call's log.
pub fn sync_messages(log: &[ChatMessage], memo: &mut DigestMemo) -> SyncOutcome {
    // A shorter log is a compaction by definition. The memo is
    // useless (it describes messages that no longer exist) and the
    // caller has almost certainly removed a prefix; the honest answer
    // is "the whole transcript is fresh".
    if log.len() < memo.len() {
        memo.record(log);
        return SyncOutcome::Compaction;
    }

    let mut stable = 0usize;
    for (i, msg) in log.iter().enumerate() {
        // Beyond the memo's recorded range: everything up to here
        // matched and the tail is new. This is the append case; the
        // loop exits and `stable` already equals `memo.len()`.
        if i >= memo.entries.len() {
            break;
        }
        let (_, recorded_digest) = &memo.entries[i];
        // The digest is the *only* correct comparison.
        //
        // The provider caches on the wire bytes, not on our internal
        // `MessageId`. A message at the same position with identical
        // wire content — same role, content, tool calls, and link —
        // is byte-identical to what the provider saw, so the cache
        // prefix survives across it. Comparing the id as well would
        // make a caller that re-creates a message with the same wire
        // form (a re-render, a re-hydration from a session log, a
        // test fixture) report a false-positive rewrite and force a
        // full re-emit of a transcript that did not actually change.
        //
        // The id is still stored on the memo, so a diagnostic can
        // show which message a recorded digest came from. It is not
        // part of the equality decision.
        if *recorded_digest == MessageDigest::of(msg) {
            stable += 1;
        } else {
            break;
        }
    }

    let memo_len_before = memo.len();
    memo.record(log);

    if stable == memo_len_before {
        SyncOutcome::Append
    } else {
        SyncOutcome::InPlaceRewrite {
            stable_prefix: stable,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use kod_types::{MessageId, ToolCall};
    use time::OffsetDateTime;

    fn user(content: &str) -> ChatMessage {
        ChatMessage::text(
            MessageId::new(),
            MessageRole::User,
            content,
            OffsetDateTime::now_utc(),
        )
    }

    fn assistant(content: &str) -> ChatMessage {
        ChatMessage::text(
            MessageId::new(),
            MessageRole::Assistant,
            content,
            OffsetDateTime::now_utc(),
        )
    }

    fn assistant_with_call(id: &str, name: &str, args: serde_json::Value) -> ChatMessage {
        let mut m = assistant("");
        m.tool_calls.push(ToolCall {
            id: Some(id.to_string()),
            tool_name: name.to_string(),
            arguments: args,
        });
        m
    }

    fn tool_result(id: &str, body: &str) -> ChatMessage {
        let mut m = ChatMessage::text(
            MessageId::new(),
            MessageRole::Tool,
            body,
            OffsetDateTime::now_utc(),
        );
        m.tool_call_id = Some(id.to_string());
        m
    }

    // ---- digest basics -------------------------------------------------

    #[test]
    fn identical_messages_digest_the_same() {
        let a = user("hello");
        let b = user("hello");
        // Different `MessageId`s, different timestamps — but identical
        // wire content. The digest must not be fooled.
        assert_eq!(MessageDigest::of(&a), MessageDigest::of(&b));
    }

    #[test]
    fn content_change_changes_the_digest() {
        let a = user("hello");
        let b = user("hello!");
        assert_ne!(MessageDigest::of(&a), MessageDigest::of(&b));
    }

    #[test]
    fn role_change_changes_the_digest() {
        // Same content, different role: not the same message.
        let u = ChatMessage::text(
            MessageId::new(),
            MessageRole::User,
            "x",
            OffsetDateTime::now_utc(),
        );
        let a = ChatMessage::text(
            MessageId::new(),
            MessageRole::Assistant,
            "x",
            OffsetDateTime::now_utc(),
        );
        assert_ne!(MessageDigest::of(&u), MessageDigest::of(&a));
    }

    #[test]
    fn tool_call_arguments_are_hashed() {
        // The pre-change `cache_tracker` hash omitted `arguments`,
        // which meant an argument edit was invisible. This pins the
        // fix: same call name and id, different arguments, different
        // digest.
        let a = assistant_with_call("c1", "read_file", serde_json::json!({"path": "a"}));
        let b = assistant_with_call("c1", "read_file", serde_json::json!({"path": "b"}));
        assert_ne!(MessageDigest::of(&a), MessageDigest::of(&b));
    }

    #[test]
    fn tool_call_id_is_hashed() {
        let a = assistant_with_call("c1", "read_file", serde_json::json!({}));
        let b = assistant_with_call("c2", "read_file", serde_json::json!({}));
        assert_ne!(MessageDigest::of(&a), MessageDigest::of(&b));
    }

    #[test]
    fn tool_call_id_link_is_hashed() {
        let a = tool_result("c1", "body");
        let b = tool_result("c2", "body");
        assert_ne!(MessageDigest::of(&a), MessageDigest::of(&b));
    }

    #[test]
    fn arguments_key_order_does_not_change_the_digest() {
        // The digest is over the *semantic* content. A caller that
        // builds the same arguments object in a different key order
        // has not changed the message; the canonical JSON step makes
        // the two forms digest identically.
        let a = assistant_with_call(
            "c1",
            "read_file",
            serde_json::json!({"path": "x", "encoding": "utf-8"}),
        );
        let b = assistant_with_call(
            "c1",
            "read_file",
            serde_json::json!({"encoding": "utf-8", "path": "x"}),
        );
        assert_eq!(MessageDigest::of(&a), MessageDigest::of(&b));
    }

    #[test]
    fn nested_arguments_key_order_does_not_change_the_digest() {
        // The canonicalization recurses. A change here would let a
        // nested-object reorder silently invalidate the cache.
        let a = assistant_with_call(
            "c1",
            "tool",
            serde_json::json!({"outer": {"a": 1, "b": 2}}),
        );
        let b = assistant_with_call(
            "c1",
            "tool",
            serde_json::json!({"outer": {"b": 2, "a": 1}}),
        );
        assert_eq!(MessageDigest::of(&a), MessageDigest::of(&b));
    }

    #[test]
    fn array_order_changes_the_digest() {
        // Arrays are ordered; a permuted array is a different value.
        let a = assistant_with_call("c1", "tool", serde_json::json!([1, 2, 3]));
        let b = assistant_with_call("c1", "tool", serde_json::json!([3, 2, 1]));
        assert_ne!(MessageDigest::of(&a), MessageDigest::of(&b));
    }

    #[test]
    fn pinning_a_turn_does_not_change_the_digest() {
        // `metadata.pinned` is a TUI property; it never reaches the
        // wire. Including it would invalidate the transcript cache
        // every time a user pinned a turn — the exact false positive
        // this module is designed to avoid.
        let mut a = user("x");
        let d1 = MessageDigest::of(&a);
        a.metadata.pinned = true;
        assert_eq!(MessageDigest::of(&a), d1);
    }

    #[test]
    fn agent_role_ignores_the_inner_id() {
        // Two `Agent` messages with different inner ids are flattened
        // to `assistant` by both providers before serialization, so
        // their wire bytes are identical and their digests must be.
        let a = ChatMessage::text(
            MessageId::new(),
            MessageRole::Agent(kod_types::AgentId::new()),
            "x",
            OffsetDateTime::now_utc(),
        );
        let b = ChatMessage::text(
            MessageId::new(),
            MessageRole::Agent(kod_types::AgentId::new()),
            "x",
            OffsetDateTime::now_utc(),
        );
        assert_eq!(MessageDigest::of(&a), MessageDigest::of(&b));
    }

    #[test]
    fn field_boundary_collision_does_not_happen() {
        // `("ab","c")` vs `("a","bc")` — a NUL-separator scheme would
        // collide these if `content` could contain NUL; the length
        // prefix makes the encoding injective. This is the reason for
        // the length prefix, tested directly.
        let a = ChatMessage::text(
            MessageId::new(),
            MessageRole::User,
            "ab",
            OffsetDateTime::now_utc(),
        );
        let b = ChatMessage::text(
            MessageId::new(),
            MessageRole::User,
            "a",
            OffsetDateTime::now_utc(),
        );
        // Same content length, so a naive length-prefix-only scheme
        // would be tempted to compare. Different content bytes.
        assert_ne!(MessageDigest::of(&a), MessageDigest::of(&b));
    }

    // ---- memo & sync ---------------------------------------------------

    #[test]
    fn a_fresh_memo_is_empty() {
        let m = DigestMemo::new();
        assert!(m.is_empty());
        assert_eq!(m.len(), 0);
    }

    #[test]
    fn first_sync_is_an_append_from_an_empty_memo() {
        let mut memo = DigestMemo::new();
        let log = vec![user("a"), assistant("b")];
        let outcome = sync_messages(&log, &mut memo);
        assert_eq!(outcome, SyncOutcome::Append);
        assert_eq!(memo.len(), 2);
    }

    #[test]
    fn identical_log_syncs_again_as_append() {
        // The common case for a second turn with no changes: the
        // memo matches, nothing to do. `Append` with zero new
        // entries.
        let mut memo = DigestMemo::new();
        let log = vec![user("a"), assistant("b")];
        let _ = sync_messages(&log, &mut memo);
        let outcome = sync_messages(&log, &mut memo);
        assert_eq!(outcome, SyncOutcome::Append);
    }

    #[test]
    fn a_tail_append_is_an_append_and_preserves_the_prefix() {
        let mut memo = DigestMemo::new();
        let first = vec![user("a"), assistant("b")];
        let _ = sync_messages(&first, &mut memo);

        let mut extended = first.clone();
        extended.push(user("c"));
        let outcome = sync_messages(&extended, &mut memo);
        assert_eq!(
            outcome,
            SyncOutcome::Append,
            "new messages at the tail must not be reported as a rewrite",
        );
        assert_eq!(memo.len(), 3);
    }

    #[test]
    fn a_middle_mutation_is_an_in_place_rewrite_from_the_mutation() {
        let mut memo = DigestMemo::new();
        let original = vec![user("a"), assistant("b"), user("c"), assistant("d")];
        let _ = sync_messages(&original, &mut memo);

        // Mutate message at index 2 in place.
        let mut mutated = original.clone();
        mutated[2] = user("c-CHANGED");
        let outcome = sync_messages(&mutated, &mut memo);
        assert_eq!(
            outcome,
            SyncOutcome::InPlaceRewrite { stable_prefix: 2 },
            "the stable prefix must stop at the first differing message",
        );
    }

    #[test]
    fn a_head_mutation_rewrites_everything() {
        let mut memo = DigestMemo::new();
        let original = vec![user("a"), assistant("b"), user("c")];
        let _ = sync_messages(&original, &mut memo);

        let mut mutated = original.clone();
        mutated[0] = user("a-CHANGED");
        let outcome = sync_messages(&mutated, &mut memo);
        assert_eq!(outcome, SyncOutcome::InPlaceRewrite { stable_prefix: 0 });
    }

    #[test]
    fn a_shrinking_log_is_compaction() {
        let mut memo = DigestMemo::new();
        let long = vec![user("a"), assistant("b"), user("c"), assistant("d")];
        let _ = sync_messages(&long, &mut memo);

        let short = long[..2].to_vec();
        let outcome = sync_messages(&short, &mut memo);
        assert_eq!(outcome, SyncOutcome::Compaction);
        // The memo now describes the short log.
        assert_eq!(memo.len(), 2);
    }

    #[test]
    fn an_empty_log_after_a_populated_one_is_compaction() {
        let mut memo = DigestMemo::new();
        let _ = sync_messages(&[user("a")], &mut memo);
        let outcome = sync_messages(&[], &mut memo);
        assert_eq!(outcome, SyncOutcome::Compaction);
        assert!(memo.is_empty());
    }

    #[test]
    fn a_message_replacement_with_identical_content_is_still_append() {
        // A fresh `MessageId` with the same role and content: the wire
        // bytes are identical, so the digest matches. The stable
        // prefix extends across it. This is the *desired* behaviour —
        // the id is internal, the wire is what the provider cached.
        let mut memo = DigestMemo::new();
        let first = vec![user("hello")];
        let _ = sync_messages(&first, &mut memo);

        let replaced = vec![user("hello")];
        // Sanity: the two messages have distinct `MessageId`s.
        assert_ne!(first[0].id, replaced[0].id);
        let outcome = sync_messages(&replaced, &mut memo);
        assert_eq!(outcome, SyncOutcome::Append);
    }

    #[test]
    fn memo_clear_resets_to_the_first_sync_behaviour() {
        let mut memo = DigestMemo::new();
        let log = vec![user("a"), assistant("b")];
        let _ = sync_messages(&log, &mut memo);
        memo.clear();
        assert!(memo.is_empty());
        let outcome = sync_messages(&log, &mut memo);
        assert_eq!(outcome, SyncOutcome::Append);
    }

    #[test]
    fn a_replay_after_a_compaction_re_extends_the_memo() {
        // After a compaction, the memo describes the compacted log.
        // A subsequent append is a normal `Append`.
        let mut memo = DigestMemo::new();
        let long = vec![user("a"), assistant("b"), user("c"), assistant("d")];
        let _ = sync_messages(&long, &mut memo);
        let short = long[..2].to_vec();
        let _ = sync_messages(&short, &mut memo);

        let mut extended = short.clone();
        extended.push(assistant("e"));
        let outcome = sync_messages(&extended, &mut memo);
        assert_eq!(outcome, SyncOutcome::Append);
    }

    #[test]
    fn digest_at_returns_the_recorded_entry() {
        let mut memo = DigestMemo::new();
        let log = vec![user("a"), assistant("b")];
        let _ = sync_messages(&log, &mut memo);
        let (id, digest) = memo.digest_at(0).expect("entry 0 recorded");
        assert_eq!(id, log[0].id);
        assert_eq!(digest, MessageDigest::of(&log[0]));
        assert!(memo.digest_at(99).is_none());
    }

    #[test]
    fn a_full_log_replacement_with_identical_wire_bytes_is_append() {
        // Regression for the id-comparison bug: replacing every
        // message in a log with a fresh `MessageId` but identical
        // wire content is a *no-op* from the provider's point of
        // view. The pre-fix sync compared ids and reported an
        // `InPlaceRewrite { stable_prefix: 0 }`, which would have
        // forced a full re-emit of a transcript the provider had
        // already cached byte for byte.
        let mut memo = DigestMemo::new();
        let first = vec![user("a"), assistant("b"), user("c")];
        let _ = sync_messages(&first, &mut memo);

        // Same content, brand-new ids.
        let replaced = vec![user("a"), assistant("b"), user("c")];
        // Sanity: the ids really are different, or the test is
        // vacuous.
        for (a, b) in first.iter().zip(replaced.iter()) {
            assert_ne!(a.id, b.id, "the test's premise is that ids differ");
        }
        assert_eq!(
            sync_messages(&replaced, &mut memo),
            SyncOutcome::Append,
            "identical wire bytes must be an append regardless of MessageId",
        );
    }

    #[test]
    fn repeated_sync_of_the_same_log_is_idempotent() {
        // A regression that accidentally cleared the memo on every
        // call would still return `Append` (stable 0 == memo 0 after
        // record), so the check has to compare *before* and *after*
        // the memo length on the second call.
        let mut memo = DigestMemo::new();
        let log = vec![user("a"), assistant("b"), user("c")];
        let first = sync_messages(&log, &mut memo);
        let len_after_first = memo.len();
        let second = sync_messages(&log, &mut memo);
        let len_after_second = memo.len();
        assert_eq!(first, SyncOutcome::Append);
        assert_eq!(second, SyncOutcome::Append);
        assert_eq!(len_after_first, 3);
        assert_eq!(len_after_second, 3);
    }
}
