//! Rate limit state management for coordinated backoff.
//!
//! When the Anthropic API returns 429 (rate limited), all loops must back off
//! globally to prevent hammering the API. This module provides shared state
//! for coordinated rate limiting.

use std::time::{Duration, Instant};

/// Global rate limit state shared across all loops.
#[derive(Debug)]
pub struct RateLimitState {
    /// When we can resume API calls (None = no active limit).
    pub backoff_until: Option<Instant>,
    /// Number of consecutive rate limit hits.
    pub consecutive_hits: u32,
    /// Last successful API call time.
    pub last_success: Option<Instant>,
}

impl RateLimitState {
    /// Create a new rate limit state.
    pub fn new() -> Self {
        Self {
            backoff_until: None,
            consecutive_hits: 0,
            last_success: None,
        }
    }

    /// Check if we are currently rate limited.
    pub fn is_rate_limited(&self) -> bool {
        self.backoff_until.map(|until| Instant::now() < until).unwrap_or(false)
    }

    /// Get remaining backoff duration if rate limited.
    pub fn remaining_backoff(&self) -> Option<Duration> {
        self.backoff_until.and_then(|until| {
            let now = Instant::now();
            if now < until { Some(until - now) } else { None }
        })
    }

    /// Record a rate limit response.
    ///
    /// Uses exponential backoff: the delay is the maximum of:
    /// - The API's suggested retry_after
    /// - 2^consecutive_hits seconds (capped at 64s)
    pub fn record_rate_limit(&mut self, retry_after: Duration) {
        self.consecutive_hits += 1;

        // Exponential backoff: 2^hits seconds, capped at 64s
        let exp_backoff_secs = 2u64.pow(self.consecutive_hits.min(6));
        let exp_backoff = Duration::from_secs(exp_backoff_secs);

        // Use max of API's retry_after and our calculated delay
        let delay = retry_after.max(exp_backoff);

        self.backoff_until = Some(Instant::now() + delay);

        tracing::warn!(
            retry_after_secs = delay.as_secs(),
            consecutive_hits = self.consecutive_hits,
            "Rate limited, backing off globally"
        );
    }

    /// Record a successful API call.
    ///
    /// Resets the consecutive hit counter and clears the backoff.
    pub fn record_success(&mut self) {
        self.consecutive_hits = 0;
        self.backoff_until = None;
        self.last_success = Some(Instant::now());
    }

    /// Reset the rate limit state.
    pub fn reset(&mut self) {
        self.backoff_until = None;
        self.consecutive_hits = 0;
        // Keep last_success for metrics
    }

    /// Get time since last successful call.
    pub fn time_since_success(&self) -> Option<Duration> {
        self.last_success.map(|t| t.elapsed())
    }
}

impl Default for RateLimitState {
    fn default() -> Self {
        Self::new()
    }
}

/// Rate limit configuration.
#[derive(Debug, Clone)]
pub struct RateLimitConfig {
    /// Initial backoff on first rate limit (seconds).
    pub initial_backoff_secs: u64,
    /// Maximum backoff (seconds).
    pub max_backoff_secs: u64,
    /// How long to wait after backoff before resuming full speed.
    pub recovery_period_secs: u64,
    /// Max concurrent API calls (soft limit to prevent rate limits).
    pub max_concurrent_api_calls: usize,
}

impl Default for RateLimitConfig {
    fn default() -> Self {
        Self {
            initial_backoff_secs: 5,
            max_backoff_secs: 120,
            recovery_period_secs: 30,
            max_concurrent_api_calls: 10,
        }
    }
}

