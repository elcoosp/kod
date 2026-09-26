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
        let (scheme, rest) = url.split_once("://")?;
        let authority = rest.split(['/', '?', '#']).next()?;
        // Drop userinfo.
        let authority = authority.rsplit('@').next()?;
        let (host, port) = match authority.rsplit_once(':') {
            Some((h, p)) => match p.parse::<u16>() {
                Ok(n) => (h.to_string(), Some(n)),
                Err(_) => (authority.to_string(), None),
            },
            None => (authority.to_string(), None),
        };
        Some(Origin {
            scheme: scheme.to_ascii_lowercase(),
            host: host.to_ascii_lowercase(),
            port,
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
    Follow {
        attach_configured_headers: bool,
    },
    /// Refuse the redirect; the caller surfaces the response.
    Refuse { reason: &'static str },
}

/// Decide whether to follow a redirect.
///
/// `from` is the URL that produced the response, `to` is the `Location`
/// header, `status` is the response status, and `method` is the method
/// of the original request.
pub fn decide_redirect(
    from: &str,
    to: &str,
    status: u16,
    method: Method,
) -> RedirectDecision {
    // Only the redirect statuses the spec defines are followed.
    let method_preserving = matches!(status, 307 | 308);
    let method_changing = matches!(status, 301 | 302 | 303);
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
        assert_eq!(d, RedirectDecision::Follow { attach_configured_headers: true });
    }

    #[test]
    fn a_cross_origin_redirect_drops_headers() {
        let d = decide_redirect(
            "https://example.com/a",
            "https://evil.example/b",
            307,
            Method::Get,
        );
        assert_eq!(d, RedirectDecision::Follow { attach_configured_headers: false });
    }

    #[test]
    fn a_cross_origin_post_on_308_keeps_the_body_but_drops_headers() {
        let d = decide_redirect(
            "https://example.com/a",
            "https://other.example/b",
            308,
            Method::Other,
        );
        assert_eq!(d, RedirectDecision::Follow { attach_configured_headers: false });
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
        assert_eq!(d, RedirectDecision::Follow { attach_configured_headers: true });
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
