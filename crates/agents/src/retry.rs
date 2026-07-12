//! Bounded retry policy for transient LLM failures.
//!
//! The `llm` crate surfaces a typed retry taxonomy (`LlmError::Retryable`
//! carrying a `RetryableReason`) but deliberately owns no retry *policy*:
//! per its `CLAUDE.md`, "Retry / escalation / advisor strategy selection"
//! belongs to `agents`. This module is that policy — the first consumer of
//! the taxonomy. Before it, a single HTTP 429 propagated straight out of a
//! `complete_free` call site and killed the whole Work run.
//!
//! The helper is a bounded combinator: it re-issues the caller's identical
//! call on a `Retryable` error, backing off between attempts (honoring a
//! `retry-after` header for `RateLimited`, exponential backoff otherwise),
//! up to `max_attempts` total. A `Fatal` error short-circuits immediately —
//! retrying it would fail identically. The attempt count is hard-bounded, so
//! the loop can never spin forever.

use std::future::Future;
use std::time::Duration;

use llm::{LlmError, RetryableReason};
use tracing::warn;

/// Total attempts (initial call + retries) before a persistent transient
/// failure is surfaced to the caller.
pub const RETRY_MAX_ATTEMPTS: u32 = 5;

/// Base delay for exponential backoff (the wait after the first failure).
pub const RETRY_BASE_DELAY_MS: u64 = 500;

/// Ceiling on any single backoff wait. Caps both the exponential growth and
/// a server-supplied `retry-after` so one hostile header can't park a task
/// for an unbounded time.
pub const RETRY_MAX_DELAY_MS: u64 = 60_000;

/// Shift cap for the exponential factor, so `1 << shift` can never overflow
/// `u64`. The delay saturates at `max_delay` long before this bites; it is a
/// pure overflow guard.
const RETRY_MAX_SHIFT: u32 = 20;

/// Bounded-retry policy: how many attempts and how long to back off. Default
/// values come from the module consts; tests construct a fast policy so the
/// bounded-retry behavior is exercised without real sleeps.
#[derive(Debug, Clone)]
pub struct RetryPolicy {
    /// Total attempts (initial + retries). Must be `>= 1`.
    pub max_attempts: u32,
    /// Base delay for exponential backoff.
    pub base_delay: Duration,
    /// Hard ceiling on any single backoff wait.
    pub max_delay: Duration,
}

impl Default for RetryPolicy {
    fn default() -> Self {
        Self {
            max_attempts: RETRY_MAX_ATTEMPTS,
            base_delay: Duration::from_millis(RETRY_BASE_DELAY_MS),
            max_delay: Duration::from_millis(RETRY_MAX_DELAY_MS),
        }
    }
}

impl RetryPolicy {
    /// Backoff duration before the retry that follows `attempt` (1-based: the
    /// attempt that just failed). A `RateLimited` reason with a parsed
    /// `retry-after` honors it (capped at `max_delay`); every other reason
    /// uses exponential backoff `base_delay * 2^(attempt-1)`, capped at
    /// `max_delay`.
    fn backoff(&self, attempt: u32, reason: &RetryableReason) -> Duration {
        if let RetryableReason::RateLimited {
            retry_after: Some(secs),
        } = reason
        {
            return Duration::from_secs(*secs).min(self.max_delay);
        }
        let shift = attempt.saturating_sub(1).min(RETRY_MAX_SHIFT);
        let factor = 1u64 << shift;
        let base_ms = self.base_delay.as_millis() as u64;
        Duration::from_millis(base_ms.saturating_mul(factor)).min(self.max_delay)
    }
}

/// Run `op` under the bounded retry `policy`. `op` is re-invoked (producing a
/// fresh future each attempt) on `LlmError::Retryable`, with a backoff sleep
/// between attempts; `Ok` and `LlmError::Fatal` return immediately. After
/// `policy.max_attempts` retryable failures the last error is surfaced.
///
/// The caller supplies the identical call each time (same prompt, same
/// messages): retrying a transient 429/5xx/network blip is safe precisely
/// because the inputs don't change.
pub async fn with_llm_retry<T, F, Fut>(policy: &RetryPolicy, mut op: F) -> Result<T, LlmError>
where
    F: FnMut() -> Fut,
    Fut: Future<Output = Result<T, LlmError>>,
{
    let mut attempt: u32 = 1;
    loop {
        match op().await {
            Ok(value) => return Ok(value),
            Err(LlmError::Retryable { reason }) => {
                if attempt >= policy.max_attempts {
                    warn!(
                        attempt,
                        max_attempts = policy.max_attempts,
                        %reason,
                        "LLM retry budget exhausted; surfacing retryable error"
                    );
                    return Err(LlmError::Retryable { reason });
                }
                let delay = policy.backoff(attempt, &reason);
                warn!(
                    attempt,
                    max_attempts = policy.max_attempts,
                    %reason,
                    delay_ms = delay.as_millis() as u64,
                    "retryable LLM failure; backing off before retry"
                );
                tokio::time::sleep(delay).await;
                attempt = attempt.saturating_add(1);
            }
            Err(fatal) => return Err(fatal),
        }
    }
}

#[cfg(test)]
mod tests;
