//! Secret redaction (Tier 1.3).
//!
//! One compiled rule set applied at three entry points: log-write,
//! prompt-build, and policy-read. Built-in rules cover the common
//! provider keys and PEM-encoded material; users can extend via
//! `~/.kod/redact.toml`. A Shannon-entropy heuristic catches
//! high-entropy tokens near key-ish keywords (`key`, `token`,
//! `secret`, `password`, `api_key`).
//!
//! # Guarantees
//!
//! * Redaction is deterministic — the same input always yields the
//!   same output. A rule cannot fire on a substring of an earlier
//!   substitution.
//! * The default rule set is append-only across builds; removing a
//!   rule would un-redact historical logs on replay.
//! * A redacted value is replaced by `[REDACTED:<rule>]`, so a
//!   reader can still reason about *what kind* of secret was there
//!   without seeing the value.
//!
//! # What this does not do
//!
//! It is not a substitute for not having the secret in the first
//! place. A `read_file(".env")` still happens — the content the model
//! sees is redacted, but a caller that later `execute_command("cat
//! .env")` gets the raw file. The policy layer's `read_protection`
//! block refuses that pattern in the default configuration.

use regex::Regex;
use serde::{Deserialize, Serialize};
use std::sync::OnceLock;

/// One redaction rule. `pattern` is compiled once, `head`/`tail`
/// characters of the matched value are preserved (0 means drop
/// entirely), and `name` appears in the replacement marker.
#[derive(Debug, Clone)]
pub struct RedactRule {
    pub name: String,
    pub pattern: Regex,
    pub head: usize,
    pub tail: usize,
}

/// A single redaction event, for logging.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Redaction {
    pub rule: String,
    pub count: usize,
}

/// The redactor. Cheap to clone; the compiled rules live behind an
/// `Arc` shared across clones.
#[derive(Clone)]
pub struct Redactor {
    rules: std::sync::Arc<Vec<RedactRule>>,
    entropy_floor: f64,
    entropy_scan: bool,
}

impl std::fmt::Debug for Redactor {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Redactor")
            .field("rule_count", &self.rules.len())
            .field("entropy_floor", &self.entropy_floor)
            .field("entropy_scan", &self.entropy_scan)
            .finish()
    }
}

impl Default for Redactor {
    fn default() -> Self {
        Self::with_builtin_rules()
    }
}

impl Redactor {
    /// Build with the built-in rules and the default entropy floor.
    ///
    /// The compiled rule set is shared process-wide. `builtin_rules()`
    /// compiles 13 regexes (one per rule), and the
    /// `redactor_construction/builtin_rules_unshared` bench measures
    /// that at **2.54 ms** -- not the ~100 us a per-regex estimate
    /// suggests, because the lazy multi-line PEM pattern and the
    /// case-insensitive alternations are expensive to compile. The
    /// `default` fixture, which hits this `OnceLock`, measures
    /// **11 ns**. The rules are
    /// immutable after construction -- `rules` is an `Arc<Vec<_>>`
    /// and every method reads it -- so a single compiled set can be
    /// shared. `Redactor::default()` is constructed per HTTP error in
    /// `kod-error::KodError::provider_status_with_hint` (to strip
    /// secrets from the response body before it reaches `Display`),
    /// which made the per-call cost additive on a retry loop. The
    /// `Arc` clone keeps `with_rules` and `with_entropy` independent:
    /// a caller that supplies custom rules gets its own set, and the
    /// entropy knobs are per-instance `f64`/`bool`.
    pub fn with_builtin_rules() -> Self {
        static RULES: std::sync::OnceLock<std::sync::Arc<Vec<RedactRule>>> =
            std::sync::OnceLock::new();
        Self {
            rules: std::sync::Arc::clone(
                RULES.get_or_init(|| std::sync::Arc::new(builtin_rules())),
            ),
            entropy_floor: 4.0,
            entropy_scan: true,
        }
    }

