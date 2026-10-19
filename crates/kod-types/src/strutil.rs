//! UTF-8-safe string helpers, shared across the workspace.
//!
//! Every site that used to write `&s[..N]` — provider error
//! snippets, tool-result previews, log lines, path shortening —
//! panics the first time byte N lands inside a multi-byte UTF-8
//! character. For a coding agent that edits source in any language
//! that is not an edge case: it is a matter of when, not if.
//!
//! Two helpers replace it, distinguished by what the budget means:
//!
//! * [`truncate_chars`] caps by **character count** — for display
//!   sites whose constants are named `MAX_*_CHARS` or whose literal
//!   budgets are expressed in characters.
//! * [`truncate_bytes`] caps by **byte count** — for prompt
//!   assembly, hook output, and content caps whose constants are
//!   named `MAX_*_BYTES` or whose values are computed from a byte
//!   budget.
//!
//! Both never panic and never produce a partial character; the
//! returned slice always ends on a UTF-8 boundary.

/// The largest byte index `≤ max` that is a UTF-8 char boundary.
///
/// `max` larger than `s.len()` returns `s.len()`. `max == 0`
/// returns 0. The result is always a valid index into `s`.
pub fn floor_char_boundary(s: &str, max: usize) -> usize {
    if max >= s.len() {
        return s.len();
    }
    let mut i = max;
    // A char is at most 4 bytes; the loop runs at most 3 times.
    while i > 0 && !s.is_char_boundary(i) {
        i -= 1;
    }
    i
}

/// The longest prefix of `s` that is at most `max` **characters**
/// and ends on a UTF-8 char boundary.
///
/// Never panics. `truncate_chars("", n)` is `""`;
/// `truncate_chars(&s, 0)` is `""`; `truncate_chars(&s, s.chars().count() + 1)`
/// is `s` unchanged.
///
/// Historical note: before session 16 this function capped by
/// **bytes**, not characters, despite its name. On CJK/accented
/// content it silently returned roughly 1/2 to 1/3 the requested
/// count. Callers whose budget was in bytes have been migrated to
/// [`truncate_bytes`]; the fix is the one named in
/// `docs/BUG_REPORT_TRUNCATE_CHARS.md`.
#[inline]
pub fn truncate_chars(s: &str, max: usize) -> &str {
    match s.char_indices().nth(max) {
        Some((i, _)) => &s[..i],
        None => s,
    }
}

/// Truncate `s` to at most `max` **characters**, appending `ellipsis`
/// only when the truncation actually shortened the string.
///
/// The naive form — `if s.len() > max { format!("{}{ellipsis}",
/// truncate_chars(s, max)) } else { s.to_string() }` — fires on
/// non-ASCII input whose **char count** is at the cap but whose
/// **byte count** exceeds it (CJK is 3 bytes per char), because the
/// guard is a byte-length check and `truncate_chars` is a char cap.
/// The result is a spurious ellipsis on a string that was never
/// truncated. This helper removes the outer guard entirely: it
/// compares the returned prefix's byte length to the input's —
/// strictly shorter iff truncation happened — so it is exact in
/// whatever units the caller uses.
///
/// `ellipsis` may be empty (`""`) or multi-character
/// (`"… [truncated]"`), depending on the caller's display.
///
/// Never panics. `truncate_with_ellipsis("hello", 10, "…")` is
/// `"hello"`; `truncate_with_ellipsis("hello world", 5, "…")` is
/// `"hello…"`.
pub fn truncate_with_ellipsis(s: &str, max: usize, ellipsis: &str) -> String {
    let shown = truncate_chars(s, max);
    if shown.len() < s.len() {
        format!("{shown}{ellipsis}")
    } else {
        s.to_string()
    }
}

