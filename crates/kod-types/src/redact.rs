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
    pub fn with_builtin_rules() -> Self {
        Self {
            rules: std::sync::Arc::new(builtin_rules()),
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
                    let head_end = rule.head.min(matched.len());
                    let tail_start = matched.len().saturating_sub(rule.tail);
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
        fn walk(
            r: &Redactor,
            v: &mut serde_json::Value,
            out: &mut Vec<Redaction>,
        ) {
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
        r(
            "slack-token",
            r"xox[aboprs]-[A-Za-z0-9-]{10,}",
            0,
            0,
        ),
        r(
            "google-api-key",
            r"AIza[0-9A-Za-z_\-]{35}",
            4,
            4,
        ),
        r(
            "stripe-key",
            r"sk_(?:live|test)_[A-Za-z0-9]{20,}",
            0,
            0,
        ),
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

#[cfg(test)]
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
        let (out, _) = r.redact("x-api-key: sk-ant-aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa");
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
        let pem = "-----BEGIN RSA PRIVATE KEY-----\nMIIEowIBAAKCAQEA...\n-----END RSA PRIVATE KEY-----";
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
        let s = "OPENAI=sk-abcdef1234567890ABCDEFGH GITHUB=ghp_aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
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
        assert!(v["outer"]["inner"].as_str().unwrap().contains("[REDACTED:openai-key]"));
        assert!(v["outer"]["list"][1].as_str().unwrap().contains("[REDACTED:aws-access-key]"));
        assert_eq!(v["count"], 42);
        assert_eq!(v["flag"], true);
        assert!(events.len() >= 2);
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
