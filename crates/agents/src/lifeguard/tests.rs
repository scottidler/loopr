#![allow(clippy::unwrap_used)]

use serde_json::json;

use super::{Decision, Lifeguard, canonical_hash};
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
    assert!(matches!(lg.check_action(&a), Decision::Continue));
}

#[test]
fn repeated_action_escalates_after_max_repeat() {
    let mut lg = Lifeguard::new(3, 5);
    let a = bash_action("ls");
    assert!(matches!(lg.check_action(&a), Decision::Continue));
    assert!(matches!(lg.check_action(&a), Decision::Continue));
    // Third occurrence trips max_repeat=3.
    match lg.check_action(&a) {
        Decision::Escalate(reason) => assert!(reason.contains("repeated 3 times")),
        Decision::Continue => panic!("expected escalate at 3rd occurrence"),
    }
}

#[test]
fn different_actions_do_not_escalate() {
    let mut lg = Lifeguard::new(3, 5);
    assert!(matches!(lg.check_action(&bash_action("ls")), Decision::Continue));
    assert!(matches!(lg.check_action(&bash_action("pwd")), Decision::Continue));
    assert!(matches!(lg.check_action(&bash_action("echo")), Decision::Continue));
    // Even though we hit 3 total actions, each is unique; no escalation.
}

#[test]
fn interleaved_repeats_do_not_escalate_consecutive_only() {
    // A,B,A,B,A: the same action recurs 3 times TOTAL but never 3 times
    // in a row. Cumulative-per-hash counting (the bug) would escalate on
    // the 5th call; consecutive-run semantics must NOT — a legitimately
    // repeated `cargo test` between distinct edits is healthy.
    let mut lg = Lifeguard::new(3, 5);
    let a = bash_action("cargo test");
    let b = bash_action("write src/foo.rs");
    assert!(matches!(lg.check_action(&a), Decision::Continue));
    assert!(matches!(lg.check_action(&b), Decision::Continue));
    assert!(matches!(lg.check_action(&a), Decision::Continue));
    assert!(matches!(lg.check_action(&b), Decision::Continue));
    assert!(
        matches!(lg.check_action(&a), Decision::Continue),
        "A,B,A,B,A must not escalate: no 3 consecutive identical actions"
    );
}

#[test]
fn consecutive_run_resets_after_interruption() {
    // A,A,B,A,A: the run of A is broken by B, so the trailing A,A is only
    // length 2 — below max_repeat=3. No escalation.
    let mut lg = Lifeguard::new(3, 5);
    let a = bash_action("ls");
    let b = bash_action("pwd");
    assert!(matches!(lg.check_action(&a), Decision::Continue));
    assert!(matches!(lg.check_action(&a), Decision::Continue));
    assert!(matches!(lg.check_action(&b), Decision::Continue));
    assert!(matches!(lg.check_action(&a), Decision::Continue));
    assert!(
        matches!(lg.check_action(&a), Decision::Continue),
        "the run reset at B, so trailing A,A is below threshold"
    );
}

#[test]
fn parse_failure_escalates_at_max() {
    let mut lg = Lifeguard::new(3, 5);
    assert!(matches!(lg.record_parse_failure(), Decision::Continue));
    assert!(matches!(lg.record_parse_failure(), Decision::Continue));
    assert!(matches!(lg.record_parse_failure(), Decision::Continue));
    assert!(matches!(lg.record_parse_failure(), Decision::Continue));
    match lg.record_parse_failure() {
        Decision::Escalate(reason) => assert!(reason.contains("5 consecutive")),
        Decision::Continue => panic!("expected escalate on 5th parse failure"),
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
        assert!(matches!(lg.record_parse_failure(), Decision::Continue));
    }
    // 5th after reset should escalate.
    assert!(matches!(lg.record_parse_failure(), Decision::Escalate(_)));
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
    assert!(matches!(lg.check_action(&a), Decision::Continue));
    assert!(matches!(lg.check_action(&b), Decision::Continue));
    match lg.check_action(&c) {
        Decision::Escalate(_) => {}
        Decision::Continue => panic!("expected escalate: a/b/c differ only in key order"),
    }
}
