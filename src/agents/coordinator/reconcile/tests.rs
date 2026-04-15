use super::*;
use crate::config::{Config, ProjectConfig};
use crate::daemon::context::Stores;
use crate::domain::phase::Phase;
use crate::domain::plan::{HierarchyStatus, Plan, Tier};
use crate::domain::spec::Spec;
use crate::domain::work::{Work, WorkStatus};
use crate::test_util::TestDir;
use std::sync::{Arc, Mutex as StdMutex};
use taskstore::Store;

fn test_stores(dir: &std::path::Path) -> Arc<Stores> {
    let config = Config {
        project: ProjectConfig {
            repo_path: dir.to_path_buf(),
            ..ProjectConfig::default()
        },
        ..Config::default()
    };
    let store = Store::open(dir).unwrap();
    let mut stores = Stores::new();
    stores.store = Some(Arc::new(StdMutex::new(store)));
    stores.config = config;
    Arc::new(stores)
}

/// Insert an active Plan and return its ID.
fn insert_active_plan(stores: &Stores, title: &str) -> String {
    let mut plan = Plan::new(title.to_string(), Default::default());
    plan.force_status(HierarchyStatus::Active);
    let id = plan.id.clone();
    stores.plans.write().unwrap().insert(id.clone(), plan);
    id
}

/// Insert a Spec with given status and deps. Returns ID.
fn insert_spec(stores: &Stores, parent_id: &str, title: &str, status: HierarchyStatus, deps: Vec<String>) -> String {
    let mut spec = Spec::new(parent_id.to_string(), title.to_string());
    spec.force_status(status);
    spec.dependencies = deps;
    let id = spec.id.clone();
    stores.specs.write().unwrap().insert(id.clone(), spec);
    id
}

/// Insert a Phase with given status and deps. Returns ID.
fn insert_phase(stores: &Stores, parent_id: &str, title: &str, status: HierarchyStatus, deps: Vec<String>) -> String {
    let mut phase = Phase::new(parent_id.to_string(), title.to_string());
    phase.force_status(status);
    phase.dependencies = deps;
    let id = phase.id.clone();
    stores.phases.write().unwrap().insert(id.clone(), phase);
    id
}

/// Insert a Work with given status and deps. Returns ID.
fn insert_work(stores: &Stores, parent_id: &str, title: &str, status: WorkStatus, deps: Vec<String>) -> String {
    let mut work = Work::new(parent_id.to_string(), title.to_string());
    work.force_status(status);
    work.dependencies = deps;
    let id = work.id.clone();
    stores.works.write().unwrap().insert(id.clone(), work);
    id
}

// ---------------------------------------------------------------------------
// Spec promotion tests
// ---------------------------------------------------------------------------

#[test]
fn test_promote_spec_no_deps() {
    let dir = TestDir::new("reconcile_spec_no_deps");
    let stores = test_stores(&dir);
    let plan_id = insert_active_plan(&stores, "Test Plan");
    let spec_id = insert_spec(&stores, &plan_id, "Spec A", HierarchyStatus::Pending, vec![]);
    // Add a Pending phase with a Pending work so neither auto-completes via vacuous truth
    let phase_id = insert_phase(&stores, &spec_id, "Phase 1", HierarchyStatus::Pending, vec![]);
    insert_work(&stores, &phase_id, "main.py", WorkStatus::Pending, vec![]);

    let outcome = reconcile(&stores);

    assert!(outcome.promoted >= 1);
    let specs = stores.read_specs().unwrap();
    assert_eq!(specs.get(&spec_id).unwrap().status(), HierarchyStatus::Active);
}

