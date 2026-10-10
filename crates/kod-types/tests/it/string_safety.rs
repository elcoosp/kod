//! Property tests for the workspace's string helpers. The
//! hand-enumerated unit tests pin specific inputs; these exercise
//! generated inputs so a new failure mode on a shape the enumerated
//! list does not cover is caught by the same commit that introduces
//! it.
//!
//! `proptest` is used to generate UTF-8 strings — including CJK,
//! combining marks, emoji, and the ASCII cases — and the helpers are
//! asserted to never panic and to preserve their documented
//! invariants on every input.

use kod_types::redact::{RedactRule, Redactor};
use kod_types::strutil::{floor_char_boundary, truncate_chars};
use proptest::prelude::*;

/// A strategy producing arbitrary `String` values. The proptest
/// default of "any char" is fine — the exercises below are as much
/// about the byte/char boundary interaction as about the specific
/// code points.
fn any_string() -> impl Strategy<Value = String> {
    proptest::collection::vec(any::<char>(), 0..64).prop_map(|v| v.into_iter().collect())
}

proptest! {
    /// `floor_char_boundary(s, n)` never returns a value that is not
    /// a char boundary, and never exceeds `n` or `s.len()`.
    #[test]
    fn floor_char_boundary_always_returns_a_boundary(s in any_string(), n in 0usize..256) {
        let i = floor_char_boundary(&s, n);
        prop_assert!(i <= n.min(s.len()));
        prop_assert!(s.is_char_boundary(i));
    }

    /// `truncate_chars(s, n)` never panics and returns a slice of the
    /// input that is a valid UTF-8 string (it is a `&str` by type, so
    /// this is the compiler's guarantee — the property is that the
    /// call does not panic and the slice is at most `n` bytes long).
    #[test]
    fn truncate_chars_never_panics_and_is_bounded(s in any_string(), n in 0usize..256) {
        let out = truncate_chars(&s, n);
        prop_assert!(out.len() <= n.min(s.len()));
        prop_assert!(s.starts_with(out));
    }

    /// A redactor with a custom rule whose pattern matches any
    /// non-empty substring must not panic on any input. Pre-fix,
    /// `head`/`tail` counted bytes and the code sliced at those byte
    /// offsets; a match containing a multibyte character and a
    /// non-zero head or tail panicked at `&matched[..head_end]`.
    #[test]
    fn redactor_with_nonascii_custom_rule_never_panics(s in any_string()) {
        // Use a rule whose pattern is the literal string `s` — it
        // matches the whole input, so head/tail slicing runs on the
        // full (possibly multibyte) match.
        let rule = RedactRule {
            name: "test".to_string(),
            pattern: regex::Regex::new(&regex::escape(&s)).unwrap(),
            head: 1,
            tail: 1,
        };
        let r = Redactor::with_rules(vec![rule]);
        let (_out, _events) = r.redact(&s);
    }

    /// A redactor with `head`/`tail` larger than the match length
    /// must not panic and must not duplicate characters.
    #[test]
    fn redactor_saturates_head_tail_on_short_matches(c in any::<char>()) {
        // A one-character match with head=4, tail=4.
        let rule = RedactRule {
            name: "short".to_string(),
            pattern: regex::Regex::new(&regex::escape(&c.to_string())).unwrap(),
            head: 4,
            tail: 4,
        };
        let r = Redactor::with_rules(vec![rule]);
        let input = c.to_string();
        let (out, _) = r.redact(&input);
        // The original character appears at most once *outside the
        // marker text*. The marker is `[REDACTED:short]`, which can
        // itself contain the character (`R` in `REDACTED`), so the
        // count is taken on the prefix before the marker.
        let marker_start = out.find('…').or_else(|| out.find('[')).unwrap_or(out.len());
        let before = &out[..marker_start];
        prop_assert!(
            before.matches(c).count() <= 1,
            "before={before:?} out={out:?} c={c:?}",
        );
    }

    /// Shannon entropy of any string is in `[0, log2(n_chars)]`. The
    /// upper bound is `log2(min(len_bytes, 256))` — 256 is the maximum
    /// distinct byte values.
    #[test]
    fn shannon_entropy_is_bounded(s in any_string()) {
        let h = kod_types::redact::shannon_entropy(&s);
        prop_assert!(h >= 0.0);
        if !s.is_empty() {
            let upper = (s.len() as f64).log2();
            prop_assert!(h <= upper + 1e-9, "h={h} upper={upper} s={s:?}");
        }
    }
}
