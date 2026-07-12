//! Phase 3 success criterion: a scripted 429-then-200 client, driven
//! through the public `with_llm_retry` combinator, completes rather than
//! killing the run — and a persistently-429 client is bounded, not infinite.
//!
//! This exercises the real `LlmClient::complete_free` seam (via the
//! `ScriptedLlm` stub) that the implementer / reviewer / director call sites
//! now wrap, not just a bare closure.

use std::time::Duration;

use agents::{RetryPolicy, with_llm_retry};
use llm::{LlmClient, LlmError, RetryableReason, ScriptedLlm};

fn fast_policy(max_attempts: u32) -> RetryPolicy {
    RetryPolicy {
        max_attempts,
        base_delay: Duration::from_millis(0),
        max_delay: Duration::from_millis(0),
    }
}

fn rate_limited() -> LlmError {
    LlmError::Retryable {
        reason: RetryableReason::RateLimited { retry_after: None },
    }
}

#[tokio::test]
async fn scripted_429_then_200_completes() {
    let stub = ScriptedLlm::new();
    stub.queue_free(Err(rate_limited())); // first call: HTTP 429
    stub.queue_free(Ok("done".to_string())); // retry: HTTP 200

    let (raw, _usage) = with_llm_retry(&fast_policy(5), || stub.complete_free("sys", &[], None))
        .await
        .expect("429-then-200 must complete, not kill the run");

    assert_eq!(raw, "done");
    let (_tool, free) = stub.remaining();
    assert_eq!(free, 0, "both scripted responses consumed");
}

#[tokio::test]
async fn scripted_persistent_429_is_bounded() {
    let stub = ScriptedLlm::new();
    // Queue exactly max_attempts 429s: the helper must stop at the budget and
    // surface the retryable error rather than draining forever / panicking on
    // an empty queue.
    for _ in 0..3 {
        stub.queue_free(Err(rate_limited()));
    }

    let result = with_llm_retry(&fast_policy(3), || stub.complete_free("sys", &[], None)).await;

    assert!(matches!(result, Err(LlmError::Retryable { .. })));
    let (_tool, free) = stub.remaining();
    assert_eq!(free, 0, "exactly max_attempts calls were made");
}
