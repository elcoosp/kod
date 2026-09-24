//! Where registered secrets come from (delta §14.1, part 2).
//!
//! Three sources, checked in order of cheapest-first:
//!
//! 1. **Environment variables** matching a name heuristic
//!    (`(*KEY|SECRET|TOKEN|PASSWORD|PASS|AUTH|CREDENTIAL|PRIVATE|
//!    OAUTH)(_|$)`) whose value is at least
//!    [`MIN_ENV_VALUE_LENGTH`] characters. The name pattern is the
//!    same one the docs' heuristic names; the length floor rejects
//!    a variable like `TOKEN=1` that is a flag, not a secret.
//!
//! 2. **Vendor-shaped values** matched by a small set of regexes:
//!    `sk-ant-…`, `ghp_…`, `github_pat_…`, `xox[baprs]-…`,
//!    `AKIA…`/`ASIA…`, JWT (three base64url segments), PEM blocks,
//!    etc. Each regex carries a `literalPrefixes` list; the scanner
//!    checks those substrings first and skips the regex work when
//!    none is present. That is what makes scanning a large
//!    transcript cheap — most text has none of these prefixes.
//!
//! 3. **Connection URLs** with an embedded password:
//!    `scheme://user:password@host`. The password is the secret;
//!    the rest of the URL is not.
//!
//! # What this does NOT do
//!
//! * **Not a full secret detector.** A secret in a shape not covered
//!   by a vendor regex, with no env var declaring it, and not in a
//!   connection URL is not found. The design's own examples are the
//!   ones this module covers; anything else requires the caller to
//!   register the value directly.
//! * **Not a source of load.** The env scan is a single pass over
//!   `std::env::vars()`; the vendor scan runs over a caller-supplied
//!   text and short-circuits on the literal-prefix check. Nothing
//!   here does I/O beyond reading the environment.
//! * **Not a config-file reader.** A `secrets.yml`-style file is
//!   the caller's concern; the caller reads it and passes the
//!   `(name, value)` pairs in. This keeps the module
//!   dependency-free of a config format.

use crate::secret_placeholder::{SecretSource, SecretVault};
use regex::Regex;
use std::sync::OnceLock;

/// The shortest env-var value the heuristic accepts. Below this, a
/// variable named `TOKEN` is more likely a mode flag (`TOKEN=1`) or
/// a sentinel than a secret.
pub const MIN_ENV_VALUE_LENGTH: usize = 8;

/// Substring patterns in the env-var name. Any match, at a word
/// boundary (start, `_`, or end), makes the variable a candidate.
const NAME_KEYWORDS: &[&str] = &[
    "KEY",
    "SECRET",
    "TOKEN",
    "PASSWORD",
    "PASS",
    "AUTH",
    "CREDENTIAL",
    "PRIVATE",
    "OAUTH",
];

/// A vendor pattern. `literal_prefixes` short-circuits the regex —
/// if none is present in the text, the regex cannot match and the
/// scan skips it.
pub struct VendorPattern {
    pub name: &'static str,
    pub regex: Regex,
    pub literal_prefixes: &'static [&'static str],
}

