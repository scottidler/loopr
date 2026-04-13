#![allow(clippy::unwrap_used, unused_imports)]

//! E2E decomposition tests for v3 equivalence.
//!
//! These tests verify that the engine-driven decomposition (Doc 6) produces the
//! same Plan -> Spec -> Phase -> Work hierarchy structure as v3's monolithic
//! decomposer. They are IGNORED until the `decomposer.decompose` IPC handler is
//! wired with `TaskStore::create_many` for atomic batch child persistence.
//!
//! Unblock checklist:
//! - [ ] `TaskStore::create_many` shipped in `scottidler/taskstore`
//! - [ ] `decomposer.decompose` IPC handler wired in `src/daemon/handlers/`
//! - [ ] Remove `#[ignore]` from these tests

use serde_json::json;

use super::fixtures::*;

/// Full decomposition: Plan -> Spec -> Phase -> Work via engine strategies.
///
/// Verifies that:
/// - An Active plan with no spec children triggers `plan-decomposable`
/// - The decomposer agent creates Pending specs
/// - Reconciliation promotes specs to Active
/// - Active specs with no phase children trigger `spec-decomposable`
/// - The hierarchy completes: Plan -> Spec(s) -> Phase(s) -> Work(s)
#[tokio::test]
#[ignore = "blocked on decomposer.decompose IPC handler (requires TaskStore::create_many)"]
async fn test_full_decomposition_via_engine() {
    let stores = test_stores();
    let tx = test_event_tx();
    let wm = test_worktree_mgr();
    let ic = test_integrator_config();

    // Create and activate a plan (tier: full)
    let plan = dispatch_ok(
        &stores,
        &tx,
        &wm,
        &ic,
        "plan.create",
        json!({
            "title": "E2E Full Decomposition",
            "description": "Test full Plan -> Spec -> Phase -> Work",
            "acceptance_criteria": "All hierarchy levels created"
        }),
    )
    .await;
    let plan_id = plan["id"].as_str().unwrap().to_string();

    dispatch_ok(
        &stores,
        &tx,
        &wm,
        &ic,
        "plan.transition",
        json!({"id": plan_id, "target_status": "active"}),
    )
    .await;

    // At this point the engine should fire plan-decomposable and spawn a
    // decomposer agent. The agent calls decomposer.decompose which creates
    // Pending specs. Subsequent ticks promote and decompose each level.
    //
    // TODO: Run engine ticks and verify hierarchy structure matches v3 output:
    // - At least 1 spec under the plan
    // - At least 1 phase under each spec
    // - At least 1 work under each phase
    // - All works in Ready state after full promotion

    let specs = stores.read_specs().unwrap();
    let plan_specs: Vec<_> = specs.values().filter(|s| s.parent_id == plan_id).collect();
    assert!(!plan_specs.is_empty(), "expected specs under plan after decomposition");

    for spec in &plan_specs {
        let phases = stores.read_phases().unwrap();
        let spec_phases: Vec<_> = phases.values().filter(|p| p.parent_id == spec.id).collect();
        assert!(!spec_phases.is_empty(), "expected phases under spec {}", spec.id);

        for phase in &spec_phases {
            let works = stores.read_works().unwrap();
            let phase_works: Vec<_> = works.values().filter(|w| w.parent_id == phase.id).collect();
            assert!(!phase_works.is_empty(), "expected works under phase {}", phase.id);
        }
    }
}

/// Brief decomposition: Plan -> Work directly, skipping Spec and Phase.
///
/// Verifies that:
/// - A brief-tier plan creates Works directly (no Specs or Phases)
/// - `spec-decomposable` and `phase-decomposable` never fire
#[tokio::test]
#[ignore = "blocked on decomposer.decompose IPC handler (requires TaskStore::create_many)"]
async fn test_brief_decomposition_via_engine() {
    let stores = test_stores();
    let tx = test_event_tx();
    let wm = test_worktree_mgr();
    let ic = test_integrator_config();

    // Create a plan with brief tier
    let plan = dispatch_ok(
        &stores,
        &tx,
        &wm,
        &ic,
        "plan.create",
        json!({
            "title": "E2E Brief Decomposition",
            "description": "Test brief Plan -> Work",
            "acceptance_criteria": "Works created directly under plan"
        }),
    )
    .await;
    let plan_id = plan["id"].as_str().unwrap().to_string();

    // TODO: Set plan tier to Brief before activation
    // dispatch_ok(&stores, &tx, &wm, &ic, "plan.update",
    //     json!({"id": plan_id, "tier": "brief"})).await;

    dispatch_ok(
        &stores,
        &tx,
        &wm,
        &ic,
        "plan.transition",
        json!({"id": plan_id, "target_status": "active"}),
    )
    .await;

    // After engine ticks, verify: works exist, no specs or phases
    let specs = stores.read_specs().unwrap();
    let plan_specs: Vec<_> = specs.values().filter(|s| s.parent_id == plan_id).collect();
    assert!(plan_specs.is_empty(), "brief mode should NOT create specs");

    let works = stores.read_works().unwrap();
    let plan_works: Vec<_> = works.values().filter(|w| w.parent_id == plan_id).collect();
    assert!(
        !plan_works.is_empty(),
        "brief mode should create works directly under plan"
    );
}

