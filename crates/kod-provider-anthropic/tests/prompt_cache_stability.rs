//! Anthropic prompt-cache stability across turns (design §4 D1.2,
//! AD-16).
//!
//! # What this test exists to catch
//!
//! The design's cacheable-prefix invariant is enforced at three layers:
//!
//! - The **text** layer: `crates/kod-core/tests/golden_prefix.rs`
//!   asserts the router's rendered prompt prefix is byte-stable.
//! - The **plan** layer: `crates/kod-core/tests/prompt_plan.rs`
//!   asserts `PromptPlan::cacheable_prefix()` is byte-stable.
//! - The **wire** layer: this file. The Anthropic Messages API is not
//!   given a single prefix string; it is given an **array** of system
//!   content blocks whose last cacheable entry carries
//!   `cache_control: {"type": "ephemeral"}`. Anthropic caches
//!   everything up to and including that block. A change to
//!   `wire::system_blocks` could reorder blocks, drop the marker, or
//!   split a cacheable segment — the two upstream tests would still
//!   pass, because they never look at the wire.
//!
//! # The invariant, expressed on the wire
//!
//! Given two `CompletionRequest`s built from the same session with the
//! same cacheable system segments but different volatile tails and
//! different user turns, the serialized JSON of the `system` array
//! **up to and including the `cache_control` block** is byte-identical.
//! Everything after that block is volatile by design and may differ.
//!
//! The test does not exercise a live server; it calls `wire::
//! build_messages_body` directly and compares the JSON value it
//! returns. That is the same function `AnthropicProvider::complete`
//! and `::stream_completion` call.

use kod_provider::request::{CompletionRequest, ModelRef, SystemPrompt, SystemSegment};
use kod_provider::traits::GenerationOptions;
use kod_provider_anthropic::wire::build_messages_body;
use kod_types::{ChatMessage, MessageId, MessageRole};
use time::OffsetDateTime;

fn user(text: &str) -> ChatMessage {
    ChatMessage::text(
        MessageId::new(),
        MessageRole::User,
        text,
        OffsetDateTime::now_utc(),
    )
}

fn request(cacheable: Vec<&str>, volatile: Vec<&str>, user_text: &str) -> CompletionRequest {
    let mut segments: Vec<SystemSegment> = Vec::new();
    for text in cacheable {
        segments.push(SystemSegment {
            text: text.into(),
            cacheable: true,
        });
    }
    for text in volatile {
        segments.push(SystemSegment {
            text: text.into(),
            cacheable: false,
        });
    }
    let mut req = CompletionRequest::new(ModelRef::new("test", "claude-sonnet-4-5"));
    req.system = SystemPrompt { segments };
    req.messages = vec![user(user_text)];
    req.options = GenerationOptions {
        max_tokens: Some(256),
        ..Default::default()
    };
    req
}

/// The last index in `system` whose block carries `cache_control`.
fn last_marked_index(system: &[serde_json::Value]) -> Option<usize> {
    system
        .iter()
        .rposition(|b| b.get("cache_control").is_some())
}

#[test]
fn cacheable_prefix_is_byte_stable_across_turns() {
    // Turn 1: identity + repo map cacheable, small volatile tail,
    // request A.
    let req_a = request(
        vec!["You are kod.", "Repository map: a, b, c."],
        vec!["Environment: turn 1."],
        "fix the parser",
    );
    // Turn 2: same cacheable prefix, different volatile tail, request B.
    let req_b = request(
        vec!["You are kod.", "Repository map: a, b, c."],
        vec!["Environment: turn 2.", "Skills: code-review"],
        "and the tests?",
    );

    let sys_a = build_messages_body(&req_a)["system"]
        .as_array()
        .expect("system array")
        .clone();
    let sys_b = build_messages_body(&req_b)["system"]
        .as_array()
        .expect("system array")
        .clone();

    let mark_a =
        last_marked_index(&sys_a).expect("turn 1 system must carry a cache_control marker");
    let mark_b =
        last_marked_index(&sys_b).expect("turn 2 system must carry a cache_control marker");
    assert_eq!(
        mark_a, mark_b,
        "the marker must land on the same block index across turns — a shift \
         means the cacheable segments themselves moved",
    );

    // Compare the two prefix slices, byte-for-byte after serialization.
    // `serde_json::Value` equality is structural, not byte-level; the
    // design's invariant is byte-level, so serialize both to strings
    // and compare.
    let prefix_a = serde_json::to_string(&sys_a[..=mark_a]).unwrap();
    let prefix_b = serde_json::to_string(&sys_b[..=mark_b]).unwrap();
    assert_eq!(
        prefix_a, prefix_b,
        "cacheable prefix drifted on the wire between two turns of the same \
         session.\n\
         Turn 1 prefix:\n{prefix_a}\n\
         Turn 2 prefix:\n{prefix_b}",
    );

    // Sanity: the volatile tails differ, or the test is not exercising
    // a real change.
    let tail_a = serde_json::to_string(&sys_a[mark_a + 1..]).unwrap();
    let tail_b = serde_json::to_string(&sys_b[mark_b + 1..]).unwrap();
    assert_ne!(
        tail_a, tail_b,
        "the volatile tail should differ between turns; the test inputs are \
         not exercising a real change",
    );
}

#[test]
fn no_marker_when_all_segments_are_volatile() {
    let req = request(vec![], vec!["volatile only"], "hi");
    let sys = build_messages_body(&req)["system"]
        .as_array()
        .expect("system array")
        .clone();
    assert!(
        last_marked_index(&sys).is_none(),
        "a system prompt with no cacheable segment must not carry a \
         cache_control marker — caching an entirely volatile prompt would \
         waste the cache slot",
    );
}

#[test]
fn marker_is_on_the_last_cacheable_segment_not_the_first() {
    let req = request(
        vec!["first cacheable", "second cacheable"],
        vec!["volatile tail"],
        "hi",
    );
    let sys = build_messages_body(&req)["system"]
        .as_array()
        .expect("system array")
        .clone();
    // The marker must be on index 1 (the second cacheable segment),
    // not index 0. Anthropic caches "everything up to and including a
    // marked block"; a marker on the first block would cache only
    // that one, defeating the whole invariant prefix.
    assert_eq!(sys.len(), 3);
    assert!(
        sys[0].get("cache_control").is_none(),
        "index 0 must not be marked"
    );
    assert!(
        sys[1].get("cache_control").is_some(),
        "index 1 must be marked"
    );
    assert!(
        sys[2].get("cache_control").is_none(),
        "index 2 (volatile) must not be marked",
    );
}
