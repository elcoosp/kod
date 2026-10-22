//! MCP HTTP transport policy (borrow from oh-my-pi, delta §14.5).
//!
//! # The transport this governs
//!
//! MCP servers are reached over stdio today (see [`crate::client`]). A
//! future HTTP transport needs a redirect policy before it can be
//! written safely, and that policy is what this module is: pure
//! decisions about which headers to attach on which hop, and when to
//! refuse a redirect outright.
//!
//! # The leak this prevents
//!
//! An MCP server URL often carries a bearer token in a configured
//! header. `reqwest`'s default redirect policy re-sends every header on
//! every hop — so a server that answers `302 https://evil.example/`
//! receives the token. The rule here: **configured headers are attached
//! only on same-origin hops**. A cross-origin redirect drops them.
//!
//! # The method-changing refusal
//!
//! A `301`/`302`/`303` may change a `POST` to a `GET`. For an MCP
//! request that carries a JSON-RPC body, following the redirect would
//! drop the body and produce a `GET` the server never asked for. Only
//! `307`/`308` (which preserve the method by definition) are followed
//! for a non-`GET`; every other status is refused.
//!
//! # The hop cap
//!
//! [`MAX_REDIRECT_HOPS`] bounds a redirect loop.

/// The maximum redirect hops a request follows.
pub const MAX_REDIRECT_HOPS: usize = 5;

/// An HTTP method, reduced to what the policy needs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Method {
    Get,
    Other,
}

/// One hop's origin: scheme + host + port, lower-cased.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Origin {
    pub scheme: String,
    pub host: String,
    pub port: Option<u16>,
}

impl Origin {
    pub fn parse(url: &str) -> Option<Origin> {
        // Delegated to the `url` crate — the same WHATWG parser
        // `reqwest` uses to actually follow the redirect. Any
        // hand-rolled disagreement with WHATWG was a real
        // over-acceptance bug: `decide_redirect` uses `same_as` to
        // decide whether to forward the configured auth header, and a
        // "same origin" answer on a URL the wire client would parse as
        // cross-origin means credentials go to the wrong host.
        //
        // Concrete over-acceptances the previous hand-rolled parser
        // had (each one is a `prop_rejects_what_url_rejects` or
        // `prop_agrees_with_url_crate` case in the proptests):
        //
        // - `https:///x` — empty host accepted, WHATWG rejects.
        // - `https://example.com:abc/x` — non-numeric port treated as
        //   part of the host (`example.com:abc`), WHATWG rejects.
        // - `https://evil.com\@example.com/` — `\` not treated as
        //   `/` for special schemes, so the authority ends at the
        //   backslash in WHATWG but at the `@` here; the two parsers
        //   disagree on the host (`evil.com` vs `example.com`).
        //
        // `Url::port()` already normalizes the scheme's default port
        // to `None` (so `https://x:443` and `https://x` both yield
        // `port: None`); `effective_port()` is a no-op for this
        // constructor. The struct's `pub` fields are unchanged, so an
        // external `Origin { port: Some(443), .. }` still behaves
        // correctly via `effective_port`.
        let u = url::Url::parse(url).ok()?;
        Some(Origin {
            scheme: u.scheme().to_string(),
            host: u.host_str()?.to_ascii_lowercase(),
            port: u.port(),
        })
    }

    /// Whether two origins match. The default port for a scheme is
    /// treated as absent, so `https://x` and `https://x:443` match.
    pub fn same_as(&self, other: &Origin) -> bool {
        self.scheme == other.scheme
            && self.host == other.host
            && self.effective_port() == other.effective_port()
    }

    fn effective_port(&self) -> Option<u16> {
        match (self.port, self.scheme.as_str()) {
            (Some(443), "https") | (Some(80), "http") => None,
            (p, _) => p,
        }
    }
}

/// What to do with a redirect response.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RedirectDecision {
    /// Follow to the target. `attach_configured_headers` is false when
    /// the target is cross-origin.
    Follow { attach_configured_headers: bool },
    /// Refuse the redirect; the caller surfaces the response.
    Refuse { reason: &'static str },
}