    /// Build with only the given rules. The entropy heuristic is off
    /// — a caller that supplied rules wants exactly those rules.
    pub fn with_rules(rules: Vec<RedactRule>) -> Self {
        Self {
            rules: std::sync::Arc::new(rules),
            entropy_floor: 0.0,
            entropy_scan: false,
        }
    }

    /// Turn the entropy heuristic on or off. `floor` in `[0.0, 8.0]`;
    /// values below 3.5 false-positive on normal code identifiers.
    pub fn with_entropy(mut self, on: bool, floor: f64) -> Self {
        self.entropy_scan = on;
        self.entropy_floor = floor.clamp(0.0, 8.0);
        self
    }

    /// Redact `input`, returning the sanitized string and the list of
    /// redactions that fired. A rule that fires zero times is not
    /// reported.
    pub fn redact(&self, input: &str) -> (String, Vec<Redaction>) {
        let mut current = input.to_string();
        let mut out = Vec::new();
        for rule in self.rules.iter() {
            let mut count = 0_usize;
            let mut replacement = String::new();
            replacement.push_str("[REDACTED:");
            replacement.push_str(&rule.name);
            replacement.push(']');
            // Manual replacement so we can count fires and so the
            // substitution text itself is never rescanned.
            let mut result = String::with_capacity(current.len());
            let mut last = 0_usize;
            for m in rule.pattern.find_iter(&current) {
                result.push_str(&current[last..m.start()]);
                let matched = m.as_str();
                if rule.head > 0 || rule.tail > 0 {
                    // `head` / `tail` are documented as CHARACTER
                    // counts. Convert to byte offsets via
                    // `char_indices` so a non-ASCII match (a custom
                    // rule whose pattern matches multibyte input,
                    // or a future builtin) does not slice inside a
                    // codepoint. The pre-fix code used `matched.len()`
                    // and `&matched[..head_end]` byte arithmetic and
                    // panicked on `é`/`日`/emoji at head or tail.
                    let head_end = if rule.head == 0 {
                        0
                    } else {
                        matched
                            .char_indices()
                            .nth(rule.head)
                            .map(|(i, _)| i)
                            .unwrap_or(matched.len())
                    };
                    let tail_start = if rule.tail == 0 {
                        matched.len()
                    } else {
                        let chars = matched.chars().count();
                        if rule.tail >= chars {
                            0
                        } else {
                            matched
                                .char_indices()
                                .nth(chars - rule.tail)
                                .map(|(i, _)| i)
                                .unwrap_or(matched.len())
                        }
                    };
                    if head_end > 0 {
                        result.push_str(&matched[..head_end]);
                    }
                    result.push_str("…[REDACTED:");
                    result.push_str(&rule.name);
                    result.push(']');
                    if rule.tail > 0 && tail_start > head_end {
                        result.push_str(&matched[tail_start..]);
                    }
                } else {
                    result.push_str(&replacement);
                }
                last = m.end();
                count += 1;
            }
            result.push_str(&current[last..]);
            if count > 0 {
                out.push(Redaction {
                    rule: rule.name.clone(),
                    count,
                });
                current = result;
            }
        }
        if self.entropy_scan {
            let (s, extra) = redact_high_entropy(&current, self.entropy_floor);
            current = s;
            out.extend(extra);
        }
        (current, out)
    }

