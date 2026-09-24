//! Streaming stall detection (borrow from oh-my-pi, delta §9.3).
//!
//! # The problem
//!
//! Reasoning models can get stuck. A degenerate stream of the same
//! sentence, the same code snippet, or a run of empty section headers
//! consumes the whole completion budget while producing nothing
//! useful. The user sees a spinner for minutes and a result they
//! cannot use.
//!
//! # What this is
//!
//! A [`StreamGuard`] that watches the tail of the streaming output
//! and reports a stall the moment it identifies one. Detection is
//! cheap, bounded, and has no external dependencies. When it fires,
//! the caller aborts the stream and retries within the existing
//! turn-failure taxonomy; the guard itself does not decide policy.
//!
//! # Detectors implemented
//!
//! Two detectors, both precise enough to land without calibration
//! data:
//!
//! * **Exact suffix cycles.** A Z-array over the tail finds the
//!   smallest period `p` such that the tail is `p`-periodic. A short
//!   period (`p <= 60`) needs four repeats and a tail of at least
//!   180 bytes; a longer one (`60 < p <= 1024`) needs three repeats
//!   and 1024 bytes. Model-agnostic: a periodic stream is a stall
//!   regardless of which model produced it.
//!
//! * **Header runaway.** A run of 36 or more heading-shaped lines
//!   (`##` ATX headings or `**bold**` titles occupying a whole line)
//!   with no intervening prose. Observed on Gemini, whose "here are
//!   the sections I will cover" prelude can loop unboundedly; cheap
//!   to detect on any model.
//!
//! # Detectors *not* implemented
//!
//! The design note's §9.3 lists two further detectors whose
//! thresholds are named but whose calibration the workspace does
//! not yet have data for:
//!
//! * **Near-duplicate paragraphs** by word-trigram Jaccard. The note
//!   gives `Jaccard >= 0.8`, `cluster >= 4 in window 16`,
//!   `warm-up >= 8`. Landing this without a corpus to calibrate
//!   against risks a false-positive rate that stops legitimate long
//!   replies — the failure mode the doc's own "hardest negative"
//!   discussion warns about.
//!
//! * **Progress-lexicon stall.** Rolling 8-segment vocabulary
//!   novelty <= 0.2, "no new concrete anchor". The anchor definition
//!   (code spans, paths, identifiers) is itself a heuristic that
//!   needs tuning against real transcripts.
//!
//! Both are queued. Landing half the design honestly beats landing
//! all of it with detectors that fire on ordinary work.
//!
//! # Tail as bytes
//!
//! The tail is a 4096-*byte* ring, not 4096 characters. For the
//! ASCII-heavy stream this detector actually cares about (repeating
//! prose, headers, code) the two coincide. A CJK stall would have a
//! shorter effective char tail, which only makes detection *later*,
//! never wrong.
//!
//! # Integration
//!
//! The caller feeds bytes as they arrive; `feed` returns a verdict
//! after each scan. The engine's streaming loop turns
//! [`StallVerdict::Loop`] into a transient `KodError::Provider` with
//! the detector name in the message, then re-samples within the
//! existing retry budget. The guard holds no state outside the tail
//! and the scan counter; a fresh stream needs a fresh guard.
//!
//! A `final_verdict()` call handles the case where a stream ends
//! before the most recent delta crossed the scan stride — the last
//! few bytes can complete a pattern the last scan did not see.

/// Configuration for a [`StreamGuard`].
#[derive(Debug, Clone)]
pub struct StreamGuardConfig {
    /// Number of new bytes between scans. Scanning every byte would
    /// be correct but wasteful; 128 amortizes the O(tail) scan cost
    /// across a reasonable cadence.
    pub scan_stride: usize,
    /// Maximum tail bytes retained. Older bytes are dropped; the
    /// detectors only reason about the recent window anyway.
    pub tail_capacity: usize,
    /// Whether to scan for a header runaway. Cheap; on by default.
    pub header_runaway_enabled: bool,
}

impl Default for StreamGuardConfig {
    fn default() -> Self {
        Self {
            scan_stride: 128,
            tail_capacity: 4096,
            header_runaway_enabled: true,
        }
    }
}

