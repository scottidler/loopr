//! Serde round-trip and schema tests for `Verdict`, `ReviewIssue`,
//! `Severity`.
//!
//! The v5 tagged-Verdict schema (`{"kind": "accept", ...}`) replaces
//! v4's flat `{"verdict": "approve", ...}`. A dedicated test asserts
//! that a v4-shape string fails to deserialize cleanly, so prompt
//! drift is caught at the type boundary rather than silently
//! mis-routed at review time.

use super::*;

#[test]
fn accept_round_trip() {
    let v = Verdict::Accept {
        summary: "looks good".to_string(),
    };
    let json = serde_json::to_string(&v).expect("serialize");
    assert!(json.contains(r#""kind":"accept""#), "got: {json}");
    assert!(json.contains(r#""summary":"looks good""#), "got: {json}");
    let back: Verdict = serde_json::from_str(&json).expect("deserialize");
    assert_eq!(v, back);
}

#[test]
fn change_requested_round_trip_zero_reasons() {
    let v = Verdict::ChangeRequested {
        summary: "see below".to_string(),
        reasons: vec![],
    };
    let json = serde_json::to_string(&v).expect("serialize");
    let back: Verdict = serde_json::from_str(&json).expect("deserialize");
    assert_eq!(v, back);
}

#[test]
fn change_requested_round_trip_many_reasons() {
    let v = Verdict::ChangeRequested {
        summary: "multiple issues".to_string(),
        reasons: vec![
            ReviewIssue {
                severity: Severity::Error,
                file: "src/lib.rs".to_string(),
                line: Some(42),
                message: "missing null check".to_string(),
                suggestion: Some("guard with .ok_or()".to_string()),
            },
            ReviewIssue {
                severity: Severity::Warning,
                file: "src/main.rs".to_string(),
                line: None,
                message: "unused import".to_string(),
                suggestion: None,
            },
            ReviewIssue {
                severity: Severity::Info,
                file: "README.md".to_string(),
                line: Some(1),
                message: "typo".to_string(),
                suggestion: None,
            },
        ],
    };
    let json = serde_json::to_string(&v).expect("serialize");
    let back: Verdict = serde_json::from_str(&json).expect("deserialize");
    assert_eq!(v, back);
}

#[test]
fn reject_round_trip() {
    let v = Verdict::Reject {
        reason: "wrong approach entirely".to_string(),
    };
    let json = serde_json::to_string(&v).expect("serialize");
    assert!(json.contains(r#""kind":"reject""#), "got: {json}");
    let back: Verdict = serde_json::from_str(&json).expect("deserialize");
    assert_eq!(v, back);
}

#[test]
fn severity_round_trip() {
    for sev in [Severity::Error, Severity::Warning, Severity::Info] {
        let json = serde_json::to_string(&sev).expect("serialize");
        let back: Severity = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(sev, back);
    }
}

#[test]
fn severity_wire_form_is_lowercase() {
    assert_eq!(serde_json::to_string(&Severity::Error).unwrap(), r#""error""#);
    assert_eq!(serde_json::to_string(&Severity::Warning).unwrap(), r#""warning""#);
    assert_eq!(serde_json::to_string(&Severity::Info).unwrap(), r#""info""#);
}

#[test]
fn v4_flat_schema_fails_to_deserialize() {
    let v4 = r#"{"verdict":"approve","summary":"ok"}"#;
    let err = serde_json::from_str::<Verdict>(v4).unwrap_err();
    assert!(err.to_string().contains("kind") || err.to_string().contains("verdict"));
}

#[test]
fn unknown_fields_rejected_on_verdict() {
    let bad = r#"{"kind":"accept","summary":"ok","extra":"field"}"#;
    let err = serde_json::from_str::<Verdict>(bad).unwrap_err();
    assert!(err.to_string().contains("extra") || err.to_string().contains("unknown"));
}

#[test]
fn unknown_fields_rejected_on_review_issue() {
    let bad = r#"{"severity":"error","file":"x","message":"y","extra":"z"}"#;
    let err = serde_json::from_str::<ReviewIssue>(bad).unwrap_err();
    assert!(err.to_string().contains("extra") || err.to_string().contains("unknown"));
}
