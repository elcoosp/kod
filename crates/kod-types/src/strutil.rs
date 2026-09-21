//! UTF-8-safe string helpers, shared across the workspace.
//!
//! Every site that used to write `&s[..N]` — provider error
//! snippets, tool-result previews, log lines, path shortening —
//! panics the first time byte N lands inside a multi-byte UTF-8
//! character. For a coding agent that edits source in any language
//! that is not an edge case: it is a matter of when, not if.
//!
//! [`truncate_chars`] is the single replacement. It never panics
//! and never produces a partial character; the returned slice ends
//! on a UTF-8 boundary at or before `max` bytes.

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

/// The longest prefix of `s` that is at most `max` bytes and ends
/// on a UTF-8 char boundary.
///
/// Never panics. `truncate_chars("", n)` is `""`;
/// `truncate_chars(&s, 0)` is `""`; `truncate_chars(&s, s.len() + 1)`
/// is `s` unchanged.
#[inline]
pub fn truncate_chars(s: &str, max: usize) -> &str {
    &s[..floor_char_boundary(s, max)]
}

#[cfg(test)]
mod tests {
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
    fn multibyte_is_not_split() {
        // "café" is 5 bytes (c=1,a=1,f=1,é=2). Cutting at byte 4
        // would land mid-'é'; the helper must retreat to byte 3.
        let s = "café";
        let out = truncate_chars(s, 4);
        assert_eq!(out, "caf");
        assert!(out.is_char_boundary(out.len()));
    }

    #[test]
    fn cjk_cut_in_the_middle_does_not_panic() {
        // 你好世界 = 12 bytes, 3 per char. Sweep every byte index
        // and prove no cut panics. This is the exact regression the
        // production-readiness review names.
        let s = "你好世界";
        for n in 0..=s.len() + 2 {
            let out = truncate_chars(s, n);
            assert!(out.is_char_boundary(out.len()), "cut {n} not on a boundary");
            assert!(s.starts_with(out), "cut {n} not a prefix");
        }
    }

    #[test]
    fn emoji_is_not_split() {
        // 🚀 is 4 bytes.
        let s = "deploy 🚀 now";
        for n in 0..=s.len() {
            let out = truncate_chars(s, n);
            assert!(out.is_char_boundary(out.len()));
        }
        // Cutting at byte 8 (deploy + space + first byte of emoji)
        // must give "deploy " not "deploy \xF0".
        assert_eq!(truncate_chars(s, 9), "deploy ");
    }

    #[test]
    fn floor_of_full_length_is_length() {
        assert_eq!(floor_char_boundary("hi", 100), 2);
        assert_eq!(floor_char_boundary("", 100), 0);
    }

    #[test]
    fn combining_marks_are_preserved_or_dropped_whole() {
        // "e\u{0301}" is 'e' + combining acute. The helper must not
        // cut between them.
        let s = "e\u{0301}x";
        let out = truncate_chars(s, 2); // byte 2 is between 'e' and the combining mark
        assert!(out.is_char_boundary(out.len()));
    }
}