    /// Redact a JSON value in place. Object values and string array
    /// elements are recursed; numbers, bools, nulls are unchanged.
    pub fn redact_json(&self, value: &mut serde_json::Value) -> Vec<Redaction> {
        let mut out = Vec::new();
        fn walk(r: &Redactor, v: &mut serde_json::Value, out: &mut Vec<Redaction>) {
            match v {
                serde_json::Value::String(s) => {
                    let (redacted, events) = r.redact(s);
                    if !events.is_empty() {
                        *s = redacted;
                        out.extend(events);
                    }
                }
                serde_json::Value::Array(a) => {
                    for item in a.iter_mut() {
                        walk(r, item, out);
                    }
                }
                serde_json::Value::Object(o) => {
                    for (_, item) in o.iter_mut() {
                        walk(r, item, out);
                    }
                    // Keys carry content too. A tool argument or a
                    // provider payload can name a secret in a key
                    // (`{"sk-...": "..."}`), and the session log
                    // writes these objects to disk through this
                    // function. Redact every key and rebuild the map
                    // when at least one changed.
                    let mut changed = false;
                    for k in o.keys() {
                        if !r.redact(k).1.is_empty() {
                            changed = true;
                            break;
                        }
                    }
                    if changed {
                        let old = std::mem::take(o);
                        for (k, mut v) in old {
                            let (rk, ev) = r.redact(&k);
                            out.extend(ev);
                            o.insert(rk, std::mem::replace(&mut v, serde_json::Value::Null));
                        }
                    }
                }
                _ => {}
            }
        }
        walk(self, value, &mut out);
        out
    }

    pub fn rule_count(&self) -> usize {
        self.rules.len()
    }
}

/// The built-in rules. Adding a rule is safe; renaming or removing
/// one changes the on-disk schema of historical session logs.
pub fn builtin_rules() -> Vec<RedactRule> {
    fn r(name: &str, pat: &str, head: usize, tail: usize) -> RedactRule {
        RedactRule {
            name: name.to_string(),
            pattern: Regex::new(pat).expect("builtin rule must compile"),
            head,
            tail,
        }
    }
    vec![
        r("openai-key", r"sk-[A-Za-z0-9]{20,}", 0, 0),
        r("anthropic-key", r"sk-ant-[A-Za-z0-9\-_]{40,}", 0, 0),
        r("github-pat", r"gh[ps]_[A-Za-z0-9]{36,}", 0, 0),
        r("github-pat-v2", r"github_pat_[A-Za-z0-9_]{20,}", 0, 0),
        r("aws-access-key", r"AKIA[0-9A-Z]{16}", 4, 4),
        r(
            "aws-secret",
            r"(?i)aws_secret_access_key\s*[=:]\s*[A-Za-z0-9/+=]{40}",
            0,
            0,
        ),
        r(
            "pem-private-key",
            r"-----BEGIN (?:RSA |EC |OPENSSH |DSA |PGP )?PRIVATE KEY-----[\s\S]*?-----END (?:RSA |EC |OPENSSH |DSA |PGP )?PRIVATE KEY-----",
            0,
            0,
        ),
        r(
            "jwt",
            r"eyJ[A-Za-z0-9_\-]{10,}\.[A-Za-z0-9_\-]{10,}\.[A-Za-z0-9_\-]{10,}",
            8,
            8,
        ),
        r("bearer-token", r"(?i)bearer\s+[A-Za-z0-9._\-]{20,}", 0, 0),
        r("slack-token", r"xox[aboprs]-[A-Za-z0-9-]{10,}", 0, 0),
        r("google-api-key", r"AIza[0-9A-Za-z_\-]{35}", 4, 4),
        r("stripe-key", r"sk_(?:live|test)_[A-Za-z0-9]{20,}", 0, 0),
        r(
            "cohere-key",
            r"(?i)cohere[_\-]?api[_\-]?key\s*[=:]\s*[A-Za-z0-9]{30,}",
            0,
            0,
        ),
    ]
}