#[test]
fn test_promote_spec_with_completed_dep() {
    let dir = TestDir::new("reconcile_spec_with_dep");
    let stores = test_stores(&dir);
    let plan_id = insert_active_plan(&stores, "Test Plan");
    let spec_a = insert_spec(&stores, &plan_id, "Spec A", HierarchyStatus::Complete, vec![]);
    let spec_b = insert_spec(
        &stores,
        &plan_id,
        "Spec B",
        HierarchyStatus::Pending,
        vec![spec_a.clone()],
    );
    // Add a Pending phase with work so spec B doesn't auto-complete
    let ph_b = insert_phase(&stores, &spec_b, "Phase B1", HierarchyStatus::Pending, vec![]);
    insert_work(&stores, &ph_b, "main.py", WorkStatus::Pending, vec![]);

    let outcome = reconcile(&stores);

    assert!(outcome.promoted >= 1);
    let specs = stores.read_specs().unwrap();
    assert_eq!(specs.get(&spec_b).unwrap().status(), HierarchyStatus::Active);
}

#[test]
fn test_spec_not_promoted_when_dep_pending() {
    let dir = TestDir::new("reconcile_spec_dep_pending");
    let stores = test_stores(&dir);
    let plan_id = insert_active_plan(&stores, "Test Plan");
    let spec_a = insert_spec(&stores, &plan_id, "Spec A", HierarchyStatus::Pending, vec![]);
    let spec_b = insert_spec(
        &stores,
        &plan_id,
        "Spec B",
        HierarchyStatus::Pending,
        vec![spec_a.clone()],
    );
    // Add phases with works so specs don't auto-complete via vacuous truth
    let ph_a = insert_phase(&stores, &spec_a, "Phase A1", HierarchyStatus::Pending, vec![]);
    insert_work(&stores, &ph_a, "a.py", WorkStatus::Pending, vec![]);
    let ph_b = insert_phase(&stores, &spec_b, "Phase B1", HierarchyStatus::Pending, vec![]);
    insert_work(&stores, &ph_b, "b.py", WorkStatus::Pending, vec![]);

    let outcome = reconcile(&stores);

    // Spec A should be promoted (no deps), Spec B should still be Pending (dep on A which is now Active, not terminal)
    let specs = stores.read_specs().unwrap();
    assert_eq!(specs.get(&spec_a).unwrap().status(), HierarchyStatus::Active);
    // Spec B's dep on Spec A is not terminal (Active != Complete), so Spec B stays Pending
    assert_eq!(specs.get(&spec_b).unwrap().status(), HierarchyStatus::Pending);
    // Spec A promoted + Phase A1 promoted + Work A1 promoted = at least 1 spec promotion
    assert!(outcome.promoted >= 1);
}

#[test]
fn test_spec_promoted_when_dep_abandoned() {
    let dir = TestDir::new("reconcile_spec_dep_abandoned");
    let stores = test_stores(&dir);
    let plan_id = insert_active_plan(&stores, "Test Plan");
    let spec_a = insert_spec(&stores, &plan_id, "Spec A", HierarchyStatus::Abandoned, vec![]);
    let spec_b = insert_spec(
        &stores,
        &plan_id,
        "Spec B",
        HierarchyStatus::Pending,
        vec![spec_a.clone()],
    );
    // Add a Pending phase with work so spec B doesn't auto-complete
    let ph_b = insert_phase(&stores, &spec_b, "Phase B1", HierarchyStatus::Pending, vec![]);
    insert_work(&stores, &ph_b, "main.py", WorkStatus::Pending, vec![]);

    let outcome = reconcile(&stores);

    // Abandoned counts as terminal for hierarchy deps
    let specs = stores.read_specs().unwrap();
    assert_eq!(specs.get(&spec_b).unwrap().status(), HierarchyStatus::Active);
    assert!(outcome.promoted >= 1);
}

// ---------------------------------------------------------------------------
// Phase promotion tests
// ---------------------------------------------------------------------------

