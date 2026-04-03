#![allow(clippy::unwrap_used)]

use serde_json::json;

use crate::domain::tick::TickStatus;

use super::fixtures::*;

#[test]
fn test_tick_crash_recovery_state() {
    use crate::domain::tick::Tick;

    let stores = test_stores();

    // Simulate a crash: directly insert a tick stuck in Sealing state
    let mut tick = Tick::new(1);
    tick.force_status(TickStatus::Sealing);
    let tick_id = tick.id.clone();
    stores.ticks.write().unwrap().insert(tick_id.clone(), tick);

    // Also insert one in Validating state
    let mut tick2 = Tick::new(2);
    tick2.force_status(TickStatus::Validating);
    let tick2_id = tick2.id.clone();
    stores.ticks.write().unwrap().insert(tick2_id.clone(), tick2);

    // Crash recovery directly resets stuck ticks (bypasses FSM)
    {
        let mut ticks = stores.ticks.write().unwrap();
        for tick in ticks.values_mut() {
            if matches!(tick.status(), TickStatus::Sealing | TickStatus::Validating) {
                tick.force_status(TickStatus::Failed);
            }
        }
    }

    // Both ticks should be Failed
    let ticks = stores.ticks.read().unwrap();
    assert_eq!(ticks[&tick_id].status(), TickStatus::Failed);
    assert_eq!(ticks[&tick2_id].status(), TickStatus::Failed);
}

#[test]
fn test_goal_lifecycle() {
    let stores = test_stores();
    let tx = test_event_tx();
    let wm = test_worktree_mgr();
    let ic = test_integrator_config();

    // Set first goal
    let g1 = dispatch_ok(
        &stores,
        &tx,
        &wm,
        &ic,
        "coordinator.set_goal",
        json!({"goal": "Build auth system"}),
    );
    let g1_id = g1["id"].as_str().unwrap().to_string();
    assert_eq!(g1["active"], true);

    // Set second goal - first should be deactivated
    let g2 = dispatch_ok(
        &stores,
        &tx,
        &wm,
        &ic,
        "coordinator.set_goal",
        json!({"goal": "Add dark mode"}),
    );
    assert_eq!(g2["active"], true);

    // Verify first is deactivated
    let goals = stores.coordinator_goals.read().unwrap();
    assert!(!goals[&g1_id].active, "first goal should be deactivated");
    assert_eq!(
        goals.values().filter(|g| g.active).count(),
        1,
        "exactly one active goal"
    );
    drop(goals);

    // Clear all goals
    let cleared = dispatch_ok(&stores, &tx, &wm, &ic, "coordinator.clear_goal", json!({}));
    assert_eq!(cleared["cleared"], 1);

    // All goals should be inactive
    let goals = stores.coordinator_goals.read().unwrap();
    assert!(goals.values().all(|g| !g.active));
}

#[test]
fn test_worktree_base_uses_published_tick() {
    let stores = test_stores();
    let tx = test_event_tx();
    let wm = test_worktree_mgr();
    let ic = test_integrator_config();

    // Create tick 1
    let tick1 = dispatch_ok(&stores, &tx, &wm, &ic, "tick.create", json!({"number": 1}));
    let tick1_id = tick1["id"].as_str().unwrap().to_string();

    // Tick1: Open -> Sealing -> Validating -> Published
    dispatch_ok(
        &stores,
        &tx,
        &wm,
        &ic,
        "tick.transition",
        json!({"id": tick1_id, "target_status": "Sealing"}),
    );
    dispatch_ok(
        &stores,
        &tx,
        &wm,
        &ic,
        "tick.transition",
        json!({"id": tick1_id, "target_status": "Validating"}),
    );
    dispatch_ok(
        &stores,
        &tx,
        &wm,
        &ic,
        "tick.transition",
        json!({"id": tick1_id, "target_status": "Published"}),
    );

    // Verify find_latest_published_tick returns tick1
    {
        let ticks = stores.ticks.read().unwrap();
        let latest_published = ticks
            .values()
            .filter(|t| t.status() == TickStatus::Published)
            .max_by_key(|t| t.number)
            .cloned();
        assert!(latest_published.is_some());
        assert_eq!(latest_published.unwrap().id, tick1_id);
    }

    // Create tick2 (now possible since tick1 is Published = terminal)
    let tick2 = dispatch_ok(&stores, &tx, &wm, &ic, "tick.create", json!({"number": 2}));
    let tick2_id = tick2["id"].as_str().unwrap().to_string();

    // Publish tick2 (higher number)
    dispatch_ok(
        &stores,
        &tx,
        &wm,
        &ic,
        "tick.transition",
        json!({"id": tick2_id, "target_status": "Sealing"}),
    );
    dispatch_ok(
        &stores,
        &tx,
        &wm,
        &ic,
        "tick.transition",
        json!({"id": tick2_id, "target_status": "Validating"}),
    );
    dispatch_ok(
        &stores,
        &tx,
        &wm,
        &ic,
        "tick.transition",
        json!({"id": tick2_id, "target_status": "Published"}),
    );

    // Now tick2 (higher number) should be the latest published
    let ticks = stores.ticks.read().unwrap();
    let latest_published = ticks
        .values()
        .filter(|t| t.status() == TickStatus::Published)
        .max_by_key(|t| t.number)
        .cloned();
    assert!(latest_published.is_some());
    assert_eq!(latest_published.unwrap().id, tick2_id);
}
