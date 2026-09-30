//! Delta §14.5: structured search-query parser.
//!
//! A model typed a web/search query in the Google idiom:
//! `site:docs.rs -deprecated "retry backoff" filetype:md before:2024`.
//! A backend that does not understand `site:` or `before:` either
//! ignores the token (returning hits the caller did not ask for) or
//! rejects the query. This module parses the idiom into a structured
//! form a caller can map onto whatever the backend supports, and
//! reports which constraints it recognized so an unsupported one can
//! be relaxed *explicitly* rather than silently.
//!
//! # The relaxation note
//!
//! The design's contract: a search that finds nothing because of a
//! constraint must say so. `relax` drops constraints one at a time
//! (least important first) and returns which it dropped, so a caller
//! can append `Note: filetype: constraint was relaxed` instead of
//! returning an empty page with no explanation.
//!
//! # What this does NOT do
//!
//! * Not the search itself. This parses and structures; the caller
//!   runs the query against a backend.
//! * Not engine-specific translation. `QuerySyntax` describes what a
//!   backend can do; mapping a parsed query onto one is the caller's
//!   job.

/// A parsed search query.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ParsedQuery {
    /// Free-text terms (everything not a `key:value` operator or a
    /// quoted phrase).
    pub terms: Vec<String>,
    /// Quoted phrases, kept verbatim (the quotes are not part of the
    /// value).
    pub phrases: Vec<String>,
    /// Negated terms (a leading `-`).
    pub excluded: Vec<String>,
    /// `site:host`.
    pub site: Option<String>,
    /// `filetype:ext` (or `ext:`).
    pub filetype: Option<String>,
    /// `before:YYYY[-MM[-DD]]`.
    pub before: Option<String>,
    /// `after:YYYY[-MM[-DD]]`.
    pub after: Option<String>,
    /// `inurl:token`.
    pub inurl: Option<String>,
    /// `intitle:token`.
    pub intitle: Option<String>,
    /// `lang:code`.
    pub lang: Option<String>,
    /// The literal `OR` alternation groups, if the query used `OR`.
    /// Each group is a set of alternatives.
    pub or_groups: Vec<Vec<String>>,
}

impl ParsedQuery {
    /// Every constraint that is set, as `(name, value)` pairs, in a
    /// fixed order. Used to relax them one at a time.
    pub fn constraints(&self) -> Vec<(&'static str, String)> {
        let mut out = Vec::new();
        if let Some(v) = &self.filetype {
            out.push(("filetype", v.clone()));
        }
        if let Some(v) = &self.site {
            out.push(("site", v.clone()));
        }
        if let Some(v) = &self.inurl {
            out.push(("inurl", v.clone()));
        }
        if let Some(v) = &self.intitle {
            out.push(("intitle", v.clone()));
        }
        if let Some(v) = &self.after {
            out.push(("after", v.clone()));
        }
        if let Some(v) = &self.before {
            out.push(("before", v.clone()));
        }
        if let Some(v) = &self.lang {
            out.push(("lang", v.clone()));
        }
        out
    }

    /// Drop one constraint by name. Returns the dropped value, or
    /// `None` if it was not set.
    pub fn drop_constraint(&mut self, name: &str) -> Option<String> {
        match name {
            "filetype" => self.filetype.take(),
            "site" => self.site.take(),
            "inurl" => self.inurl.take(),
            "intitle" => self.intitle.take(),
            "after" => self.after.take(),
            "before" => self.before.take(),
            "lang" => self.lang.take(),
            _ => None,
        }
    }
}

/// Parse a Google-idiom search query. Always succeeds — an
/// unrecognized `key:value` is kept as a free-text term (the token
/// the user typed), never dropped.
pub fn parse(query: &str) -> ParsedQuery {
    let mut q = ParsedQuery::default();
    let mut tokens = tokenize(query).into_iter().peekable();
    let mut pending_or: Vec<String> = Vec::new();

    while let Some(tok) = tokens.next() {
        // `OR` alternation: the term before and after are grouped.
        if tok.eq_ignore_ascii_case("OR") {
            let prev = q
                .terms
                .pop()
                .or_else(|| q.phrases.pop())
                .unwrap_or_default();
            let next = tokens.next().unwrap_or_default();
            if !prev.is_empty() {
                pending_or.push(prev);
            }
            if !next.is_empty() {
                pending_or.push(next);
            }
            continue;
        }
        if !pending_or.is_empty() {
            q.or_groups.push(std::mem::take(&mut pending_or));
        }
        // A quoted phrase.
        if tok.len() >= 2 && tok.starts_with('"') && tok.ends_with('"') {
            q.phrases.push(tok[1..tok.len() - 1].to_string());
            continue;
        }
        // A leading `-` is a negation.
        let (negated, body) = match tok.strip_prefix('-') {
            Some(rest) if !rest.is_empty() => (true, rest.to_string()),
            _ => (false, tok.clone()),
        };
        // A `key:value` operator.
        if let Some((key, value)) = body.split_once(':')
            && !value.is_empty()
            && !body.starts_with("://")
        {
            let k = key.to_ascii_lowercase();
            match k.as_str() {
                "site" => {
                    q.site = Some(value.to_string());
                    continue;
                }
                "filetype" | "ext" => {
                    q.filetype = Some(value.to_string());
                    continue;
                }
                "before" => {
                    q.before = Some(value.to_string());
                    continue;
                }
                "after" => {
                    q.after = Some(value.to_string());
                    continue;
                }
                "inurl" => {
                    q.inurl = Some(value.to_string());
                    continue;
                }
                "intitle" => {
                    q.intitle = Some(value.to_string());
                    continue;
                }
                "lang" => {
                    q.lang = Some(value.to_string());
                    continue;
                }
                _ => {
                    // Unknown operator: keep as a free term so nothing
                    // the user typed is silently lost.
                }
            }
        }
        if negated {
            q.excluded.push(body);
        } else {
            q.terms.push(tok);
        }
    }
    if !pending_or.is_empty() {
        q.or_groups.push(pending_or);
    }
    q
}