/// The built-in vendor patterns. Ordered roughly by how often the
/// values appear in real transcripts.
pub fn vendor_patterns() -> &'static [VendorPattern] {
    static PATTERNS: OnceLock<Vec<VendorPattern>> = OnceLock::new();
    PATTERNS.get_or_init(|| {
        vec![
            pattern("anthropic", r"sk-ant-[A-Za-z0-9_\-]{20,}", &["sk-ant-"]),
            // An Anthropic key (`sk-ant-...`) matches the plain
            // `sk-...` shape too, so without a guard one value would
            // be reported as both an OpenAI and an Anthropic secret.
            //
            // `regex` has no lookahead, so the guard is a
            // character-class exclusion of the three ways the next
            // bytes could spell `ant-`: the first character is not
            // `a`, or it is `a` followed by not-`n`, or `an` followed
            // by not-`t`. Every other `sk-`-prefixed value matches,
            // which is what the plain OpenAI shape wants.
            pattern(
                "openai",
                r"sk-(?:[^a]|a[^n]|an[^t])[A-Za-z0-9_\-]{20,}",
                &["sk-"],
            ),
            pattern(
                "github-pat",
                r"github_pat_[A-Za-z0-9_]{20,}",
                &["github_pat_"],
            ),
            pattern(
                "github-token",
                r"gh[posur]_[A-Za-z0-9]{36,}",
                &["ghp_", "gho_", "ghs_", "ghu_", "ghr_"],
            ),
            pattern(
                "slack",
                r"xox[baprs]-[A-Za-z0-9\-]{10,}",
                &["xoxb-", "xoxa-", "xoxp-", "xoxr-", "xoxs-"],
            ),
            pattern(
                "aws-access-key",
                r"(?:AKIA|ASIA)[0-9A-Z]{16}",
                &["AKIA", "ASIA"],
            ),
            pattern("google-api", r"AIza[0-9A-Za-z_\-]{35}", &["AIza"]),
            pattern(
                "npm",
                r"npm_[A-Za-z0-9]{36,}",
                &["npm_"],
            ),
            pattern(
                "pypi",
                r"pypi-[A-Za-z0-9_\-]{50,}",
                &["pypi-"],
            ),
            pattern(
                "stripe",
                r"sk_(?:live|test)_[A-Za-z0-9]{24,}",
                &["sk_live_", "sk_test_"],
            ),
            // JWT: three base64url segments separated by dots. The
            // first segment is a base64-encoded header that always
            // starts with `eyJ` (`{"`), which is the cheap prefix.
            pattern(
                "jwt",
                r"eyJ[A-Za-z0-9_\-]+\.eyJ[A-Za-z0-9_\-]+\.[A-Za-z0-9_\-]+",
                &["eyJ"],
            ),
            // PEM: a whole key block. The `--` disambiguation keeps
            // the pattern from matching the word "BEGIN" in prose.
            pattern(
                "pem-private-key",
                r"-----BEGIN [A-Z ]*PRIVATE KEY-----[\s\S]*?-----END [A-Z ]*PRIVATE KEY-----",
                &["-----BEGIN", "PRIVATE KEY"],
            ),
        ]
    })
}