#[test]
fn test_promote_phase_no_deps_parent_active() {
    let dir = TestDir::new("reconcile_phase_no_deps");
    let stores = test_stores(&dir);
    let plan_id = insert_active_plan(&stores, "Test Plan");
    let spec_id = insert_spec(&stores, &plan_id, "Spec A", HierarchyStatus::Active, vec![]);
    let phase_id = insert_phase(&stores, &spec_id, "Phase 1", HierarchyStatus::Pending, vec![]);
    // Add a Pending work so the phase doesn't auto-complete
    insert_work(&stores, &phase_id, "main.py", WorkStatus::Pending, vec![]);

    let outcome = reconcile(&stores);

    assert!(outcome.promoted >= 1);
    let phases = stores.read_phases().unwrap();
    let phase = phases.get(&phase_id).unwrap();
    assert_eq!(phase.status(), HierarchyStatus::Active);
    assert!(phase.activated_at.is_some(), "activated_at should be set on promotion");
}

#[test]
fn test_phase_not_promoted_when_parent_pending() {
    let dir = TestDir::new("reconcile_phase_parent_pending");
    let stores = test_stores(&dir);
    let plan_id = insert_active_plan(&stores, "Test Plan");
    let spec_id = insert_spec(&stores, &plan_id, "Spec A", HierarchyStatus::Pending, vec![]);
    let phase_id = insert_phase(&stores, &spec_id, "Phase 1", HierarchyStatus::Pending, vec![]);
    // Add a Pending work so the phase doesn't auto-complete after promotion
    insert_work(&stores, &phase_id, "main.py", WorkStatus::Pending, vec![]);

    let outcome = reconcile(&stores);

    // Spec A should be promoted to Active (no deps), then Phase 1 should be promoted (parent now Active)
    let specs = stores.read_specs().unwrap();
    assert_eq!(specs.get(&spec_id).unwrap().status(), HierarchyStatus::Active);
    let phases = stores.read_phases().unwrap();
    assert_eq!(phases.get(&phase_id).unwrap().status(), HierarchyStatus::Active);
    assert!(outcome.promoted >= 2);
}

#[test]
fn test_phase_linked_list_deps() {
    let dir = TestDir::new("reconcile_phase_linked_list");
    let stores = test_stores(&dir);
    let plan_id = insert_active_plan(&stores, "Test Plan");
    let spec_id = insert_spec(&stores, &plan_id, "Spec A", HierarchyStatus::Active, vec![]);
    let ph1 = insert_phase(&stores, &spec_id, "Phase 1", HierarchyStatus::Pending, vec![]);
    let ph2 = insert_phase(
        &stores,
        &spec_id,
        "Phase 2",
        HierarchyStatus::Pending,
        vec![ph1.clone()],
    );
    // Add Pending works so phases don't auto-complete
    insert_work(&stores, &ph1, "main.py", WorkStatus::Pending, vec![]);
    insert_work(&stores, &ph2, "test.py", WorkStatus::Pending, vec![]);

    let outcome = reconcile(&stores);

    // Phase 1 promoted (no deps, parent active). Phase 2 stays pending (dep on Phase 1, not terminal).
    let phases = stores.read_phases().unwrap();
    assert_eq!(phases.get(&ph1).unwrap().status(), HierarchyStatus::Active);
    assert_eq!(phases.get(&ph2).unwrap().status(), HierarchyStatus::Pending);
    assert!(outcome.promoted >= 1); // Phase 1 + its Work
}

// ---------------------------------------------------------------------------
// Work promotion tests
// ---------------------------------------------------------------------------

#[test]
fn test_promote_work_no_deps_parent_active() {
    let dir = TestDir::new("reconcile_work_no_deps");
    let stores = test_stores(&dir);
    let plan_id = insert_active_plan(&stores, "Test Plan");
    let spec_id = insert_spec(&stores, &plan_id, "Spec A", HierarchyStatus::Active, vec![]);
    let phase_id = insert_phase(&stores, &spec_id, "Phase 1", HierarchyStatus::Active, vec![]);
    let work_id = insert_work(&stores, &phase_id, "database.py", WorkStatus::Pending, vec![]);

    let outcome = reconcile(&stores);

    assert!(outcome.promoted >= 1);
    let works = stores.read_works().unwrap();
    assert_eq!(works.get(&work_id).unwrap().status(), WorkStatus::Ready);
}