/// Scan for high-entropy tokens near key-ish keywords. Returns the
/// sanitized text and any events. Kept separate from the rule pass
/// so the regex rules always fire before the heuristic — a value
/// that matches a specific rule is redacted by name, not by entropy.
fn redact_high_entropy(input: &str, floor: f64) -> (String, Vec<Redaction>) {
    if floor <= 0.0 {
        return (input.to_string(), Vec::new());
    }
    static WORD: OnceLock<Regex> = OnceLock::new();
    static KEYWORD: OnceLock<Regex> = OnceLock::new();
    let word = WORD.get_or_init(|| Regex::new(r"[A-Za-z0-9/+_\-]{24,}").unwrap());
    let keyword = KEYWORD.get_or_init(|| {
        Regex::new(r"(?i)key|token|secret|password|passwd|pwd|api[_-]?key|access[_-]?key").unwrap()
    });

    let mut count = 0_usize;
    let mut result = String::with_capacity(input.len());
    let mut last = 0_usize;
    for m in word.find_iter(input) {
        let candidate = m.as_str();
        // Must look like a secret: mixed classes and enough entropy.
        let has_digit = candidate.chars().any(|c| c.is_ascii_digit());
        let has_alpha = candidate.chars().any(|c| c.is_ascii_alphabetic());
        if !(has_digit && has_alpha) {
            continue;
        }
        if shannon_entropy(candidate) < floor {
            continue;
        }
        // Look at 40 chars before for a keyword.
        let before_start = m.start().saturating_sub(40);
        // Byte offset floored to a char boundary: m.start() is a char
        // boundary but start-40 can land inside a multibyte char, and
        // slicing there panicked the always-on session-log redaction.
        let before_start = crate::strutil::floor_char_boundary(input, before_start);
        let before = &input[before_start..m.start()];
        if !keyword.is_match(before) {
            continue;
        }
        result.push_str(&input[last..m.start()]);
        result.push_str("[REDACTED:high-entropy]");
        last = m.end();
        count += 1;
    }
    result.push_str(&input[last..]);
    if count == 0 {
        (input.to_string(), Vec::new())
    } else {
        (
            result,
            vec![Redaction {
                rule: "high-entropy".to_string(),
                count,
            }],
        )
    }
}

/// Shannon entropy of a string, in bits per character.
pub fn shannon_entropy(s: &str) -> f64 {
    if s.is_empty() {
        return 0.0;
    }
    let mut counts = [0_u32; 256];
    for b in s.bytes() {
        counts[b as usize] += 1;
    }
    let len = s.len() as f64;
    let mut h = 0.0_f64;
    for &c in counts.iter() {
        if c == 0 {
            continue;
        }
        let p = c as f64 / len;
        h -= p * p.log2();
    }
    h
}

#[cfg(all(test, not(miri)))]
mod tests {
    use super::*;