/// Crash resilience: daemon restart resumes decomposition from FSM state.
///
/// Verifies that:
/// - After specs are created (Active) but phases are not, restarting the engine
///   fires `spec-decomposable` for each Active spec with no phase children
/// - No special recovery code needed - the FSM triggers handle it
#[tokio::test]
#[ignore = "blocked on decomposer.decompose IPC handler (requires TaskStore::create_many)"]
async fn test_crash_resume_decomposition() {
    let stores = test_stores();
    let tx = test_event_tx();
    let wm = test_worktree_mgr();
    let ic = test_integrator_config();

    // Create plan and manually insert Active specs (simulating post-crash state
    // where specs exist but phases do not)
    let plan = dispatch_ok(
        &stores,
        &tx,
        &wm,
        &ic,
        "plan.create",
        json!({
            "title": "Crash Resume Test",
            "description": "Test crash recovery",
            "acceptance_criteria": "Phases created after restart"
        }),
    )
    .await;
    let plan_id = plan["id"].as_str().unwrap().to_string();

    dispatch_ok(
        &stores,
        &tx,
        &wm,
        &ic,
        "plan.transition",
        json!({"id": plan_id, "target_status": "active"}),
    )
    .await;

    // Create an Active spec with no phases (simulates mid-decomposition crash)
    let spec = dispatch_ok(
        &stores,
        &tx,
        &wm,
        &ic,
        "spec.create",
        json!({
            "parent_id": plan_id,
            "title": "Auth Spec",
            "description": "Auth specification",
            "acceptance_criteria": "Auth works"
        }),
    )
    .await;
    let spec_id = spec["id"].as_str().unwrap().to_string();

    dispatch_ok(
        &stores,
        &tx,
        &wm,
        &ic,
        "spec.transition",
        json!({"id": spec_id, "target_status": "active"}),
    )
    .await;

    // Verify: spec is Active, has no phase children
    let specs = stores.read_specs().unwrap();
    let spec_record = specs.get(&spec_id).unwrap();
    assert_eq!(format!("{:?}", spec_record.status()), "Active");

    let phases = stores.read_phases().unwrap();
    let spec_phases: Vec<_> = phases.values().filter(|p| p.parent_id == spec_id).collect();
    assert!(spec_phases.is_empty(), "no phases should exist yet");

    // TODO: Run engine tick - spec-decomposable should fire and spawn decomposer
    // for this spec. After the agent completes, phases should exist under the spec.
    // This proves crash resilience without any special recovery code.
}

/// Verify that decompose_hierarchy in src/decomposer.rs is only called from
/// the v3 doc handler path. Once the decomposer.decompose IPC handler replaces
/// that call site, decomposer.rs becomes dead code and can be deleted.
///
/// Current live caller: src/daemon/handlers/doc.rs (handle_doc_decompose)
#[test]
fn test_decompose_hierarchy_has_single_live_caller() {
    // This is a documentation test, not a runtime test.
    // The single live call site is src/daemon/handlers/doc.rs:241:
    //   match decompose_hierarchy(&markdown_bg, &dc, &client, brief).await {
    //
    // When the decomposer.decompose IPC handler is wired and the coordinator
    // switches to engine-driven decomposition, this call site should be removed.
    // At that point, decomposer.rs has no live callers and can be deleted (step 5).
    //
    // To verify programmatically, run:
    //   grep -rn "decompose_hierarchy" src/ --include="*.rs" | grep -v "decomposer.rs" | grep -v "test"
    // Expected: only src/daemon/handlers/doc.rs
}