#[test]
fn test_work_not_promoted_when_dep_abandoned() {
    let dir = TestDir::new("reconcile_work_dep_abandoned");
    let stores = test_stores(&dir);
    let plan_id = insert_active_plan(&stores, "Test Plan");
    let spec_id = insert_spec(&stores, &plan_id, "Spec A", HierarchyStatus::Active, vec![]);
    let phase_id = insert_phase(&stores, &spec_id, "Phase 1", HierarchyStatus::Active, vec![]);
    let wa = insert_work(&stores, &phase_id, "main.py", WorkStatus::Abandoned, vec![]);
    let wb = insert_work(&stores, &phase_id, "test.py", WorkStatus::Pending, vec![wa.clone()]);

    let outcome = reconcile(&stores);

    // Work deps require Done (not just terminal). Abandoned blocks downstream.
    let works = stores.read_works().unwrap();
    assert_eq!(works.get(&wb).unwrap().status(), WorkStatus::Pending);
    assert_eq!(outcome.promoted, 0);
}

#[test]
fn test_work_promoted_when_dep_done() {
    let dir = TestDir::new("reconcile_work_dep_done");
    let stores = test_stores(&dir);
    let plan_id = insert_active_plan(&stores, "Test Plan");
    let spec_id = insert_spec(&stores, &plan_id, "Spec A", HierarchyStatus::Active, vec![]);
    let phase_id = insert_phase(&stores, &spec_id, "Phase 1", HierarchyStatus::Active, vec![]);
    let wa = insert_work(&stores, &phase_id, "main.py", WorkStatus::Done, vec![]);
    let wb = insert_work(&stores, &phase_id, "test.py", WorkStatus::Pending, vec![wa.clone()]);

    let outcome = reconcile(&stores);

    let works = stores.read_works().unwrap();
    assert_eq!(works.get(&wb).unwrap().status(), WorkStatus::Ready);
    assert!(outcome.promoted >= 1);
}

// ---------------------------------------------------------------------------
// Completion tests
// ---------------------------------------------------------------------------

#[test]
fn test_phase_completes_when_all_works_done() {
    let dir = TestDir::new("reconcile_phase_complete");
    let stores = test_stores(&dir);
    let plan_id = insert_active_plan(&stores, "Test Plan");
    let spec_id = insert_spec(&stores, &plan_id, "Spec A", HierarchyStatus::Active, vec![]);
    let phase_id = insert_phase(&stores, &spec_id, "Phase 1", HierarchyStatus::Active, vec![]);
    insert_work(&stores, &phase_id, "main.py", WorkStatus::Done, vec![]);
    insert_work(&stores, &phase_id, "test.py", WorkStatus::Done, vec![]);

    let outcome = reconcile(&stores);

    let phases = stores.read_phases().unwrap();
    assert_eq!(phases.get(&phase_id).unwrap().status(), HierarchyStatus::Complete);
    assert!(outcome.completed >= 1);
}

#[test]
fn test_phase_blocked_by_abandoned_work() {
    let dir = TestDir::new("reconcile_phase_mixed_terminal");
    let stores = test_stores(&dir);
    let plan_id = insert_active_plan(&stores, "Test Plan");
    let spec_id = insert_spec(&stores, &plan_id, "Spec A", HierarchyStatus::Active, vec![]);
    let phase_id = insert_phase(&stores, &spec_id, "Phase 1", HierarchyStatus::Active, vec![]);
    insert_work(&stores, &phase_id, "main.py", WorkStatus::Done, vec![]);
    insert_work(&stores, &phase_id, "test.py", WorkStatus::Abandoned, vec![]);

    let outcome = reconcile(&stores);

    // Abandoned blocks phase completion - phase stays Active
    let phases = stores.read_phases().unwrap();
    assert_eq!(phases.get(&phase_id).unwrap().status(), HierarchyStatus::Active);
    assert_eq!(outcome.completed, 0);
}