/// Decide whether to follow a redirect.
///
/// `from` is the URL that produced the response, `to` is the `Location`
/// header, `status` is the response status, and `method` is the method
/// of the original request.
pub fn decide_redirect(from: &str, to: &str, status: u16, method: Method) -> RedirectDecision {
    // Only the redirect statuses the spec defines are followed.
    let method_preserving = matches!(status, 307 | 308);
    let method_changing = matches!(status, 301..=303);
    if !method_preserving && !method_changing {
        return RedirectDecision::Refuse {
            reason: "not a redirect status",
        };
    }
    // A method-changing redirect on a non-GET request would drop the
    // body. Refuse.
    if method_changing && method == Method::Other {
        return RedirectDecision::Refuse {
            reason: "a method-changing redirect would drop the request body",
        };
    }
    let (Some(from_o), Some(to_o)) = (Origin::parse(from), Origin::parse(to)) else {
        return RedirectDecision::Refuse {
            reason: "unparseable origin",
        };
    };
    RedirectDecision::Follow {
        attach_configured_headers: from_o.same_as(&to_o),
    }
}

/// Headers to strip from a request on a cross-origin hop.
///
/// The design's `withoutHeader` list: transport-reserved headers that a
/// configured header must not override, plus the configured auth
/// header on a cross-origin hop. Case-insensitive.
pub const TRANSPORT_RESERVED: &[&str] = &[
    "host",
    "content-length",
    "connection",
    "transfer-encoding",
    "upgrade",
];

/// Whether `name` is transport-reserved and must not be set by config.
pub fn is_transport_reserved(name: &str) -> bool {
    let l = name.to_ascii_lowercase();
    TRANSPORT_RESERVED.contains(&l.as_str())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn origins_parse_scheme_host_port() {
        let o = Origin::parse("https://example.com:8080/path").unwrap();
        assert_eq!(o.scheme, "https");
        assert_eq!(o.host, "example.com");
        assert_eq!(o.port, Some(8080));
    }

    #[test]
    fn origins_lowercase_the_host() {
        let o = Origin::parse("https://EXAMPLE.com/x").unwrap();
        assert_eq!(o.host, "example.com");
    }

    #[test]
    fn origins_drop_userinfo() {
        let o = Origin::parse("https://user:pw@example.com/x").unwrap();
        assert_eq!(o.host, "example.com");
    }

    #[test]
    fn the_default_port_matches_an_absent_one() {
        let a = Origin::parse("https://example.com").unwrap();
        let b = Origin::parse("https://example.com:443").unwrap();
        assert!(a.same_as(&b));
    }

    #[test]
    fn a_different_host_does_not_match() {
        let a = Origin::parse("https://a.com").unwrap();
        let b = Origin::parse("https://b.com").unwrap();
        assert!(!a.same_as(&b));
    }

    #[test]
    fn a_different_scheme_does_not_match() {
        let a = Origin::parse("https://example.com").unwrap();
        let b = Origin::parse("http://example.com").unwrap();
        assert!(!a.same_as(&b));
    }

    #[test]
    fn a_same_origin_redirect_attaches_headers() {
        let d = decide_redirect(
            "https://example.com/a",
            "https://example.com/b",
            307,
            Method::Get,
        );
        assert_eq!(
            d,
            RedirectDecision::Follow {
                attach_configured_headers: true
            }
        );
    }

    #[test]
    fn a_cross_origin_redirect_drops_headers() {
        let d = decide_redirect(
            "https://example.com/a",
            "https://evil.example/b",
            307,
            Method::Get,
        );
        assert_eq!(
            d,
            RedirectDecision::Follow {
                attach_configured_headers: false
            }
        );
    }

    #[test]
    fn a_cross_origin_post_on_308_keeps_the_body_but_drops_headers() {
        let d = decide_redirect(
            "https://example.com/a",
            "https://other.example/b",
            308,
            Method::Other,
        );
        assert_eq!(
            d,
            RedirectDecision::Follow {
                attach_configured_headers: false
            }
        );
    }

    #[test]
    fn a_method_changing_redirect_of_a_post_is_refused() {
        let d = decide_redirect(
            "https://example.com/a",
            "https://example.com/b",
            302,
            Method::Other,
        );
        assert!(matches!(d, RedirectDecision::Refuse { .. }));
    }

    #[test]
    fn a_method_changing_redirect_of_a_get_is_followed() {
        let d = decide_redirect(
            "https://example.com/a",
            "https://example.com/b",
            301,
            Method::Get,
        );
        assert_eq!(
            d,
            RedirectDecision::Follow {
                attach_configured_headers: true
            }
        );
    }

    #[test]
    fn a_303_of_a_get_is_followed() {
        let d = decide_redirect(
            "https://example.com/a",
            "https://example.com/b",
            303,
            Method::Get,
        );
        assert!(matches!(d, RedirectDecision::Follow { .. }));
    }

    #[test]
    fn a_non_redirect_status_is_refused() {
        let d = decide_redirect("https://a.com", "https://b.com", 200, Method::Get);
        assert!(matches!(d, RedirectDecision::Refuse { .. }));
    }

    #[test]
    fn an_unparseable_origin_is_refused() {
        let d = decide_redirect("not a url", "https://b.com", 307, Method::Get);
        assert!(matches!(d, RedirectDecision::Refuse { .. }));
    }

    #[test]
    fn the_hop_cap_is_five() {
        assert_eq!(MAX_REDIRECT_HOPS, 5);
    }

    #[test]
    fn transport_reserved_headers_are_recognized() {
        assert!(is_transport_reserved("Host"));
        assert!(is_transport_reserved("content-length"));
        assert!(is_transport_reserved("CONNECTION"));
    }

    #[test]
    fn a_configured_auth_header_is_not_transport_reserved() {
        assert!(!is_transport_reserved("Authorization"));
        assert!(!is_transport_reserved("X-Api-Key"));
    }
}