fn pattern(name: &'static str, regex: &str, literal_prefixes: &'static [&'static str]) -> VendorPattern {
    VendorPattern {
        name,
        regex: Regex::new(regex).unwrap_or_else(|e| panic!("vendor regex {name} fails: {e}")),
        literal_prefixes,
    }
}

/// A discovered secret value + where it came from.
#[derive(Debug, Clone)]
pub struct Discovered {
    pub value: String,
    pub source: SecretSource,
    /// For a vendor match, the pattern name (`"anthropic"`). For an
    /// env var, the variable name. For a connection URL, the scheme.
    pub origin: String,
}

/// Scan the process environment for secret-shaped variables.
///
/// The `getenv`-style accessor is a caller-supplied closure so a
/// test can supply a fixed map without touching the process's real
/// environment (which is `unsafe` to mutate under threads).
pub fn scan_env<F>(iter: F) -> Vec<Discovered>
where
    F: FnOnce() -> Vec<(String, String)>,
{
    let mut out = Vec::new();
    for (name, value) in iter() {
        if value.len() < MIN_ENV_VALUE_LENGTH {
            continue;
        }
        if !name_is_secret_shaped(&name) {
            continue;
        }
        out.push(Discovered {
            value,
            source: SecretSource::Env,
            origin: name,
        });
    }
    out
}

/// True when `name` matches the env-var heuristic: a keyword at a
/// word boundary (`start`, `_`, or `end`).
fn name_is_secret_shaped(name: &str) -> bool {
    let upper = name.to_ascii_uppercase();
    for kw in NAME_KEYWORDS {
        // Word-boundary check: kw must be preceded by start or `_`
        // and followed by end or `_`.
        let mut from = 0;
        while let Some(pos) = upper[from..].find(kw) {
            let abs = from + pos;
            let before_ok = abs == 0 || upper.as_bytes()[abs - 1] == b'_';
            let after = abs + kw.len();
            let after_ok =
                after == upper.len() || upper.as_bytes()[after] == b'_';
            if before_ok && after_ok {
                return true;
            }
            from = abs + 1;
        }
    }
    false
}

/// Scan `text` for vendor-shaped secret values.
///
/// The literal-prefix check runs before each regex; text that
/// contains none of a pattern's prefixes cannot match, so the
/// expensive compile-and-run is skipped. On a normal transcript this
/// is a couple of `str::contains` calls per pattern.
pub fn scan_vendors(text: &str) -> Vec<Discovered> {
    let mut out = Vec::new();
    for p in vendor_patterns() {
        if !p.literal_prefixes.iter().any(|pre| text.contains(pre)) {
            continue;
        }
        for m in p.regex.find_iter(text) {
            out.push(Discovered {
                value: m.as_str().to_string(),
                source: SecretSource::Vendor,
                origin: p.name.to_string(),
            });
        }
    }
    out
}

/// Scan `text` for connection URLs with an embedded password.
///
/// The pattern matches `scheme://user:password@host`. The password
/// is the secret; the rest of the URL is not. A URL with no password
/// (`scheme://user@host`) does not match.
pub fn scan_connection_urls(text: &str) -> Vec<Discovered> {
    static RE: OnceLock<Regex> = OnceLock::new();
    let re = RE.get_or_init(|| {
        // Scheme: RFC 3986's `scheme = ALPHA *( ALPHA / DIGIT / "+"
        // / "-" / "." )`. Credentials: `user:password` — the
        // password is the capture. Host: any non-slash character.
        Regex::new(r"(?P<scheme>[A-Za-z][A-Za-z0-9+.\-]*)://[^:/\s@]+:(?P<pw>[^@\s/]+)@")
            .expect("connection-URL regex compiles")
    });
    re.captures_iter(text)
        .filter_map(|c| {
            let pw = c.name("pw")?.as_str();
            if pw.len() < MIN_ENV_VALUE_LENGTH {
                return None;
            }
            let scheme = c.name("scheme")?.as_str().to_string();
            Some(Discovered {
                value: pw.to_string(),
                source: SecretSource::ConnectionUrl,
                origin: scheme,
            })
        })
        .collect()
}

/// Scan a text through both the vendor and connection-URL sources,
/// deduplicating by value. The vendor source wins on a tie — it
/// carries a more specific origin.
pub fn scan_text(text: &str) -> Vec<Discovered> {
    let mut seen = std::collections::HashSet::new();
    let mut out = Vec::new();
    for d in scan_vendors(text) {
        if seen.insert(d.value.clone()) {
            out.push(d);
        }
    }
    for d in scan_connection_urls(text) {
        if seen.insert(d.value.clone()) {
            out.push(d);
        }
    }
    out
}

/// Register every discovered secret with `vault`. Returns the count
/// of *newly* registered secrets (a value already in the vault is
/// not re-counted).
pub fn register_discovered(vault: &SecretVault, discovered: Vec<Discovered>) -> usize {
    let pairs: Vec<(String, SecretSource)> = discovered
        .into_iter()
        .map(|d| (d.value, d.source))
        .collect();
    vault.register_all(pairs).len()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env(pairs: &[(&str, &str)]) -> Vec<(String, String)> {
        pairs.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect()
    }

    // ---- env scanner -------------------------------------------------

    #[test]
    fn a_keyword_name_with_a_long_value_is_found() {
        let d = scan_env(|| env(&[("OPENAI_API_KEY", "sk-abcdefghij")]));
        assert_eq!(d.len(), 1);
        assert_eq!(d[0].value, "sk-abcdefghij");
        assert_eq!(d[0].source, SecretSource::Env);
        assert_eq!(d[0].origin, "OPENAI_API_KEY");
    }

    #[test]
    fn a_short_value_is_skipped() {
        // Below MIN_ENV_VALUE_LENGTH.
        let d = scan_env(|| env(&[("TOKEN", "1")]));
        assert!(d.is_empty(), "{d:?}");
    }

    #[test]
    fn a_non_keyword_name_is_skipped() {
        let d = scan_env(|| env(&[("PATH", "/usr/local/bin:/usr/bin")]));
        assert!(d.is_empty());
    }

    #[test]
    fn a_keyword_must_be_at_a_word_boundary() {
        // `MONKEY` contains `KEY` but not at a boundary.
        let d = scan_env(|| env(&[("MONKEY", "abcdefghijklm")]));
        assert!(d.is_empty(), "{d:?}");
    }

    #[test]
    fn a_keyword_at_the_end_of_the_name_matches() {
        let d = scan_env(|| env(&[("MY_TOKEN", "abcdefghij")]));
        assert_eq!(d.len(), 1);
    }

    #[test]
    fn a_keyword_at_the_start_of_the_name_matches() {
        let d = scan_env(|| env(&[("KEY_FOR_THING", "abcdefghij")]));
        assert_eq!(d.len(), 1);
    }

    #[test]
    fn all_the_keywords_are_recognized() {
        for kw in NAME_KEYWORDS {
            let name = format!("MY_{kw}");
            let d = scan_env(|| env(&[(&name, "abcdefghij")]));
            assert_eq!(d.len(), 1, "keyword {kw} did not match name {name}");
        }
    }

    // ---- vendor scanner ---------------------------------------------

    #[test]
    fn an_anthropic_key_is_found() {
        let d = scan_vendors("the key is sk-ant-api03-abcdefghijklmnopqrstuv");
        assert_eq!(d.len(), 1);
        assert_eq!(d[0].origin, "anthropic");
    }

    #[test]
    fn an_aws_access_key_is_found() {
        let d = scan_vendors("AKIAIOSFODNN7EXAMPLE");
        assert_eq!(d.len(), 1);
        assert_eq!(d[0].origin, "aws-access-key");
    }

    #[test]
    fn a_github_token_is_found() {
        let d = scan_vendors("ghp_aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa");
        assert_eq!(d.len(), 1);
        assert_eq!(d[0].origin, "github-token");
    }

    #[test]
    fn a_jwt_is_found() {
        let jwt = "eyJhbGciOiJIUzI1NiJ9.eyJzdWIiOiIxMjM0NTY3ODkwIn0.\
                   abc123_-def";
        let d = scan_vendors(jwt);
        assert_eq!(d.len(), 1);
        assert_eq!(d[0].origin, "jwt");
    }

    #[test]
    fn a_pem_private_key_is_found() {
        let pem = "-----BEGIN RSA PRIVATE KEY-----\nMIIE...\n\
                   -----END RSA PRIVATE KEY-----";
        let d = scan_vendors(pem);
        assert_eq!(d.len(), 1);
        assert_eq!(d[0].origin, "pem-private-key");
    }

    #[test]
    fn a_short_openai_like_string_is_not_matched() {
        // `sk-short` fails the 20-char floor.
        let d = scan_vendors("sk-short");
        assert!(d.is_empty(), "{d:?}");
    }

    #[test]
    fn an_anthropic_key_is_not_also_reported_as_openai() {
        // The anthropic and openai patterns overlap on the `sk-`
        // prefix. The openai pattern's negative lookahead (`(?!ant-)`)
        // makes them disjoint, so an Anthropic key produces exactly
        // one match.
        let d = scan_vendors("sk-ant-api03-abcdefghijklmnopqrstuv");
        assert_eq!(d.len(), 1, "expected one match, got {d:?}");
        assert_eq!(d[0].origin, "anthropic");
    }

    #[test]
    fn a_plain_openai_key_is_still_matched() {
        // The guard only excludes `sk-ant-`; a plain `sk-...` key
        // still matches.
        let d = scan_vendors("sk-proj-abcdefghijklmnopqrstuvwx");
        assert_eq!(d.len(), 1, "expected one match, got {d:?}");
        assert_eq!(d[0].origin, "openai");
    }

    #[test]
    fn text_with_no_prefixes_skips_the_regex_work() {
        // No literal prefix present → the regex is never run. The
        // assertion is that the result is empty; the *perf* is that
        // the function is O(n) `contains` calls over the pattern set.
        let d = scan_vendors("just some ordinary prose about tokens");
        assert!(d.is_empty());
    }

    #[test]
    fn multiple_vendor_secrets_in_one_text_are_all_found() {
        let text = "keys: AKIAIOSFODNN7EXAMPLE and ghp_\
                    aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
        let d = scan_vendors(text);
        assert_eq!(d.len(), 2);
        let origins: std::collections::HashSet<_> =
            d.iter().map(|x| x.origin.clone()).collect();
        assert!(origins.contains("aws-access-key"));
        assert!(origins.contains("github-token"));
    }

    // ---- connection-URL scanner -------------------------------------

    #[test]
    fn a_postgres_url_with_password_is_found() {
        let d = scan_connection_urls(
            "DATABASE_URL=postgres://user:supersecret@localhost:5432/db",
        );
        assert_eq!(d.len(), 1);
        assert_eq!(d[0].value, "supersecret");
        assert_eq!(d[0].origin, "postgres");
    }

    #[test]
    fn a_url_with_no_password_is_skipped() {
        let d = scan_connection_urls("postgres://user@localhost/db");
        assert!(d.is_empty());
    }

    #[test]
    fn a_short_password_is_skipped() {
        let d = scan_connection_urls("postgres://user:short@host/db");
        assert!(d.is_empty());
    }

    #[test]
    fn multiple_urls_are_all_found() {
        let text = "a postgres://u:secretone@h/d and redis://u:secrettwo@h";
        let d = scan_connection_urls(text);
        assert_eq!(d.len(), 2);
    }

    // ---- combined scanner --------------------------------------------

    #[test]
    fn scan_text_dedupes_vendor_and_url_matches() {
        // A text containing the same secret as both a vendor match
        // and a URL password is not double-registered.
        let text = "url: postgres://u:AAAAAAAAAAAAAAAAAAAA@h \
                    and sk-ant-api03-AAAAAAAAAAAAAAAAAAAA";
        let d = scan_text(text);
        // Two distinct values (the URL password, the anthropic key).
        assert_eq!(d.len(), 2);
    }

    #[test]
    fn register_discovered_registers_everything() {
        let v = SecretVault::with_key([1u8; 32]);
        let d = scan_text("key sk-ant-api03-abcdefghijklmnopqrstuv");
        let n = register_discovered(&v, d);
        assert_eq!(n, 1);
        assert_eq!(v.len(), 1);
    }

    #[test]
    fn register_discovered_reports_only_new() {
        let v = SecretVault::with_key([1u8; 32]);
        let text = "key sk-ant-api03-abcdefghijklmnopqrstuv";
        register_discovered(&v, scan_text(text));
        let second = register_discovered(&v, scan_text(text));
        assert_eq!(second, 0, "the same secret is not re-counted");
    }

    // ---- name_is_secret_shaped ---------------------------------------

    #[test]
    fn name_shapes_are_classified() {
        assert!(name_is_secret_shaped("OPENAI_API_KEY"));
        assert!(name_is_secret_shaped("MY_TOKEN"));
        assert!(name_is_secret_shaped("PASSWORD"));
        assert!(name_is_secret_shaped("AWS_SECRET"));
        assert!(!name_is_secret_shaped("PATH"));
        assert!(!name_is_secret_shaped("HOME"));
        assert!(!name_is_secret_shaped("MONKEY"));
    }
}
