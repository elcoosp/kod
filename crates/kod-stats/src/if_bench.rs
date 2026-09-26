//! if-bench: cheapest-first working-memory eval (borrow from oh-my-pi,
//! delta §13.3).
//!
//! # The shape
//!
//! One growing, fully cacheable conversation per model. Every turn the
//! model must:
//!
//! 1. **Track an array** it reported last turn. The harness applies a
//!    deterministic action (a swap of two positions) and the model must
//!    report the result. Its own previous reply is the only state it
//!    has — the conversation carries it.
//! 2. **Obey a rotating directive**: a cat sound must appear at the
//!    start, middle, or end of the reply, cycling.
//!
//! Score = the turn depth before the model loses the array OR drops
//! the sound. Two separable failure modes (working memory, instruction
//! following) fall out, and no task corpus is needed.
//!
//! # What this module is
//!
//! The **scoring core**: the initial array, the per-turn action, the
//! reply parser, and the depth computation. All pure, all testable
//! without a provider.
//!
//! # What it is NOT
//!
//! * Not the driver. Sending the prompts to a model is a separate
//!   harness; this module tells it what to send and how to read the
//!   reply.
//! * Not a benchmark runner. It computes a depth from a list of
//!   replies; collecting those replies is the caller's job.

/// The design's defaults.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct IfBenchConfig {
    /// How many turns the benchmark runs.
    pub turns: usize,
    /// How many glyphs the array holds.
    pub array_size: usize,
    /// The max output tokens per turn.
    pub max_tokens: usize,
    /// The "par" depth a model is expected to reach.
    pub par: usize,
}

impl Default for IfBenchConfig {
    fn default() -> Self {
        Self {
            turns: 24,
            array_size: 24,
            max_tokens: 32_768,
            par: 4,
        }
    }
}

/// Where the cat sound must appear this turn.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SoundPosition {
    Start,
    Middle,
    End,
}

impl SoundPosition {
    /// The position for turn `n` (0-based), cycling start/middle/end.
    pub fn for_turn(turn: usize) -> Self {
        match turn % 3 {
            0 => Self::Start,
            1 => Self::Middle,
            _ => Self::End,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Start => "start",
            Self::Middle => "middle",
            Self::End => "end",
        }
    }
}

/// The cat-sound token the model must include.
pub const CAT_SOUND: &str = "meow";

/// The initial array: the glyphs `a..` repeated to `n`.
pub fn initial_array(n: usize) -> Vec<char> {
    (0..n)
        .map(|i| (b'a' + (i % 26) as u8) as char)
        .collect()
}

/// The swap the harness applies on `turn` (0-based).
///
/// Deterministic: positions `turn % len` and `(turn * 7 + 3) % len`.
/// Deterministic means the harness and the checker agree without
/// sharing state.
pub fn action_for_turn(turn: usize, len: usize) -> (usize, usize) {
    if len == 0 {
        return (0, 0);
    }
    (turn % len, (turn * 7 + 3) % len)
}

/// Apply a swap to a copy of `arr`.
pub fn apply(arr: &[char], action: (usize, usize)) -> Vec<char> {
    let mut out = arr.to_vec();
    if out.is_empty() {
        return out;
    }
    let (a, b) = action;
    let n = out.len();
    out.swap(a % n, b % n);
    out
}

/// Parse the array the model reported.
///
/// The reply must contain a line of the form `array: <glyphs>`. The
/// glyphs are the non-whitespace characters after the colon, up to the
/// end of the line. Returns `None` when no such line is present.
pub fn parse_reported_array(text: &str) -> Option<Vec<char>> {
    for line in text.lines() {
        let lower = line.trim_start().to_lowercase();
        if let Some(rest) = lower.strip_prefix("array:") {
            let glyphs: Vec<char> = rest.chars().filter(|c| !c.is_whitespace()).collect();
            if !glyphs.is_empty() {
                return Some(glyphs);
            }
        }
    }
    None
}

/// Whether the cat sound appears at the required position.
///
/// The reply is split at the midpoint (by lines); the sound must be in
/// the first third, the middle third, or the last third of the lines.
pub fn cat_sound_at(text: &str, position: SoundPosition) -> bool {
    let lines: Vec<&str> = text.lines().collect();
    if lines.is_empty() {
        return false;
    }
    let n = lines.len();
    let idx = lines.iter().position(|l| l.to_lowercase().contains(CAT_SOUND));
    let Some(idx) = idx else { return false };
    match position {
        SoundPosition::Start => idx < n.div_ceil(3),
        SoundPosition::Middle => {
            let third = n.div_ceil(3);
            idx >= third && idx < n - third
        }
        SoundPosition::End => idx >= n - n.div_ceil(3),
    }
}

/// One turn's reply, reduced to what the scorer needs.
#[derive(Debug, Clone, PartialEq)]
pub struct TurnReply {
    /// The array the model reported, if any.
    pub reported: Option<Vec<char>>,
    /// Whether the cat sound was at the required position.
    pub sound_ok: bool,
}