    #[test]
    fn openai_key_is_redacted_by_name() {
        let r = Redactor::default();
        let (out, events) = r.redact("export OPENAI_API_KEY=sk-abcdef1234567890ABCDEFGH");
        assert!(out.contains("[REDACTED:openai-key]"));
        assert!(!out.contains("sk-abcdef1234567890ABCDEFGH"));
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].rule, "openai-key");
        assert_eq!(events[0].count, 1);
    }

    #[test]
    fn anthropic_key_is_redacted() {
        let r = Redactor::default();
        let (out, _) =
            r.redact("x-api-key: sk-ant-aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa");
        assert!(out.contains("[REDACTED:anthropic-key]"));
    }

    #[test]
    fn aws_access_key_keeps_head_and_tail() {
        let r = Redactor::default();
        let (out, _) = r.redact("AKIAIOSFODNN7EXAMPLE");
        assert!(out.starts_with("AKIA…"));
        assert!(out.ends_with("MPLE"));
        assert!(out.contains("[REDACTED:aws-access-key]"));
    }

    #[test]
    fn pem_private_key_is_redacted() {
        let r = Redactor::default();
        let pem =
            "-----BEGIN RSA PRIVATE KEY-----\nMIIEowIBAAKCAQEA...\n-----END RSA PRIVATE KEY-----";
        let (out, events) = r.redact(pem);
        assert!(out.contains("[REDACTED:pem-private-key]"));
        assert_eq!(events[0].rule, "pem-private-key");
    }

    #[test]
    fn jwt_is_redacted_with_edges_preserved() {
        let r = Redactor::default();
        let jwt = "eyJhbGciOiJIUzI1NiJ9.eyJzdWIiOiIxMjM0NTY3ODkwIn0.SflKxwRJSMeKKF2QT4fwpMeJf36POk6yJV_adQssw5c";
        let (out, _) = r.redact(jwt);
        assert!(out.contains("[REDACTED:jwt]"));
        // First 8 chars survive.
        assert!(out.starts_with("eyJhbGci"));
    }

    #[test]
    fn clean_text_is_unchanged() {
        let r = Redactor::default();
        let (out, events) = r.redact("fn main() { println!(\"hello\"); }");
        assert_eq!(out, "fn main() { println!(\"hello\"); }");
        assert!(events.is_empty());
    }

    #[test]
    fn multiple_rules_fire_on_a_mixed_payload() {
        let r = Redactor::default();
        let s =
            "OPENAI=sk-abcdef1234567890ABCDEFGH GITHUB=ghp_aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
        let (_, events) = r.redact(s);
        let names: Vec<&str> = events.iter().map(|e| e.rule.as_str()).collect();
        assert!(names.contains(&"openai-key"));
        assert!(names.contains(&"github-pat"));
    }

    #[test]
    fn substitution_text_is_not_rescanned() {
        // A pathological case: after redaction, the replacement text
        // contains the literal "[REDACTED:openai-key]" which itself
        // could look like a token to another rule. It must not be
        // re-redacted.
        let r = Redactor::default();
        let (out, _) = r.redact("sk-abcdef1234567890ABCDEFGH");
        assert_eq!(out, "[REDACTED:openai-key]");
    }

    #[test]
    fn high_entropy_near_keyword_is_redacted() {
        let r = Redactor::default();
        // 32-char mixed alnum, near the word "token".
        let secret = "aB3xK9mNpQ2rS7tUvW4yZ6hJ8kL5nM1P";
        let (out, events) = r.redact(&format!("access_token={secret}"));
        assert!(out.contains("[REDACTED:high-entropy]"));
        assert!(events.iter().any(|e| e.rule == "high-entropy"));
    }

    #[test]
    fn high_entropy_far_from_keyword_is_kept() {
        let r = Redactor::default();
        // A long identifier that is not near a keyword.
        let s = "let very_long_function_name_here = compute_something();";
        let (out, _) = r.redact(s);
        assert_eq!(out, s);
    }

    #[test]
    fn pure_lowercase_word_is_not_high_entropy() {
        // No digit → skipped by the heuristic.
        let r = Redactor::default();
        let s = "the token is entirely lowercase without any digits";
        let (out, _) = r.redact(s);
        assert_eq!(out, s);
    }

    #[test]
    fn entropy_helper_is_correct_on_known_inputs() {
        assert!((shannon_entropy("") - 0.0).abs() < 1e-9);
        assert!((shannon_entropy("aaaa") - 0.0).abs() < 1e-9);
        assert!((shannon_entropy("ab") - 1.0).abs() < 1e-9);
        // 4 distinct chars, uniform → 2.0 bits.
        assert!((shannon_entropy("abcd") - 2.0).abs() < 1e-9);
    }

    #[test]
    fn json_redaction_walks_nested_structures() {
        let r = Redactor::default();
        let mut v = serde_json::json!({
            "outer": {
                "inner": "key=sk-abcdef1234567890ABCDEFGH",
                "list": ["clean", "AKIAIOSFODNN7EXAMPLE"],
            },
            "count": 42,
            "flag": true,
        });
        let events = r.redact_json(&mut v);
        assert!(
            v["outer"]["inner"]
                .as_str()
                .unwrap()
                .contains("[REDACTED:openai-key]")
        );
        assert!(
            v["outer"]["list"][1]
                .as_str()
                .unwrap()
                .contains("[REDACTED:aws-access-key]")
        );
        assert_eq!(v["count"], 42);
        assert_eq!(v["flag"], true);
        assert!(events.len() >= 2);
    }

    /// A rule's `head` / `tail` are CHARACTER counts. The pre-fix
    /// code sliced by byte offset, so a non-ASCII match with head or
    /// tail > 0 panicked inside `&matched[..head_end]`.
    #[test]
    fn custom_rule_head_tail_count_chars_not_bytes() {
        // 1 head + 1 tail on a 3-char all-multibyte match.
        let r = Redactor::with_rules(vec![RedactRule {
            name: "custom".into(),
            pattern: Regex::new(r"ééé").unwrap(),
            head: 1,
            tail: 1,
        }]);
        let (out, events) = r.redact("ééé");
        assert!(events.iter().any(|e| e.rule == "custom"), "got {events:?}");
        // Exactly one 'é' kept on each side.
        assert!(out.starts_with('é'), "head kept wrong: {out:?}");
        assert!(out.ends_with('é'), "tail kept wrong: {out:?}");
        assert!(out.contains("[REDACTED:custom]"));
    }

    /// Same shape with CJK and an emoji: bytes-per-char vary (3 and
    /// 4), so a byte-vs-char bug would surface differently.
    #[test]
    fn custom_rule_head_tail_on_cjk_and_emoji() {
        for sample in ["日本語", "🔒🔑🗝"] {
            let r = Redactor::with_rules(vec![RedactRule {
                name: "custom".into(),
                pattern: Regex::new(&regex::escape(sample)).unwrap(),
                head: 1,
                tail: 1,
            }]);
            let (out, _) = r.redact(sample);
            assert!(
                out.contains("[REDACTED:custom]"),
                "sample {sample:?}: {out:?}"
            );
            // A leading and trailing char survive (the first and last
            // char of the sample, exact byte shape depends on encoding).
            assert!(out.starts_with(sample.chars().next().unwrap()));
            assert!(out.ends_with(sample.chars().last().unwrap()));
        }
    }

    /// head + tail >= matched char-count: the guard `tail_start >
    /// head_end` must short-circuit to "head only, no tail".
    #[test]
    fn custom_rule_head_tail_exceed_match_length() {
        let r = Redactor::with_rules(vec![RedactRule {
            name: "custom".into(),
            pattern: Regex::new(r"é").unwrap(),
            head: 4,
            tail: 4,
        }]);
        let (out, _) = r.redact("é");
        assert!(out.contains("[REDACTED:custom]"), "got {out:?}");
        // Only one 'é' total — not duplicated or re-inserted.
        assert_eq!(out.matches('é').count(), 1, "got {out:?}");
    }

    #[test]
    fn custom_rules_only_run_the_given_rules() {
        let rule = RedactRule {
            name: "custom".to_string(),
            pattern: Regex::new(r"XYZ-\d+").unwrap(),
            head: 0,
            tail: 0,
        };
        let r = Redactor::with_rules(vec![rule]);
        let (out, events) = r.redact("XYZ-42 and sk-abcdef1234567890ABCDEFGH");
        assert!(out.contains("[REDACTED:custom]"));
        // The openai key survives because we only supplied the
        // custom rule and the entropy scan is off by default here.
        assert!(out.contains("sk-abcdef1234567890ABCDEFGH"));
        assert_eq!(events[0].rule, "custom");
    }
}