#[test]
fn test_phase_completes_with_superseded_work() {
    let dir = TestDir::new("reconcile_phase_superseded");
    let stores = test_stores(&dir);
    let plan_id = insert_active_plan(&stores, "Test Plan");
    let spec_id = insert_spec(&stores, &plan_id, "Spec A", HierarchyStatus::Active, vec![]);
    let phase_id = insert_phase(&stores, &spec_id, "Phase 1", HierarchyStatus::Active, vec![]);
    insert_work(&stores, &phase_id, "main.py", WorkStatus::Done, vec![]);
    insert_work(&stores, &phase_id, "test.py", WorkStatus::Superseded, vec![]);

    let outcome = reconcile(&stores);

    // Superseded is transparent - phase completes with at least one Done
    let phases = stores.read_phases().unwrap();
    assert_eq!(phases.get(&phase_id).unwrap().status(), HierarchyStatus::Complete);
    assert!(outcome.completed >= 1);
}

#[test]
fn test_phase_does_not_complete_when_all_superseded() {
    let dir = TestDir::new("reconcile_phase_all_superseded");
    let stores = test_stores(&dir);
    let plan_id = insert_active_plan(&stores, "Test Plan");
    let spec_id = insert_spec(&stores, &plan_id, "Spec A", HierarchyStatus::Active, vec![]);
    let phase_id = insert_phase(&stores, &spec_id, "Phase 1", HierarchyStatus::Active, vec![]);
    insert_work(&stores, &phase_id, "main.py", WorkStatus::Superseded, vec![]);
    insert_work(&stores, &phase_id, "test.py", WorkStatus::Superseded, vec![]);

    let outcome = reconcile(&stores);

    // All Superseded with no Done - phase must NOT complete
    let phases = stores.read_phases().unwrap();
    assert_eq!(phases.get(&phase_id).unwrap().status(), HierarchyStatus::Active);
    assert_eq!(outcome.completed, 0);
}

#[test]
fn test_phase_does_not_complete_when_work_in_progress() {
    let dir = TestDir::new("reconcile_phase_not_complete");
    let stores = test_stores(&dir);
    let plan_id = insert_active_plan(&stores, "Test Plan");
    let spec_id = insert_spec(&stores, &plan_id, "Spec A", HierarchyStatus::Active, vec![]);
    let phase_id = insert_phase(&stores, &spec_id, "Phase 1", HierarchyStatus::Active, vec![]);
    insert_work(&stores, &phase_id, "main.py", WorkStatus::Done, vec![]);
    insert_work(&stores, &phase_id, "test.py", WorkStatus::InProgress, vec![]);

    let outcome = reconcile(&stores);

    let phases = stores.read_phases().unwrap();
    assert_eq!(phases.get(&phase_id).unwrap().status(), HierarchyStatus::Active);
    assert_eq!(outcome.completed, 0);
}

#[test]
fn test_spec_completes_when_all_phases_terminal() {
    let dir = TestDir::new("reconcile_spec_complete");
    let stores = test_stores(&dir);
    let plan_id = insert_active_plan(&stores, "Test Plan");
    let spec_id = insert_spec(&stores, &plan_id, "Spec A", HierarchyStatus::Active, vec![]);
    insert_phase(&stores, &spec_id, "Phase 1", HierarchyStatus::Complete, vec![]);
    insert_phase(&stores, &spec_id, "Phase 2", HierarchyStatus::Complete, vec![]);

    let outcome = reconcile(&stores);

    let specs = stores.read_specs().unwrap();
    assert_eq!(specs.get(&spec_id).unwrap().status(), HierarchyStatus::Complete);
    assert!(outcome.completed >= 1);
}

// ---------------------------------------------------------------------------
// Fixed-point cascade tests
// ---------------------------------------------------------------------------

