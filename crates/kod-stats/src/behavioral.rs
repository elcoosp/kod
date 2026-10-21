//! Behavioral user metrics (borrow from oh-my-pi, delta §13.2).
//!
//! Five lexical signals over the user's message: `negation`,
//! `repetition`, `blame`, `anguish`, `yelling`. A spike in negation or
//! repetition is the cheapest available signal that the agent is not
//! doing what the user asked.

/// The counts for one user message.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct BehavioralSignals {
    pub negation: u32,
    pub repetition: u32,
    pub blame: u32,
    pub anguish: u32,
    pub yelling: u32,
}

impl BehavioralSignals {
    pub fn total(&self) -> u32 {
        self.negation + self.repetition + self.blame + self.anguish + self.yelling
    }

    pub fn any(&self) -> bool {
        self.total() > 0
    }

    pub fn merge(&self, other: &BehavioralSignals) -> BehavioralSignals {
        BehavioralSignals {
            negation: self.negation.saturating_add(other.negation),
            repetition: self.repetition.saturating_add(other.repetition),
            blame: self.blame.saturating_add(other.blame),
            anguish: self.anguish.saturating_add(other.anguish),
            yelling: self.yelling.saturating_add(other.yelling),
        }
    }
}

const NEGATION_CUES: &[&str] = &[
    "no,",
    "no.",
    "nope",
    "nah",
    "wrong",
    "that's not",
    "that is not",
    "not what i",
    "incorrect",
];

const REPETITION_CUES: &[&str] = &[
    "i meant",
    "i said",
    "i asked",
    "still doesn't",
    "still does not",
    "still not",
    "again:",
    "as i said",
    "like i said",
];

const BLAME_CUES: &[&str] = &[
    "you didn't",
    "you did not",
    "why did you",
    "why didn't you",
    "you should have",
    "you were supposed to",
    "you broke",
];

const ANGUISH_CUES: &[&str] = &[
    "ffs",
    "damn",
    "ugh",
    "argh",
    "wtf",
    "come on",
    "seriously?",
    "why is this",
];

fn count_cues(lower: &str, cues: &[&str]) -> u32 {
    let mut n = 0;
    for cue in cues {
        n += lower.matches(cue).count() as u32;
    }
    n
}

/// Five or more consecutive uppercase letters on one line.
fn is_yelling(line: &str) -> bool {
    let mut run = 0usize;
    for c in line.chars() {
        if c.is_ascii_uppercase() {
            run += 1;
            if run >= 5 {
                return true;
            }
        } else {
            run = 0;
        }
    }
    false
}

