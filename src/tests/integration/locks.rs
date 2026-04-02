#![allow(clippy::unwrap_used)]

use serde_json::json;

use super::fixtures::*;

#[test]
fn test_lock_lifecycle_via_dispatch() {
    let stores = test_stores();
    let tx = test_event_tx();
    let wm = test_worktree_mgr();
    let ic = test_integrator_config();

    // Create a lock
    let lock = dispatch_ok(
        &stores,
        &tx,
        &wm,
        &ic,
        "lock.create",
        json!({
            "resource": "src/main.rs",
            "holder_id": "wi-1",
            "granted_by": "coordinator"
        }),
    );
    let lock_id = lock["id"].as_str().unwrap().to_string();
    assert_eq!(lock["resource"], "src/main.rs");
    assert_eq!(lock["status"], "active");

    // List locks - should have one active
    let locks = dispatch_ok(&stores, &tx, &wm, &ic, "lock.list", json!({}));
    assert_eq!(locks.as_array().unwrap().len(), 1);

    // Release the lock
    dispatch_ok(&stores, &tx, &wm, &ic, "lock.release", json!({"id": lock_id}));

    // Lock should be released
    let lock_state = stores.locks.read().unwrap();
    assert_eq!(lock_state[&lock_id].status, crate::domain::lock::LockStatus::Released);
}
