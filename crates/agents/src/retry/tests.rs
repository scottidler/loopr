use std::cell::Cell;
use std::time::Duration;

use llm::{FatalReason, LlmError, RetryableReason};

use super::*;

/// A fast policy so bounded-retry behavior is exercised without real waits.
fn fast_policy(max_attempts: u32) -> RetryPolicy {
    RetryPolicy {
        max_attempts,
        base_delay: Duration::from_millis(0),
        max_delay: Duration::from_millis(0),
    }
}

fn rate_limited(retry_after: Option<u64>) -> LlmError {
    LlmError::Retryable {
        reason: RetryableReason::RateLimited { retry_after },
    }
}

#[tokio::test]
async fn rate_limited_then_ok_completes_within_budget() {
    // Success criterion: a scripted 429-then-200 sequence completes rather
    // than killing the run. Break-to-prove: on the pre-Phase-3 call sites
    // the first 429 propagated out via `?` and this would have been an Err.
    let calls = Cell::new(0u32);
    let result: Result<&str, LlmError> = with_llm_retry(&fast_policy(5), || {
        let n = calls.get();
        calls.set(n + 1);
        async move { if n == 0 { Err(rate_limited(None)) } else { Ok("ok") } }
    })
    .await;
    assert_eq!(result.unwrap(), "ok");
    assert_eq!(calls.get(), 2, "one retry after the 429, then success");
}

#[tokio::test]
async fn persistent_retryable_is_bounded_not_infinite() {
    // Break-to-prove the bound: an always-429 op must terminate after exactly
    // `max_attempts` calls and surface the retryable error. If the loop were
    // unbounded this test would hang instead of failing.
    let calls = Cell::new(0u32);
    let result: Result<(), LlmError> = with_llm_retry(&fast_policy(4), || {
        calls.set(calls.get() + 1);
        async { Err(rate_limited(Some(1))) }
    })
    .await;
    assert!(matches!(result, Err(LlmError::Retryable { .. })));
    assert_eq!(calls.get(), 4, "initial call + 3 retries == max_attempts");
}

#[tokio::test]
async fn fatal_error_short_circuits_without_retry() {
    let calls = Cell::new(0u32);
    let result: Result<(), LlmError> = with_llm_retry(&fast_policy(5), || {
        calls.set(calls.get() + 1);
        async {
            Err(LlmError::Fatal {
                reason: FatalReason::Auth("bad key".to_string()),
            })
        }
    })
    .await;
    assert!(matches!(result, Err(LlmError::Fatal { .. })));
    assert_eq!(calls.get(), 1, "a fatal error is never retried");
}

#[tokio::test]
async fn immediate_ok_calls_op_once() {
    let calls = Cell::new(0u32);
    let result: Result<u8, LlmError> = with_llm_retry(&fast_policy(5), || {
        calls.set(calls.get() + 1);
        async { Ok(7) }
    })
    .await;
    assert_eq!(result.unwrap(), 7);
    assert_eq!(calls.get(), 1);
}

#[test]
fn backoff_honors_retry_after_capped_at_max_delay() {
    let policy = RetryPolicy {
        max_attempts: 5,
        base_delay: Duration::from_millis(500),
        max_delay: Duration::from_secs(10),
    };
    // Below the cap: honored verbatim.
    assert_eq!(
        policy.backoff(1, &RetryableReason::RateLimited { retry_after: Some(3) }),
        Duration::from_secs(3)
    );
    // Above the cap: clamped to max_delay so a hostile header can't park us.
    assert_eq!(
        policy.backoff(1, &RetryableReason::RateLimited { retry_after: Some(600) }),
        Duration::from_secs(10)
    );
}

#[test]
fn backoff_is_exponential_and_capped() {
    let policy = RetryPolicy {
        max_attempts: 10,
        base_delay: Duration::from_millis(500),
        max_delay: Duration::from_secs(10),
    };
    let net = RetryableReason::Network {
        detail: "blip".to_string(),
    };
    assert_eq!(policy.backoff(1, &net), Duration::from_millis(500));
    assert_eq!(policy.backoff(2, &net), Duration::from_millis(1000));
    assert_eq!(policy.backoff(3, &net), Duration::from_millis(2000));
    // Grows until it saturates at max_delay, never beyond.
    assert_eq!(policy.backoff(20, &net), Duration::from_secs(10));
}

#[test]
fn default_policy_uses_module_consts() {
    let policy = RetryPolicy::default();
    assert_eq!(policy.max_attempts, RETRY_MAX_ATTEMPTS);
    assert_eq!(policy.base_delay, Duration::from_millis(RETRY_BASE_DELAY_MS));
    assert_eq!(policy.max_delay, Duration::from_millis(RETRY_MAX_DELAY_MS));
}
