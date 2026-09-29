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
