use super::*;

#[test]
fn scaled_timeout_grows_with_max_tokens() {
    // BASE (60) + max_tokens / 40.
    assert_eq!(scaled_timeout_secs(0), 60);
    assert_eq!(scaled_timeout_secs(1024), 60 + 25);
    assert_eq!(scaled_timeout_secs(8192), 60 + 204);
    // Monotonic: a larger token budget never shrinks the ceiling.
    assert!(scaled_timeout_secs(16384) > scaled_timeout_secs(8192));
}

#[test]
fn parse_retry_after_reads_integer_seconds() {
    let mut h = HeaderMap::new();
    h.insert("retry-after", HeaderValue::from_static("30"));
    assert_eq!(parse_retry_after(&h), Some(30));
}

#[test]
fn parse_retry_after_absent_or_garbage_is_none() {
    let empty = HeaderMap::new();
    assert_eq!(parse_retry_after(&empty), None);
    let mut h = HeaderMap::new();
    h.insert("retry-after", HeaderValue::from_static("soon"));
    assert_eq!(parse_retry_after(&h), None);
}

#[test]
fn request_id_prefers_request_id_then_x_request_id() {
    let mut h = HeaderMap::new();
    h.insert("x-request-id", HeaderValue::from_static("xreq-1"));
    assert_eq!(request_id_header(&h).as_deref(), Some("xreq-1"));
    h.insert("request-id", HeaderValue::from_static("req-1"));
    assert_eq!(request_id_header(&h).as_deref(), Some("req-1"));
}

#[test]
fn classify_status_typed_reasons() {
    let r = |c: u16, ra: Option<u64>| classify_status(c, "body".to_string(), ra);
    assert!(matches!(
        r(429, Some(5)),
        LlmError::Retryable {
            reason: RetryableReason::RateLimited { retry_after: Some(5) }
        }
    ));
    assert!(matches!(
        r(529, None),
        LlmError::Retryable {
            reason: RetryableReason::Overloaded
        }
    ));
    assert!(matches!(
        r(503, None),
        LlmError::Retryable {
            reason: RetryableReason::ServerError { status: 503 }
        }
    ));
    assert!(matches!(
        r(401, None),
        LlmError::Fatal {
            reason: FatalReason::Auth(_)
        }
    ));
    assert!(matches!(
        r(400, None),
        LlmError::Fatal {
            reason: FatalReason::BadRequest(_)
        }
    ));
}
