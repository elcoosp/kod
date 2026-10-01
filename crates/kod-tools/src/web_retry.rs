//! Delta §14.5: fetch retry policy for `web_fetch`.
//!
//! The design's rules for a fetch that a server declined:
//!
//! * **429** — retry *once*, honoring `Retry-After`, clamped to
//!   [`RETRY_AFTER_CAP_SECS`]. A longer window is not worth blocking
//!   the turn; the caller gets the 429.
//! * **Bot block** — a 403/503 whose body names a bot wall
//!   (Cloudflare, a captcha) is retried with the *next* user agent.
//!   A different UA occasionally slips past a naive rule; it never
//!   helps against a real one, so it is one rotation, not a loop.
//!
//! This module is the pure decision: given a status, a body, and the
//! current attempt, what does the caller do next? The HTTP call is the
//! caller's.

/// `Retry-After` above this many seconds is not slept out — the fetch
/// returns the 429 instead of blocking the turn.
pub const RETRY_AFTER_CAP_SECS: u64 = 10;

/// How many times a bot-block rotates the user agent.
pub const MAX_UA_ROTATIONS: usize = 3;

/// The user agents tried in order. The first is kod's own identifier;
/// the rest are common desktop strings a naive wall lets through.
pub const USER_AGENTS: &[&str] = &[
    concat!("kod/", env!("CARGO_PKG_VERSION"), " (+https://github.com/elcoosp/kod)"),
    "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/537.36 \
     (KHTML, like Gecko) Chrome/120.0.0.0 Safari/537.36",
    "Mozilla/5.0 (X11; Linux x86_64; rv:120.0) Gecko/20100101 Firefox/120.0",
    "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 \
     (KHTML, like Gecko) Chrome/120.0.0.0 Safari/537.36",
];

/// What to do after a response.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Next {
    /// The response is fine (or not retryable); return it.
    Return,
    /// Sleep this long, then retry with the same UA.
    RetrySameAgent { wait_secs: u64 },
    /// Retry with the next UA in [`USER_AGENTS`].
    RetryNextAgent,
}

/// Decide what to do with a response. `body_preview` is the first few
/// hundred characters of the body, used for bot-wall detection.
/// `attempt` is 0-based.
pub fn decide(status: u16, body_preview: &str, attempt: usize) -> Next {
    // 429: retry once, honoring Retry-After when the caller threaded
    // it in via the preview is not how this works — the caller passes
    // the parsed hint separately. See `decide_with_retry_after`.
    if status == 429 && attempt == 0 {
        return Next::RetrySameAgent { wait_secs: 1 };
    }
    if is_bot_block(status, body_preview) && attempt < MAX_UA_ROTATIONS {
        return Next::RetryNextAgent;
    }
    Next::Return
}

/// [`decide`] with the server's `Retry-After` header, if any. A hint
/// over the cap clamps to the cap; a missing hint is 1 second.
pub fn decide_with_retry_after(
    status: u16,
    body_preview: &str,
    attempt: usize,
    retry_after_secs: Option<u64>,
) -> Next {
    if status == 429 && attempt == 0 {
        let wait = retry_after_secs.unwrap_or(1).min(RETRY_AFTER_CAP_SECS);
        return Next::RetrySameAgent { wait_secs: wait };
    }
    if is_bot_block(status, body_preview) && attempt < MAX_UA_ROTATIONS {
        return Next::RetryNextAgent;
    }
    Next::Return
}

/// True when `status` + `body` look like a bot wall.
pub fn is_bot_block(status: u16, body_preview: &str) -> bool {
    if !matches!(status, 403 | 503) {
        return false;
    }
    let l = body_preview.to_lowercase();
    const MARKERS: &[&str] = &[
        "cloudflare",
        "captcha",
        "cf-chl",
        "checking your browser",
        "access denied",
        "automated queries",
        "are you a robot",
    ];
    MARKERS.iter().any(|m| l.contains(m))
}

/// The user agent for `attempt` (clamped to the last).
pub fn user_agent_for(attempt: usize) -> &'static str {
    USER_AGENTS
        .get(attempt)
        .copied()
        .unwrap_or_else(|| USER_AGENTS.last().copied().unwrap_or("kod"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_200_is_returned() {
        assert_eq!(decide(200, "", 0), Next::Return);
    }

    #[test]
    fn a_429_retries_once() {
        assert_eq!(
            decide(429, "", 0),
            Next::RetrySameAgent { wait_secs: 1 },
        );
        // Second attempt: no more 429 retries.
        assert_eq!(decide(429, "", 1), Next::Return);
    }

    #[test]
    fn a_429_honors_retry_after_up_to_the_cap() {
        assert_eq!(
            decide_with_retry_after(429, "", 0, Some(5)),
            Next::RetrySameAgent { wait_secs: 5 },
        );
        // Over the cap: clamped.
        assert_eq!(
            decide_with_retry_after(429, "", 0, Some(3600)),
            Next::RetrySameAgent { wait_secs: RETRY_AFTER_CAP_SECS },
        );
    }

    #[test]
    fn a_cloudflare_403_rotates_the_agent() {
        assert_eq!(
            decide(403, "Just a moment... cloudflare", 0),
            Next::RetryNextAgent,
        );
    }

    #[test]
    fn a_plain_403_is_not_a_bot_block() {
        // A 403 with no wall marker is a real authorization failure;
        // rotating the UA would not help.
        assert_eq!(decide(403, "Forbidden: you lack permission", 0), Next::Return);
    }

    #[test]
    fn a_503_captcha_is_a_bot_block() {
        assert_eq!(
            decide(503, "please solve this captcha", 0),
            Next::RetryNextAgent,
        );
    }

    #[test]
    fn bot_block_rotations_are_bounded() {
        // Past MAX_UA_ROTATIONS, no more rotation.
        assert_eq!(
            decide(403, "cloudflare", MAX_UA_ROTATIONS),
            Next::Return,
        );
    }

    #[test]
    fn user_agents_are_distinct_and_bounded() {
        assert!(USER_AGENTS.len() >= 2);
        // `user_agent_for` clamps.
        assert_eq!(user_agent_for(0), USER_AGENTS[0]);
        assert_eq!(user_agent_for(999), *USER_AGENTS.last().unwrap());
        // No two adjacent agents are the same string.
        for w in USER_AGENTS.windows(2) {
            assert_ne!(w[0], w[1]);
        }
    }
}