#[cfg(test)]
mod redact_proptests {
    //! Properties for `Redactor::redact`.
    //!
    //! The existing tests in `tests/it/string_safety.rs` pin the
    //! head/tail saturation and non-ASCII cases. What they don't
    //! pin is what happens when *multiple rules* interact:
    //!
    //! * The doc says "A rule cannot fire on a substring of an
    //!   earlier substitution" — but nothing in the body enforces
    //!   it. The substitution text is `[REDACTED:<name>]`; if a
    //!   later rule's pattern happens to match part of that, it
    //!   fires again. That would be silent (the caller sees a
    //!   mangled marker).
    //! * Redaction is applied rule-by-rule on the *output* of the
    //!   previous rule. Whether that's idempotent overall — redact
    //!   once, redact again, nothing changes — is a property of
    //!   the whole rule set, not of any single rule.
    //!
    //! **No bug expected for the built-in rule set.** The built-in
    //! rule names are short (`openai-key`, `aws-access-key`,
    //! `pem-private-key`) and no built-in pattern matches
    //! `[REDACTED:` or a short name. A *custom* rule with a long
    //! name could break idempotence; the properties pin idempotence
    //! for the default set, which is what ships.

    use super::*;
    use proptest::prelude::*;

    /// ASCII text with no built-in secret shape: lowercase letters
    /// and spaces only, bounded to 20 chars. No `sk-`, no `AKIA`,
    /// no PEM header, no 24+ alnum run (so the entropy rule can't
    /// fire either).
    fn safe_text() -> impl Strategy<Value = String> {
        "[a-z ]{0,20}"
    }