#[cfg(test)]
mod proptests {
    //! Properties of `Origin::parse` and `Origin::same_as`.
    //!
    //! `Origin::parse` is hand-rolled; the semantic reference is the
    //! WHATWG URL spec, which is what `url::Url::parse` implements and
    //! what reqwest (the client actually following the redirect) uses.
    //! Any disagreement between the two matters: `decide_redirect`
    //! uses `same_as` to decide whether to forward the configured auth
    //! header, and a disagreement in the dangerous direction — kod
    //! says "same origin", the wire client says "different" — would
    //! forward credentials to a cross-origin target.
    //!
    //! The properties:
    //!
    //! - `prop_never_panics`: `Origin::parse` and `decide_redirect`
    //!   accept arbitrary input without panicking.
    //! - `prop_rejects_what_url_rejects`: if `url::Url::parse` rejects
    //!   an input, `Origin::parse` must reject it too. Over-acceptance
    //!   is the unsafe direction (a malformed Location that the wire
    //!   client refuses to follow should not have its origin trusted).
    //! - `prop_agrees_with_url_crate`: for inputs both parsers accept,
    //!   the `(scheme, host, effective_port)` triples must match.
    //! - `prop_same_as_implies_url_same_origin`: the security
    //!   property. If `Origin::parse(a).same_as(Origin::parse(b))`,
    //!   the WHATWG origins of `a` and `b` must be equal — no
    //!   credential forwarding to a different origin.
    //!
    //! The corpus forces in the disagreement cases with `Just(...)`
    //! (L43): a purely random `any::<String>()` corpus rarely produces
    //! a URL that both parsers *accept but disagree on*, so the same-
    //! origin property would be vacuous against a hand-rolled parser
    //! bug. `arb_url_like` mixes hand-picked adversarial cases with
    //! generated well-formed URLs and arbitrary bytes.
    use super::*;
    use proptest::prelude::*;
    use url::Url;

    fn triplet(u: &Url) -> (String, Option<String>, Option<u16>) {
        let scheme = u.scheme().to_string();
        let host = u.host_str().map(|h| h.to_ascii_lowercase());
        let effective_port = match (u.port(), scheme.as_str()) {
            (Some(443), "https") | (Some(80), "http") => None,
            (p, _) => p,
        };
        (scheme, host, effective_port)
    }

    fn our_triplet(o: &Origin) -> (String, Option<String>, Option<u16>) {
        (o.scheme.clone(), Some(o.host.clone()), o.effective_port())
    }

    /// Well-formed URLs (both parsers accept and agree on these).
    fn arb_valid_url() -> impl Strategy<Value = String> {
        (
            prop::sample::select(vec!["http", "https"]),
            prop::sample::select(vec![
                "example.com",
                "EXAMPLE.com",
                "localhost",
                "127.0.0.1",
                "[::1]",
            ]),
            prop::option::of(any::<u16>()),
            "[a-zA-Z0-9/._-]{0,20}",
        )
            .prop_map(|(scheme, host, port, path)| match port {
                Some(p) => format!("{scheme}://{host}:{p}/{path}"),
                None => format!("{scheme}://{host}/{path}"),
            })
    }

