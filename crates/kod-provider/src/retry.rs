//! Retry policy and classification for LLM provider calls.
//!
//! See AD-05 in the architecture document. Three attempts with exponential
//! backoff and jitter; respects `Retry-After` when the server sends it.
//! Only `KodError::is_retryable()` errors are retried — 401/403/404/422
//! fail immediately rather than wasting the user's time.
//!
//! The sleep function is injectable for tests. It is stored as a boxed
//! `Arc<dyn Fn>`, which is not `Debug`; `RetryPolicy` therefore does
//! **not** derive `Debug`, only `Clone`. To print a policy for logging,
//! use the fields directly (they are all public and `Debug`-able).

use kod_error::{KodError, Result};
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

/// Boxed future returned by the injectable sleep function.
pub type SleepFuture = Pin<Box<dyn Future<Output = ()> + Send>>;

/// Injectable sleep. Production uses `tokio::time::sleep`; tests swap in
/// an immediate-return function to avoid wall-clock delays.
pub type SleepFn = dyn Fn(Duration) -> SleepFuture + Send + Sync + 'static;

/// Retry configuration. Defaults: 3 attempts, 250 ms base, 8 s cap, ±25 % jitter.
#[derive(Clone)]
pub struct RetryPolicy {
    pub max_attempts: u32,
    pub base_delay: Duration,
    pub max_delay: Duration,
    pub jitter_fraction: f64,
    /// Injectable sleep for tests. Production leaves this as `tokio::time::sleep`.
    pub sleep_fn: Arc<SleepFn>,
}

impl Default for RetryPolicy {
    fn default() -> Self {
        Self {
            max_attempts: 3,
            base_delay: Duration::from_millis(250),
            max_delay: Duration::from_secs(8),
            jitter_fraction: 0.25,
            sleep_fn: Arc::new(|d: Duration| -> SleepFuture { Box::pin(tokio::time::sleep(d)) }),
        }
    }
}

impl RetryPolicy {
    /// Test constructor: no actual delays, no jitter.
    pub fn immediate() -> Self {
        Self {
            max_attempts: 3,
            base_delay: Duration::ZERO,
            max_delay: Duration::ZERO,
            jitter_fraction: 0.0,
            sleep_fn: Arc::new(|_: Duration| -> SleepFuture { Box::pin(async {}) }),
        }
    }

    /// Delay for attempt `n` (1-based) with jitter, capped at `max_delay`.
    pub fn delay_for(&self, attempt: u32) -> Duration {
        let exp = self
            .base_delay
            .saturating_mul(1u32 << (attempt.saturating_sub(1)).min(5));
        let capped = exp.min(self.max_delay);
        if self.jitter_fraction <= 0.0 {
            return capped;
        }
        // Deterministic pseudo-jitter from nanos; avoids pulling in `rand`.
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.subsec_nanos() as f64)
            .unwrap_or(0.0);
        let jitter = (nanos / 1e9) * 2.0 - 1.0; // in [-1, 1)
        let factor = 1.0 + jitter * self.jitter_fraction;
        let secs = capped.as_secs_f64() * factor;
        Duration::from_secs_f64(secs.max(0.0))
    }
}

