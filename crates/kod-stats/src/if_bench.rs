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
    (0..n).map(|i| (b'a' + (i % 26) as u8) as char).collect()
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
pub // T5-C34: this method measures "middle" by line index, but a
// single-line reply always classifies as End regardless of where
// the sound sits in the line. A character-offset based check would
// be more accurate; the current test suite pins the line-index
// behavior, so a fix must update the tests as well.
    fn cat_sound_at(text: &str, position: SoundPosition) -> bool {
    let lines: Vec<&str> = text.lines().collect();
    if lines.is_empty() {
        return false;
    }
    let n = lines.len();
    let idx = lines
        .iter()
        .position(|l| l.to_lowercase().contains(CAT_SOUND));
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
        assert_eq!(
            parse_reported_array(text),
            Some(vec!['a', 'b', 'c', 'd', 'e', 'f'])
        );
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
        for p in [
            SoundPosition::Start,
            SoundPosition::Middle,
            SoundPosition::End,
        ] {
            assert!(!cat_sound_at(text, p), "{p:?}");
        }
    }

    #[test]
    fn depth_counts_survived_turns() {
        let initial = vec!['a', 'b', 'c', 'd'];
        let expected0 = apply(&initial, action_for_turn(0, 4)); // swap(0,3)
        let expected1 = apply(&expected0, action_for_turn(1, 4)); // swap(1,2)
        let replies = vec![
            TurnReply {
                reported: Some(expected0.clone()),
                sound_ok: true,
            },
            TurnReply {
                reported: Some(expected1),
                sound_ok: true,
            },
        ];
        assert_eq!(depth(&replies, &initial), 2);
    }

    #[test]
    fn depth_stops_at_the_first_wrong_array() {
        let initial = vec!['a', 'b', 'c', 'd'];
        let replies = vec![
            TurnReply {
                reported: Some(vec!['x', 'y']),
                sound_ok: true,
            },
            TurnReply {
                reported: None,
                sound_ok: true,
            },
        ];
        assert_eq!(depth(&replies, &initial), 0);
    }

    #[test]
    fn depth_stops_at_a_dropped_sound() {
        let initial = vec!['a', 'b', 'c', 'd'];
        let expected0 = apply(&initial, action_for_turn(0, 4));
        let expected1 = apply(&expected0, action_for_turn(1, 4));
        let replies = vec![
            TurnReply {
                reported: Some(expected0),
                sound_ok: true,
            },
            TurnReply {
                reported: Some(expected1),
                sound_ok: false,
            },
        ];
        assert_eq!(depth(&replies, &initial), 1);
    }

    #[test]
    fn depth_of_no_replies_is_zero() {
        assert_eq!(depth(&[], &initial_array(4)), 0);
    }
}

// ---------------------------------------------------------------------------
// Delta §13.3: the driver.
//
// The scoring core above is pure. This is the half that talks to a
// provider: it composes each turn's prompt, collects the reply, parses
// it, and folds the replies into a depth. Kept in the same module so
// the prompt shape and the parser stay in step — a change to what the
// model is asked must ship alongside a change to how its reply is read.
//
// The driver takes a closure `run_turn: Fn(&str) -> Result<String>`
// rather than a provider trait. That keeps the module free of a
// provider dependency (kod-stats depends on no other kod crate) and
// lets a test pass a synchronous closure without a runtime.

/// Compose the prompt for `turn`.
///
/// Turn 0 is the setup prompt: it explains the game, gives the initial
/// array, and asks the model to report the array it would have after
/// the turn's action. Every subsequent turn says "the array you
/// reported last turn, after this swap is now …", so the model's own
/// previous reply is the only state carried.
///
/// The cat-sound directive rotates per `SoundPosition::for_turn`.
pub fn prompt_for_turn(turn: usize, initial: &[char]) -> String {
    let pos = SoundPosition::for_turn(turn);
    let sound_clause = match pos {
        SoundPosition::Start => format!(
            "Your reply MUST include the word \"{CAT_SOUND}\" on one of the \
             first lines.",
        ),
        SoundPosition::Middle => format!(
            "Your reply MUST include the word \"{CAT_SOUND}\" near the \
             middle of the reply.",
        ),
        SoundPosition::End => format!(
            "Your reply MUST include the word \"{CAT_SOUND}\" on one of the \
             last lines.",
        ),
    };

    if turn == 0 {
        let glyphs: String = initial.iter().collect();
        format!(
            "You are playing a memory game. I will give you an array of \
             single characters, then on each turn I will ask you to apply a \
             swap and report the new array.\n\n\
             The initial array is:\n{glyphs}\n\n\
             Turn 0: swap positions {a} and {b} (0-based). Report the new \
             array on a single line exactly as:\n\
             array: <glyphs>\n\n\
             {sound_clause}",
            a = action_for_turn(0, initial.len()).0,
            b = action_for_turn(0, initial.len()).1,
        )
    } else {
        let (a, b) = action_for_turn(turn, initial.len());
        format!(
            "Turn {turn}: take the array you reported on the previous turn \
             and swap positions {a} and {b} (0-based). Report the new array \
             on a single line exactly as:\n\
             array: <glyphs>\n\n\
             {sound_clause}",
        )
    }
}