/// Split a query into tokens, keeping a quoted phrase as one token
/// (quotes included) and treating a `-` before a token as its own
/// prefix on that token.
fn tokenize(query: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut in_quote = false;
    for c in query.chars() {
        match c {
            '"' => {
                cur.push(c);
                if in_quote {
                    out.push(std::mem::take(&mut cur));
                }
                in_quote = !in_quote;
            }
            c if c.is_whitespace() && !in_quote => {
                if !cur.is_empty() {
                    out.push(std::mem::take(&mut cur));
                }
            }
            _ => cur.push(c),
        }
    }
    if !cur.is_empty() {
        out.push(cur);
    }
    out
}

/// Drop constraints (least important first) until `predicate` accepts
/// the query, returning the constraints dropped, in order.
///
/// `predicate` is the caller's "did the backend return anything for
/// this query" test. The relaxation order is the design's: filetype
/// and lang first (narrowest formatting), then site/inurl/intitle
/// (locality), then the date bounds (broadest).
pub fn relax_until<F>(q: &mut ParsedQuery, mut predicate: F) -> Vec<String>
where
    F: FnMut(&ParsedQuery) -> bool,
{
    const ORDER: &[&str] = &[
        "filetype", "lang", "site", "inurl", "intitle", "after", "before",
    ];
    let mut dropped = Vec::new();
    if predicate(q) {
        return dropped;
    }
    for name in ORDER {
        if q.drop_constraint(name).is_some() {
            dropped.push(format!("{name}:"));
            if predicate(q) {
                return dropped;
            }
        }
    }
    dropped
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_operators() {
        let q = parse("site:docs.rs filetype:md before:2024 lang:en retry");
        assert_eq!(q.site.as_deref(), Some("docs.rs"));
        assert_eq!(q.filetype.as_deref(), Some("md"));
        assert_eq!(q.before.as_deref(), Some("2024"));
        assert_eq!(q.lang.as_deref(), Some("en"));
        assert_eq!(q.terms, vec!["retry"]);
    }

    #[test]
    fn parses_phrases_negation_and_or() {
        let q = parse(r#"hello "exact phrase" -deprecated cat OR dog"#);
        assert_eq!(q.terms[0], "hello");
        assert_eq!(q.phrases, vec!["exact phrase"]);
        assert_eq!(q.excluded, vec!["deprecated"]);
        assert_eq!(q.or_groups.len(), 1);
        assert_eq!(q.or_groups[0], vec!["cat", "dog"]);
    }

    #[test]
    fn an_unknown_operator_is_kept_as_a_term() {
        // Nothing the user typed is silently dropped.
        let q = parse("frob:value normal");
        assert!(q.terms.contains(&"frob:value".to_string()));
        assert!(q.terms.contains(&"normal".to_string()));
    }

    #[test]
    fn a_url_is_not_mistaken_for_an_operator() {
        // `https://x` must not parse as `key: https` + `//x`.
        let q = parse("https://example.com/page");
        assert!(q.terms.iter().any(|t| t.contains("https://example.com")), "got: {:?}", q.terms);
        assert!(q.site.is_none());
    }

    #[test]
    fn constraints_lists_what_is_set() {
        let q = parse("site:a filetype:b before:c");
        let names: Vec<&str> = q.constraints().iter().map(|(n, _)| *n).collect();
        assert_eq!(names, vec!["filetype", "site", "before"]);
    }

    #[test]
    fn relax_drops_in_order_until_the_predicate_passes() {
        let mut q = parse("site:a filetype:b before:c retry");
        // The backend accepts only when `filetype` is gone.
        let dropped = relax_until(&mut q, |q| q.filetype.is_none());
        assert_eq!(dropped, vec!["filetype:"]);
        assert!(q.filetype.is_none());
        // site and before are untouched.
        assert!(q.site.is_some());
        assert!(q.before.is_some());
    }

    #[test]
    fn relax_returns_empty_when_the_first_predicate_passes() {
        let mut q = parse("site:a retry");
        let dropped = relax_until(&mut q, |_| true);
        assert!(dropped.is_empty());
        assert!(q.site.is_some(), "nothing dropped");
    }

    #[test]
    fn relax_through_every_constraint() {
        let mut q = parse("site:a filetype:b before:c after:d lang:e inurl:f intitle:g");
        let dropped = relax_until(&mut q, |_| false);
        assert_eq!(dropped.len(), 7);
        assert!(q.constraints().is_empty());
    }

    #[test]
    fn empty_query_parses_to_empty() {
        let q = parse("");
        assert_eq!(q, ParsedQuery::default());
    }

    #[test]
    fn quoted_phrase_with_spaces_is_one_token() {
        let q = parse(r#""a b c" d"#);
        assert_eq!(q.phrases, vec!["a b c"]);
        assert_eq!(q.terms, vec!["d"]);
    }
}
