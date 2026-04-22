#![allow(clippy::unwrap_used)]

use serde_json::json;

use super::{Lifeguard, Verdict, canonical_hash};
use crate::action::AgentAction;

fn bash_action(command: &str) -> AgentAction {
    AgentAction::RunTool {
        tool: "bash".to_string(),
        input: json!({ "command": command }),
    }
}

#[test]
fn single_action_does_not_escalate() {
    let mut lg = Lifeguard::new(3, 5);
    let a = AgentAction::Done { message: "ok".into() };
    assert!(matches!(lg.check_action(&a), Verdict::Continue));
}

#[test]
fn repeated_action_escalates_after_max_repeat() {
    let mut lg = Lifeguard::new(3, 5);
    let a = bash_action("ls");
    assert!(matches!(lg.check_action(&a), Verdict::Continue));
    assert!(matches!(lg.check_action(&a), Verdict::Continue));
    // Third occurrence trips max_repeat=3.
    match lg.check_action(&a) {
        Verdict::Escalate(reason) => assert!(reason.contains("repeated 3 times")),
        Verdict::Continue => panic!("expected escalate at 3rd occurrence"),
    }
}

#[test]
fn different_actions_do_not_escalate() {
    let mut lg = Lifeguard::new(3, 5);
    assert!(matches!(lg.check_action(&bash_action("ls")), Verdict::Continue));
    assert!(matches!(lg.check_action(&bash_action("pwd")), Verdict::Continue));
    assert!(matches!(lg.check_action(&bash_action("echo")), Verdict::Continue));
    // Even though we hit 3 total actions, each is unique; no escalation.
}

#[test]
fn parse_failure_escalates_at_max() {
    let mut lg = Lifeguard::new(3, 5);
    assert!(matches!(lg.record_parse_failure(), Verdict::Continue));
    assert!(matches!(lg.record_parse_failure(), Verdict::Continue));
    assert!(matches!(lg.record_parse_failure(), Verdict::Continue));
    assert!(matches!(lg.record_parse_failure(), Verdict::Continue));
    match lg.record_parse_failure() {
        Verdict::Escalate(reason) => assert!(reason.contains("5 consecutive")),
        Verdict::Continue => panic!("expected escalate on 5th parse failure"),
    }
}

#[test]
fn reset_parse_failures_clears_counter() {
    let mut lg = Lifeguard::new(3, 5);
    lg.record_parse_failure();
    lg.record_parse_failure();
    lg.record_parse_failure();
    lg.reset_parse_failures();
    // After reset, we need 5 more failures to escalate.
    for _ in 0..4 {
        assert!(matches!(lg.record_parse_failure(), Verdict::Continue));
    }
    // 5th after reset should escalate.
    assert!(matches!(lg.record_parse_failure(), Verdict::Escalate(_)));
}

// ---------------------------------------------------------------------------
// Canonical hashing: THE critical invariant from Architect R2 and the
// design doc. If this test ever fails, it means `serde_json` key
// ordering leaked into our dedupe hash.
// ---------------------------------------------------------------------------

#[test]
fn canonical_hash_stable_across_key_order_in_input() {
    // Same logical action; the inner `input` map keys are emitted
    // in different orders. The canonicalized hash must be identical.
    let a = AgentAction::RunTool {
        tool: "bash".into(),
        input: json!({ "command": "ls", "timeout": 30, "cwd": "/tmp" }),
    };
    let b = AgentAction::RunTool {
        tool: "bash".into(),
        input: json!({ "timeout": 30, "cwd": "/tmp", "command": "ls" }),
    };
    let c = AgentAction::RunTool {
        tool: "bash".into(),
        input: json!({ "cwd": "/tmp", "command": "ls", "timeout": 30 }),
    };
    let ha = canonical_hash(&a);
    let hb = canonical_hash(&b);
    let hc = canonical_hash(&c);
    assert_eq!(ha, hb, "hash must be independent of key order (a vs b)");
    assert_eq!(ha, hc, "hash must be independent of key order (a vs c)");
}

#[test]
fn canonical_hash_stable_across_nested_key_order() {
    // Nested objects must also be canonicalized.
    let a = AgentAction::RunTool {
        tool: "t".into(),
        input: json!({ "env": { "A": "1", "B": "2" }, "cmd": "x" }),
    };
    let b = AgentAction::RunTool {
        tool: "t".into(),
        input: json!({ "cmd": "x", "env": { "B": "2", "A": "1" } }),
    };
    assert_eq!(
        canonical_hash(&a),
        canonical_hash(&b),
        "nested keys must canonicalize too"
    );
}

#[test]
fn canonical_hash_differs_across_different_content() {
    let a = bash_action("ls");
    let b = bash_action("pwd");
    assert_ne!(canonical_hash(&a), canonical_hash(&b));
}

#[test]
fn canonical_hash_differs_across_variants() {
    let a = AgentAction::Done { message: "x".into() };
    let b = AgentAction::NeedHelp { reason: "x".into() };
    assert_ne!(canonical_hash(&a), canonical_hash(&b));
}

#[test]
fn repeated_action_dedup_works_across_key_reorderings() {
    // Seam-level check: the Lifeguard's escalation path must fire
    // even when the LLM emits the same action with different key
    // orderings each iteration. This is why we canonicalize.
    let mut lg = Lifeguard::new(3, 5);
    let a = AgentAction::RunTool {
        tool: "bash".into(),
        input: json!({ "command": "ls", "timeout": 30 }),
    };
    let b = AgentAction::RunTool {
        tool: "bash".into(),
        input: json!({ "timeout": 30, "command": "ls" }),
    };
    let c = AgentAction::RunTool {
        tool: "bash".into(),
        input: json!({ "command": "ls", "timeout": 30 }),
    };
    assert!(matches!(lg.check_action(&a), Verdict::Continue));
    assert!(matches!(lg.check_action(&b), Verdict::Continue));
    match lg.check_action(&c) {
        Verdict::Escalate(_) => {}
        Verdict::Continue => panic!("expected escalate: a/b/c differ only in key order"),
    }
}