    /// Adversarial cases that the hand-rolled parser is known (by
    /// inspection) to mishandle. Forced in via `prop_oneof!` so a
    /// regression is caught on the first run, not probed by chance.
    fn arb_adversarial() -> impl Strategy<Value = String> {
        prop::sample::select(vec![
            // Empty host — WHATWG rejects for special schemes.
            "https:///x".to_string(),
            // Backslash in authority — WHATWG treats as '/', kod does not.
            "https://evil.com\\@example.com/".to_string(),
            "https://example.com\\@evil.com/".to_string(),
            // Non-numeric port after colon — kod treats whole authority
            // as host, WHATWG rejects.
            "https://example.com:abc/x".to_string(),
            // Trailing colon.
            "https://example.com:/x".to_string(),
            // Empty input.
            String::new(),
            // No scheme.
            "//example.com/x".to_string(),
        ])
    }

    fn arb_url_like() -> impl Strategy<Value = String> {
        prop_oneof![arb_valid_url(), arb_adversarial(), any::<String>(),]
    }

    proptest! {
        #[test]
        fn prop_never_panics(s in any::<String>()) {
            let _ = Origin::parse(&s);
            let _ = decide_redirect(&s, &s, 307, Method::Get);
            let _ = decide_redirect(&s, &s, 302, Method::Other);
        }

        /// `Origin::parse` must not accept anything `url::Url::parse`
        /// rejects. Over-acceptance is the unsafe direction: a
        /// malformed `Location` that the wire client refuses to follow
        /// should not have its origin trusted for a same-origin check.
        ///
        /// The first `Just(...)` case, `https:///x`, is one where the
        /// current code returns `Some(Origin { host: "" ... })` and
        /// `url::Url::parse` rejects. This property fires on that case
        /// and on every other over-acceptance in the corpus.
        #[test]
        fn prop_rejects_what_url_rejects(s in arb_url_like()) {
            if Url::parse(&s).is_err() {
                prop_assert!(
                    Origin::parse(&s).is_none(),
                    "Origin::parse accepted a URL that url::Url::parse rejects: {:?}",
                    s,
                );
            }
        }

        /// For inputs both parsers accept, the
        /// `(scheme, host, effective_port)` triples must agree. The
        /// hand-rolled parser lowercases scheme and host; the `url`
        /// crate already lowercases the host. This pins the
        /// agreement, so a parser drift on either side is caught.
        #[test]
        fn prop_agrees_with_url_crate(s in arb_url_like()) {
            let Ok(u) = Url::parse(&s) else { return Ok(()) };
            let Some(o) = Origin::parse(&s) else { return Ok(()) };
            let expected = triplet(&u);
            let actual = our_triplet(&o);
            prop_assert_eq!(
                actual, expected,
                "Origin::parse disagrees with url::Url::parse on {:?}", s,
            );
        }

        /// The security property: if `Origin::parse(a).same_as(b)`,
        /// the WHATWG origins of `a` and `b` must be equal. A
        /// disagreement in this direction means `decide_redirect`
        /// would attach the configured auth header to a cross-origin
        /// target — the exact failure mode this module exists to
        /// prevent.
        ///
        /// `arb_valid_url` is the same-origin direction (both sides
        /// well-formed); `arb_adversarial` is the disagreement
        /// direction. Both are in the corpus, so the property cannot
        /// pass vacuously.
        #[test]
        fn prop_same_as_implies_url_same_origin(
            a in arb_url_like(),
            b in arb_url_like(),
        ) {
            let (Some(oa), Some(ob)) = (Origin::parse(&a), Origin::parse(&b)) else {
                return Ok(());
            };
            if !oa.same_as(&ob) {
                return Ok(());
            }
            // same_as returned true. Both URL parses must succeed
            // (by prop_rejects_what_url_rejects) and their WHATWG
            // triples must be equal. If either URL fails to parse,
            // the disagreement has already been caught by
            // prop_rejects_what_url_rejects and we exit here.
            let (Ok(ua), Ok(ub)) = (Url::parse(&a), Url::parse(&b)) else {
                return Ok(());
            };
            prop_assert_eq!(
                triplet(&ua), triplet(&ub),
                "same_as returned true for two URLs whose WHATWG \
                 origins differ: a={:?} b={:?}", a, b,
            );
        }
    }
}
