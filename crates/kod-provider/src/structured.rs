//! `run_structured`: ask for JSON, validate it, retry with the error.
//!
//! A model asked for JSON produces JSON *ish* output — a fenced
//! block, a leading sentence, a trailing comma. Parsing it is a
//! per-caller concern today, and each caller that does it badly
//! produces a different failure. This is the one implementation: call
//! the model, extract the JSON, validate it against the schema, and
//! on failure re-ask with the validator's own error message appended,
//! bounded.
//!
//! The retries are the point. A model that returned a wrong-typed
//! field can usually fix it when told which field and what type was
//! expected; the validator's message is exactly that, which is why it
//! is quoted verbatim rather than paraphrased.

use serde_json::Value;

use crate::traits::{GenerationOptions, LlmProvider};
use kod_error::{KodError, Result};

/// How many corrective re-asks a `run_structured` call makes.
pub const DEFAULT_MAX_RETRIES: u32 = 2;

/// Ask `provider` for JSON matching `schema`, retrying on a validation
/// failure.
///
/// Returns the parsed value on success. After the retry budget is
/// spent, returns an error carrying the last validation failure — the
/// caller sees *what* was wrong with the final attempt, not a generic
/// "invalid JSON."
pub async fn run_structured(
    provider: &dyn LlmProvider,
    prompt: &str,
    schema: &Value,
    options: &GenerationOptions,
    max_retries: u32,
) -> Result<Value> {
    let schema_text = serde_json::to_string_pretty(schema).unwrap_or_default();
    let base = format!(
        "{prompt}\n\nReply with a single JSON value matching this schema. \
         No prose, no code fence.\n\n{schema_text}",
    );

    let mut attempt = 0u32;
    let mut last_error = String::new();
    loop {
        let ask = if last_error.is_empty() {
            base.clone()
        } else {
            format!(
                "{base}\n\nYour previous reply did not match the schema:\n\
                 {last_error}\n\nReply with corrected JSON only.",
            )
        };

        let raw = provider.generate(&ask, options).await?;
        match extract_json(&raw) {
            Some(value) => match crate::validation::validate(schema, &value) {
                Ok(()) => return Ok(value),
                Err(e) => last_error = e,
            },
            None => last_error = "no JSON value found in the reply".to_string(),
        }

        if attempt >= max_retries {
            return Err(KodError::InvalidParameters {
                reason: format!(
                    "structured reply failed validation after {} attempt(s): {last_error}",
                    attempt + 1,
                ),
            });
        }
        attempt += 1;
    }
}

