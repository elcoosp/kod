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
        Some(kw) => RetryPlan::RetryWithoutConstruct { keyword: kw.clone() },
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
            RetryPlan::RetryWithoutConstruct { keyword: "const".into() },
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