    /// A custom rule pattern that can't appear in `safe_text`.
    fn custom_rule() -> RedactRule {
        RedactRule {
            name: "custom".to_string(),
            pattern: Regex::new(r"XYZ-[0-9]+").unwrap(),
            head: 0,
            tail: 0,
        }
    }

    // ---- idempotence: the sharp property --------------------------

    proptest! {
        /// Redacting an already-redacted string is a no-op.
        ///
        /// If this fails, a rule fired on the output of an earlier
        /// rule — either it matched a substring of `[REDACTED:name]`
        /// or it re-matched text the first pass should have
        /// eliminated. Either is a real defect: a caller that
        /// redacts twice (log-write and prompt-build both call
        /// `redact`, per the module doc) would see different output
        /// depending on how many times it happened to call.
        #[test]
        fn prop_idempotent_default(input in "\\PC{0,200}") {
            let r = Redactor::default();
            let (out1, _ev1) = r.redact(&input);
            let (out2, ev2) = r.redact(&out1);
            prop_assert_eq!(&out1, &out2, "redact not idempotent on {:?}", input);
            prop_assert!(
                ev2.is_empty(),
                "second redact of {:?} fired {} events: {:?}",
                input, ev2.len(), ev2,
            );
        }

        /// No nested markers in the default-rule output. A
        /// `[REDACTED:[REDACTED:` substring means a rule fired on a
        /// marker.
        #[test]
        fn prop_no_nested_markers_default(input in "\\PC{0,200}") {
            let r = Redactor::default();
            let (out, _) = r.redact(&input);
            prop_assert!(
                !out.contains("[REDACTED:[REDACTED:"),
                "nested markers in {:?} for input {:?}", out, input,
            );
        }

        /// Every `[REDACTED:` in the output is closed by a `]`
        /// within a reasonable distance (a rule name is short). A
        /// marker that runs to the end of the string with no `]`
        /// means a subsequent rule ate the closing bracket.
        #[test]
        fn prop_markers_are_well_formed_default(input in "\\PC{0,200}") {
            let r = Redactor::default();
            let (out, _) = r.redact(&input);
            let mut rest = out.as_str();
            while let Some(pos) = rest.find("[REDACTED:") {
                let after = &rest[pos + "[REDACTED:".len()..];
                let close = after.find(']');
                prop_assert!(
                    close.is_some(),
                    "unclosed marker in {:?} for input {:?}", out, input,
                );
                let name_len = close.unwrap();
                prop_assert!(
                    name_len <= 64,
                    "marker name {} chars long in {:?}", name_len, out,
                );
                rest = &after[name_len + 1..];
            }
        }
    }

    // ---- identity on non-secret text ------------------------------

    proptest! {
        /// Text that contains no secret shape is returned unchanged
        /// by the default rule set. The `safe_text` generator is
        /// lowercase letters + spaces, max 20 chars — no key prefix,
        /// no PEM, no long alnum run, no keyword.
        #[test]
        fn prop_identity_on_safe_text(input in safe_text()) {
            let r = Redactor::default();
            let (out, events) = r.redact(&input);
            prop_assert_eq!(&out, &input, "non-secret input {:?} was modified", input);
            prop_assert!(events.is_empty(), "non-secret input fired {:?}", events);
        }
    }

    // ---- custom-rule round trips ---------------------------------