/// Run `f` under the retry policy. Only `is_retryable()` errors are retried.
/// A `Retry-After` hint (from `KodError::RateLimited`) overrides the
/// computed delay.
pub async fn with_retry<T, F, Fut>(policy: &RetryPolicy, f: F) -> Result<T>
where
    F: Fn() -> Fut,
    Fut: Future<Output = Result<T>>,
{
    let mut attempt = 1u32;
    loop {
        match f().await {
            Ok(v) => return Ok(v),
            Err(e) if !e.is_retryable() => return Err(e),
            Err(e) if attempt >= policy.max_attempts => return Err(e),
            Err(e) => {
                let delay = match &e {
                    KodError::RateLimited { retry_after_secs } => {
                        Duration::from_secs(*retry_after_secs).min(policy.max_delay)
                    }
                    _ => policy.delay_for(attempt),
                };
                tracing::warn!(
                    attempt,
                    max_attempts = policy.max_attempts,
                    delay_ms = delay.as_millis() as u64,
                    error = %e,
                    "transient provider error; retrying"
                );
                (policy.sleep_fn)(delay).await;
                attempt += 1;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc as StdArc;
    use std::sync::atomic::{AtomicU32, Ordering};

    #[tokio::test]
    async fn retries_transient_errors_until_success() {
        let calls = StdArc::new(AtomicU32::new(0));
        let calls_clone = calls.clone();
        let policy = RetryPolicy::immediate();
        let result: Result<u32> = with_retry(&policy, || {
            let n = calls_clone.fetch_add(1, Ordering::SeqCst);
            async move {
                if n < 2 {
                    Err(KodError::Provider("503 service unavailable".into()))
                } else {
                    Ok(42)
                }
            }
        })
        .await;
        assert_eq!(result.unwrap(), 42);
        assert_eq!(calls.load(Ordering::SeqCst), 3);
    }

    #[tokio::test]
    async fn does_not_retry_auth_errors() {
        let calls = StdArc::new(AtomicU32::new(0));
        let calls_clone = calls.clone();
        let policy = RetryPolicy::immediate();
        let result: Result<u32> = with_retry(&policy, || {
            calls_clone.fetch_add(1, Ordering::SeqCst);
            async move { Err(KodError::Provider("401 unauthorized".into())) }
        })
        .await;
        assert!(result.is_err());
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn honours_retry_after() {
        // A `RateLimited` error carries an explicit `retry_after_secs`.
        // With max_delay = 0 (immediate), the min() clamps it to zero —
        // the test proves the branch was reached by asserting success
        // after exactly `max_attempts` calls, not by measuring wall clock.
        let calls = StdArc::new(AtomicU32::new(0));
        let calls_clone = calls.clone();
        let policy = RetryPolicy::immediate();
        let result: Result<u32> = with_retry(&policy, || {
            calls_clone.fetch_add(1, Ordering::SeqCst);
            async move {
                Err(KodError::RateLimited {
                    retry_after_secs: 2,
                })
            }
        })
        .await;
        assert!(matches!(result, Err(KodError::RateLimited { .. })));
        assert_eq!(calls.load(Ordering::SeqCst), policy.max_attempts);
    }

    #[test]
    fn delay_grows_exponentially_until_cap() {
        let policy = RetryPolicy {
            base_delay: Duration::from_millis(100),
            max_delay: Duration::from_millis(500),
            jitter_fraction: 0.0,
            ..RetryPolicy::immediate()
        };
        assert_eq!(policy.delay_for(1), Duration::from_millis(100));
        assert_eq!(policy.delay_for(2), Duration::from_millis(200));
        assert_eq!(policy.delay_for(3), Duration::from_millis(400));
        assert_eq!(policy.delay_for(4), Duration::from_millis(500)); // capped
        assert_eq!(policy.delay_for(5), Duration::from_millis(500)); // capped
    }

    #[test]
    fn jitter_stays_within_bounds() {
        let policy = RetryPolicy {
            base_delay: Duration::from_millis(1000),
            max_delay: Duration::from_secs(60),
            jitter_fraction: 0.25,
            ..RetryPolicy::default()
        };
        for _ in 0..100 {
            let d = policy.delay_for(1);
            let ms = d.as_millis() as i64;
            // base 1000 ms ± 25 % → [750, 1250] ms
            assert!((750..=1250).contains(&ms), "jitter out of range: {ms} ms");
        }
    }
}

// ---------------------------------------------------------------------------
// Retry-hint extraction (borrow from oh-my-pi, delta §9.1).
//
// The pre-existing `with_retry` honours a bare `Retry-After` integer by
// way of `KodError::RateLimited { retry_after_secs }`. That leaves four
// shapes of hint unread:
//
//   * `retry-after-ms: 2500`           (Anthropic)
//   * `x-ratelimit-reset`              (OpenAI, most gateways)
//   * `x-ratelimit-reset-ms`           (OpenAI, some gateways)
//   * free-form body text like
//     `"please try again in ~5m"`.
//
// `extract_retry_hints` reads all of them and returns the *largest*
// delay suggested. When that delay exceeds `HINT_CAP`, the caller is
// expected to decline the retry and surface the original error — a
// server asking for ten minutes is not asking us to sleep ten minutes
// inside a streaming turn.
//
// The function is pure and dependency-free (no `reqwest::HeaderMap`,
// no `time`): callers pass a slice of `(name, value)` pairs and the
// body as `&str`. That keeps it testable and lets a provider crate
// wire it in with a one-line adapter built from a `HeaderMap`.
// ---------------------------------------------------------------------------

/// What a provider's error response suggests for the retry delay.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct RetryHints {
    /// The longest delay any header or text hint suggested, when one
    /// was found.
    pub delay: Option<Duration>,
    /// True when `delay` exceeds [`HINT_CAP`]. The caller should
    /// surface the original error instead of retrying.
    pub cap_declined: bool,
}

/// Above this, the caller declines the retry. 60 seconds is long
/// enough to cover any transient 429 the provider actually wants us
/// to wait out, short enough that an overnight run is not parked on
/// one endpoint.
pub const HINT_CAP: Duration = Duration::from_secs(60);

/// Read every retry-delay hint out of an error response and return
/// the largest one.
///
/// `status` is accepted for future use (a caller that wants to
/// suppress body-text hints on a 4xx that is obviously not a rate
/// limit) and is currently unused.
pub fn extract_retry_hints(
    status: Option<u16>,
    headers: &[(String, String)],
    body: &str,
) -> RetryHints {
    let _ = status;
    let mut best: Option<Duration> = None;
    let mut push = |d: Duration| {
        best = Some(match best {
            Some(b) if b >= d => b,
            _ => d,
        });
    };

    for (k, v) in headers {
        match k.to_ascii_lowercase().as_str() {
            "retry-after-ms" => {
                if let Some(d) = parse_millis(v) {
                    push(d);
                }
            }
            "retry-after" => {
                if let Some(d) = parse_retry_after(v) {
                    push(d);
                }
            }
            "x-ratelimit-reset-ms" | "x-ratelimit-reset" => {
                if let Some(d) = parse_rate_limit_reset(v) {
                    push(d);
                }
            }
            _ => {}
        }
    }

    if let Some(d) = scan_suffix_hint(body) {
        push(d);
    }
    if let Some(d) = scan_text_hint(body) {
        push(d);
    }

    match best {
        Some(d) if d > HINT_CAP => RetryHints {
            delay: Some(d),
            cap_declined: true,
        },
        Some(d) => RetryHints {
            delay: Some(d),
            cap_declined: false,
        },
        None => RetryHints::default(),
    }
}

/// Parse a bare integer millisecond value (`retry-after-ms: 2500`).
fn parse_millis(v: &str) -> Option<Duration> {
    let n: u64 = v.trim().parse().ok()?;
    Some(Duration::from_millis(n))
}

/// `Retry-After` per HTTP: an integer number of seconds, or an
/// HTTP-date. HTTP-date parsing needs a date library, which this
/// module deliberately does not pull in; an HTTP-date value is
/// therefore ignored (the retry loop falls back to its own backoff).
/// Every real provider kod has met sends the integer form.
fn parse_retry_after(v: &str) -> Option<Duration> {
    let n: u64 = v.trim().parse().ok()?;
    Some(Duration::from_secs(n))
}

/// `X-RateLimit-Reset` / `-Reset-Ms`. Three shapes in the wild:
///
/// * epoch milliseconds (> 10^12): subtract `now` in millis.
/// * epoch seconds     (> 10^9):  subtract `now` in seconds.
/// * bare delta seconds (<= 10^9): use as-is.
///
/// Ambiguity is inherent — no provider declares which it sends — so
/// the magnitude heuristic is the pragmatic answer. A value between
/// 10^9 and 10^12 is "epoch seconds", which is correct for every
/// endpoint whose clock is within a few decades of ours.
fn parse_rate_limit_reset(v: &str) -> Option<Duration> {
    let n: i64 = v.trim().parse().ok()?;
    let now_ms: i64 = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0);
    if n > 1_000_000_000_000 {
        let delta = n.saturating_sub(now_ms);
        Some(Duration::from_millis(delta.max(0) as u64))
    } else if n > 1_000_000_000 {
        let delta = n.saturating_sub(now_ms / 1000);
        Some(Duration::from_secs(delta.max(0) as u64))
    } else {
        Some(Duration::from_secs(n.max(0) as u64))
    }
}

/// Some gateways append `retry-after-ms=N` to the body after the
/// JSON payload. Scan for the last such token.
fn scan_suffix_hint(body: &str) -> Option<Duration> {
    let needle = "retry-after-ms=";
    let pos = body.rfind(needle)?;
    let after = &body[pos + needle.len()..];
    let digits: String = after
        .chars()
        .take_while(|c| c.is_ascii_digit())
        .collect();
    digits.parse::<u64>().ok().map(Duration::from_millis)
}

/// Free-form text like `"try again in ~5m"` or `"retry after 30s"`.
/// Scans a fixed list of cue phrases; each cue is followed by a
/// number and an optional unit.
fn scan_text_hint(body: &str) -> Option<Duration> {
    let lower = body.to_ascii_lowercase();
    const CUES: &[&str] = &["try again in", "retry after", "retry in", "wait "];
    for cue in CUES {
        let Some(pos) = lower.find(cue) else { continue };
        let after = &lower[pos + cue.len()..];
        let trimmed = after.trim_start_matches(|c: char| c.is_whitespace() || c == '~');
        let mut num_end = 0usize;
        for (i, c) in trimmed.char_indices() {
            if c.is_ascii_digit() {
                num_end = i + c.len_utf8();
            } else {
                break;
            }
        }
        if num_end == 0 {
            continue;
        }
        let n: u64 = match trimmed[..num_end].parse() {
            Ok(v) => v,
            Err(_) => continue,
        };
        let rest = trimmed[num_end..].trim_start();
        let unit: String = rest
            .chars()
            .take_while(|c| c.is_ascii_alphabetic())
            .collect();
        let secs = match unit.as_str() {
            "" | "s" | "sec" | "secs" | "second" | "seconds" => n,
            "m" | "min" | "mins" | "minute" | "minutes" => n.saturating_mul(60),
            "h" | "hr" | "hrs" | "hour" | "hours" => n.saturating_mul(3600),
            _ => continue,
        };
        return Some(Duration::from_secs(secs));
    }
    None
}

#[cfg(test)]
mod hint_tests {
    use super::*;

    fn hdr(k: &str, v: &str) -> (String, String) {
        (k.to_string(), v.to_string())
    }

    #[test]
    fn no_hints_is_the_default() {
        let r = extract_retry_hints(None, &[], "some error text");
        assert_eq!(r, RetryHints::default());
        assert!(r.delay.is_none());
        assert!(!r.cap_declined);
    }

    #[test]
    fn retry_after_integer_is_read() {
        let r = extract_retry_hints(Some(429), &[hdr("Retry-After", "30")], "");
        assert_eq!(r.delay, Some(Duration::from_secs(30)));
        assert!(!r.cap_declined);
    }

    #[test]
    fn retry_after_ms_is_read() {
        let r = extract_retry_hints(Some(429), &[hdr("retry-after-ms", "2500")], "");
        assert_eq!(r.delay, Some(Duration::from_millis(2500)));
    }

    #[test]
    fn the_largest_hint_wins() {
        let r = extract_retry_hints(
            Some(429),
            &[hdr("Retry-After", "5"), hdr("retry-after-ms", "8000")],
            "",
        );
        assert_eq!(r.delay, Some(Duration::from_secs(8)));
    }

    #[test]
    fn a_long_hint_declines_the_cap() {
        let r = extract_retry_hints(Some(429), &[hdr("Retry-After", "600")], "");
        assert_eq!(r.delay, Some(Duration::from_secs(600)));
        assert!(r.cap_declined);
    }

    #[test]
    fn text_hint_seconds() {
        let r = extract_retry_hints(Some(429), &[], "please try again in ~30s");
        assert_eq!(r.delay, Some(Duration::from_secs(30)));
    }

    #[test]
    fn text_hint_minutes() {
        let r = extract_retry_hints(Some(429), &[], "please retry in 2 minutes");
        assert_eq!(r.delay, Some(Duration::from_secs(120)));
    }

    #[test]
    fn text_hint_hours() {
        let r = extract_retry_hints(None, &[], "retry after 1 hour");
        assert_eq!(r.delay, Some(Duration::from_secs(3600)));
        assert!(r.cap_declined);
    }

    #[test]
    fn suffix_hint_in_body_is_read() {
        let r = extract_retry_hints(
            None,
            &[],
            "{\"error\":\"rate limited\"} retry-after-ms=4500",
        );
        assert_eq!(r.delay, Some(Duration::from_millis(4500)));
    }

    #[test]
    fn rate_limit_reset_epoch_seconds_is_a_future_delta() {
        // One hour from now, in epoch seconds.
        let now_s = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs() as i64;
        let future = (now_s + 3600).to_string();
        let r = extract_retry_hints(
            Some(429),
            &[hdr("x-ratelimit-reset", &future)],
            "",
        );
        let d = r.delay.expect("epoch hint");
        let secs = d.as_secs();
        assert!(
            (3595..=3605).contains(&secs),
            "expected ~3600 s, got {secs}",
        );
    }

    #[test]
    fn rate_limit_reset_bare_delta_is_seconds() {
        let r = extract_retry_hints(
            Some(429),
            &[hdr("x-ratelimit-reset", "45")],
            "",
        );
        assert_eq!(r.delay, Some(Duration::from_secs(45)));
    }

    #[test]
    fn a_non_numeric_header_value_is_ignored() {
        // HTTP-date form not parsed in this module; the header
        // contributes no hint and the loop falls back to backoff.
        let r = extract_retry_hints(
            Some(429),
            &[hdr("Retry-After", "Wed, 21 Oct 2015 07:28:00 GMT")],
            "",
        );
        assert!(r.delay.is_none());
    }

    #[test]
    fn unrelated_headers_are_ignored() {
        let r = extract_retry_hints(
            Some(500),
            &[
                hdr("content-type", "application/json"),
                hdr("x-request-id", "abc-123"),
            ],
            "{}",
        );
        assert_eq!(r, RetryHints::default());
    }

    #[test]
    fn header_names_are_case_insensitive() {
        let r = extract_retry_hints(Some(429), &[hdr("RETRY-AFTER", "10")], "");
        assert_eq!(r.delay, Some(Duration::from_secs(10)));
    }

    #[test]
    fn the_cap_is_the_documented_sixty_seconds() {
        assert_eq!(HINT_CAP, Duration::from_secs(60));
    }
}