/// The longest prefix of `s` that is at most `max` bytes and ends
/// on a UTF-8 char boundary.
///
/// Byte-cap counterpart to [`truncate_chars`]. Use this at sites
/// whose budget is expressed in **bytes** (prompt assembly, hook
/// output, memory-content caps, embedder-input caps). Passing a
/// byte budget to a char cap would allow up to four times the
/// intended bytes for CJK content — this function exists so the
/// two contracts do not collide under a single name.
///
/// Never panics. Same boundary behavior as the pre-split
/// `truncate_chars`.
#[inline]
pub fn truncate_bytes(s: &str, max: usize) -> &str {
    &s[..floor_char_boundary(s, max)]
}

#[cfg(test)]
mod truncate_chars_tests {
    use super::*;

    #[test]
    fn ascii_under_cap_is_unchanged() {
        assert_eq!(truncate_chars("hello", 10), "hello");
    }

    #[test]
    fn ascii_over_cap_is_cut_at_max() {
        assert_eq!(truncate_chars("hello world", 5), "hello");
    }

    #[test]
    fn empty_string_returns_empty() {
        assert_eq!(truncate_chars("", 5), "");
    }

    #[test]
    fn zero_cap_returns_empty() {
        assert_eq!(truncate_chars("hello", 0), "");
        assert_eq!(truncate_chars("héllo", 0), "");
    }

    #[test]
    fn one_char_nonascii_is_kept_whole() {
        // The P2 bug from session 15: "¡" is 1 char, 2 bytes. The
        // pre-fix byte-cap truncated to floor_char_boundary(s, 1) = 0,
        // so the whole 1-char input was discarded when asked to keep
        // 1 char. The char cap must keep it.
        assert_eq!(truncate_chars("¡", 1), "¡");
        assert_eq!(truncate_chars("🚀", 1), "🚀");
    }

    #[test]
    fn multibyte_is_counted_by_char_not_byte() {
        // "café" is 4 chars, 5 bytes. A char cap of 3 must keep "caf";
        // a char cap of 4 must keep the whole string.
        let s = "café";
        assert_eq!(truncate_chars(s, 3), "caf");
        assert_eq!(truncate_chars(s, 4), "café");
        assert!(s.starts_with(truncate_chars(s, 3)));
        assert!(s.starts_with(truncate_chars(s, 4)));
    }

    #[test]
    fn cjk_count_matches_char_count() {
        // 你好世界 = 4 chars, 12 bytes. A char cap of n must yield
        // exactly min(n, 4) chars, not min(n / 3, 4) chars.
        let s = "你好世界";
        let total = s.chars().count();
        for n in 0..=total + 2 {
            let out = truncate_chars(s, n);
            assert_eq!(out.chars().count(), n.min(total));
            assert!(s.starts_with(out));
        }
    }

    #[test]
    fn emoji_is_not_split() {
        // 🚀 is 1 char, 4 bytes. A char cap must yield whole chars.
        let s = "deploy 🚀 now";
        for n in 0..=s.chars().count() {
            let out = truncate_chars(s, n);
            assert!(out.is_char_boundary(out.len()));
        }
        assert_eq!(truncate_chars(s, 9), "deploy 🚀 ");
        assert_eq!(truncate_chars(s, 8), "deploy 🚀");
    }

    #[test]
    fn combining_marks_are_counted_separately() {
        // "e\u{0301}x" is 3 chars (e, combining acute, x). A char
        // cap of 2 keeps "e\u{0301}"; a cap of 3 keeps the whole.
        let s = "e\u{0301}x";
        assert_eq!(truncate_chars(s, 2), "e\u{0301}");
        assert_eq!(truncate_chars(s, 3), s);
        let out = truncate_chars(s, 2);
        assert!(out.is_char_boundary(out.len()));
    }
}

#[cfg(test)]
mod truncate_bytes_tests {
    use super::*;

    #[test]
    fn ascii_under_cap_is_unchanged() {
        assert_eq!(truncate_bytes("hello", 10), "hello");
    }

