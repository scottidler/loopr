#![allow(clippy::unwrap_used)]

#[test]
fn test_is_correctable_error_classification() {
    use crate::agents::implementer::is_correctable_error;

    // Correctable errors (schema/path issues the LLM can fix)
    assert!(is_correctable_error("missing field `summary` in Done action"));
    assert!(is_correctable_error("unknown field `files`"));
    assert!(is_correctable_error("path escapes sandbox: ../../etc"));
    assert!(is_correctable_error("unknown tool: cargo_test"));

    // Non-correctable errors (require full-iteration reasoning)
    assert!(!is_correctable_error("cargo test failed with exit code 101"));
    assert!(!is_correctable_error("error[E0308]: mismatched types"));
    assert!(!is_correctable_error("network timeout"));
}

#[test]
fn test_lifeguard_escalates_after_max_requeries_exceeded() {
    use crate::agents::lifeguard::{Lifeguard, Verdict};

    let mut lg = Lifeguard::new();

    // max_parse_retries = 3 in Lifeguard::new()
    // After 3 parse failures, it should continue (threshold is >3)
    assert_eq!(lg.record_parse_failure(), Verdict::Continue);
    assert_eq!(lg.record_parse_failure(), Verdict::Continue);
    assert_eq!(lg.record_parse_failure(), Verdict::Continue);
    // 4th failure exceeds threshold -> escalate
    assert!(matches!(lg.record_parse_failure(), Verdict::Escalate(_)));
}
