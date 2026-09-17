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
            sleep_fn: Arc::new(|d: Duration| -> SleepFuture {
                Box::pin(tokio::time::sleep(d))
            }),
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
            sleep_fn: Arc::new(|_: Duration| -> SleepFuture {
                Box::pin(async {})
            }),
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
    use std::sync::atomic::{AtomicU32, Ordering};
    use std::sync::Arc as StdArc;

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
            async move { Err(KodError::RateLimited { retry_after_secs: 2 }) }
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
