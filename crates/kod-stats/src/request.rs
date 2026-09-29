//! Per-request analytics and their aggregates (borrow from oh-my-pi,
//! delta §13.2).
//!
//! One [`RequestRecord`] per provider call; [`Aggregates`] folds them
//! into the numbers a `/stats` surface shows.
//!
//! # Cache savings
//!
//! `(input_rate - cache_read_rate) * cached_tokens` minus the
//! cache-write premium `(cache_write_rate - input_rate) *
//! written_tokens`. It can go **negative** when a session wrote a
//! cache it did not live long enough to read back.

/// One provider call.
#[derive(Debug, Clone, PartialEq)]
pub struct RequestRecord {
    pub model: String,
    pub endpoint: String,
    pub duration_ms: u64,
    pub ttft_ms: Option<u64>,
    pub stop_reason: Option<String>,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cache_read_tokens: u64,
    pub cache_write_tokens: u64,
    pub input_rate: f64,
    pub cache_read_rate: f64,
    pub cache_write_rate: f64,
    pub error: bool,
}

impl RequestRecord {
    /// USD saved by cache reads minus the write premium.
    pub fn cache_savings_usd(&self) -> f64 {
        let read_saved = (self.input_rate - self.cache_read_rate) * (self.cache_read_tokens as f64)
            / 1_000_000.0;
        let write_premium = (self.cache_write_rate - self.input_rate)
            * (self.cache_write_tokens as f64)
            / 1_000_000.0;
        read_saved - write_premium
    }

    /// Cached fraction of the input window. `None` with no input.
    pub fn cache_rate(&self) -> Option<f64> {
        let total = self.input_tokens + self.cache_read_tokens;
        if total == 0 {
            return None;
        }
        Some(self.cache_read_tokens as f64 / total as f64)
    }

    /// Output tokens per second. `None` with no duration or on error.
    pub fn tokens_per_second(&self) -> Option<f64> {
        if self.duration_ms == 0 || self.error {
            return None;
        }
        Some(self.output_tokens as f64 / (self.duration_ms as f64 / 1000.0))
    }
}

/// The fold over a set of records.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Aggregates {
    pub requests: u64,
    pub errors: u64,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cache_read_tokens: u64,
    pub cache_write_tokens: u64,
    pub total_duration_ms: u64,
    pub total_cache_savings_usd: f64,
    pub ttft_sum_ms: u64,
    pub ttft_count: u64,
}

impl Aggregates {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn observe(&mut self, r: &RequestRecord) {
        self.requests += 1;
        if r.error {
            self.errors += 1;
        }
        self.input_tokens += r.input_tokens;
        self.output_tokens += r.output_tokens;
        self.cache_read_tokens += r.cache_read_tokens;
        self.cache_write_tokens += r.cache_write_tokens;
        self.total_duration_ms += r.duration_ms;
        self.total_cache_savings_usd += r.cache_savings_usd();
        if let Some(t) = r.ttft_ms {
            self.ttft_sum_ms += t;
            self.ttft_count += 1;
        }
    }

    pub fn error_rate(&self) -> Option<f64> {
        if self.requests == 0 {
            return None;
        }
        Some(self.errors as f64 / self.requests as f64)
    }

    pub fn avg_ttft_ms(&self) -> Option<f64> {
        if self.ttft_count == 0 {
            return None;
        }
        Some(self.ttft_sum_ms as f64 / self.ttft_count as f64)
    }

    pub fn cache_rate(&self) -> Option<f64> {
        let total = self.input_tokens + self.cache_read_tokens;
        if total == 0 {
            return None;
        }
        Some(self.cache_read_tokens as f64 / total as f64)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record() -> RequestRecord {
        RequestRecord {
            model: "m".to_string(),
            endpoint: "e".to_string(),
            duration_ms: 1000,
            ttft_ms: Some(200),
            stop_reason: Some("end_turn".to_string()),
            input_tokens: 1000,
            output_tokens: 100,
            cache_read_tokens: 0,
            cache_write_tokens: 0,
            input_rate: 3.0,
            cache_read_rate: 0.3,
            cache_write_rate: 3.75,
            error: false,
        }
    }

    #[test]
    fn cache_savings_are_positive_for_reads() {
        let mut r = record();
        r.cache_read_tokens = 500_000;
        assert!((r.cache_savings_usd() - 1.35).abs() < 1e-9);
    }

    #[test]
    fn cache_savings_go_negative_for_a_write_only_session() {
        let mut r = record();
        r.cache_write_tokens = 500_000;
        assert!(r.cache_savings_usd() < 0.0);
    }

    #[test]
    fn cache_rate_is_none_with_no_input() {
        let mut r = record();
        r.input_tokens = 0;
        r.cache_read_tokens = 0;
        assert_eq!(r.cache_rate(), None);
    }

    #[test]
    fn cache_rate_is_the_cached_fraction() {
        let mut r = record();
        r.input_tokens = 500;
        r.cache_read_tokens = 500;
        assert_eq!(r.cache_rate(), Some(0.5));
    }

    #[test]
    fn tokens_per_second_is_computed() {
        assert_eq!(record().tokens_per_second(), Some(100.0));
    }

    #[test]
    fn tokens_per_second_is_none_on_error() {
        let mut r = record();
        r.error = true;
        assert_eq!(r.tokens_per_second(), None);
    }

    #[test]
    fn tokens_per_second_is_none_with_no_duration() {
        let mut r = record();
        r.duration_ms = 0;
        assert_eq!(r.tokens_per_second(), None);
    }

    #[test]
    fn aggregates_sum_the_counters() {
        let mut a = Aggregates::new();
        a.observe(&record());
        a.observe(&record());
        assert_eq!(
            (
                a.requests,
                a.input_tokens,
                a.output_tokens,
                a.total_duration_ms
            ),
            (2, 2000, 200, 2000)
        );
    }

    #[test]
    fn error_rate_counts_errors() {
        let mut a = Aggregates::new();
        a.observe(&record());
        let mut e = record();
        e.error = true;
        a.observe(&e);
        assert_eq!(a.error_rate(), Some(0.5));
    }

    #[test]
    fn error_rate_is_none_with_no_calls() {
        assert_eq!(Aggregates::new().error_rate(), None);
    }

    #[test]
    fn avg_ttft_uses_the_ttft_denominator() {
        let mut a = Aggregates::new();
        a.observe(&record());
        let mut r2 = record();
        r2.ttft_ms = None;
        a.observe(&r2);
        assert_eq!(a.avg_ttft_ms(), Some(200.0));
    }

    #[test]
    fn avg_ttft_is_none_when_none_reported() {
        let mut a = Aggregates::new();
        let mut r = record();
        r.ttft_ms = None;
        a.observe(&r);
        assert_eq!(a.avg_ttft_ms(), None);
    }

    #[test]
    fn aggregate_cache_rate_is_the_overall_fraction() {
        let mut a = Aggregates::new();
        let mut r = record();
        r.input_tokens = 500;
        r.cache_read_tokens = 500;
        a.observe(&r);
        assert_eq!(a.cache_rate(), Some(0.5));
    }

    #[test]
    fn total_cache_savings_accumulate() {
        let mut a = Aggregates::new();
        let mut r = record();
        r.cache_read_tokens = 1_000_000;
        a.observe(&r);
        a.observe(&r);
        assert!((a.total_cache_savings_usd - 5.4).abs() < 1e-9);
    }
}
