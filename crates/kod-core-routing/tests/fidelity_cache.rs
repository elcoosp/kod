use kod_core_routing::context_engine::{
    Chunk, ChunkScorer, FidelityCache, LexicalScorer, Query, render_scored,
};
use kod_types::{ChatMessage, MessageId, MessageRole};

fn msg(text: &str) -> ChatMessage {
    ChatMessage::text(
        MessageId::new(),
        MessageRole::User,
        text.to_string(),
        time::OffsetDateTime::now_utc(),
    )
}

/// A message's fidelity depends on its age, but the cache keyed only
/// on `(id, content_hash, query)` -- so a message that aged out of the
/// tail kept its fresh `Full` rating and never left the prompt.
#[test]
fn aged_message_is_not_served_a_stale_fidelity() {
    let scorer = LexicalScorer::new().with_tail(2);
    let q = Query::from_text("retry backoff");
    let m = msg("unrelated chatter about lunch");

    // The fixture must be age-sensitive, or it proves nothing.
    let fresh = scorer.score(
        &Chunk {
            message: &m,
            age_turns: 1,
            pinned: false,
        },
        &q,
    );
    let aged = scorer.score(
        &Chunk {
            message: &m,
            age_turns: 400,
            pinned: false,
        },
        &q,
    );
    assert_ne!(fresh, aged, "fixture must change with age");

    let mut cache = FidelityCache::new();
    let mut turns = vec![m.clone()];
    let _ = render_scored(&turns, q.clone(), &scorer, &mut cache, 100_000, false);

    // Age it 400 turns: age counts back from the end, so the fillers
    // must come *after* it.
    let mut older: Vec<ChatMessage> = turns.drain(..).collect();
    older.extend((0..400).map(|i| msg(&format!("filler {i}"))));
    let _ = render_scored(&older, q.clone(), &scorer, &mut cache, 100_000, false);

    let mut h = std::collections::hash_map::DefaultHasher::new();
    use std::hash::{Hash, Hasher};
    m.content.hash(&mut h);
    // After the fix the age is part of the key, so the stale entry is
    // a miss (re-scored at its new age) rather than a wrong hit.
    let cached = cache.lookup(&m.id, h.finish(), 400, &q);
    assert_ne!(
        cached,
        Some(fresh),
        "a 400-turn-old message must not be served its fresh `Full` rating",
    );
    // And the aged re-score is what the render now uses.
    assert_eq!(cached, Some(aged), "a miss here must record the aged value");
}

/// The cache still hits for a repeat render in the same turn, where no
/// message's age has changed.
#[test]
fn a_repeat_render_in_the_same_turn_hits() {
    let scorer = LexicalScorer::new().with_tail(2);
    let q = Query::from_text("retry backoff");
    let turns: Vec<ChatMessage> = (0..10).map(|i| msg(&format!("chatter {i}"))).collect();

    let mut cache = FidelityCache::new();
    let _ = render_scored(&turns, q.clone(), &scorer, &mut cache, 100_000, false);

    let mut h = std::collections::hash_map::DefaultHasher::new();
    use std::hash::{Hash, Hasher};
    turns[0].content.hash(&mut h);
    // age of turns[0] in a 10-message transcript is 9.
    assert!(
        cache.lookup(&turns[0].id, h.finish(), 9, &q).is_some(),
        "a same-turn repeat render must hit the cache",
    );
}
