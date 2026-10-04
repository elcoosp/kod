//! Delta §7.4: jfind — a judge-model cascade for semantic code search.
//!
//! # The gap
//!
//! `grep` is lexical: a query like "where is retry backoff computed"
//! finds nothing unless the identifier literally contains those words.
//! A model wants the *semantic* match — the function that does the
//! thing, whatever it is named.
//!
//! # The cascade
//!
//! Three waves, cheapest first, a judge model scoring candidates:
//!
//! 1. **Lexical.** Rank files by IDF-weighted keyword hits (the query's
//!    `grep_keywords`). Keep the top [`MAX_FILE_CANDIDATES`].
//! 2. **Filename judge.** Batches of [`FILENAME_BATCH`], ask the judge
//!    which filenames look relevant. Keep the best.
//! 3. **Window judge.** Read [`MAX_FILES_TO_READ`] files, cut
//!    [`WINDOW_LINES`]-line windows, and judge [`SKETCH_BYTES`]-byte
//!    sketch cards in batches of [`SKETCH_BATCH`].
//! 4. **Verify.** Only a *complete passage* scores a final heat; a
//!    sketch is a routing signal. A file is a hit at
//!    [`FILE_THRESHOLD`].
//!
//! # The judge is injected
//!
//! The cascade does not know about Jev. A caller supplies a [`Judge`]
//! — production wires Jev's batch scorer, a test supplies a stub that
//! scores by keyword overlap. That is what makes the whole cascade
//! testable without a live model.
//!
//! # What this does NOT do
//!
//! * No parallel window judging (`PARALLEL=16` in the design). The
//!   batch API is sequential here; a caller with a fast judge can
//!   parallelize at the [`Judge`] layer. Sequential is correct, just
//!   slower on a large file set.
//! * Not a tool yet. The `jfind` tool wrapper is a follow-up; this
//!   module is the algorithm.

use std::path::{Path, PathBuf};
use std::sync::Arc;

/// The most files the lexical wave keeps for judging.
pub const MAX_FILE_CANDIDATES: usize = 128;

/// Filenames per judge request in the filename wave.
pub const FILENAME_BATCH: usize = 64;

/// Files read for the window wave.
pub const MAX_FILES_TO_READ: usize = 20;

/// Lines per window cut from a file.
pub const WINDOW_LINES: usize = 24;

/// Bytes per sketch card shown to the judge.
pub const SKETCH_BYTES: usize = 384;

/// Sketch cards per judge request.
pub const SKETCH_BATCH: usize = 48;

/// A sketch scoring at or above this is a *routing signal* worth
/// verifying as a complete passage.
pub const CUTOFF: f32 = 0.45;

/// A file whose verified passage scores at or above this is a hit.
pub const FILE_THRESHOLD: f32 = 0.2;

/// The judge the cascade consults. Production wires Jev's batch
/// scorer; a test supplies a stub.
#[async_trait::async_trait]
pub trait Judge: Send + Sync {
    /// Score each candidate on `[0.0, 1.0]` for relevance to
    /// `question`. The returned vec is parallel to `candidates`.
    async fn score_batch(&self, question: &str, candidates: &[String]) -> Vec<f32>;
}

/// One hit: a file, a line range, and the verified score.
#[derive(Debug, Clone, PartialEq)]
pub struct Hit {
    pub path: PathBuf,
    pub start_line: usize,
    pub end_line: usize,
    pub score: f32,
}

/// The query a caller supplies.
#[derive(Debug, Clone)]
pub struct Query {
    /// Plain-language description, sent to the judge.
    pub text: String,
    /// Lexical keywords for the cheap first wave.
    pub grep_keywords: Vec<String>,
}

