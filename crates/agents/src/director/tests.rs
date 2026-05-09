#![allow(clippy::unwrap_used)]

use serde_json::json;

use super::{DirectorAction, parse_director_actions};

#[test]
fn parse_accept_bundle() {
    let resp = json!([{ "action": "accept_bundle", "bundle_id": "bd-001" }]).to_string();
    let actions = parse_director_actions(&resp).expect("parse");
    assert_eq!(
        actions,
        vec![DirectorAction::AcceptBundle {
            bundle_id: "bd-001".to_string()
        }]
    );
}

#[test]
fn parse_override_work() {
    let resp = json!([{
        "action": "override_work",
        "work_id": "wk-001",
        "target_status": "Ready",
        "reason": "dep resolved"
    }])
    .to_string();
    let actions = parse_director_actions(&resp).expect("parse");
    assert_eq!(
        actions,
        vec![DirectorAction::OverrideWork {
            work_id: "wk-001".to_string(),
            target_status: "Ready".to_string(),
            reason: "dep resolved".to_string(),
        }]
    );
}

#[test]
fn parse_assign_work() {
    let resp = json!([{ "action": "assign_work", "work_id": "wk-002" }]).to_string();
    let actions = parse_director_actions(&resp).expect("parse");
    assert_eq!(
        actions,
        vec![DirectorAction::AssignWork {
            work_id: "wk-002".to_string()
        }]
    );
}

#[test]
fn parse_done() {
    let resp = json!([{ "action": "done", "summary": "all reviewed" }]).to_string();
    let actions = parse_director_actions(&resp).expect("parse");
    assert_eq!(
        actions,
        vec![DirectorAction::Done {
            summary: "all reviewed".to_string()
        }]
    );
}

#[test]
fn parse_need_help() {
    let resp = json!([{ "action": "need_help", "reason": "stuck" }]).to_string();
    let actions = parse_director_actions(&resp).expect("parse");
    assert_eq!(
        actions,
        vec![DirectorAction::NeedHelp {
            reason: "stuck".to_string()
        }]
    );
}

#[test]
fn parse_multiple_actions_in_one_response() {
    let resp = json!([
        { "action": "accept_bundle", "bundle_id": "bd-001" },
        { "action": "assign_work", "work_id": "wk-001" }
    ])
    .to_string();
    let actions = parse_director_actions(&resp).expect("parse");
    assert_eq!(actions.len(), 2);
    assert!(matches!(actions[0], DirectorAction::AcceptBundle { .. }));
    assert!(matches!(actions[1], DirectorAction::AssignWork { .. }));
}

#[test]
fn parse_single_object_wrapped_into_vec() {
    let resp = json!({ "action": "done", "summary": "ok" }).to_string();
    let actions = parse_director_actions(&resp).expect("parse");
    assert_eq!(
        actions,
        vec![DirectorAction::Done {
            summary: "ok".to_string()
        }]
    );
}

#[test]
fn parse_unknown_action_kind_errors() {
    let resp = json!([{ "action": "frobnicate", "bogus": "yes" }]).to_string();
    let err = parse_director_actions(&resp).unwrap_err();
    assert!(matches!(err, super::DirectorError::Parse(_)));
}

#[test]
fn parse_malformed_json_errors() {
    let err = parse_director_actions("{not json").unwrap_err();
    assert!(matches!(err, super::DirectorError::Parse(_)));
}

#[test]
fn parse_empty_response_errors() {
    let err = parse_director_actions("   \n").unwrap_err();
    match err {
        super::DirectorError::Parse(msg) => assert!(msg.contains("empty"), "got: {msg}"),
        other => panic!("expected Parse, got {other:?}"),
    }
}

#[test]
fn director_config_default_values() {
    let cfg = crate::config::DirectorConfig::default();
    assert_eq!(cfg.poll_interval_secs, 5);
    assert_eq!(cfg.idle_interval_secs, 15);
    assert_eq!(cfg.max_restarts, 3);
    assert_eq!(cfg.model, "claude-opus-4-7");
    assert_eq!(cfg.token_budget, 100_000);
}