/// Verdict from a scan.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StallVerdict {
    /// Nothing suspicious.
    Clean,
    /// A stall was detected. `detector` names which one, so a caller
    /// can attach it to the resulting error and a log can attribute
    /// the abort.
    Loop {
        /// Short identifier: `"exact-cycle"` or `"header-runaway"`.
        detector: &'static str,
    },
}

/// Per-stream stall detector.
pub struct StreamGuard {
    config: StreamGuardConfig,
    tail: Vec<u8>,
    bytes_since_scan: usize,
}

impl StreamGuard {
    pub fn new() -> Self {
        Self::with_config(StreamGuardConfig::default())
    }

    pub fn with_config(config: StreamGuardConfig) -> Self {
        Self {
            tail: Vec::with_capacity(config.tail_capacity),
            config,
            bytes_since_scan: 0,
        }
    }

    /// Feed the next delta and get a verdict.
    ///
    /// The verdict reflects only the *scan* this call may have
    /// triggered. If the delta is small, no scan happens and the
    /// return is `Clean` even if the tail has accumulated a pattern;
    /// the caller keeps feeding, and the next scan (or a
    /// [`Self::final_verdict`] call) picks it up. That is by design —
    /// the stride is what bounds the cost.
    pub fn feed(&mut self, delta: &[u8]) -> StallVerdict {
        self.tail.extend_from_slice(delta);
        if self.tail.len() > self.config.tail_capacity {
            let excess = self.tail.len() - self.config.tail_capacity;
            self.tail.drain(..excess);
        }
        self.bytes_since_scan = self.bytes_since_scan.saturating_add(delta.len());

        if self.bytes_since_scan < self.config.scan_stride {
            return StallVerdict::Clean;
        }
        self.bytes_since_scan = 0;
        self.scan()
    }

    /// Scan the current tail without regard to the stride. Call on
    /// stream termination so a short final delta can still complete
    /// a pattern the previous scan did not see.
    pub fn final_verdict(&self) -> StallVerdict {
        self.scan()
    }

    fn scan(&self) -> StallVerdict {
        if detect_exact_cycle(&self.tail).is_some() {
            return StallVerdict::Loop {
                detector: "exact-cycle",
            };
        }
        if self.config.header_runaway_enabled && detect_header_runaway(&self.tail) {
            return StallVerdict::Loop {
                detector: "header-runaway",
            };
        }
        StallVerdict::Clean
    }

    /// Bytes currently retained.
    pub fn tail_len(&self) -> usize {
        self.tail.len()
    }
}

impl Default for StreamGuard {
    fn default() -> Self {
        Self::new()
    }
}

/// Find the smallest period `p` of the tail that is long enough to
/// count as a stall. See the module doc for the thresholds.
fn detect_exact_cycle(tail: &[u8]) -> Option<usize> {
    let n = tail.len();
    if n < 180 {
        return None;
    }
    let z = z_array(tail);
    let max_p = 1024.min(n - 1);
    for p in 1..=max_p {
        if z[p] >= n - p {
            let repeats = n / p;
            let ok = if p <= 60 {
                repeats >= 4 && n >= 180
            } else {
                repeats >= 3 && n >= 1024
            };
            if ok {
                return Some(p);
            }
        }
    }
    None
}

/// Standard linear-time Z-array: `z[i]` is the length of the longest
/// substring of `s` starting at `i` that matches a prefix of `s`.
/// `z[0] = n` by convention.
fn z_array(s: &[u8]) -> Vec<usize> {
    let n = s.len();
    let mut z = vec![0usize; n];
    if n == 0 {
        return z;
    }
    z[0] = n;
    let mut l = 0usize;
    let mut r = 0usize;
    for i in 1..n {
        if i < r {
            z[i] = (r - i).min(z[i - l]);
        }
        while i + z[i] < n && s[z[i]] == s[i + z[i]] {
            z[i] += 1;
        }
        if i + z[i] > r {
            l = i;
            r = i + z[i];
        }
    }
    z
}

/// Count a run of heading-shaped lines. A run is broken by any
/// non-blank, non-heading line. Blank lines are skipped (a heading,
/// blank, heading sequence is still a runaway).
fn detect_header_runaway(tail: &[u8]) -> bool {
    const HEADER_RUNAWAY_THRESHOLD: usize = 36;
    let text = String::from_utf8_lossy(tail);
    let mut run = 0usize;
    for line in text.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        if is_heading_line(trimmed) {
            run += 1;
            if run >= HEADER_RUNAWAY_THRESHOLD {
                return true;
            }
        } else {
            run = 0;
        }
    }
    false
}