/// Extract the signals from one user message.
pub fn analyze(text: &str) -> BehavioralSignals {
    let lower = text.to_ascii_lowercase();
    let negation = count_cues(&lower, NEGATION_CUES);
    let repetition = count_cues(&lower, REPETITION_CUES);
    let blame = count_cues(&lower, BLAME_CUES);
    let anguish = count_cues(&lower, ANGUISH_CUES);
    let yelling = text.lines().filter(|l| is_yelling(l)).count() as u32;
    BehavioralSignals {
        negation,
        repetition,
        blame,
        anguish,
        yelling,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_plain_message_has_no_signals() {
        assert!(!analyze("please add a function to parse the config").any());
    }

    #[test]
    fn a_negation_is_counted() {
        assert!(analyze("No, that's not right").negation >= 1);
    }

    #[test]
    fn a_repetition_is_counted() {
        assert!(analyze("I meant the other file").repetition >= 1);
    }

    #[test]
    fn a_blame_is_counted() {
        assert!(analyze("you didn't run the tests").blame >= 1);
    }

    #[test]
    fn an_anguish_is_counted() {
        assert!(analyze("ugh, still broken").anguish >= 1);
    }

    #[test]
    fn yelling_needs_five_capitals() {
        assert_eq!(analyze("WHY").yelling, 0);
        assert_eq!(analyze("WHYNOT").yelling, 1);
    }

    #[test]
    fn yelling_is_per_line() {
        assert_eq!(analyze("HELLO there\nworld").yelling, 1);
    }

    #[test]
    fn a_case_insensitive_cue_matches() {
        assert!(analyze("NOPE").negation >= 1);
    }

    #[test]
    fn multiple_signals_accumulate() {
        let s = analyze("No, I meant the other file. you didn't run tests");
        assert!(s.negation >= 1 && s.repetition >= 1 && s.blame >= 1);
        assert!(s.total() >= 3);
    }

    #[test]
    fn merging_sums_every_field() {
        let a = BehavioralSignals {
            negation: 1,
            ..Default::default()
        };
        let b = BehavioralSignals {
            repetition: 2,
            yelling: 1,
            ..Default::default()
        };
        let m = a.merge(&b);
        assert_eq!(
            (m.negation, m.repetition, m.yelling, m.total()),
            (1, 2, 1, 4)
        );
    }

    #[test]
    fn any_is_false_for_the_default() {
        assert!(!BehavioralSignals::default().any());
    }
}

#[cfg(test)]
mod behavioral_proptests {
    //! Properties and characterizations for the behavioral signals.
    //!
    //! This is a **characterization round**: the module doc says
    //! "five lexical signals" and promises no word boundaries, so two
    //! behaviors below are pinned as they are, not asserted correct.
    //!
    //! 1. `count_cues` is plain substring matching. `"ugh"` is a cue,
    //!    so `"enough"`, `"rough"`, `"tough"`, `"laugh"`, `"through"`
    //!    each score an anguish point. Same class: `"ffs"` inside
    //!    `"staffs"`/`"offs"`. A boundary-aware matcher would be a
    //!    design change; named here so the false positive is visible.
    //! 2. `merge` saturates but `total` uses plain `+`. Unreachable
    //!    from real text (a message would need billions of cue hits)
    //!    but the inconsistency is real.
    use super::*;
    use proptest::prelude::*;

    proptest! {
        #[test]
        fn prop_analyze_never_panics(text in "\\PC{0,400}") {
            let _ = analyze(&text);
        }

        #[test]
        fn prop_analyze_deterministic(text in "\\PC{0,400}") {
            let a = analyze(&text);
            let b = analyze(&text);
            prop_assert_eq!(a, b);
        }

        /// `analyze`'s fields are exactly the per-table counts of the
        /// lowercased text. Pinning the decomposition means a refactor
        /// that drops a table or double-counts one is caught.
        #[test]
        fn prop_analyze_decomposes_into_the_tables(text in "\\PC{0,400}") {
            let lower = text.to_ascii_lowercase();
            let got = analyze(&text);
            prop_assert_eq!(got.negation, count_cues(&lower, NEGATION_CUES));
            prop_assert_eq!(got.repetition, count_cues(&lower, REPETITION_CUES));
            prop_assert_eq!(got.blame, count_cues(&lower, BLAME_CUES));
            prop_assert_eq!(got.anguish, count_cues(&lower, ANGUISH_CUES));
            let lines = text.lines().filter(|l| is_yelling(l)).count() as u32;
            prop_assert_eq!(got.yelling, lines);
        }

        /// `total` is the field sum and `any` is `total > 0`.
        #[test]
        fn prop_total_and_any_agree_with_fields(text in "\\PC{0,300}") {
            let s = analyze(&text);
            let expected = s.negation + s.repetition + s.blame + s.anguish + s.yelling;
            prop_assert_eq!(s.total(), expected);
            prop_assert_eq!(s.any(), expected > 0);
        }

        /// `merge` is field-wise addition (saturating at the u32 cap,
        /// unreachable from real text).
        #[test]
        fn prop_merge_is_fieldwise(
            a_neg in any::<u32>(), a_rep in any::<u32>(), a_bl in any::<u32>(),
            a_ang in any::<u32>(), a_yel in any::<u32>(),
            b_neg in any::<u32>(), b_rep in any::<u32>(), b_bl in any::<u32>(),
            b_ang in any::<u32>(), b_yel in any::<u32>(),
        ) {
            let a = BehavioralSignals { negation: a_neg, repetition: a_rep, blame: a_bl, anguish: a_ang, yelling: a_yel };
            let b = BehavioralSignals { negation: b_neg, repetition: b_rep, blame: b_bl, anguish: b_ang, yelling: b_yel };
            let m = a.merge(&b);
            prop_assert_eq!(m.negation, a_neg.saturating_add(b_neg));
            prop_assert_eq!(m.repetition, a_rep.saturating_add(b_rep));
            prop_assert_eq!(m.blame, a_bl.saturating_add(b_bl));
            prop_assert_eq!(m.anguish, a_ang.saturating_add(b_ang));
            prop_assert_eq!(m.yelling, a_yel.saturating_add(b_yel));
        }

        /// `is_yelling` is a run of five consecutive ASCII uppercase
        /// characters, on a single line. Exact restatement of the body;
        /// pinned so a rewrite must preserve it.
        #[test]
        fn prop_yelling_is_five_consecutive_ascii_uppercase(
            prefix in "[a-z ]{0,10}",
            run in "[A-Z]{0,10}",
            suffix in "[a-z ]{0,10}",
        ) {
            let line = format!("{}{}{}", prefix, run, suffix);
            let want = line
                .chars()
                .scan(0usize, |acc, c| {
                    if c.is_ascii_uppercase() { *acc += 1; Some(*acc) } else { *acc = 0; Some(*acc) }
                })
                .any(|n| n >= 5);
            prop_assert_eq!(is_yelling(&line), want, "line={:?}", line);
        }
    }

    /// **Characterization.** `"ugh"` matches inside common words.
    #[test]
    fn ugh_cue_fires_inside_common_words() {
        for word in ["enough", "rough", "tough", "laugh", "through"] {
            let s = analyze(&format!("that is {}", word));
            assert_eq!(s.anguish, 1, "{:?} should score anguish via 'ugh'", word);
        }
    }

    /// **Characterization.** `"ffs"` matches inside `"offs"`/`"staffs"`.
    #[test]
    fn ffs_cue_fires_inside_ffs_words() {
        for word in ["offs", "staffs", "offsets"] {
            let s = analyze(word);
            assert!(s.anguish >= 1, "{:?} should score anguish via 'ffs'", word);
        }
    }

    /// Exactly four consecutive uppercase does not fire; five does.
    #[test]
    fn yelling_boundary_is_five() {
        assert!(!is_yelling("abcd"));
        assert!(!is_yelling("ABCD"));
        assert!(is_yelling("ABCDE"));
        assert!(is_yelling("abcABCDEdef"));
    }

    /// Non-ASCII uppercase does not count — the check is
    /// `is_ascii_uppercase`.
    #[test]
    fn yelling_is_ascii_only() {
        assert!(!is_yelling("ÉÉÉÉÉ"));
        assert!(!is_yelling("ÄÖÜÄÖ"));
    }

    /// A cue is matched case-insensitively because `analyze`
    /// lowercases the text first.
    #[test]
    fn cue_matching_is_case_insensitive() {
        let lower = analyze("wrong");
        let upper = analyze("WRONG");
        assert_eq!(lower.negation, upper.negation);
        assert_eq!(upper.negation, 1);
    }

    /// A message with no cues and no yelling has an empty signal.
    #[test]
    fn plain_message_is_empty() {
        let s = analyze("please continue with the next step");
        assert_eq!(s, BehavioralSignals::default());
    }
}
