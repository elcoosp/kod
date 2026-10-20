//! Source lint: every foreground `embedder.embed(` call in
//! `src/manager.rs` goes through the panic-isolation barrier.
//!
//! The three foreground embed call sites in `manager.rs` are wrapped
//! by `embed_safe` (session 18, commits 69c94aa and fd84489). The
//! background embed in `spawn_embed` is isolated by its own spawned
//! task (covered by `flush_returns_after_panicking_embed_task`). A
//! future fourth call site — say, a new maintenance pass — could
//! easily be added without the barrier, reintroducing the
//! unwind-through-retrieval bug both of those commits fixed.
//!
//! This lint is the class-level guard: it fails if any `.embed(`
//! call in `src/manager.rs` appears outside `embed_safe` and outside
//! `spawn_embed`, and it fails if either barrier body has lost its
//! call site (a regression that gutted the barrier while leaving the
//! call that should have used it elsewhere).
//!
//! Shape mirrors
//! `kod-types/tests/it/source_lints.rs::truncate_helpers_name_matches_contract`
//! (session 16), which guards the analogous invariant for the
//! truncate helpers.

/// Byte-range of a function body, found by its signature and the
/// first 4-space-indented closing brace after it. Matches the
/// `src.find(...)` + `find("\n    }\n")` pattern already used by the
/// in-file `memory_cache_and_index_invalidation` test.
fn body_range(src: &str, start_marker: &str, end_marker: &str) -> (usize, usize) {
    let start = src
        .find(start_marker)
        .unwrap_or_else(|| panic!("marker not found: {start_marker}"));
    let tail = &src[start..];
    let end = tail
        .find(end_marker)
        .map(|i| start + i)
        .unwrap_or(src.len());
    (start, end)
}

#[test]
fn every_foreground_embed_call_goes_through_embed_safe() {
    let src = std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/src/manager.rs",))
        .expect("read src/manager.rs");

    // The panic barrier itself — its body contains the unwrapped
    // `.embed(` call by design.
    let (embed_safe_start, embed_safe_end) = body_range(&src, "async fn embed_safe(", "\n    }\n");
    // The background task — its body calls the embedder inside a
    // `tokio::spawn`, isolated by the existing flush-drop test.
    let (spawn_embed_start, spawn_embed_end) =
        body_range(&src, "fn spawn_embed(&self", "\n    }\n");

    let needle = ".embed(";
    let mut pos = 0;
    let mut checked = 0;
    let mut bad: Vec<(usize, String)> = Vec::new();
    let mut inside_embed_safe = 0;
    let mut inside_spawn_embed = 0;
    while let Some(i) = src[pos..].find(needle) {
        let abs = pos + i;
        // Skip comment lines — the file documents the panic case with
        // an `embedder.embed(...)` mention in prose.
        let line_start = src[..abs].rfind('\n').map(|n| n + 1).unwrap_or(0);
        let line_end = src[abs..].find('\n').map(|n| abs + n).unwrap_or(src.len());
        let trimmed = src[line_start..line_end].trim_start();
        if trimmed.starts_with("//") {
            pos = abs + needle.len();
            continue;
        }
        let in_embed_safe = abs >= embed_safe_start && abs < embed_safe_end;
        let in_spawn_embed = abs >= spawn_embed_start && abs < spawn_embed_end;
        if in_embed_safe {
            inside_embed_safe += 1;
        } else if in_spawn_embed {
            inside_spawn_embed += 1;
        } else {
            let line_no = src[..abs].matches('\n').count() + 1;
            bad.push((line_no, src[line_start..line_end].trim().to_string()));
        }
        checked += 1;
        pos = abs + needle.len();
    }

    assert!(
        bad.is_empty(),
        "embed() call sites not inside embed_safe or spawn_embed — a new \
         foreground embed must go through the embed_safe panic barrier \
         (see the doc on embed_safe):\n{}",
        bad.iter()
            .map(|(n, l)| format!("  line {n}: {l}"))
            .collect::<Vec<_>>()
            .join("\n"),
    );

    // Sanity: the two known sites are where the invariant expects
    // them. If either barrier lost its body's call (e.g. a refactor
    // moved the embed out of `embed_safe`), the barrier has been
    // gutted even if no new bad site exists.
    assert!(
        inside_embed_safe >= 1,
        "embed_safe must contain the unwrapped `.embed(` call it exists to \
         guard; checked {checked} call site(s) but none inside embed_safe. \
         If the barrier was refactored, update this lint to match its new \
         shape — do not remove the assertion.",
    );
    assert!(
        inside_spawn_embed >= 1,
        "spawn_embed must contain its background `.embed(` call; checked \
         {checked} call site(s) but none inside spawn_embed.",
    );
}