/// A heading-shaped line: an ATX heading (`##` or longer) or a
/// `**bold**` title that occupies the whole line.
fn is_heading_line(line: &str) -> bool {
    if line.starts_with("##") {
        return true;
    }
    if let Some(rest) = line.strip_prefix("**")
        && let Some(idx) = rest.find("**")
    {
        let after = rest[idx + 2..].trim();
        return after.is_empty();
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    fn non_periodic_bytes(n: usize) -> Vec<u8> {
        let mut state: u64 = 0xDEAD_BEEF_CAFE_BABE;
        let mut out = Vec::with_capacity(n);
        for _ in 0..n {
            state = state
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            out.push((((state >> 40) & 0x3f) as u8) + 0x20);
        }
        out
    }

    #[test]
    fn exact_cycle_clean_on_short_input() {
        let tail = b"hello world";
        assert!(detect_exact_cycle(tail).is_none());
    }

    #[test]
    fn exact_cycle_clean_on_prose() {
        let prose = b"The quick brown fox jumps over the lazy dog. It was \
                      the best of times, it was the worst of times. Call me \
                      Ishmael. Some years ago, never mind how long precisely.";
        assert!(detect_exact_cycle(prose).is_none());
    }

    #[test]
    fn exact_cycle_fires_on_period_3() {
        let tail: Vec<u8> = "abc".bytes().cycle().take(300).collect();
        assert_eq!(detect_exact_cycle(&tail), Some(3));
    }

    #[test]
    fn exact_cycle_fires_on_a_sentence_loop() {
        let tail: Vec<u8> = "I will analyze this. "
            .bytes()
            .cycle()
            .take(1_050)
            .collect();
        assert!(detect_exact_cycle(&tail).is_some());
    }

    #[test]
    fn exact_cycle_clean_when_tail_is_too_short() {
        let tail: Vec<u8> = "abc".bytes().cycle().take(150).collect();
        assert!(detect_exact_cycle(&tail).is_none());
    }

    #[test]
    fn exact_cycle_clean_on_too_few_repeats_of_a_long_period() {
        let pattern = non_periodic_bytes(100);
        let tail: Vec<u8> = pattern.iter().cycle().take(250).copied().collect();
        assert!(detect_exact_cycle(&tail).is_none());
    }

    #[test]
    fn exact_cycle_fires_on_many_repeats_of_a_long_period() {
        let pattern = non_periodic_bytes(100);
        let tail: Vec<u8> = pattern.iter().cycle().take(1_200).copied().collect();
        assert_eq!(detect_exact_cycle(&tail), Some(100));
    }

    #[test]
    fn exact_cycle_clean_when_the_pattern_is_almost_periodic() {
        let pattern = non_periodic_bytes(100);
        let mut tail: Vec<u8> = pattern.iter().cycle().take(1_200).copied().collect();
        tail.push(b'!');
        assert!(detect_exact_cycle(&tail).is_none());
    }

    #[test]
    fn header_runaway_clean_below_threshold() {
        let mut tail = String::new();
        for i in 0..35 {
            tail.push_str(&format!("## Section {i}\n"));
        }
        assert!(!detect_header_runaway(tail.as_bytes()));
    }

    #[test]
    fn header_runaway_fires_at_threshold() {
        let mut tail = String::new();
        for i in 0..36 {
            tail.push_str(&format!("## Section {i}\n"));
        }
        assert!(detect_header_runaway(tail.as_bytes()));
    }

    #[test]
    fn header_runaway_tolerates_blank_lines_between_headers() {
        let mut tail = String::new();
        for i in 0..36 {
            tail.push_str(&format!("## Section {i}\n\n"));
        }
        assert!(detect_header_runaway(tail.as_bytes()));
    }

    #[test]
    fn header_runaway_resets_on_prose_between_headers() {
        let mut tail = String::new();
        for i in 0..36 {
            tail.push_str(&format!("## Section {i}\nSome prose content here.\n"));
        }
        assert!(!detect_header_runaway(tail.as_bytes()));
    }

    #[test]
    fn header_runaway_fires_on_bold_titles() {
        let mut tail = String::new();
        for i in 0..36 {
            tail.push_str(&format!("**Summary {i}**\n"));
        }
        assert!(detect_header_runaway(tail.as_bytes()));
    }

    #[test]
    fn header_runaway_ignores_a_bold_inline_label() {
        let mut tail = String::new();
        for i in 0..36 {
            tail.push_str(&format!("**Note {i}:** some text after it\n"));
        }
        assert!(!detect_header_runaway(tail.as_bytes()));
    }

    #[test]
    fn guard_reports_clean_for_ordinary_stream() {
        let mut g = StreamGuard::new();
        let content = non_periodic_bytes(2_000);
        assert_eq!(g.feed(&content), StallVerdict::Clean);
    }

    #[test]
    fn guard_fires_on_a_degenerate_stream() {
        let mut g = StreamGuard::new();
        let degenerate: Vec<u8> = "I will analyze this. "
            .bytes()
            .cycle()
            .take(2_000)
            .collect();
        let v = g.feed(&degenerate);
        assert!(
            matches!(v, StallVerdict::Loop { detector: "exact-cycle" }),
            "expected exact-cycle loop, got {v:?}",
        );
    }

    #[test]
    fn guard_chunked_feeding_eventually_fires() {
        // The chunk must be a whole number of periods, or the
        // concatenation is not periodic. 99 bytes = 33 * "abc"; ten
        // chunks give a 990-byte 3-periodic tail, comfortably over
        // the 180-byte floor and well within the short-period rule's
        // 4-repeat requirement.
        //
        // The earlier version of this test used `take(100)` — 33
        // cycles plus one extra byte — which produces a 100-byte
        // period on concatenation. That is a *long* period
        // (100 > 60), which needs three repeats and 1024 bytes;
        // ten 100-byte chunks sum to 1000, just under the floor, so
        // the test never fired.
        let mut g = StreamGuard::new();
        let chunk: Vec<u8> = "abc".bytes().cycle().take(99).collect();
        let mut fired = None;
        for _ in 0..10 {
            let v = g.feed(&chunk);
            if matches!(v, StallVerdict::Loop { .. }) {
                fired = Some(v);
                break;
            }
        }
        assert!(
            fired.is_some(),
            "chunked feeding must eventually detect the loop",
        );
    }

    #[test]
    fn guard_tail_is_bounded() {
        let mut g = StreamGuard::new();
        let huge = non_periodic_bytes(20_000);
        let _ = g.feed(&huge);
        assert!(
            g.tail_len() <= 4096,
            "tail must be bounded to 4096 bytes, got {}",
            g.tail_len(),
        );
    }

    #[test]
    fn final_verdict_catches_a_pattern_a_stride_scan_missed() {
        let mut g = StreamGuard::new();
        let first: Vec<u8> = "abc".bytes().cycle().take(150).collect();
        assert_eq!(g.feed(&first), StallVerdict::Clean);

        let second: Vec<u8> = "abc".bytes().cycle().take(60).collect();
        assert_eq!(g.feed(&second), StallVerdict::Clean);

        let v = g.final_verdict();
        assert!(
            matches!(v, StallVerdict::Loop { detector: "exact-cycle" }),
            "final_verdict should catch the completed pattern: {v:?}",
        );
    }

    #[test]
    fn header_runaway_can_be_disabled() {
        let mut g = StreamGuard::with_config(StreamGuardConfig {
            header_runaway_enabled: false,
            ..StreamGuardConfig::default()
        });
        let mut tail = String::new();
        for i in 0..40 {
            tail.push_str(&format!("## Section {i}\n"));
        }
        let v = g.feed(tail.as_bytes());
        assert_eq!(v, StallVerdict::Clean, "disabled detector must not fire");
    }

    #[test]
    fn verdict_reports_the_detector_name() {
        let mut g = StreamGuard::new();
        let tail: Vec<u8> = "abc".bytes().cycle().take(400).collect();
        let v = g.feed(&tail);
        match v {
            StallVerdict::Loop { detector } => {
                assert_eq!(detector, "exact-cycle");
            }
            other => panic!("expected Loop, got {other:?}"),
        }
    }
}