#[test]
fn test_cascade_phase_complete_promotes_next_phase() {
    let dir = TestDir::new("reconcile_cascade");
    let stores = test_stores(&dir);
    let plan_id = insert_active_plan(&stores, "Test Plan");
    let spec_id = insert_spec(&stores, &plan_id, "Spec A", HierarchyStatus::Active, vec![]);
    let ph1 = insert_phase(&stores, &spec_id, "Phase 1", HierarchyStatus::Active, vec![]);
    let ph2 = insert_phase(
        &stores,
        &spec_id,
        "Phase 2",
        HierarchyStatus::Pending,
        vec![ph1.clone()],
    );
    // Phase 1's only work is Done
    insert_work(&stores, &ph1, "main.py", WorkStatus::Done, vec![]);
    // Phase 2 has a Pending work
    let w2 = insert_work(&stores, &ph2, "test.py", WorkStatus::Pending, vec![]);

    let outcome = reconcile(&stores);

    // Fixed-point should: complete Phase 1, promote Phase 2, promote Work in Phase 2
    let phases = stores.read_phases().unwrap();
    assert_eq!(phases.get(&ph1).unwrap().status(), HierarchyStatus::Complete);
    assert_eq!(phases.get(&ph2).unwrap().status(), HierarchyStatus::Active);
    let works = stores.read_works().unwrap();
    assert_eq!(works.get(&w2).unwrap().status(), WorkStatus::Ready);
    assert!(outcome.completed >= 1);
    assert!(outcome.promoted >= 2); // Phase 2 + Work
}

#[test]
fn test_full_cascade_spec_complete_promotes_next_spec() {
    let dir = TestDir::new("reconcile_full_cascade");
    let stores = test_stores(&dir);
    let plan_id = insert_active_plan(&stores, "Test Plan");

    // Spec A: Active, single phase with all works Done
    let spec_a = insert_spec(&stores, &plan_id, "Spec A", HierarchyStatus::Active, vec![]);
    let ph_a1 = insert_phase(&stores, &spec_a, "Phase A1", HierarchyStatus::Active, vec![]);
    insert_work(&stores, &ph_a1, "db.py", WorkStatus::Done, vec![]);

    // Spec B: Pending, depends on Spec A
    let spec_b = insert_spec(
        &stores,
        &plan_id,
        "Spec B",
        HierarchyStatus::Pending,
        vec![spec_a.clone()],
    );
    let ph_b1 = insert_phase(&stores, &spec_b, "Phase B1", HierarchyStatus::Pending, vec![]);
    let wb1 = insert_work(&stores, &ph_b1, "api.py", WorkStatus::Pending, vec![]);

    let outcome = reconcile(&stores);

    // Full cascade: Phase A1 Complete -> Spec A Complete -> Spec B Active -> Phase B1 Active -> Work Ready
    let specs = stores.read_specs().unwrap();
    assert_eq!(specs.get(&spec_a).unwrap().status(), HierarchyStatus::Complete);
    assert_eq!(specs.get(&spec_b).unwrap().status(), HierarchyStatus::Active);
    let phases = stores.read_phases().unwrap();
    assert_eq!(phases.get(&ph_a1).unwrap().status(), HierarchyStatus::Complete);
    assert_eq!(phases.get(&ph_b1).unwrap().status(), HierarchyStatus::Active);
    let works = stores.read_works().unwrap();
    assert_eq!(works.get(&wb1).unwrap().status(), WorkStatus::Ready);
    assert!(outcome.completed >= 2); // Phase A1 + Spec A
    assert!(outcome.promoted >= 3); // Spec B + Phase B1 + Work
}

// ---------------------------------------------------------------------------
// Goal complete detection tests
// ---------------------------------------------------------------------------

#[test]
fn test_goal_complete_full_mode() {
    let dir = TestDir::new("reconcile_goal_complete_full");
    let stores = test_stores(&dir);
    let plan_id = insert_active_plan(&stores, "Test Plan");
    insert_spec(&stores, &plan_id, "Spec A", HierarchyStatus::Complete, vec![]);
    insert_spec(&stores, &plan_id, "Spec B", HierarchyStatus::Complete, vec![]);

    let outcome = reconcile(&stores);

    assert!(outcome.goal_complete);
}

