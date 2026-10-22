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
    /// Longest provider-suggested rate-limit window the loop may sleep
    /// out in full. `ZERO` (default) preserves the legacy behavior: any
    /// hint is clamped to `max_delay`. A hint at or below this budget is
    /// slept exactly ONCE per call before the next attempt (a second
    /// long window fails the call instead of parking it again).
    pub max_rate_limit_wait: Duration,
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
            max_rate_limit_wait: Duration::ZERO,
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
            max_rate_limit_wait: Duration::ZERO,
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
            .map(|d| d.as_nanos() as u64)
            .unwrap_or(0);
        let jitter = Self::jitter_for(nanos);
        let factor = 1.0 + jitter * self.jitter_fraction;
        let secs = capped.as_secs_f64() * factor;
        Duration::from_secs_f64(secs.max(0.0))
    }

    /// Derive the raw jitter value in `[-1, 1]` from a wall-clock
    /// `nanos` value. Extracted from `delay_for` so a test can
    /// exercise the seed derivation without a clock dependency.
    ///
    /// **Seed diversity.** The seed is a function of wall-clock
    /// nanos, thread id, and process id. On a fast machine,
    /// `SystemTime::now()` (microsecond resolution on macOS) does
    /// not advance between back-to-back calls: a 1000-call burst
    /// completed in well under a millisecond sees every sample get
    /// the same nanos, the same seed, and therefore the same jitter.
    /// That is the exact lockstep-retry problem jitter exists to
    /// prevent, and it caused a real flake in
    /// `jitter_is_centered_and_signed`. The counter fix lands in the
    /// next commit.
    fn jitter_for(nanos: u64) -> f64 {
        // T2-H11: mix thread id + process id + nanos into the jitter
        // seed. The pre-fix shape read only `SystemTime::subsec_nanos`,
        // which is correlated across threads: a swarm hitting the same
        // 429 at the same wall-clock instant computed identical jitter
        // and retried in lockstep — the exact failure mode jitter
        // exists to prevent.
        let tid = {
            use std::hash::{Hash, Hasher};
            let mut h = std::collections::hash_map::DefaultHasher::new();
            std::thread::current().id().hash(&mut h);
            h.finish()
        };
        // Fix (L48 round): mix a monotonic counter into the seed.
        // The pre-fix seed was `nanos ^ tid ^ pid`; on a fast machine
        // `SystemTime::now()` (microsecond resolution on macOS) does
        // not advance between back-to-back calls, so a burst of
        // retries saw one `nanos`, one `tid`, one `pid` — and
        // therefore identical jitter, the exact lockstep-retry
        // problem jitter exists to prevent. The counter is a global
        // atomic; a per-instance counter would collide across
        // `RetryPolicy` values that share a nanos.
        static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let seq = SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        // Fold `seq` into the state through the golden-ratio
        // constant (Knuth's multiplier) so adjacent counters do not
        // land on adjacent states: `seq * GOLDEN` spreads the
        // counter across the full u64 bit-space, and XOR mixes it
        // into the (nanos, tid, pid) seed without disturbing the
        // existing bit layout.
        let mut state =
            nanos ^ tid ^ (std::process::id() as u64) ^ seq.wrapping_mul(0x9E37_79B9_7F4A_7C15);
        state = state
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        // T2-H11 follow-up: the pre-fix formula was
        //     let jitter = ((state >> 33) as f64) / (u32::MAX as f64) * 2.0 - 1.0;
        // `state >> 33` keeps 31 bits, but it was divided by
        // `u32::MAX` (2^32 - 1). The numerator was always at most half
        // the denominator, so the ratio was in [0, 0.5) and the jitter
        // — after `* 2 - 1` — was ALWAYS NEGATIVE, in [-1, 0).
        // Fixed by taking the top 32 bits and dividing by `2^32`.
        let top32 = (state >> 32) as u32;
        (top32 as f64) / (u32::MAX as f64 + 1.0) * 2.0 - 1.0
    }
}

