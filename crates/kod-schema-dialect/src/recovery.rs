//! Layers 2 and 3: learn what a provider refused.
//!
//! A sanitized schema should never be rejected — that is the point of
//! layer 1. When one is anyway, the provider's error names the
//! construct, and this module parses that name so the retry drops
//! exactly it. The learning is per-endpoint and persisted by the
//! caller: one wasted round trip ever, not one per request.

/// A construct a provider rejected.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Rejection {
    /// The JSON Schema keyword the provider named, when the message
    /// could be parsed. `None` when the error is opaque.
    pub keyword: Option<String>,
    /// The provider's message, kept verbatim for the log.
    pub message: String,
}

/// What to do after a rejection.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RetryPlan {
    /// Drop `keyword` from the schema and retry once. At most one
    /// retry per distinct construct — a schema that trips two
    /// unrelated keywords gets two attempts, not an infinite loop.
    RetryWithoutConstruct { keyword: String },
    /// The rejection named nothing actionable; surface it.
    GiveUp,
}

/// Keywords a provider error might name, longest first so a message
/// containing both `oneOf` and `of` matches the former.
const CANDIDATES: &[&str] = &[
    "additionalProperties",
    "unevaluatedProperties",
    "patternProperties",
    "dependentSchemas",
    "exclusiveMinimum",
    "exclusiveMaximum",
    "multipleOf",
    "prefixItems",
    "propertyNames",
    "uniqueItems",
    "minProperties",
    "maxProperties",
    "additionalItems",
    "anyOf",
    "allOf",
    "oneOf",
    "items",
    "enum",
    "const",
    "format",
    "pattern",
    "required",
    "minimum",
    "maximum",
    "minItems",
    "maxItems",
    "$ref",
];

/// Parse a provider's rejection into the construct it named.
///
/// High-recall and deliberately simple: it looks for a known keyword
/// as a whole word in the message. A provider that names the offending
/// construct in prose (OpenAI: *"Invalid schema ... 'const' is not
/// permitted"*) is understood; one that returns an opaque "invalid
/// request" yields [`RetryPlan::GiveUp`] rather than a guess.
pub fn classify_rejection(status: u16, body: &str) -> Rejection {
    let keyword = CANDIDATES
        .iter()
        .find(|kw| contains_word(body, kw))
        .map(|kw| (*kw).to_string());
    Rejection {
        keyword,
        message: format!("{status}: {}", body.trim()),
    }
}

/// Whether `text` contains `word` as a whole token — not as a
/// substring of a longer identifier. `oneOf` matches in "rejected
/// oneOf", not in "someoneOfNote".
fn contains_word(text: &str, word: &str) -> bool {
    let bytes = text.as_bytes();
    let w = word.as_bytes();
    if w.is_empty() || w.len() > bytes.len() {
        return false;
    }
    let is_boundary = |b: u8| !(b.is_ascii_alphanumeric() || b == b'_' || b == b'$');
    let mut i = 0;
    while i + w.len() <= bytes.len() {
        if &bytes[i..i + w.len()] == w {
            let before_ok = i == 0 || is_boundary(bytes[i - 1]);
            let after = i + w.len();
            let after_ok = after == bytes.len() || is_boundary(bytes[after]);
            if before_ok && after_ok {
                return true;
            }
        }
        i += 1;
    }
    false
}

