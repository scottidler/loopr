#![allow(clippy::unwrap_used)]

use serde_json::json;

use super::fixtures::*;

#[test]
fn test_learning_auto_promotion_via_dispatch() {
    let stores = test_stores();
    let tx = test_event_tx();
    let wm = test_worktree_mgr();
    let ic = test_integrator_config();

    // Create a learning
    let learning = dispatch_ok(
        &stores,
        &tx,
        &wm,
        &ic,
        "learning.create",
        json!({
            "source_id": "wi-123",
            "scope": "work",
            "content": "Always check null pointers"
        }),
    );
    let learning_id = learning["id"].as_str().unwrap().to_string();
    assert_eq!(learning["promoted"], false);
    assert_eq!(learning["reinforcements"], 0);

    // Reinforce 3 times (min_reinforcements default = 3)
    for i in 1..=3 {
        let result = dispatch_ok(&stores, &tx, &wm, &ic, "learning.reinforce", json!({"id": learning_id}));
        assert_eq!(result["reinforcements"], i);
    }

    // After 3 reinforcements with 0 contradictions, should be auto-promoted
    let learnings = stores.learnings.read().unwrap();
    let l = &learnings[&learning_id];
    assert!(l.promoted, "learning should be auto-promoted after 3 reinforcements");
    assert_eq!(l.reinforcements, 3);
    assert_eq!(l.contradictions, 0);
    assert!(l.confidence > 0.9, "confidence should be near 1.0");
}

#[test]
fn test_learning_contradiction_blocks_promotion() {
    let stores = test_stores();
    let tx = test_event_tx();
    let wm = test_worktree_mgr();
    let ic = test_integrator_config();

    // Create learning
    let learning = dispatch_ok(
        &stores,
        &tx,
        &wm,
        &ic,
        "learning.create",
        json!({"source_id": "wi-1", "scope": "global", "content": "Use tabs not spaces"}),
    );
    let id = learning["id"].as_str().unwrap().to_string();

    // Reinforce twice
    dispatch_ok(&stores, &tx, &wm, &ic, "learning.reinforce", json!({"id": id}));
    dispatch_ok(&stores, &tx, &wm, &ic, "learning.reinforce", json!({"id": id}));

    // Contradict once
    dispatch_ok(&stores, &tx, &wm, &ic, "learning.contradict", json!({"id": id}));

    // Reinforce again (total 3 reinforcements, 1 contradiction)
    dispatch_ok(&stores, &tx, &wm, &ic, "learning.reinforce", json!({"id": id}));

    // Should NOT be promoted (contradictions > 0)
    let learnings = stores.learnings.read().unwrap();
    let l = &learnings[&id];
    assert!(!l.promoted, "learning with contradictions should not auto-promote");
    assert_eq!(l.reinforcements, 3);
    assert_eq!(l.contradictions, 1);
}

#[test]
fn test_learning_confidence_computation() {
    let stores = test_stores();
    let tx = test_event_tx();
    let wm = test_worktree_mgr();
    let ic = test_integrator_config();

    let learning = dispatch_ok(
        &stores,
        &tx,
        &wm,
        &ic,
        "learning.create",
        json!({"source_id": "wi-1", "scope": "global", "content": "Test"}),
    );
    let id = learning["id"].as_str().unwrap().to_string();

    // Initial confidence = 0.5
    assert!((learning["confidence"].as_f64().unwrap() - 0.5).abs() < 0.01);

    // After 1 reinforce: 1/(1+0) = 1.0
    let r1 = dispatch_ok(&stores, &tx, &wm, &ic, "learning.reinforce", json!({"id": id}));
    assert!((r1["confidence"].as_f64().unwrap() - 1.0).abs() < 0.01);

    // After 1 contradict: 1/(1+1) = 0.5
    let c1 = dispatch_ok(&stores, &tx, &wm, &ic, "learning.contradict", json!({"id": id}));
    assert!((c1["confidence"].as_f64().unwrap() - 0.5).abs() < 0.01);

    // After 2 more reinforces: 3/(3+1) = 0.75
    dispatch_ok(&stores, &tx, &wm, &ic, "learning.reinforce", json!({"id": id}));
    let r3 = dispatch_ok(&stores, &tx, &wm, &ic, "learning.reinforce", json!({"id": id}));
    assert!((r3["confidence"].as_f64().unwrap() - 0.75).abs() < 0.01);
}

#[test]
fn test_failure_learning_creation() {
    let stores = test_stores();
    let tx = test_event_tx();
    let wm = test_worktree_mgr();
    let ic = test_integrator_config();

    let (_, _, phase_id) = create_test_hierarchy(&stores, &tx, &wm, &ic);

    // Create a work item
    let wi = dispatch_ok(
        &stores,
        &tx,
        &wm,
        &ic,
        "work.create",
        json!({
            "phase_id": phase_id,
            "title": "Add error handling",
            "description": "Implement error types",
            "resource_tags": ["src/error.rs"],
            "acceptance_criteria": ["Error types defined"]
        }),
    );
    let wi_id = wi["id"].as_str().unwrap().to_string();

    // Create a failure learning linked to the work item
    let learning = dispatch_ok(
        &stores,
        &tx,
        &wm,
        &ic,
        "learning.create",
        json!({
            "source_id": wi_id,
            "scope": "work",
            "content": "thiserror derive requires Display impl on inner types; use #[from] for auto-conversion"
        }),
    );

    let learning_id = learning["id"].as_str().unwrap().to_string();
    assert!(!learning_id.is_empty());

    // Retrieve and verify the learning
    let retrieved = dispatch_ok(&stores, &tx, &wm, &ic, "learning.get", json!({"id": learning_id}));
    assert_eq!(retrieved["source_id"].as_str().unwrap(), wi_id);
    assert_eq!(retrieved["scope"].as_str().unwrap(), "work");
    assert!(retrieved["content"].as_str().unwrap().contains("thiserror"));

    // Update with resource_tags (set via learning.update)
    dispatch_ok(
        &stores,
        &tx,
        &wm,
        &ic,
        "learning.update",
        json!({"id": learning_id, "resource_tags": ["src/error.rs"]}),
    );

    // Verify resource_tags persisted
    let updated = dispatch_ok(&stores, &tx, &wm, &ic, "learning.get", json!({"id": learning_id}));
    let tags: Vec<String> = serde_json::from_value(updated["resource_tags"].clone()).unwrap();
    assert_eq!(tags, vec!["src/error.rs"]);
}