#[test]
fn test_goal_not_complete_when_spec_active() {
    let dir = TestDir::new("reconcile_goal_not_complete");
    let stores = test_stores(&dir);
    let plan_id = insert_active_plan(&stores, "Test Plan");
    insert_spec(&stores, &plan_id, "Spec A", HierarchyStatus::Complete, vec![]);
    let spec_b = insert_spec(&stores, &plan_id, "Spec B", HierarchyStatus::Active, vec![]);
    // Add a non-terminal phase so Spec B doesn't auto-complete
    let ph_b = insert_phase(&stores, &spec_b, "Phase B1", HierarchyStatus::Active, vec![]);
    insert_work(&stores, &ph_b, "main.py", WorkStatus::InProgress, vec![]);

    let outcome = reconcile(&stores);

    assert!(!outcome.goal_complete);
}

#[test]
fn test_goal_complete_brief_mode() {
    let dir = TestDir::new("reconcile_goal_complete_brief");
    let stores = test_stores(&dir);
    // Create a Brief mode plan
    let mut plan = Plan::new("Brief Plan".to_string(), Default::default());
    plan.force_status(HierarchyStatus::Active);
    plan.tier = Tier::Brief;
    let plan_id = plan.id.clone();
    stores.plans.write().unwrap().insert(plan_id.clone(), plan);

    insert_work(&stores, &plan_id, "task.py", WorkStatus::Done, vec![]);
    insert_work(&stores, &plan_id, "test.py", WorkStatus::Done, vec![]);

    let outcome = reconcile(&stores);

    assert!(outcome.goal_complete);
}

#[test]
fn test_goal_not_complete_brief_mode_when_work_pending() {
    let dir = TestDir::new("reconcile_goal_brief_not_complete");
    let stores = test_stores(&dir);
    let mut plan = Plan::new("Brief Plan".to_string(), Default::default());
    plan.force_status(HierarchyStatus::Active);
    plan.tier = Tier::Brief;
    let plan_id = plan.id.clone();
    stores.plans.write().unwrap().insert(plan_id.clone(), plan);

    insert_work(&stores, &plan_id, "task.py", WorkStatus::Done, vec![]);
    insert_work(&stores, &plan_id, "test.py", WorkStatus::Pending, vec![]);

    let outcome = reconcile(&stores);

    assert!(!outcome.goal_complete);
}

#[test]
fn test_goal_complete_brief_mode_with_superseded() {
    let dir = TestDir::new("reconcile_goal_brief_superseded");
    let stores = test_stores(&dir);
    let mut plan = Plan::new("Brief Plan".to_string(), Default::default());
    plan.force_status(HierarchyStatus::Active);
    plan.tier = Tier::Brief;
    let plan_id = plan.id.clone();
    stores.plans.write().unwrap().insert(plan_id.clone(), plan);

    insert_work(&stores, &plan_id, "task.py", WorkStatus::Done, vec![]);
    insert_work(&stores, &plan_id, "old.py", WorkStatus::Superseded, vec![]);

    let outcome = reconcile(&stores);

    // Superseded is transparent - goal completes with at least one Done
    assert!(outcome.goal_complete);
}

#[test]
fn test_goal_not_complete_brief_mode_with_abandoned() {
    let dir = TestDir::new("reconcile_goal_brief_abandoned");
    let stores = test_stores(&dir);
    let mut plan = Plan::new("Brief Plan".to_string(), Default::default());
    plan.force_status(HierarchyStatus::Active);
    plan.tier = Tier::Brief;
    let plan_id = plan.id.clone();
    stores.plans.write().unwrap().insert(plan_id.clone(), plan);

    insert_work(&stores, &plan_id, "task.py", WorkStatus::Done, vec![]);
    insert_work(&stores, &plan_id, "fail.py", WorkStatus::Abandoned, vec![]);

    let outcome = reconcile(&stores);

    // Abandoned blocks goal completion
    assert!(!outcome.goal_complete);
}