    #[test]
    fn ascii_over_cap_is_cut_at_max() {
        assert_eq!(truncate_bytes("hello world", 5), "hello");
    }

    #[test]
    fn empty_string_returns_empty() {
        assert_eq!(truncate_bytes("", 5), "");
    }

    #[test]
    fn zero_cap_returns_empty() {
        assert_eq!(truncate_bytes("hello", 0), "");
        assert_eq!(truncate_bytes("héllo", 0), "");
    }

    #[test]
    fn multibyte_is_not_split() {
        // "café" is 5 bytes. Cutting at byte 4 lands mid-'é'; the
        // helper must retreat to byte 3.
        let s = "café";
        let out = truncate_bytes(s, 4);
        assert_eq!(out, "caf");
        assert!(out.is_char_boundary(out.len()));
    }

    #[test]
    fn one_char_nonascii_byte_cap_drops_whole_char() {
        // Byte-contract pin: "¡" is 2 bytes. A byte cap of 1 must
        // produce "". This is the documented byte behavior; the
        // char-cap counterpart keeps the char (see
        // truncate_chars_tests::one_char_nonascii_is_kept_whole).
        assert_eq!(truncate_bytes("¡", 1), "");
        assert_eq!(truncate_bytes("¡", 2), "¡");
    }

    #[test]
    fn cjk_cut_in_the_middle_does_not_panic() {
        let s = "你好世界";
        for n in 0..=s.len() + 2 {
            let out = truncate_bytes(s, n);
            assert!(out.is_char_boundary(out.len()));
            assert!(s.starts_with(out));
        }
    }

    #[test]
    fn emoji_is_not_split() {
        let s = "deploy 🚀 now";
        for n in 0..=s.len() {
            let out = truncate_bytes(s, n);
            assert!(out.is_char_boundary(out.len()));
        }
        assert_eq!(truncate_bytes(s, 9), "deploy ");
    }
}

#[cfg(test)]
mod truncate_with_ellipsis_tests {
    use super::*;

    #[test]
    fn ascii_under_cap_is_unchanged() {
        assert_eq!(truncate_with_ellipsis("hello", 10, "…"), "hello");
    }

    #[test]
    fn ascii_over_cap_is_cut_with_ellipsis() {
        assert_eq!(truncate_with_ellipsis("hello world", 5, "…"), "hello…");
    }

    #[test]
    fn cjk_at_char_cap_gets_no_spurious_ellipsis() {
        // The bug this helper exists to prevent: 5 CJK chars = 15
        // bytes; a byte-length guard (15 > 5) fires, truncate_chars
        // returns the whole string unchanged, and the naive caller
        // appends an ellipsis anyway.
        let s = "你好世界呀";
        assert_eq!(s.chars().count(), 5);
        assert!(s.len() > 5, "fixture must be >5 bytes to trigger a \
                              byte-based guard");
        assert_eq!(
            truncate_with_ellipsis(s, 5, "…"),
            s,
            "at the char cap, no ellipsis must be added",
        );
    }

    #[test]
    fn cjk_over_cap_is_cut_with_ellipsis() {
        let s = "你好世界呀";
        assert_eq!(truncate_with_ellipsis(s, 3, "…"), "你好世…");
    }

    #[test]
    fn empty_ellipsis_is_allowed() {
        assert_eq!(truncate_with_ellipsis("hello world", 5, ""), "hello");
    }

    #[test]
    fn multichar_ellipsis_is_allowed() {
        assert_eq!(
            truncate_with_ellipsis("hello world", 5, "… [truncated]"),
            "hello… [truncated]",
        );
    }

    #[test]
    fn empty_input_returns_empty() {
        assert_eq!(truncate_with_ellipsis("", 5, "…"), "");
    }

    #[test]
    fn zero_cap_returns_only_ellipsis() {
        assert_eq!(truncate_with_ellipsis("hello", 0, "…"), "…");
    }
}