/// Drive a full run of `config.turns` turns, using `run_turn` to make
/// the provider call for each prompt.
///
/// Returns the per-turn replies and the final depth. A turn whose
/// provider call errors is recorded as `reported: None, sound_ok:
/// false` — the run stops at that depth rather than aborting, so a
/// flaky provider produces a low score instead of no score.
///
/// `run_turn` is synchronous: the caller blocks inside it. The runner
/// in the CLI wraps an async provider by `block_on`-ing inside the
/// closure.
pub fn drive<F>(config: &IfBenchConfig, mut run_turn: F) -> (Vec<TurnReply>, usize)
where
    F: FnMut(&str) -> Result<String, String>,
{
    let initial = initial_array(config.array_size);
    let mut replies: Vec<TurnReply> = Vec::with_capacity(config.turns);
    let mut current = initial.clone();
    for turn in 0..config.turns {
        let prompt = prompt_for_turn(turn, &initial);
        let reply = match run_turn(&prompt) {
            Ok(r) => r,
            Err(_) => {
                replies.push(TurnReply {
                    reported: None,
                    sound_ok: false,
                });
                break;
            }
        };
        let reported = parse_reported_array(&reply);
        let pos = SoundPosition::for_turn(turn);
        let sound_ok = cat_sound_at(&reply, pos);
        replies.push(TurnReply {
            reported: reported.clone(),
            sound_ok,
        });
        // Update the running expectation so the *next* turn's expected
        // array is derived from the one the model should have reported,
        // not from what it actually reported. A wrong answer stops the
        // run at the next comparison anyway; carrying the true array
        // forward keeps the depths comparable across models.
        current = apply(&current, action_for_turn(turn, current.len()));
    }
    let d = depth(&replies, &initial);
    (replies, d)
}

#[cfg(test)]
mod driver_tests {
    use super::*;

    #[test]
    fn turn_zero_prompt_includes_the_initial_array() {
        let initial = initial_array(4);
        let p = prompt_for_turn(0, &initial);
        assert!(p.contains("abcd"), "got: {p}");
        assert!(p.contains("array: <glyphs>"), "got: {p}");
        assert!(p.contains(CAT_SOUND), "got: {p}");
    }

    #[test]
    fn a_later_turn_prompt_names_the_swap() {
        let initial = initial_array(8);
        let p = prompt_for_turn(3, &initial);
        // action_for_turn(3, 8) = (3, (21+3) % 8) = (3, 0)
        assert!(p.contains("swap positions 3 and 0"), "got: {p}");
        // Turn 3 → Start.
        assert!(p.contains("first lines"), "got: {p}");
    }

    #[test]
    fn the_sound_clause_rotates() {
        let initial = initial_array(4);
        let p0 = prompt_for_turn(0, &initial);
        let p1 = prompt_for_turn(1, &initial);
        let p2 = prompt_for_turn(2, &initial);
        assert!(p0.contains("first lines"), "got: {p0}");
        assert!(p1.contains("middle"), "got: {p1}");
        assert!(p2.contains("last lines"), "got: {p2}");
    }

    #[test]
    fn drive_runs_the_configured_number_of_turns() {
        let config = IfBenchConfig {
            turns: 4,
            array_size: 4,
            ..Default::default()
        };
        // A closure that always answers correctly.
        let initial = initial_array(4);
        let mut expected = initial.clone();
        let expected_seq: Vec<Vec<char>> = (0..4)
            .map(|t| {
                expected = apply(&expected, action_for_turn(t, expected.len()));
                expected.clone()
            })
            .collect();
        // Six-line reply so start / middle / end are each a distinct
        // non-empty third: with n=6, the thirds are [0,2), [2,4),
        // [4,6) and each has room for one sound.
        let mut idx = 0;
        let (replies, depth) = drive(&config, |_p| {
            let arr: String = expected_seq[idx].iter().collect();
            let pos = SoundPosition::for_turn(idx);
            let sound = match pos {
                SoundPosition::Start => "meow\nline\nline\nline\nline\narray: ",
                SoundPosition::Middle => "line\nline\nmeow\nline\nline\narray: ",
                SoundPosition::End => "line\nline\nline\nline\nmeow\narray: ",
            };
            idx += 1;
            Ok(format!("{sound}{arr}\n"))
        });
        assert_eq!(replies.len(), 4);
        assert_eq!(depth, 4);
    }

    #[test]
    fn drive_stops_at_a_provider_error() {
        let config = IfBenchConfig {
            turns: 4,
            array_size: 4,
            ..Default::default()
        };
        let mut calls = 0;
        let (replies, depth) = drive(&config, |_p| {
            calls += 1;
            if calls > 2 {
                Err("provider down".to_string())
            } else {
                Ok("meow\narray: dcba\n".to_string())
            }
        });
        // Two successful calls (both wrong, but the run still records
        // them), then the error short-circuits the loop.
        assert_eq!(replies.len(), 3);
        assert!(replies.last().unwrap().reported.is_none());
        assert_eq!(depth, 0, "wrong arrays never survive");
    }

    #[test]
    fn drive_records_a_dropped_sound_as_a_failure() {
        let config = IfBenchConfig {
            turns: 3,
            array_size: 4,
            ..Default::default()
        };
        // First turn: sound at start (ok) but array wrong.
        // Second turn: sound at end but array right — the sound will be
        // tested at the required position, and only the *first* failure
        // counts.
        let mut idx = 0;
        let (_, depth) = drive(&config, |_p| {
            idx += 1;
            Ok(match idx {
                1 => "meow\narray: xxxx\n".to_string(),
                _ => "line\nline\nmeow\narray: dcba\n".to_string(),
            })
        });
        assert_eq!(depth, 0, "the first wrong array caps the depth");
    }
}