    proptest! {
        /// A custom rule with `head == 0 && tail == 0` replaces each
        /// match with `[REDACTED:<name>]` and leaves the surrounding
        /// text untouched.
        #[test]
        fn prop_custom_rule_preserves_surroundings(
            before in "[a-z]{0,20}",
            n in 0u32..1_000_000u32,
            after in "[a-z]{0,20}",
        ) {
            let r = Redactor::with_rules(vec![custom_rule()]);
            let needle = format!("XYZ-{n}");
            let input = format!("{before}{needle}{after}");
            let (out, _) = r.redact(&input);
            let expected = format!("{before}[REDACTED:custom]{after}");
            prop_assert_eq!(out, expected, "input {:?}", input);
        }

        /// N distinct custom matches in one input yield N markers.
        #[test]
        fn prop_custom_rule_count(
            nums in prop::collection::vec(0u32..1_000_000u32, 1..=8),
            sep in "[-_ ,]{1,3}",
        ) {
            let r = Redactor::with_rules(vec![custom_rule()]);
            let parts: Vec<String> = nums.iter().map(|n| format!("XYZ-{n}")).collect();
            let input = parts.join(&sep);
            let (out, events) = r.redact(&input);
            prop_assert_eq!(
                out.matches("[REDACTED:custom]").count(),
                nums.len(),
                "input {:?} -> {:?}", input, out,
            );
            prop_assert_eq!(events.len(), 1, "one rule fired, {:?}", events);
            prop_assert_eq!(events[0].count, nums.len());
        }
    }

    // ---- controls -------------------------------------------------

    /// Control: empty input, empty output, no events.
    #[test]
    fn control_empty_input() {
        let r = Redactor::default();
        let (out, events) = r.redact("");
        assert_eq!(out, "");
        assert!(events.is_empty());
    }

    /// Control: whitespace-only input is unchanged.
    #[test]
    fn control_whitespace_input() {
        let r = Redactor::default();
        let (out, events) = r.redact("   \n\t  ");
        assert_eq!(out, "   \n\t  ");
        assert!(events.is_empty());
    }

    /// Control: a custom rule with `head = 4, tail = 4` preserves
    /// the first and last 4 chars of the match. This pins the
    /// head/tail contract independent of the built-in rules.
    #[test]
    fn control_custom_head_tail() {
        let rule = RedactRule {
            name: "ht".to_string(),
            pattern: Regex::new(r"ABCDEFGHIJ").unwrap(),
            head: 4,
            tail: 4,
        };
        let r = Redactor::with_rules(vec![rule]);
        let (out, _) = r.redact("xxABCDEFGHIJyy");
        assert_eq!(out, "xxABCD…[REDACTED:ht]GHIJyy");
    }
    /// A secret used as an object *key* must be redacted too.
    ///
    /// `redact_json` walked object values and recursed through arrays,
    /// but never looked at keys. The session log writes tool arguments
    /// and tool results to disk through this function, so a payload
    /// shaped `{"sk-...": "..."}` persisted the key in the clear while
    /// the value beside it was redacted.
    #[test]
    fn a_secret_in_an_object_key_is_redacted() {
        let r = Redactor::default();
        let mut v = serde_json::json!({
            "sk-abcdef0123456789abcdef0123456789": "ordinary value",
        });
        let events = r.redact_json(&mut v);
        assert!(!events.is_empty(), "the key must produce a redaction");
        let s = v.to_string();
        assert!(
            !s.contains("sk-abcdef0123456789"),
            "the key must be redacted; got {s}",
        );
        assert!(
            s.contains("ordinary value"),
            "the value must survive; got {s}",
        );
    }

    /// The key fix must not disturb an object with no secret in it.
    #[test]
    fn ordinary_keys_are_unchanged() {
        let r = Redactor::default();
        let mut v = serde_json::json!({"path": "/tmp/x", "line": 3});
        let before = v.clone();
        let events = r.redact_json(&mut v);
        assert!(events.is_empty());
        assert_eq!(v, before);
    }
}