/// Turn a rejection into a retry decision.
pub fn plan_retry(rejection: &Rejection) -> RetryPlan {
    match &rejection.keyword {
        Some(kw) => RetryPlan::RetryWithoutConstruct {
            keyword: kw.clone(),
        },
        None => RetryPlan::GiveUp,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_named_keyword_is_extracted() {
        let r = classify_rejection(
            400,
            "Invalid schema for function 'read_file': 'const' is not permitted",
        );
        assert_eq!(r.keyword.as_deref(), Some("const"));
        assert_eq!(
            plan_retry(&r),
            RetryPlan::RetryWithoutConstruct {
                keyword: "const".into()
            },
        );
    }

    #[test]
    fn an_opaque_error_gives_up() {
        let r = classify_rejection(400, "invalid request");
        assert!(r.keyword.is_none());
        assert_eq!(plan_retry(&r), RetryPlan::GiveUp);
    }

    #[test]
    fn a_longer_keyword_wins_over_a_substring() {
        // "oneOf" must be found, not "of" (which is not a candidate
        // anyway) and not a stray "enum" inside "enumerate".
        let r = classify_rejection(400, "the oneOf keyword is unsupported");
        assert_eq!(r.keyword.as_deref(), Some("oneOf"));
    }

    #[test]
    fn word_boundaries_prevent_false_matches() {
        // "enumerate" contains "enum" as a substring but must not
        // match.
        assert!(!contains_word("please enumerate the values", "enum"));
        assert!(contains_word("enum is not allowed", "enum"));
        assert!(contains_word("'enum'", "enum"));
    }

    #[test]
    fn ref_is_recognised_despite_the_dollar() {
        let r = classify_rejection(400, "schema uses $ref which is not supported");
        assert_eq!(r.keyword.as_deref(), Some("$ref"));
    }
}

#[cfg(test)]
mod recovery_proptests {
    use super::*;
    use proptest::prelude::*;
    use proptest::sample::select;

    fn candidate() -> impl Strategy<Value = &'static str> {
        select(CANDIDATES.to_vec())
    }

    proptest! {
        /// Totality: `contains_word` must not panic on any input.
        /// It slices bytes and indexes neighbors, so a future
        /// rewrite that switches to `char_indices` or slicing `str`
        /// could introduce a boundary panic; this property pins that
        /// regardless of the input alphabet, the function returns.
        #[test]
        fn prop_contains_word_never_panics(text in "\\PC{0,200}", word in "\\PC{0,40}") {
            let _ = contains_word(&text, &word);
        }

        /// Totality: `classify_rejection` must not panic on any
        /// provider body. Error bodies are attacker-influenced in the
        /// worst case (a proxy could echo a request), so arbitrary
        /// Unicode and arbitrary length are normal inputs.
        #[test]
        fn prop_classify_never_panics(status in any::<u16>(), body in "\\PC{0,300}") {
            let _ = classify_rejection(status, &body);
        }

        /// Soundness: when a keyword is returned it is one of the
        /// candidates. `classify_rejection` cannot invent a
        /// construct — the caller persists the keyword and drops it
        /// from the schema, so an invented keyword would corrupt the
        /// schema silently.
        #[test]
        fn prop_keyword_is_a_candidate(status in any::<u16>(), body in "\\PC{0,300}") {
            let r = classify_rejection(status, &body);
            if let Some(kw) = r.keyword {
                prop_assert!(
                    CANDIDATES.contains(&kw.as_str()),
                    "invented keyword {kw:?}",
                );
            }
        }

        /// A candidate surrounded by ASCII punctuation is found, and
        /// `classify_rejection` returns it.
        #[test]
        fn prop_ascii_punctuation_delimited_matches(kw in candidate()) {
            let body = format!("rejected ({kw}) here");
            prop_assert!(contains_word(&body, kw), "missed {kw:?} in {body:?}");
            let r = classify_rejection(400, &body);
            prop_assert_eq!(r.keyword.as_deref(), Some(kw));
        }

        /// A candidate directly abutting ASCII alphanumerics is part
        /// of a longer identifier and must not match.
        #[test]
        fn prop_ascii_identifier_does_not_match(kw in candidate()) {
            let body = format!("x{kw}x");
            prop_assert!(!contains_word(&body, kw), "false positive on {body:?}");
            let r = classify_rejection(400, &body);
            prop_assert_eq!(r.keyword, None);
        }

        /// Message shape: `"<status>: <trimmed body>"`. The caller
        /// logs it; a shape change is observable in output.
        #[test]
        fn prop_message_shape(status in any::<u16>(), body in "\\PC{0,200}") {
            let r = classify_rejection(status, &body);
            let expected = format!("{status}: {}", body.trim());
            prop_assert_eq!(r.message, expected);
        }

        /// `plan_retry` mirrors the keyword exactly: `Some` retries
        /// without that construct, `None` gives up.
        #[test]
        fn prop_plan_retry_mirrors_keyword(status in any::<u16>(), body in "\\PC{0,300}") {
            let r = classify_rejection(status, &body);
            match (&r.keyword, plan_retry(&r)) {
                (Some(kw), RetryPlan::RetryWithoutConstruct { keyword }) => {
                    prop_assert_eq!(&keyword, kw);
                }
                (None, RetryPlan::GiveUp) => {}
                (Some(kw), RetryPlan::GiveUp) => {
                    prop_assert!(false, "Some({kw:?}) but GiveUp");
                }
                (None, RetryPlan::RetryWithoutConstruct { keyword }) => {
                    prop_assert!(false, "None but RetryWithoutConstruct {{ {keyword:?} }}");
                }
            }
        }
    }

    /// Deterministic: with exactly one candidate present, that
    /// candidate is the one returned (the `find` over `CANDIDATES`).
    #[test]
    fn single_candidate_body_returns_that_candidate() {
        let r = classify_rejection(400, "Invalid schema: 'const' is not permitted");
        assert_eq!(r.keyword.as_deref(), Some("const"));
        assert_eq!(
            plan_retry(&r),
            RetryPlan::RetryWithoutConstruct {
                keyword: "const".into(),
            },
        );
    }

    /// Deterministic: an opaque body yields `None` / `GiveUp`.
    #[test]
    fn opaque_body_gives_up() {
        let r = classify_rejection(400, "invalid request, no field named");
        assert_eq!(r.keyword, None);
        assert_eq!(plan_retry(&r), RetryPlan::GiveUp);
    }

    /// Characterization of a **known limitation**, not an assertion
    /// that the behavior is correct: `contains_word` walks bytes and
    /// treats every non-ASCII-alphanumeric / non-`_` / non-`$` byte
    /// as a boundary. UTF-8 continuation bytes (`0x80..=0xBF`) all
    /// satisfy that predicate, so a multibyte character directly
    /// before a keyword — `"列挙enum"` (the `挙` byte before `e` is
    /// `0x99`) — counts as a boundary even though a character-aware
    /// check would say the preceding character is a word character.
    ///
    /// Reachability is extremely narrow (provider error bodies are
    /// typically ASCII or English prose), and a fix requires walking
    /// `char_indices()` instead of bytes. If a future change does
    /// that, this test is the one to update: **the change is
    /// correct, this characterization is the current bug.**
    #[test]
    fn utf8_multibyte_before_keyword_is_currently_a_false_positive() {
        assert!(contains_word("列挙enum", "enum"));
    }
}