impl RateLimitConfig {
    /// Create config with custom values.
    pub fn new(
        initial_backoff_secs: u64,
        max_backoff_secs: u64,
        recovery_period_secs: u64,
        max_concurrent_api_calls: usize,
    ) -> Self {
        Self {
            initial_backoff_secs,
            max_backoff_secs,
            recovery_period_secs,
            max_concurrent_api_calls,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::thread;

    #[test]
    fn test_rate_limit_state_new() {
        let state = RateLimitState::new();
        assert!(!state.is_rate_limited());
        assert_eq!(state.consecutive_hits, 0);
        assert!(state.last_success.is_none());
    }

    #[test]
    fn test_record_rate_limit() {
        let mut state = RateLimitState::new();

        state.record_rate_limit(Duration::from_secs(5));

        assert!(state.is_rate_limited());
        assert_eq!(state.consecutive_hits, 1);
        assert!(state.backoff_until.is_some());
    }

    #[test]
    fn test_record_success_clears_backoff() {
        let mut state = RateLimitState::new();

        state.record_rate_limit(Duration::from_secs(5));
        assert!(state.is_rate_limited());

        state.record_success();

        assert!(!state.is_rate_limited());
        assert_eq!(state.consecutive_hits, 0);
        assert!(state.last_success.is_some());
    }

    #[test]
    fn test_exponential_backoff() {
        let mut state = RateLimitState::new();

        // First hit: 2^1 = 2 seconds (or retry_after if larger)
        state.record_rate_limit(Duration::from_secs(0));
        let first_hits = state.consecutive_hits;

        state.record_rate_limit(Duration::from_secs(0));
        let second_hits = state.consecutive_hits;

        assert_eq!(first_hits, 1);
        assert_eq!(second_hits, 2);
    }

    #[test]
    fn test_remaining_backoff() {
        let mut state = RateLimitState::new();

        // Not rate limited
        assert!(state.remaining_backoff().is_none());

        // Rate limited
        state.record_rate_limit(Duration::from_secs(10));
        let remaining = state.remaining_backoff();
        assert!(remaining.is_some());
        assert!(remaining.unwrap() <= Duration::from_secs(10));
    }

    #[test]
    fn test_backoff_expires() {
        let mut state = RateLimitState::new();

        // Very short backoff
        state.backoff_until = Some(Instant::now() + Duration::from_millis(10));
        assert!(state.is_rate_limited());

        // Wait for it to expire
        thread::sleep(Duration::from_millis(20));
        assert!(!state.is_rate_limited());
    }

    #[test]
    fn test_reset() {
        let mut state = RateLimitState::new();

        state.record_rate_limit(Duration::from_secs(60));
        state.record_success();

        state.record_rate_limit(Duration::from_secs(60));
        assert!(state.is_rate_limited());
        assert_eq!(state.consecutive_hits, 1);

        state.reset();

        assert!(!state.is_rate_limited());
        assert_eq!(state.consecutive_hits, 0);
        // last_success should be preserved
        assert!(state.last_success.is_some());
    }

    #[test]
    fn test_time_since_success() {
        let mut state = RateLimitState::new();

        // No success yet
        assert!(state.time_since_success().is_none());

        // Record success
        state.record_success();
        thread::sleep(Duration::from_millis(10));

        let elapsed = state.time_since_success();
        assert!(elapsed.is_some());
        assert!(elapsed.unwrap() >= Duration::from_millis(10));
    }

    #[test]
    fn test_rate_limit_config_default() {
        let config = RateLimitConfig::default();
        assert_eq!(config.initial_backoff_secs, 5);
        assert_eq!(config.max_backoff_secs, 120);
        assert_eq!(config.recovery_period_secs, 30);
        assert_eq!(config.max_concurrent_api_calls, 10);
    }

    #[test]
    fn test_rate_limit_config_custom() {
        let config = RateLimitConfig::new(10, 300, 60, 20);
        assert_eq!(config.initial_backoff_secs, 10);
        assert_eq!(config.max_backoff_secs, 300);
        assert_eq!(config.recovery_period_secs, 60);
        assert_eq!(config.max_concurrent_api_calls, 20);
    }

    #[test]
    fn test_use_larger_of_retry_after_and_exponential() {
        let mut state = RateLimitState::new();

        // API says wait 100 seconds, but our exponential is only 2^1 = 2
        state.record_rate_limit(Duration::from_secs(100));

        // Should use the larger (100s from API)
        if let Some(remaining) = state.remaining_backoff() {
            assert!(remaining >= Duration::from_secs(90)); // Allow some slack for test timing
        }
    }
}