/// Run `f` under the retry policy. Only `is_retryable()` errors are retried.
/// A `Retry-After` hint (from `KodError::RateLimited` or
/// `KodError::ServerBusy`) overrides the computed delay.
pub async fn with_retry<T, F, Fut>(policy: &RetryPolicy, f: F) -> Result<T>
where
    F: Fn() -> Fut,
    Fut: Future<Output = Result<T>>,
{
    let mut attempt = 1u32;
    let mut long_wait_used = false;
    loop {
        match f().await {
            Ok(v) => {
                if attempt > 1 {
                    tracing::debug!(attempt, "provider call succeeded after retry");
                }
                return Ok(v);
            }
            Err(e) if !e.is_retryable() => return Err(e),
            Err(e) if attempt >= policy.max_attempts => return Err(e),
            Err(e) => {
                let delay = match &e {
                    KodError::RateLimited { retry_after_secs } => {
                        let hint = Duration::from_secs(*retry_after_secs);
                        if hint > policy.max_delay
                            && hint <= policy.max_rate_limit_wait
                            && !long_wait_used
                        {
                            // The provider asked for a long window (e.g. a
                            // stateful tab backend's ~20-minute send
                            // frequency limit). Sleep it out exactly once;
                            // a second such window fails the call.
                            long_wait_used = true;
                            hint
                        } else {
                            hint.min(policy.max_delay)
                        }
                    }
                    KodError::ServerBusy { retry_after_secs } => {
                        let hint = Duration::from_secs(*retry_after_secs);
                        if hint > policy.max_delay
                            && hint <= policy.max_rate_limit_wait
                            && !long_wait_used
                        {
                            // Same single-long-wait contract as rate
                            // limits; the overload cooldown is ~10 min.
                            long_wait_used = true;
                            hint
                        } else {
                            hint.min(policy.max_delay)
                        }
                    }
                    _ => policy.delay_for(attempt),
                };
                tracing::warn!(
                    attempt,
                    max_attempts = policy.max_attempts,
                    delay_ms = delay.as_millis() as u64,
                    long_wait = long_wait_used,
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

    /// The pseudo-jitter must be centered: the sample mean of many
    /// `delay_for` calls at a fixed attempt approaches the capped
    /// delay, not 0.75 of it. Pre-fix, `(state >> 33) as f64 /
    /// (u32::MAX as f64) * 2.0 - 1.0` yielded values in `[-1, 0)`
    /// because 31 bits were divided by 2^32-1, so *every* retry was
    /// 25 % short and all retries had the same sign.
    ///
    /// The test exercises the actual formula through `delay_for`
    /// with a large sample: the sample min must be below the capped
    /// delay (some negative jitter) and the sample max must be above
    /// it (some positive jitter).
    /// **Failing-first.** Two calls with the same `nanos` must
    /// produce different jitter.
    ///
    /// The seed currently depends only on `(nanos, tid, pid)`. Both
    /// `tid` and `pid` are constant within a test run, so
    /// `jitter_for(12345)` twice returns the same f64 — deterministically.
    /// On a fast machine, `SystemTime::now()` (microsecond resolution
    /// on macOS) does not advance between back-to-back calls, so a
    /// burst of retries all get the same seed and retry in lockstep —
    /// the exact problem jitter exists to prevent. The test below
    /// catches that deterministically; the flaky
    /// `jitter_is_centered_and_signed` catches it probabilistically
    /// (min < 900 and max > 1100 cannot both hold when all samples
    /// are identical, and the gate saw exactly that).
    #[test]
    fn jitter_for_identical_nanos_differs_per_call() {
        let a = RetryPolicy::jitter_for(12345);
        let b = RetryPolicy::jitter_for(12345);
        assert_ne!(
            a, b,
            "jitter_for(12345) returned the same value twice; the seed \
             depends only on wall-clock nanos, which does not advance \
             between back-to-back calls on a fast machine",
        );
    }

    #[test]
    fn jitter_is_centered_and_signed() {
        let policy = RetryPolicy {
            max_attempts: 3,
            base_delay: Duration::from_millis(1000),
            max_delay: Duration::from_secs(2),
            jitter_fraction: 0.25,
            max_rate_limit_wait: Duration::ZERO,
            sleep_fn: Arc::new(|_: Duration| -> SleepFuture { Box::pin(async {}) }),
        };
        // Sample many delays. Because the jitter seed varies by
        // wall-clock nanos, each call's value differs.
        let mut min_ms = u128::MAX;
        let mut max_ms: u128 = 0;
        for _ in 0..1000 {
            let d = policy.delay_for(1); // attempt 1 -> 1000 ms capped
            let ms = d.as_millis();
            if ms < min_ms {
                min_ms = ms;
            }
            if ms > max_ms {
                max_ms = ms;
            }
        }
        // The capped base is 1000 ms. With ±25 % jitter and 1000
        // samples the observed min should be meaningfully below
        // 1000 and the observed max meaningfully above.
        assert!(
            min_ms < 900,
            "sample min must be below base (some -jitter); got {min_ms}",
        );
        assert!(
            max_ms > 1100,
            "sample max must be above base (some +jitter); got {max_ms}",
        );
    }
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

    #[tokio::test]
    async fn long_rate_limit_hint_is_slept_out_once_within_budget() {
        let sleeps: StdArc<std::sync::Mutex<Vec<Duration>>> =
            StdArc::new(std::sync::Mutex::new(Vec::new()));
        let s2 = sleeps.clone();
        let policy = RetryPolicy {
            max_rate_limit_wait: Duration::from_secs(1500),
            sleep_fn: StdArc::new(move |d: Duration| {
                s2.lock().unwrap().push(d);
                Box::pin(async {})
            }),
            ..RetryPolicy::immediate()
        };
        let calls = StdArc::new(AtomicU32::new(0));
        let c2 = calls.clone();
        let result: Result<u32> = with_retry(&policy, || {
            let n = c2.fetch_add(1, Ordering::SeqCst);
            async move {
                if n < 2 {
                    Err(KodError::RateLimited {
                        retry_after_secs: 1200,
                    })
                } else {
                    Ok(7)
                }
            }
        })
        .await;
        assert_eq!(result.unwrap(), 7);
        let s = sleeps.lock().unwrap();
        assert_eq!(s[0], Duration::from_secs(1200), "first hint slept in full");
        assert_eq!(
            *s.last().unwrap(),
            Duration::ZERO,
            "second long hint clamped (budget spent)"
        );
    }

    #[tokio::test]
    async fn zero_budget_keeps_legacy_clamp() {
        let policy = RetryPolicy::immediate(); // max_rate_limit_wait = ZERO
        let calls = StdArc::new(AtomicU32::new(0));
        let c2 = calls.clone();
        let result: Result<u32> = with_retry(&policy, || {
            let n = c2.fetch_add(1, Ordering::SeqCst);
            async move {
                if n < 2 {
                    Err(KodError::RateLimited {
                        retry_after_secs: 1200,
                    })
                } else {
                    Ok(1)
                }
            }
        })
        .await;
        assert_eq!(result.unwrap(), 1);
        assert_eq!(calls.load(Ordering::SeqCst), 3);
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

/// Longest window a *streaming* attempt loop may sleep out between
/// attempts. The engine reads provider streams under a 120 s idle
/// timeout (`engine::stream_round`, scaled by effort), so a provider
/// that silently parks a stream attempt for longer than this would be
/// killed by the reader before the retry ever fired. Windows above
/// the cap must surface as a typed `RateLimited` error instead — the
/// engine sleeps those out between rounds, where no stream is open
/// and the wait is user-visible and cancellable.
pub const STREAM_RATE_LIMIT_WAIT_CAP: Duration = Duration::from_secs(90);

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
    let digits: String = after.chars().take_while(|c| c.is_ascii_digit()).collect();
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

/// Public form of [`scan_text_hint`]: the seconds a provider body text
/// suggests, when it does. Used by adapters that only see a formatted
/// error message (adk flattens the HTTP response, so tab-bridge's
/// `"…wait ~20 minutes…"` body reaches this module as plain text).
pub fn text_hint_secs(body: &str) -> Option<u64> {
    scan_text_hint(body).map(|d| d.as_secs())
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
        let r = extract_retry_hints(Some(429), &[hdr("x-ratelimit-reset", &future)], "");
        let d = r.delay.expect("epoch hint");
        let secs = d.as_secs();
        assert!(
            (3595..=3605).contains(&secs),
            "expected ~3600 s, got {secs}",
        );
    }

    #[test]
    fn rate_limit_reset_bare_delta_is_seconds() {
        let r = extract_retry_hints(Some(429), &[hdr("x-ratelimit-reset", "45")], "");
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

    #[test]
    fn jitter_varies_across_successive_calls() {
        // T2-H11: the pre-fix shape used SystemTime::subsec_nanos() as
        // the only jitter source, so a swarm of agents retrying the
        // same attempt at the same wall-clock instant computed
        // identical delays and retried in lockstep. This test checks
        // the delay is not constant across a tight loop.
        let policy = RetryPolicy::default();
        let mut seen = std::collections::HashSet::new();
        for _ in 0..40 {
            seen.insert(policy.delay_for(1).as_nanos());
        }
        assert!(
            seen.len() >= 2,
            "jitter must vary; got {} distinct values out of 40",
            seen.len(),
        );
    }

    #[test]
    fn jitter_stays_inside_the_configured_band() {
        let policy = RetryPolicy::default();
        let base = policy.base_delay.as_nanos() as f64;
        let j = policy.jitter_fraction;
        for _ in 0..40 {
            let d = policy.delay_for(1).as_nanos() as f64;
            assert!(
                d >= base * (1.0 - j) - 1.0 && d <= base * (1.0 + j) + 1.0,
                "delay {d} outside [{}, {}]",
                base * (1.0 - j),
                base * (1.0 + j),
            );
        }
    }
}

#[cfg(test)]
mod text_hint_proptests {
    //! Properties for `scan_text_hint` and `text_hint_secs`.
    //!
    //! The 17 example tests in `hint_tests` pin specific cue/unit
    //! combinations. These properties pin the shape across input
    //! classes: case insensitivity, cue requirement, unit round-trip,
    //! saturation on overflow.
    //!
    //! `text_hint_secs` does **not** apply `HINT_CAP` — that is
    //! `extract_retry_hints`'s job, and the two callers in
    //! `kod-core-routing::retry_strategy` pass the raw value through
    //! uncapped. That caller gap is documented in the continuation
    //! record; it is not a contract of this function and is not
    //! asserted here.

    use super::*;
    use proptest::prelude::*;

    const UNITS_SECS: &[&str] = &["", "s", "sec", "secs", "second", "seconds"];
    const UNITS_MINS: &[&str] = &["m", "min", "mins", "minute", "minutes"];
    const UNITS_HOURS: &[&str] = &["h", "hr", "hrs", "hour", "hours"];

    proptest! {
        /// Empty input has no cue, so no hint.
        #[test]
        fn prop_empty_returns_none(_dummy in 0u8..1) {
            prop_assert_eq!(text_hint_secs(""), None);
        }

        /// A string with no cue word returns `None` regardless of
        /// the numbers or units it contains.
        #[test]
        fn prop_no_cue_returns_none(prefix in "[a-zA-Z ]{0,20}", n in 1u64..1000, unit in prop::sample::select(UNITS_SECS)) {
            let text = format!("{}{}{}", prefix, n, unit);
            // Ensure the string cannot accidentally contain a cue.
            prop_assume!(
                !text.to_ascii_lowercase().contains("try again in")
                    && !text.to_ascii_lowercase().contains("retry after")
                    && !text.to_ascii_lowercase().contains("retry in")
                    && !text.to_ascii_lowercase().contains("wait ")
            );
            prop_assert_eq!(text_hint_secs(&text), None);
        }

        /// Determinism: same input, same output.
        #[test]
        fn prop_deterministic(text in "[a-zA-Z0-9 .~-]{0,60}") {
            prop_assert_eq!(text_hint_secs(&text), text_hint_secs(&text));
        }

        /// Case insensitivity: `scan_text_hint` lowercases the body
        /// before searching for cues, so mixed/upper/lower all match
        /// the same way.
        #[test]
        fn prop_case_insensitive(
            n in 1u64..1000,
            unit in prop::sample::select(UNITS_SECS),
        ) {
            let lower = format!("retry after {} {}", n, unit);
            let upper = lower.to_ascii_uppercase();
            let mixed = lower
                .chars()
                .enumerate()
                .map(|(i, c)| if i % 2 == 0 { c.to_ascii_uppercase() } else { c })
                .collect::<String>();
            prop_assert_eq!(text_hint_secs(&lower), text_hint_secs(&upper));
            prop_assert_eq!(text_hint_secs(&lower), text_hint_secs(&mixed));
        }

        /// Round-trip for every seconds-suffix spelling and every cue.
        #[test]
        fn prop_seconds_round_trip(
            cue in prop::sample::select(&["try again in ", "retry after ", "retry in ", "wait "][..]),
            n in 0u64..100_000,
            unit in prop::sample::select(UNITS_SECS),
        ) {
            let text = format!("{}{} {}", cue, n, unit);
            prop_assert_eq!(
                text_hint_secs(&text),
                Some(n),
                "cue={:?} n={} unit={:?}", cue, n, unit,
            );
        }

        /// Minutes multiply by 60.
        #[test]
        fn prop_minutes_multiply(n in 0u64..10_000, unit in prop::sample::select(UNITS_MINS)) {
            let text = format!("retry after {} {}", n, unit);
            prop_assert_eq!(text_hint_secs(&text), Some(n.saturating_mul(60)));
        }

        /// Hours multiply by 3600.
        #[test]
        fn prop_hours_multiply(n in 0u64..10_000, unit in prop::sample::select(UNITS_HOURS)) {
            let text = format!("retry after {} {}", n, unit);
            prop_assert_eq!(text_hint_secs(&text), Some(n.saturating_mul(3600)));
        }

        /// A huge hour count saturates to `u64::MAX`, not wrapping.
        /// `scan_text_hint` uses `saturating_mul`, so the multiplication
        /// is safe; the property pins that it stays saturating rather
        /// than wrapping to a small value.
        #[test]
        fn prop_hours_saturate(_dummy in 0u8..1) {
            let text = format!("retry after {} hours", u64::MAX);
            prop_assert_eq!(text_hint_secs(&text), Some(u64::MAX));
        }

        /// Non-digit cue suffixes yield `None` (the parser requires
        /// digits immediately after the cue, modulo whitespace and `~`).
        #[test]
        fn prop_no_digits_after_cue_returns_none(filler in "[a-zA-Z ]{1,20}") {
            let text = format!("retry after {}", filler);
            // Filler starts with a letter, so no digits follow the cue.
            prop_assume!(filler.chars().next().map(|c| !c.is_ascii_digit()).unwrap_or(true));
            prop_assert_eq!(text_hint_secs(&text), None);
        }
    }

    /// Control: an unknown unit returns `None` (the parser only
    /// accepts the listed suffixes). Without this, a property like
    /// `prop_seconds_round_trip` could pass on a `scan_text_hint`
    /// that accepts *any* unit.
    #[test]
    fn control_unknown_unit_returns_none() {
        assert_eq!(text_hint_secs("retry after 5 fortnights"), None);
    }

    /// Control: the "no unit" form is seconds. The parser treats
    /// `"retry after 5"` as `Some(5)`, not `None` and not an error.
    #[test]
    fn control_no_unit_is_seconds() {
        assert_eq!(text_hint_secs("retry after 5"), Some(5));
    }

    /// Control: `"retry after 0 seconds"` parses to `Some(0)`.
    /// This is the current behaviour — a "retry immediately" hint.
    /// The property suite does not assert that `Some(0)` is
    /// impossible; it is a valid hint. Callers that would hot-loop on
    /// a zero hint are a caller-side concern (see the continuation
    /// record's note on the kod-core-routing call sites).
    #[test]
    fn control_zero_is_a_valid_hint() {
        assert_eq!(text_hint_secs("retry after 0 seconds"), Some(0));
    }
}

#[cfg(test)]
mod extract_retry_hints_proptests {
    //! Properties for `extract_retry_hints`.
    //!
    //! The function combines header hints (`retry-after`,
    //! `retry-after-ms`, `x-ratelimit-reset{,-ms}`), body hints
    //! (`scan_suffix_hint`, `scan_text_hint`), takes the maximum,
    //! and decides `cap_declined` by comparing against `HINT_CAP`.
    //!
    //! The primary invariant: **`cap_declined` iff `delay > HINT_CAP`**.
    //! This is the only place the caller learns whether to retry or
    //! surface the error, and it is a single comparison in the
    //! implementation — easy to get wrong if the boundary moves.
    //!
    //! Properties are driven by header-only inputs (empty body), so
    //! `scan_suffix_hint` and `scan_text_hint` cannot fire and the
    //! comparison against `HINT_CAP` is entirely attributable to the
    //! headers the property constructs.

    use super::*;
    use proptest::prelude::*;

    fn hdr(k: &str, v: &str) -> (String, String) {
        (k.to_string(), v.to_string())
    }

    /// A valid `Retry-After` header (integer seconds).
    fn retry_after_header() -> impl Strategy<Value = (String, String)> {
        (0u64..10_000u64).prop_map(|n| hdr("Retry-After", &n.to_string()))
    }
    /// An arbitrary header pair, header name from a finite set that
    /// includes the four the function recognises and one it does not.
    fn arbitrary_header() -> impl Strategy<Value = (String, String)> {
        (
            prop::sample::select(vec![
                "Retry-After",
                "retry-after",
                "RETRY-AFTER",
                "retry-after-ms",
                "x-ratelimit-reset",
                "x-request-id",
                "content-type",
            ]),
            "[0-9a-zA-Z .]{0,20}",
        )
            .prop_map(|(k, v)| (k.to_string(), v.to_string()))
    }

    proptest! {
        /// **The primary invariant.** `cap_declined` is true if and
        /// only if `delay` is present and greater than `HINT_CAP`.
        #[test]
        fn prop_cap_declined_consistent_with_delay(headers in prop::collection::vec(arbitrary_header(), 0..=3)) {
            let r = extract_retry_hints(None, &headers, "");
            let expected = r.delay.map_or(false, |d| d > HINT_CAP);
            prop_assert_eq!(
                r.cap_declined, expected,
                "cap_declined={} but delay={:?} vs HINT_CAP={:?}",
                r.cap_declined, r.delay, HINT_CAP,
            );
        }

        /// No hints at all (empty headers, empty body) is the
        /// default: no delay, no cap declined.
        #[test]
        fn prop_empty_is_default(_dummy in 0u8..1) {
            let r = extract_retry_hints(None, &[], "");
            prop_assert_eq!(r.delay, None);
            prop_assert_eq!(r.cap_declined, false);
        }

        /// A `Retry-After` header alone yields `delay >= parsed` and
        /// the cap follows. With an integer value, the parsed delay
        /// is exactly that many seconds.
        #[test]
        fn prop_retry_after_is_read(
            n in 0u64..10_000u64,
        ) {
            let headers = vec![hdr("Retry-After", &n.to_string())];
            let r = extract_retry_hints(Some(429), &headers, "");
            prop_assert_eq!(r.delay, Some(Duration::from_secs(n)));
            prop_assert_eq!(r.cap_declined, n > HINT_CAP.as_secs());
        }

        /// Adding a non-hint header never lowers `delay`. Unknown
        /// header names are ignored by the parser.
        #[test]
        fn prop_unknown_header_is_ignored(
            v in "[0-9a-zA-Z .]{0,20}",
        ) {
            let baseline = extract_retry_hints(None, &[], "");
            let with_unknown = extract_retry_hints(None, &[hdr("x-request-id", &v)], "");
            prop_assert_eq!(baseline.delay, with_unknown.delay);
            prop_assert_eq!(baseline.cap_declined, with_unknown.cap_declined);
        }

        /// Adding a valid hint to an existing header set does not
        /// lower the reported `delay`. The max-of-push behaviour.
        #[test]
        fn prop_adding_a_hint_does_not_lower_delay(
            existing in prop::collection::vec(retry_after_header(), 0..=3),
            extra_n in 0u64..10_000u64,
        ) {
            let baseline = extract_retry_hints(None, &existing, "");
            let mut augmented = existing.clone();
            augmented.push(hdr("Retry-After", &extra_n.to_string()));
            let after = extract_retry_hints(None, &augmented, "");
            match (baseline.delay, after.delay) {
                (None, _) => {}
                (Some(b), Some(a)) => prop_assert!(
                    a >= b,
                    "adding a hint lowered delay: {:?} -> {:?}", b, a,
                ),
                (Some(_), None) => prop_assert!(false, "adding a hint made delay vanish"),
            }
        }

        /// Determinism.
        #[test]
        fn prop_deterministic(headers in prop::collection::vec(arbitrary_header(), 0..=4)) {
            let a = extract_retry_hints(None, &headers, "");
            let b = extract_retry_hints(None, &headers, "");
            prop_assert_eq!(a.delay, b.delay);
            prop_assert_eq!(a.cap_declined, b.cap_declined);
        }

        /// `retry-after-ms` produces a sub-second delay for values
        /// below 1000ms, and the cap follows exactly as for
        /// `retry-after`.
        #[test]
        fn prop_retry_after_ms_is_read(
            n in 0u64..60_000u64,
        ) {
            let headers = vec![hdr("retry-after-ms", &n.to_string())];
            let r = extract_retry_hints(Some(429), &headers, "");
            prop_assert_eq!(r.delay, Some(Duration::from_millis(n)));
            prop_assert_eq!(r.cap_declined, Duration::from_millis(n) > HINT_CAP);
        }

        /// Header names are case-insensitive: `Retry-After`,
        /// `retry-after`, and `RETRY-AFTER` are equivalent.
        #[test]
        fn prop_header_case_insensitive(n in 0u64..10_000u64) {
            let v = n.to_string();
            let lower = extract_retry_hints(None, &[hdr("retry-after", &v)], "");
            let upper = extract_retry_hints(None, &[hdr("RETRY-AFTER", &v)], "");
            let mixed = extract_retry_hints(None, &[hdr("Retry-After", &v)], "");
            prop_assert_eq!(lower.delay, upper.delay);
            prop_assert_eq!(lower.delay, mixed.delay);
        }
    }

    /// Control: the boundary at `HINT_CAP` exactly. `HINT_CAP`
    /// seconds does NOT decline (the implementation uses `>`, not
    /// `>=`). `HINT_CAP + 1` seconds does. Pinning both sides means
    /// a change to either `>` or `>=` shows up here.
    #[test]
    fn control_cap_boundary_is_exact() {
        let cap_secs = HINT_CAP.as_secs();

        let at_cap =
            extract_retry_hints(Some(429), &[hdr("Retry-After", &cap_secs.to_string())], "");
        assert_eq!(at_cap.delay, Some(HINT_CAP));
        assert!(
            !at_cap.cap_declined,
            "a delay of exactly HINT_CAP must NOT decline (uses > not >=)",
        );

        let over_cap = extract_retry_hints(
            Some(429),
            &[hdr("Retry-After", &(cap_secs + 1).to_string())],
            "",
        );
        assert_eq!(over_cap.delay, Some(HINT_CAP + Duration::from_secs(1)));
        assert!(over_cap.cap_declined);
    }

    /// Control: `retry-after-ms` at exactly `HINT_CAP` in millis
    /// also does not decline. Same `>` boundary in the millisecond
    /// path.
    #[test]
    fn control_ms_boundary_is_exact() {
        let cap_ms = HINT_CAP.as_millis() as u64;
        let at_cap =
            extract_retry_hints(Some(429), &[hdr("retry-after-ms", &cap_ms.to_string())], "");
        assert_eq!(at_cap.delay, Some(HINT_CAP));
        assert!(!at_cap.cap_declined);

        let over_cap = extract_retry_hints(
            Some(429),
            &[hdr("retry-after-ms", &(cap_ms + 1).to_string())],
            "",
        );
        assert_eq!(over_cap.delay, Some(HINT_CAP + Duration::from_millis(1)));
        assert!(over_cap.cap_declined);
    }

    /// Control: an unrecognised `Retry-After` value (HTTP-date form
    /// not parsed in this module) contributes no hint, so the
    /// result is the default.
    #[test]
    fn control_http_date_retry_after_is_ignored() {
        let r = extract_retry_hints(
            Some(429),
            &[hdr("Retry-After", "Wed, 21 Oct 2015 07:28:00 GMT")],
            "",
        );
        assert_eq!(r.delay, None);
        assert!(!r.cap_declined);
    }
}