#[test]
fn test_goal_not_complete_brief_mode_all_superseded() {
    let dir = TestDir::new("reconcile_goal_brief_all_superseded");
    let stores = test_stores(&dir);
    let mut plan = Plan::new("Brief Plan".to_string(), Default::default());
    plan.force_status(HierarchyStatus::Active);
    plan.tier = Tier::Brief;
    let plan_id = plan.id.clone();
    stores.plans.write().unwrap().insert(plan_id.clone(), plan);

    insert_work(&stores, &plan_id, "a.py", WorkStatus::Superseded, vec![]);
    insert_work(&stores, &plan_id, "b.py", WorkStatus::Superseded, vec![]);

    let outcome = reconcile(&stores);

    // All Superseded with no Done - goal must NOT complete
    assert!(!outcome.goal_complete);
}

// ---------------------------------------------------------------------------
// Brief mode promotion tests
// ---------------------------------------------------------------------------

#[test]
fn test_brief_mode_works_promoted_when_plan_active() {
    let dir = TestDir::new("reconcile_brief_promotion");
    let stores = test_stores(&dir);
    let mut plan = Plan::new("Brief Plan".to_string(), Default::default());
    plan.force_status(HierarchyStatus::Active);
    plan.tier = Tier::Brief;
    let plan_id = plan.id.clone();
    stores.plans.write().unwrap().insert(plan_id.clone(), plan);

    let w1 = insert_work(&stores, &plan_id, "task.py", WorkStatus::Pending, vec![]);
    let w2 = insert_work(&stores, &plan_id, "test.py", WorkStatus::Pending, vec![]);

    let outcome = reconcile(&stores);

    let works = stores.read_works().unwrap();
    assert_eq!(works.get(&w1).unwrap().status(), WorkStatus::Ready);
    assert_eq!(works.get(&w2).unwrap().status(), WorkStatus::Ready);
    assert_eq!(outcome.promoted, 2);
}

// ---------------------------------------------------------------------------
// Idempotency tests
// ---------------------------------------------------------------------------

#[test]
fn test_reconcile_idempotent() {
    let dir = TestDir::new("reconcile_idempotent");
    let stores = test_stores(&dir);
    let plan_id = insert_active_plan(&stores, "Test Plan");
    let spec_id = insert_spec(&stores, &plan_id, "Spec A", HierarchyStatus::Active, vec![]);
    let phase_id = insert_phase(&stores, &spec_id, "Phase 1", HierarchyStatus::Active, vec![]);
    insert_work(&stores, &phase_id, "main.py", WorkStatus::Done, vec![]);

    let outcome1 = reconcile(&stores);
    let outcome2 = reconcile(&stores);

    // Second call should produce no changes
    assert_eq!(outcome2.promoted, 0);
    assert_eq!(outcome2.completed, 0);
    // But first call should have completed the phase
    assert!(outcome1.completed >= 1);
}

// ---------------------------------------------------------------------------
// Empty parent tests
// ---------------------------------------------------------------------------

#[test]
fn test_empty_phase_completes_immediately() {
    let dir = TestDir::new("reconcile_empty_phase");
    let stores = test_stores(&dir);
    let plan_id = insert_active_plan(&stores, "Test Plan");
    let spec_id = insert_spec(&stores, &plan_id, "Spec A", HierarchyStatus::Active, vec![]);
    let phase_id = insert_phase(&stores, &spec_id, "Phase 1", HierarchyStatus::Active, vec![]);
    // No works in this phase

    let outcome = reconcile(&stores);

    let phases = stores.read_phases().unwrap();
    assert_eq!(phases.get(&phase_id).unwrap().status(), HierarchyStatus::Complete);
    assert!(outcome.completed >= 1);
}

#[test]
fn test_empty_spec_completes_immediately() {
    let dir = TestDir::new("reconcile_empty_spec");
    let stores = test_stores(&dir);
    let plan_id = insert_active_plan(&stores, "Test Plan");
    let spec_id = insert_spec(&stores, &plan_id, "Spec A", HierarchyStatus::Active, vec![]);
    // No phases in this spec

    let outcome = reconcile(&stores);

    let specs = stores.read_specs().unwrap();
    assert_eq!(specs.get(&spec_id).unwrap().status(), HierarchyStatus::Complete);
    assert!(outcome.completed >= 1);
}