/// Pull a JSON value out of a model's reply.
///
/// Three shapes, in order of likelihood: a fenced block (the model was
/// told not to use one and did anyway), the whole text, and the first
/// balanced `{...}` or `[...]` in the text. A reply with none of those
/// is not JSON and the caller retries.
pub fn extract_json(raw: &str) -> Option<Value> {
    let trimmed = raw.trim();

    // Fenced block.
    if let Some(start) = trimmed.find("```") {
        let after = &trimmed[start + 3..];
        let after = match after.find('\n') {
            Some(nl) => &after[nl + 1..],
            None => after,
        };
        if let Some(end) = after.find("```")
            && let Ok(v) = serde_json::from_str(after[..end].trim())
        {
            return Some(v);
        }
    }

    // The whole text.
    if let Ok(v) = serde_json::from_str(trimmed) {
        return Some(v);
    }

    // The first balanced object or array. The scan is string-aware:
    // a `{` / `}` inside a JSON string literal is data, not
    // structure, so counting it desynchronizes the depth and the
    // balanced slice never parses — a prose-wrapped JSON value
    // whose content happens to contain a brace burns the whole
    // retry budget.
    for (open, close) in [('{', '}'), ('[', ']')] {
        if let Some(start) = trimmed.find(open) {
            let mut depth = 0i32;
            let mut in_string = false;
            let mut escaped = false;
            for (i, ch) in trimmed[start..].char_indices() {
                if in_string {
                    if escaped {
                        escaped = false;
                    } else if ch == '\\' {
                        escaped = true;
                    } else if ch == '"' {
                        in_string = false;
                    }
                    continue;
                }
                match ch {
                    '"' => in_string = true,
                    c if c == open => depth += 1,
                    c if c == close => {
                        depth -= 1;
                        if depth == 0
                            && let Ok(v) =
                                serde_json::from_str(&trimmed[start..start + i + ch.len_utf8()])
                        {
                            return Some(v);
                        }
                    }
                    _ => {}
                }
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn extract_json_ignores_braces_inside_strings() {
        // F2i-6: a `}` inside a JSON string must not close the object.
        let prose = r#"here you go: {"a": "}", "b": {"c": "}}{"} } trailing"#;
        let v = extract_json(prose).expect("must find the object");
        assert_eq!(v["a"], json!("}"));
        assert_eq!(v["b"]["c"], json!("}}{"));
    }

    #[test]
    fn a_bare_object_parses() {
        assert_eq!(extract_json(r#"{"a": 1}"#), Some(json!({"a": 1})));
    }

    #[test]
    fn a_fenced_block_parses() {
        let raw = "Here you go:\n```json\n{\"a\": 1}\n```\n";
        assert_eq!(extract_json(raw), Some(json!({"a": 1})));
    }

    #[test]
    fn prose_wrapping_an_object_parses() {
        let raw = "The answer is {\"a\": 1} as requested.";
        assert_eq!(extract_json(raw), Some(json!({"a": 1})));
    }

    #[test]
    fn an_array_parses() {
        assert_eq!(extract_json("[1, 2, 3]"), Some(json!([1, 2, 3])));
    }

    #[test]
    fn a_non_json_reply_is_none() {
        assert_eq!(extract_json("I cannot do that."), None);
    }

    #[test]
    fn nested_braces_are_balanced_not_truncated() {
        let raw = r#"prefix {"a": {"b": 1}} suffix"#;
        assert_eq!(extract_json(raw), Some(json!({"a": {"b": 1}})));
    }

    #[test]
    fn an_unterminated_object_is_none() {
        // A truncated stream must not yield a half-parsed value.
        assert_eq!(extract_json(r#"{"a": 1"#), None);
    }
}

#[cfg(test)]
mod json_extract_property_tests {
    use super::*;
    use proptest::prelude::*;
    use proptest::string::string_regex;

    // KNOWN GAP (documented, not tested here): a JSON string value
    // containing ``` inside a fenced block confuses the fence-
    // terminator search. The round-trip tests below constrain `name`
    // to [a-z]{0,10} so the value never contains backticks; the
    // ambiguity is tracked as a separate finding.
    proptest! {
        #![proptest_config(ProptestConfig::with_cases(64))]

        #[test]
        fn whole_text_object_round_trips(
            name in "[a-z]{0,10}",
            value in any::<i64>(),
        ) {
            let v = serde_json::json!({"name": name, "value": value});
            let s = serde_json::to_string(&v).expect("serialize");
            prop_assert_eq!(extract_json(&s), Some(v));
        }

        #[test]
        fn fenced_object_round_trips(
            name in "[a-z]{0,10}",
            value in any::<i64>(),
        ) {
            let v = serde_json::json!({"name": name, "value": value});
            let s = serde_json::to_string(&v).expect("serialize");
            // Plain backticks — no backslash. Rust's string-literal
            // escapes do not include backtick.
            let wrapped = format!("```json\n{s}\n```");
            prop_assert_eq!(extract_json(&wrapped), Some(v));
        }

        #[test]
        fn never_panics_on_arbitrary_utf8(
            s in string_regex("(?s).{0,80}").unwrap(),
        ) {
            let _ = extract_json(&s);
        }
    }
}

#[cfg(test)]
mod structured_extract_proptests {
    //! Properties and a characterization for `extract_json`.
    //!
    //! The extractor anchors on `{` or `[`: it scans for a balanced
    //! object or array after the whole text fails to parse. A
    //! **scalar in prose** (a bare `42`, `null`, or `true` wrapped in
    //! words) has neither anchor, so it is never found — that is the
    //! contract, pinned below as a characterization.
    use super::*;
    use proptest::prelude::*;
    use serde_json::{Map, Value};

    /// Prose with no `{`, `}`, `[`, `]`, or ```` ``` ```` — those
    /// would give the extractor an anchor it is not meant to find in
    /// the surrounding text.
    fn arb_prose() -> impl Strategy<Value = String> {
        "[^\\[\\]{}`]{0,30}"
    }

    fn arb_object() -> impl Strategy<Value = Value> {
        prop::collection::vec(("[a-z]{1,5}", -1000i64..1000), 1..4).prop_map(|pairs| {
            let mut m = Map::new();
            for (k, v) in pairs {
                m.insert(k, Value::from(v));
            }
            Value::Object(m)
        })
    }

    fn arb_array() -> impl Strategy<Value = Value> {
        prop::collection::vec(-1000i64..1000, 1..5)
            .prop_map(|v| Value::Array(v.into_iter().map(Value::from).collect()))
    }

    proptest! {
        #[test]
        fn prop_extract_never_panics(text in "\\PC{0,300}") {
            let _ = extract_json(&text);
        }

        #[test]
        fn prop_extract_deterministic(text in "\\PC{0,300}") {
            prop_assert_eq!(extract_json(&text), extract_json(&text));
        }

        /// An object wrapped in prose is found and returned whole.
        #[test]
        fn prop_object_in_prose_round_trips(
            v in arb_object(),
            pre in arb_prose(),
            post in arb_prose(),
        ) {
            let text = format!("{}{}{}", pre, v, post);
            let got = extract_json(&text);
            prop_assert_eq!(got, Some(v), "text={:?}", text);
        }

        /// An array wrapped in prose is found and returned whole.
        #[test]
        fn prop_array_in_prose_round_trips(
            v in arb_array(),
            pre in arb_prose(),
            post in arb_prose(),
        ) {
            let text = format!("{}{}{}", pre, v, post);
            let got = extract_json(&text);
            prop_assert_eq!(got, Some(v), "text={:?}", text);
        }

        /// An object inside a fenced code block is found. The fence
        /// body is `{"k":v}` with no surrounding prose.
        #[test]
        fn prop_fenced_object_round_trips(v in arb_object()) {
            let text = format!("Here you go:\n```json\n{}\n```\n", v);
            let got = extract_json(&text);
            prop_assert_eq!(got, Some(v), "text={:?}", text);
        }
    }

    /// **Characterization.** A bare scalar parses through the
    /// whole-text branch.
    #[test]
    fn scalar_alone_parses() {
        assert_eq!(extract_json("null"), Some(Value::Null));
        assert_eq!(extract_json("42"), Some(Value::from(42)));
        assert_eq!(extract_json("true"), Some(Value::Bool(true)));
        assert_eq!(extract_json("\"hi\""), Some(Value::from("hi")));
    }

    /// **Characterization.** A scalar wrapped in prose is NOT found:
    /// there is no `{` or `[` for the balanced scan to anchor on, and
    /// the whole text does not parse.
    #[test]
    fn scalar_in_prose_is_not_found() {
        assert_eq!(extract_json("the answer is 42, obviously"), None);
        assert_eq!(extract_json("value: null here"), None);
        assert_eq!(extract_json("it is true that"), None);
        assert_eq!(extract_json("called \"hi\" by everyone"), None);
    }

    /// A `}` inside a string literal must not close the object.
    /// The existing example test pins one case; this one pins the
    /// structural rule with nested braces inside strings.
    #[test]
    fn braces_inside_strings_do_not_close_the_object() {
        let text = r#"prefix {"a": "}", "b": {"c": "}}"}} suffix"#;
        let got = extract_json(text).expect("must find the object");
        assert_eq!(got["a"], Value::from("}"));
        assert_eq!(got["b"]["c"], Value::from("}}"));
    }

    /// An unterminated object returns None, never a partial value.
    #[test]
    fn unterminated_object_is_none() {
        assert_eq!(extract_json(r#"{"a": 1"#), None);
        assert_eq!(extract_json(r#"[1, 2"#), None);
    }
}
