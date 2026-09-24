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

    // The first balanced object or array.
    for (open, close) in [('{', '}'), ('[', ']')] {
        if let Some(start) = trimmed.find(open) {
            let mut depth = 0;
            for (i, ch) in trimmed[start..].char_indices() {
                if ch == open {
                    depth += 1;
                } else if ch == close {
                    depth -= 1;
                    if depth == 0
                        && let Ok(v) = serde_json::from_str(&trimmed[start..start + i + 1])
                    {
                        return Some(v);
                    }
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
