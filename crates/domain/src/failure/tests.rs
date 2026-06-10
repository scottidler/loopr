use super::*;

#[test]
fn unit_variants_round_trip_kebab_case() {
    for (reason, tag) in [
        (FailureReason::TokenBudget, "\"token-budget\""),
        (FailureReason::ReviewerRejection, "\"reviewer-rejection\""),
        (FailureReason::AcUnmet, "\"ac-unmet\""),
        (FailureReason::Panic, "\"panic\""),
        (FailureReason::CrashInterrupted, "\"crash-interrupted\""),
    ] {
        let json = serde_json::to_string(&reason).unwrap();
        assert_eq!(json, tag);
        let back: FailureReason = serde_json::from_str(&json).unwrap();
        assert_eq!(back, reason);
    }
}

#[test]
fn tool_failure_carries_tool_name() {
    let reason = FailureReason::ToolFailure { tool: "bash".to_string() };
    let json = serde_json::to_string(&reason).unwrap();
    assert_eq!(json, r#"{"tool-failure":{"tool":"bash"}}"#);
    let back: FailureReason = serde_json::from_str(&json).unwrap();
    assert_eq!(back, reason);
}

#[test]
fn other_carries_detail() {
    let reason = FailureReason::Other("disk full".to_string());
    let json = serde_json::to_string(&reason).unwrap();
    assert_eq!(json, r#"{"other":"disk full"}"#);
    let back: FailureReason = serde_json::from_str(&json).unwrap();
    assert_eq!(back, reason);
}

#[test]
fn optional_field_defaults_to_none_on_absent() {
    // Mirrors the `#[serde(default)]` on Work/Bundle: an absent key
    // deserializes to None, so old JSONL rows are forward-compatible.
    #[derive(serde::Deserialize)]
    struct Row {
        #[serde(default)]
        failure_reason: Option<FailureReason>,
    }
    let row: Row = serde_json::from_str("{}").unwrap();
    assert_eq!(row.failure_reason, None);
}