/// Run the cascade over `root`. `judge` scores candidates; `read` a
/// file's text (injected so a test needs no filesystem).
pub async fn search(
    root: &Path,
    query: &Query,
    judge: Arc<dyn Judge>,
    read: &(dyn Fn(&Path) -> Option<String> + Send + Sync),
    list: &[PathBuf],
) -> Vec<Hit> {
    if query.grep_keywords.is_empty() && query.text.is_empty() {
        return Vec::new();
    }
    // Wave 1: lexical ranking by keyword hit count.
    let candidates = lexical_rank(list, &query.grep_keywords, MAX_FILE_CANDIDATES);
    if candidates.is_empty() {
        return Vec::new();
    }
    // Wave 2: filename judge, batched.
    let kept = judge_filenames(&query.text, &candidates, judge.as_ref()).await;
    if kept.is_empty() {
        return Vec::new();
    }
    // Wave 3: window judge + passage verification.
    let mut hits: Vec<Hit> = Vec::new();
    for path in kept.iter().take(MAX_FILES_TO_READ) {
        let Some(text) = read(path) else { continue };
        let windows = cut_windows(&text, WINDOW_LINES);
        // Build sketch cards and judge them in batches.
        let mut scored: Vec<(usize, f32)> = Vec::new();
        for batch in windows.chunks(SKETCH_BATCH) {
            let cards: Vec<String> = batch.iter().map(|(_, w)| sketch(w)).collect();
            let scores = judge.score_batch(&query.text, &cards).await;
            for (i, (start, _)) in batch.iter().enumerate() {
                let s = scores.get(i).copied().unwrap_or(0.0);
                scored.push((*start, s));
            }
        }
        // Verify: a sketch at or above CUTOFF is verified as a full
        // passage; the *verified* score is what reports.
        let mut file_best: f32 = 0.0;
        let mut best_range: Option<(usize, usize)> = None;
        for (start, s) in &scored {
            if *s < CUTOFF {
                continue;
            }
            let end = (start + WINDOW_LINES - 1).min(text.lines().count());
            let passage = passage_of(&text, *start, end);
            let verified = judge
                .score_batch(&query.text, &[passage])
                .await
                .first()
                .copied()
                .unwrap_or(*s);
            if verified > file_best {
                file_best = verified;
                best_range = Some((*start, end));
            }
        }
        if file_best >= FILE_THRESHOLD
            && let Some((s, e)) = best_range
        {
            hits.push(Hit {
                path: path.clone(),
                start_line: s,
                end_line: e,
                score: file_best,
            });
        }
    }
    hits.sort_by(|a, b| {
        b.score
            .partial_cmp(&a.score)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    let _ = root;
    hits
}

/// Wave 1: rank `files` by how many keywords appear (case-folded),
/// keeping the top `limit`. IDF weighting is a follow-up; a plain
/// occurrence count is enough to order the candidate set.
fn lexical_rank(files: &[PathBuf], keywords: &[String], limit: usize) -> Vec<PathBuf> {
    if keywords.is_empty() {
        // No keywords: the whole set is a candidate (the filename
        // judge does the filtering).
        return files.iter().take(limit).cloned().collect();
    }
    let lower: Vec<String> = keywords.iter().map(|k| k.to_lowercase()).collect();
    let mut scored: Vec<(PathBuf, usize)> = files
        .iter()
        .filter_map(|p| {
            let name = p.to_string_lossy().to_lowercase();
            let hits = lower.iter().filter(|k| name.contains(k.as_str())).count();
            (hits > 0).then(|| (p.clone(), hits))
        })
        .collect();
    scored.sort_by(|a, b| b.1.cmp(&a.1));
    scored.into_iter().take(limit).map(|(p, _)| p).collect()
}

/// Wave 2: batch the candidates and ask the judge which filenames
/// look relevant. A filename scoring above zero is kept (the filename
/// is a coarse signal; the window wave does the fine work).
async fn judge_filenames(question: &str, candidates: &[PathBuf], judge: &dyn Judge) -> Vec<PathBuf> {
    let mut kept: Vec<PathBuf> = Vec::new();
    for batch in candidates.chunks(FILENAME_BATCH) {
        let names: Vec<String> = batch
            .iter()
            .map(|p| p.to_string_lossy().into_owned())
            .collect();
        let scores = judge.score_batch(question, &names).await;
        for (i, path) in batch.iter().enumerate() {
            if scores.get(i).copied().unwrap_or(0.0) > 0.0 {
                kept.push(path.clone());
            }
        }
    }
    kept
}

/// Cut `text` into `(start_line, window_text)` pairs, `size` lines
/// each. `start_line` is 1-based.
fn cut_windows(text: &str, size: usize) -> Vec<(usize, String)> {
    let lines: Vec<&str> = text.lines().collect();
    let mut out = Vec::new();
    let mut i = 0;
    while i < lines.len() {
        let end = (i + size).min(lines.len());
        out.push((i + 1, lines[i..end].join("\n")));
        i = end;
    }
    out
}

/// The first `SKETCH_BYTES` of a window, on a char boundary.
fn sketch(window: &str) -> String {
    if window.len() <= SKETCH_BYTES {
        return window.to_string();
    }
    let mut end = SKETCH_BYTES;
    while end > 0 && !window.is_char_boundary(end) {
        end -= 1;
    }
    window[..end].to_string()
}

/// The full text of lines `start..=end` (1-based).
fn passage_of(text: &str, start: usize, end: usize) -> String {
    text.lines()
        .skip(start.saturating_sub(1))
        .take(end.saturating_sub(start) + 1)
        .collect::<Vec<_>>()
        .join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A stub judge that scores by how many query words appear in the
    /// candidate (case-folded), on `[0, 1]`.
    struct KeywordJudge;
    #[async_trait::async_trait]
    impl Judge for KeywordJudge {
        async fn score_batch(&self, question: &str, candidates: &[String]) -> Vec<f32> {
            let words: Vec<String> = question
                .split_whitespace()
                .map(|w| w.to_lowercase())
                .collect();
            candidates
                .iter()
                .map(|c| {
                    let cl = c.to_lowercase();
                    let hits = words.iter().filter(|w| cl.contains(w.as_str())).count();
                    if words.is_empty() {
                        0.0
                    } else {
                        hits as f32 / words.len() as f32
                    }
                })
                .collect()
        }
    }

    fn no_read(_: &Path) -> Option<String> {
        None
    }

    #[tokio::test]
    async fn empty_query_returns_nothing() {
        let q = Query {
            text: String::new(),
            grep_keywords: Vec::new(),
        };
        let hits = search(
            Path::new("/r"),
            &q,
            Arc::new(KeywordJudge),
            &no_read,
            &[],
        )
        .await;
        assert!(hits.is_empty());
    }

    #[test]
    fn lexical_rank_keeps_keyword_files() {
        let files = vec![
            PathBuf::from("retry.rs"),
            PathBuf::from("unrelated.txt"),
            PathBuf::from("backoff.rs"),
        ];
        let ranked = lexical_rank(&files, &["retry".to_string()], 10);
        assert_eq!(ranked, vec![PathBuf::from("retry.rs")]);
    }

    #[test]
    fn cut_windows_splits_on_size() {
        let text = (1..=50).map(|i| format!("line {i}")).collect::<Vec<_>>().join("\n");
        let ws = cut_windows(&text, 24);
        assert_eq!(ws.len(), 3, "50 lines / 24 = 3 windows");
        assert_eq!(ws[0].0, 1);
        assert_eq!(ws[1].0, 25);
        assert_eq!(ws[2].0, 49);
    }

    #[test]
    fn sketch_truncates_on_a_char_boundary() {
        let long = "x".repeat(1000);
        assert_eq!(sketch(&long).len(), SKETCH_BYTES);
        let multibyte = "é".repeat(500);
        let s = sketch(&multibyte);
        assert!(s.len() <= SKETCH_BYTES);
        assert!(s.chars().all(|c| c == 'é'), "no broken char");
    }

    #[tokio::test]
    async fn a_relevant_file_becomes_a_hit() {
        let files = vec![PathBuf::from("retry.rs")];
        let content = "fn backoff(attempt) {\n    // exponential retry delay\n}\n";
        let read = |p: &Path| {
            (p == Path::new("retry.rs")).then(|| content.to_string())
        };
        let q = Query {
            text: "retry backoff".to_string(),
            grep_keywords: vec!["retry".to_string()],
        };
        let hits = search(Path::new("/r"), &q, Arc::new(KeywordJudge), &read, &files).await;
        assert_eq!(hits.len(), 1, "got: {hits:?}");
        assert_eq!(hits[0].path, PathBuf::from("retry.rs"));
        assert!(hits[0].score >= FILE_THRESHOLD);
    }

    #[tokio::test]
    async fn an_irrelevant_file_is_not_a_hit() {
        let files = vec![PathBuf::from("retry.rs")];
        // The content has none of the query words.
        let content = "fn a() {}\nfn b() {}\n";
        let read = |_: &Path| Some(content.to_string());
        let q = Query {
            text: "websocket handshake".to_string(),
            grep_keywords: vec!["retry".to_string()],
        };
        let hits = search(Path::new("/r"), &q, Arc::new(KeywordJudge), &read, &files).await;
        assert!(hits.is_empty(), "got: {hits:?}");
    }

    #[tokio::test]
    async fn a_file_with_no_keyword_in_its_name_is_filtered_out() {
        let files = vec![PathBuf::from("other.txt")];
        let read = |_: &Path| Some("retry backoff retry".to_string());
        let q = Query {
            text: "retry".to_string(),
            grep_keywords: vec!["retry".to_string()],
        };
        // `other.txt` has no "retry" in its path, so the lexical wave
        // drops it before the window wave could see the content.
        let hits = search(Path::new("/r"), &q, Arc::new(KeywordJudge), &read, &files).await;
        assert!(hits.is_empty(), "lexical wave must filter on filename");
    }

    #[tokio::test]
    async fn hits_are_sorted_by_score_descending() {
        // Filenames carry a keyword so the filename wave keeps them —
        // the stub judge scores a bare "a.rs" as 0 and drops it.
        let files = vec![PathBuf::from("retry_a.rs"), PathBuf::from("retry_b.rs")];
        let read = |p: &Path| match p.to_string_lossy().as_ref() {
            "retry_a.rs" => Some("retry retry retry backoff".to_string()),
            "retry_b.rs" => Some("retry".to_string()),
            _ => None,
        };
        let q = Query {
            text: "retry backoff".to_string(),
            grep_keywords: vec!["retry".to_string()],
        };
        let hits = search(Path::new("/r"), &q, Arc::new(KeywordJudge), &read, &files).await;
        assert!(!hits.is_empty(), "at least one hit expected");
        for w in hits.windows(2) {
            assert!(w[0].score >= w[1].score, "scores must descend");
        }
    }
}
