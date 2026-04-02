#![allow(clippy::unwrap_used)]

use crate::agents::AgentAction;
use crate::agents::bridge::AgentIpcBridge;
use crate::agents::executor::{ActionResult, execute_action};
use crate::test_util::TestDir;
use crate::worktree::manager::WorktreeManager;

use super::fixtures::*;

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

#[test]
fn test_create_spec_with_invalid_plan_id_returns_error() {
    // Verify that CreateSpec with a non-existent plan_id returns an error
    let rt = tokio::runtime::Runtime::new().unwrap();
    let dir = TestDir::new("loopr-e2e-badparent");

    let stores = test_stores();
    let tx = test_event_tx();
    let wm = WorktreeManager::new(dir.to_path_buf(), dir.join(".wt"));
    let bridge = AgentIpcBridge::new(stores.clone(), tx.clone(), wm, stores.config.clone());
    let agent_log = test_agent_logger(&dir);
    let ctx = test_agent_context(&stores, bridge, tx, agent_log);

    let result = rt
        .block_on(execute_action(
            &AgentAction::CreateSpec {
                plan_id: "nonexistent-plan".into(),
                title: "Bad Spec".into(),
                description: "Should fail".into(),
            },
            &ctx,
            &dir,
            None,
        ))
        .unwrap();

    assert!(
        matches!(result, ActionResult::ActionError(ref msg) if msg.contains("failed")),
        "expected ActionError for invalid parent, got: {:?}",
        result
    );
}