/// Score a run: the number of turns the model survived.
///
/// A turn is survived when the model reported the correct array *and*
/// the cat sound was at the required position. The run stops at the
/// first failure. `expected[0]` is the initial array; the harness
/// applies the turn's action before comparing.
pub fn depth(replies: &[TurnReply], initial: &[char]) -> usize {
    let mut current = initial.to_vec();
    for (turn, reply) in replies.iter().enumerate() {
        let expected = apply(&current, action_for_turn(turn, current.len()));
        let array_ok = reply.reported.as_deref() == Some(expected.as_slice());
        if !array_ok || !reply.sound_ok {
            return turn;
        }
        current = expected;
    }
    replies.len()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_default_config_matches_the_design() {
        let c = IfBenchConfig::default();
        assert_eq!(c.turns, 24);
        assert_eq!(c.array_size, 24);
        assert_eq!(c.max_tokens, 32_768);
        assert_eq!(c.par, 4);
    }

    #[test]
    fn the_initial_array_is_distinct_glyphs() {
        let a = initial_array(24);
        assert_eq!(a.len(), 24);
        assert_eq!(a[0], 'a');
        assert_eq!(a[23], 'x');
    }

    #[test]
    fn a_longer_array_wraps_the_alphabet() {
        let a = initial_array(30);
        assert_eq!(a[26], 'a');
    }

    #[test]
    fn sound_positions_cycle() {
        assert_eq!(SoundPosition::for_turn(0), SoundPosition::Start);
        assert_eq!(SoundPosition::for_turn(1), SoundPosition::Middle);
        assert_eq!(SoundPosition::for_turn(2), SoundPosition::End);
        assert_eq!(SoundPosition::for_turn(3), SoundPosition::Start);
    }

    #[test]
    fn the_action_is_deterministic() {
        assert_eq!(action_for_turn(0, 24), (0, 3));
        assert_eq!(action_for_turn(1, 24), (1, 10));
        assert_eq!(action_for_turn(0, 24), action_for_turn(0, 24));
    }

    #[test]
    fn apply_swaps_two_positions() {
        let a = vec!['a', 'b', 'c', 'd'];
        assert_eq!(apply(&a, (0, 2)), vec!['c', 'b', 'a', 'd']);
    }

    #[test]
    fn apply_wraps_out_of_range_indices() {
        let a = vec!['a', 'b', 'c'];
        // 5 % 3 = 2, 7 % 3 = 1: swap(2, 1) on [a,b,c] is [a,c,b].
        assert_eq!(apply(&a, (5, 7)), vec!['a', 'c', 'b']);
    }

    #[test]
    fn apply_of_an_empty_array_is_empty() {
        assert!(apply(&[], (0, 1)).is_empty());
    }

    #[test]
    fn parse_reported_array_reads_the_line() {
        let text = "some prose\narray: abcdef\nmore prose";
        assert_eq!(parse_reported_array(text), Some(vec!['a', 'b', 'c', 'd', 'e', 'f']));
    }

    #[test]
    fn parse_reported_array_is_case_insensitive_on_the_label() {
        let text = "Array: abc";
        assert_eq!(parse_reported_array(text), Some(vec!['a', 'b', 'c']));
    }

    #[test]
    fn parse_reported_array_returns_none_without_the_line() {
        assert_eq!(parse_reported_array("no array here"), None);
    }

    #[test]
    fn parse_reported_array_skips_an_empty_value() {
        assert_eq!(parse_reported_array("array:   "), None);
    }

    #[test]
    fn cat_sound_at_start() {
        let text = "meow\nline\nline\nline\nline\nline";
        assert!(cat_sound_at(text, SoundPosition::Start));
        assert!(!cat_sound_at(text, SoundPosition::End));
    }

    #[test]
    fn cat_sound_at_end() {
        let text = "line\nline\nline\nline\nline\nmeow";
        assert!(cat_sound_at(text, SoundPosition::End));
    }

    #[test]
    fn cat_sound_absent_is_never_ok() {
        let text = "line\nline\nline";
        for p in [SoundPosition::Start, SoundPosition::Middle, SoundPosition::End] {
            assert!(!cat_sound_at(text, p), "{p:?}");
        }
    }

    #[test]
    fn depth_counts_survived_turns() {
        let initial = vec!['a', 'b', 'c', 'd'];
        let expected0 = apply(&initial, action_for_turn(0, 4)); // swap(0,3)
        let expected1 = apply(&expected0, action_for_turn(1, 4)); // swap(1,2)
        let replies = vec![
            TurnReply { reported: Some(expected0.clone()), sound_ok: true },
            TurnReply { reported: Some(expected1), sound_ok: true },
        ];
        assert_eq!(depth(&replies, &initial), 2);
    }

    #[test]
    fn depth_stops_at_the_first_wrong_array() {
        let initial = vec!['a', 'b', 'c', 'd'];
        let replies = vec![
            TurnReply { reported: Some(vec!['x', 'y']), sound_ok: true },
            TurnReply { reported: None, sound_ok: true },
        ];
        assert_eq!(depth(&replies, &initial), 0);
    }

    #[test]
    fn depth_stops_at_a_dropped_sound() {
        let initial = vec!['a', 'b', 'c', 'd'];
        let expected0 = apply(&initial, action_for_turn(0, 4));
        let expected1 = apply(&expected0, action_for_turn(1, 4));
        let replies = vec![
            TurnReply { reported: Some(expected0), sound_ok: true },
            TurnReply { reported: Some(expected1), sound_ok: false },
        ];
        assert_eq!(depth(&replies, &initial), 1);
    }

    #[test]
    fn depth_of_no_replies_is_zero() {
        assert_eq!(depth(&[], &initial_array(4)), 0);
    }
}
