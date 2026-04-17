#![allow(clippy::unwrap_used, unused_imports)]

use serde_json::json;

use crate::config::IntegratorConfig;
use crate::domain::tick::TickStatus;
use crate::test_util::TestDir;
use crate::worktree::manager::WorktreeManager;

use super::fixtures::*;

#[tokio::test]
async fn test_full_pipeline_plan_to_bundle_acceptance() {
    let stores = test_stores();
    let tx = test_event_tx();
    let wm = test_worktree_mgr();
    let ic = test_integrator_config();

    seed_goal(&stores, "Implement user auth");

    // 2. Create full hierarchy
    let plan = dispatch_ok(
        &stores,
        &tx,
        &wm,
        &ic,
        "plan.create",
        json!({"title": "User Auth", "description": "Auth system", "acceptance-criteria": "Tests pass"}),
    )
    .await;
    let plan_id = plan["id"].as_str().unwrap().to_string();
    dispatch_ok(
        &stores,
        &tx,
        &wm,
        &ic,
        "plan.transition",
        json!({"id": plan_id, "target-status": "active"}),
    )
    .await;

    let spec = dispatch_ok(
        &stores,
        &tx,
        &wm,
        &ic,
        "spec.create",
        json!({"parent-id": plan_id, "title": "JWT", "description": "JWT auth", "acceptance-criteria": "OK"}),
    )
    .await;
    let spec_id = spec["id"].as_str().unwrap().to_string();
    dispatch_ok(
        &stores,
        &tx,
        &wm,
        &ic,
        "spec.transition",
        json!({"id": spec_id, "target-status": "active"}),
    )
    .await;

    let phase = dispatch_ok(
        &stores,
        &tx,
        &wm,
        &ic,
        "phase.create",
        json!({"parent-id": spec_id, "title": "Token", "description": "Token gen", "acceptance-criteria": "OK"}),
    )
    .await;
    let phase_id = phase["id"].as_str().unwrap().to_string();
    dispatch_ok(
        &stores,
        &tx,
        &wm,
        &ic,
        "phase.transition",
        json!({"id": phase_id, "target-status": "active"}),
    )
    .await;

    let wi = dispatch_ok(
        &stores,
        &tx,
        &wm,
        &ic,
        "work.create",
        json!({"parent-id": phase_id, "title": "sign()", "description": "Sign JWT", "files": ["src/"], "acceptance-criteria": ["tests pass"]}),
    ).await;
    let wi_id = wi["id"].as_str().unwrap().to_string();

    // 3. Coordinator assigns, implementer works on it (already Ready via auto-promotion)
    dispatch_ok(
        &stores,
        &tx,
        &wm,
        &ic,
        "work.transition",
        json!({"id": wi_id, "target-status": "InProgress", "role": "coordinator", "assignee": "agent-1"}),
    )
    .await;

    let bundle = dispatch_ok(
        &stores,
        &tx,
        &wm,
        &ic,
        "bundle.create",
        json!({"work-id": wi_id, "branch-name": "feat/sign", "claims": "Added sign()"}),
    )
    .await;
    let bundle_id = bundle["id"].as_str().unwrap().to_string();

    // 4. Coordinator triages, reviewer reviews
    dispatch_ok(
        &stores,
        &tx,
        &wm,
        &ic,
        "bundle.transition",
        json!({"id": bundle_id, "target-status": "Triaged", "role": "coordinator"}),
    )
    .await;
    dispatch_ok(
        &stores,
        &tx,
        &wm,
        &ic,
        "bundle.transition",
        json!({"id": bundle_id, "target-status": "Reviewed", "role": "reviewer", "verification": "tests passed"}),
    )
    .await;

    // 5. Coordinator accepts
    dispatch_ok(
        &stores,
        &tx,
        &wm,
        &ic,
        "bundle.transition",
        json!({"id": bundle_id, "target-status": "Accepted", "role": "coordinator"}),
    )
    .await;

    // 6. Integrator creates tick and publishes
    let tick = dispatch_ok(&stores, &tx, &wm, &ic, "tick.create", json!({"number": 1})).await;
    let tick_id = tick["id"].as_str().unwrap().to_string();
    dispatch_ok(
        &stores,
        &tx,
        &wm,
        &ic,
        "tick.transition",
        json!({"id": tick_id, "target-status": "Sealing", "role": "integrator"}),
    )
    .await;
    dispatch_ok(
        &stores,
        &tx,
        &wm,
        &ic,
        "tick.transition",
        json!({"id": tick_id, "target-status": "Validating", "role": "integrator"}),
    )
    .await;
    dispatch_ok(
        &stores,
        &tx,
        &wm,
        &ic,
        "tick.transition",
        json!({"id": tick_id, "target-status": "Published", "role": "integrator"}),
    )
    .await;

    // 7. Mark work item as InReview -> Integrated -> Done
    dispatch_ok(
        &stores,
        &tx,
        &wm,
        &ic,
        "work.transition",
        json!({"id": wi_id, "target-status": "InReview", "role": "implementer"}),
    )
    .await;
    dispatch_ok(
        &stores,
        &tx,
        &wm,
        &ic,
        "work.transition",
        json!({"id": wi_id, "target-status": "Integrated", "role": "integrator"}),
    )
    .await;
    dispatch_ok(
        &stores,
        &tx,
        &wm,
        &ic,
        "work.transition",
        json!({"id": wi_id, "target-status": "Done", "role": "coordinator"}),
    )
    .await;

    // Verify final state across all stores
    let plans = stores.plans.read().unwrap();
    assert_eq!(plans[&plan_id].status(), crate::domain::plan::HierarchyStatus::Active);

    let bundles = stores.bundles.read().unwrap();
    assert_eq!(
        bundles[&bundle_id].status(),
        crate::domain::bundle::BundleStatus::Accepted
    );

    let ticks = stores.ticks.read().unwrap();
    assert_eq!(ticks[&tick_id].status(), TickStatus::Published);

    let wis = stores.works.read().unwrap();
    assert_eq!(wis[&wi_id].status(), crate::domain::work::WorkStatus::Done);
}

#[tokio::test]
async fn test_full_mvp4_pipeline() {
    // End-to-end: goal -> plan -> spec -> phase -> work -> bundle -> triage -> review -> accept
    let stores = test_stores();
    let tx = test_event_tx();
    let wm = test_worktree_mgr();
    let ic = test_integrator_config();

    seed_goal(&stores, "Build example website");

    // 2. Create Plan
    let plan = dispatch_ok(
        &stores,
        &tx,
        &wm,
        &ic,
        "plan.create",
        json!({"title": "Website Plan", "description": "Build a static site", "acceptance-criteria": "Site loads"}),
    )
    .await;
    let plan_id = plan["id"].as_str().unwrap().to_string();
    dispatch_ok(
        &stores,
        &tx,
        &wm,
        &ic,
        "plan.transition",
        json!({"id": plan_id, "target-status": "active"}),
    )
    .await;

    // 3. Create Spec
    let spec = dispatch_ok(
        &stores,
        &tx,
        &wm,
        &ic,
        "spec.create",
        json!({"parent-id": plan_id, "title": "HTML Structure", "description": "Create HTML pages"}),
    )
    .await;
    let spec_id = spec["id"].as_str().unwrap().to_string();
    dispatch_ok(
        &stores,
        &tx,
        &wm,
        &ic,
        "spec.transition",
        json!({"id": spec_id, "target-status": "active"}),
    )
    .await;

    // 4. Create Phase
    let phase = dispatch_ok(
        &stores,
        &tx,
        &wm,
        &ic,
        "phase.create",
        json!({"parent-id": spec_id, "title": "Phase 1: Index", "description": "Create index.html"}),
    )
    .await;
    let phase_id = phase["id"].as_str().unwrap().to_string();
    dispatch_ok(
        &stores,
        &tx,
        &wm,
        &ic,
        "phase.transition",
        json!({"id": phase_id, "target-status": "active"}),
    )
    .await;

    // 5. Create Work
    let wi = dispatch_ok(
        &stores,
        &tx,
        &wm,
        &ic,
        "work.create",
        json!({"parent-id": phase_id, "title": "Create index.html", "description": "Write the homepage", "files": ["src/"], "acceptance-criteria": ["tests pass"]}),
    ).await;
    let wi_id = wi["id"].as_str().unwrap().to_string();

    // 6. Assign Work (transition to InProgress; already Ready via auto-promotion)
    dispatch_ok(
        &stores,
        &tx,
        &wm,
        &ic,
        "work.transition",
        json!({"id": wi_id, "target-status": "InProgress", "role": "coordinator", "assignee": "agent-1"}),
    )
    .await;

    // 7. Create Bundle (implementer output)
    let bundle = dispatch_ok(
        &stores,
        &tx,
        &wm,
        &ic,
        "bundle.create",
        json!({
            "work-id": wi_id,
            "description": "Created index.html with basic structure",
            "files-changed": ["index.html"],
            "commit_sha": "def456",
            "branch-name": "feature-index"
        }),
    )
    .await;
    let bundle_id = bundle["id"].as_str().unwrap().to_string();

    // 8. Triage (Coordinator)
    dispatch_ok(
        &stores,
        &tx,
        &wm,
        &ic,
        "bundle.transition",
        json!({"id": bundle_id, "target-status": "Triaged", "role": "coordinator"}),
    )
    .await;

    // 9. Review (Reviewer)
    dispatch_ok(
        &stores,
        &tx,
        &wm,
        &ic,
        "bundle.transition",
        json!({"id": bundle_id, "target-status": "Reviewed", "role": "reviewer", "verification": "tests passed"}),
    )
    .await;

    // 10. Accept (Coordinator)
    dispatch_ok(
        &stores,
        &tx,
        &wm,
        &ic,
        "bundle.transition",
        json!({"id": bundle_id, "target-status": "Accepted", "role": "coordinator"}),
    )
    .await;

    // Verify final state
    {
        let bundles = stores.bundles.read().unwrap();
        assert_eq!(
            bundles[&bundle_id].status(),
            crate::domain::bundle::BundleStatus::Accepted
        );
    }

    // Verify record counts
    assert_eq!(stores.plans.read().unwrap().len(), 1);
    assert_eq!(stores.specs.read().unwrap().len(), 1);
    assert_eq!(stores.phases.read().unwrap().len(), 1);
    assert_eq!(stores.works.read().unwrap().len(), 1);
    assert_eq!(stores.bundles.read().unwrap().len(), 1);
}

#[tokio::test]
async fn test_e2e_full_pipeline_with_tmpdir_git_repo() {
    let dir = TestDir::new("loopr-e2e-full");

    // Initialize a real git repo so tick publish can get HEAD sha
    std::process::Command::new("git")
        .args(["init"])
        .current_dir(&dir)
        .output()
        .expect("git init failed");
    std::process::Command::new("git")
        .args(["config", "commit.gpgsign", "false"])
        .current_dir(&dir)
        .output()
        .expect("git config gpgsign failed");
    std::process::Command::new("git")
        .args(["commit", "--allow-empty", "-m", "initial"])
        .current_dir(&dir)
        .output()
        .expect("git commit failed");

    let stores = test_stores();
    let tx = test_event_tx();
    let wm = WorktreeManager::new(dir.to_path_buf(), dir.join(".worktrees"));
    let ic = IntegratorConfig {
        validation_commands: vec!["echo ok".to_string()],
        ..Default::default()
    };

    seed_goal(&stores, "Build portfolio site");

    // --- 2. Create full hierarchy ---
    let plan = dispatch_ok(
        &stores,
        &tx,
        &wm,
        &ic,
        "plan.create",
        json!({"title": "Portfolio", "description": "Build a portfolio website"}),
    )
    .await;
    let plan_id = plan["id"].as_str().unwrap().to_string();
    dispatch_ok(
        &stores,
        &tx,
        &wm,
        &ic,
        "plan.transition",
        json!({"id": plan_id, "target-status": "active"}),
    )
    .await;

    let spec = dispatch_ok(
        &stores,
        &tx,
        &wm,
        &ic,
        "spec.create",
        json!({"parent-id": plan_id, "title": "Pages", "description": "HTML pages"}),
    )
    .await;
    let spec_id = spec["id"].as_str().unwrap().to_string();
    dispatch_ok(
        &stores,
        &tx,
        &wm,
        &ic,
        "spec.transition",
        json!({"id": spec_id, "target-status": "active"}),
    )
    .await;

    let phase = dispatch_ok(
        &stores,
        &tx,
        &wm,
        &ic,
        "phase.create",
        json!({"parent-id": spec_id, "title": "Phase 1", "description": "Structure", "order": 1}),
    )
    .await;
    let phase_id = phase["id"].as_str().unwrap().to_string();
    dispatch_ok(
        &stores,
        &tx,
        &wm,
        &ic,
        "phase.transition",
        json!({"id": phase_id, "target-status": "active"}),
    )
    .await;

    // --- 3. Create work items ---
    let wi1 = dispatch_ok(
        &stores,
        &tx,
        &wm,
        &ic,
        "work.create",
        json!({"parent-id": phase_id, "title": "Create index.html", "description": "Homepage", "files": ["src/"], "acceptance-criteria": ["tests pass"]}),
    ).await;
    let wi1_id = wi1["id"].as_str().unwrap().to_string();

    let wi2 = dispatch_ok(
        &stores,
        &tx,
        &wm,
        &ic,
        "work.create",
        json!({"parent-id": phase_id, "title": "Create about.html", "description": "About page", "files": ["src/"], "acceptance-criteria": ["tests pass"]}),
    ).await;
    let wi2_id = wi2["id"].as_str().unwrap().to_string();

    // --- 4. Lock lifecycle ---
    let lock = dispatch_ok(
        &stores,
        &tx,
        &wm,
        &ic,
        "lock.create",
        json!({"resource": "index.html", "holder-id": wi1_id, "granted-by": "coordinator"}),
    )
    .await;
    let lock_id = lock["id"].as_str().unwrap().to_string();
    assert_eq!(lock["status"], "active");

    let locks = dispatch_ok(&stores, &tx, &wm, &ic, "lock.list", json!({})).await;
    assert!(locks.as_array().unwrap().iter().any(|l| l["id"] == lock_id));

    dispatch_ok(&stores, &tx, &wm, &ic, "lock.release", json!({"id": lock_id})).await;
    let released = dispatch_ok(&stores, &tx, &wm, &ic, "lock.get", json!({"id": lock_id})).await;
    assert_eq!(released["status"], "released");

    // --- 5. Full work item lifecycle: Ready -> InProgress -> InReview -> Integrated -> Done ---
    // (auto-promoted from Draft to Ready since acceptance_criteria present)
    dispatch_ok(
        &stores,
        &tx,
        &wm,
        &ic,
        "work.transition",
        json!({"id": wi1_id, "target-status": "InProgress", "role": "coordinator", "assignee": "agent-1"}),
    )
    .await;
    // Create a bundle before InReview (required by #15 invariant)
    dispatch_ok(
        &stores,
        &tx,
        &wm,
        &ic,
        "bundle.create",
        json!({"work-id": wi1_id, "branch-name": "feature/index"}),
    )
    .await;
    dispatch_ok(
        &stores,
        &tx,
        &wm,
        &ic,
        "work.transition",
        json!({"id": wi1_id, "target-status": "InReview", "role": "implementer"}),
    )
    .await;
    dispatch_ok(
        &stores,
        &tx,
        &wm,
        &ic,
        "work.transition",
        json!({"id": wi1_id, "target-status": "Integrated", "role": "integrator"}),
    )
    .await;
    dispatch_ok(
        &stores,
        &tx,
        &wm,
        &ic,
        "work.transition",
        json!({"id": wi1_id, "target-status": "Done", "role": "coordinator"}),
    )
    .await;
    {
        let wis = stores.works.read().unwrap();
        assert_eq!(wis[&wi1_id].status(), crate::domain::work::WorkStatus::Done);
    }

    // --- 6. Bundle full lifecycle: Proposed -> Triaged -> Reviewed -> Accepted -> Integrating -> Merged ---
    // WI2 already Ready via auto-promotion; transition to InProgress
    dispatch_ok(
        &stores,
        &tx,
        &wm,
        &ic,
        "work.transition",
        json!({"id": wi2_id, "target-status": "InProgress", "role": "coordinator", "assignee": "agent-1"}),
    )
    .await;

    let bundle = dispatch_ok(
        &stores,
        &tx,
        &wm,
        &ic,
        "bundle.create",
        json!({"work-id": wi2_id, "description": "About page", "branch-name": "feature/about"}),
    )
    .await;
    let bundle_id = bundle["id"].as_str().unwrap().to_string();
    assert_eq!(bundle["status"], "Proposed");

    dispatch_ok(
        &stores,
        &tx,
        &wm,
        &ic,
        "bundle.transition",
        json!({"id": bundle_id, "target-status": "Triaged", "role": "coordinator"}),
    )
    .await;
    dispatch_ok(
        &stores,
        &tx,
        &wm,
        &ic,
        "bundle.transition",
        json!({"id": bundle_id, "target-status": "Reviewed", "role": "reviewer", "verification": "tests passed"}),
    )
    .await;
    dispatch_ok(
        &stores,
        &tx,
        &wm,
        &ic,
        "bundle.transition",
        json!({"id": bundle_id, "target-status": "Accepted", "role": "coordinator"}),
    )
    .await;
    dispatch_ok(
        &stores,
        &tx,
        &wm,
        &ic,
        "bundle.transition",
        json!({"id": bundle_id, "target-status": "Integrating", "role": "integrator"}),
    )
    .await;
    dispatch_ok(
        &stores,
        &tx,
        &wm,
        &ic,
        "bundle.transition",
        json!({"id": bundle_id, "target-status": "Merged", "role": "integrator"}),
    )
    .await;
    {
        let bundles = stores.bundles.read().unwrap();
        assert_eq!(
            bundles[&bundle_id].status(),
            crate::domain::bundle::BundleStatus::Merged
        );
    }

    // --- 7. Reviewer rejection from Proposed (new FSM rule) ---
    let bundle2 = dispatch_ok(
        &stores,
        &tx,
        &wm,
        &ic,
        "bundle.create",
        json!({"work-id": wi2_id, "description": "Bad bundle", "branch-name": "feature/bad"}),
    )
    .await;
    let bundle2_id = bundle2["id"].as_str().unwrap().to_string();
    dispatch_ok(
        &stores,
        &tx,
        &wm,
        &ic,
        "bundle.transition",
        json!({"id": bundle2_id, "target-status": "Rejected", "role": "reviewer"}),
    )
    .await;
    {
        let bundles = stores.bundles.read().unwrap();
        assert_eq!(
            bundles[&bundle2_id].status(),
            crate::domain::bundle::BundleStatus::Rejected
        );
    }

    // --- 8. Reviewer rejection from Reviewed ---
    let bundle3 = dispatch_ok(
        &stores,
        &tx,
        &wm,
        &ic,
        "bundle.create",
        json!({"work-id": wi2_id, "description": "Reviewed then rejected", "branch-name": "feature/rev-reject"}),
    )
    .await;
    let bundle3_id = bundle3["id"].as_str().unwrap().to_string();
    dispatch_ok(
        &stores,
        &tx,
        &wm,
        &ic,
        "bundle.transition",
        json!({"id": bundle3_id, "target-status": "Triaged", "role": "coordinator"}),
    )
    .await;
    dispatch_ok(
        &stores,
        &tx,
        &wm,
        &ic,
        "bundle.transition",
        json!({"id": bundle3_id, "target-status": "Reviewed", "role": "reviewer", "verification": "tests passed"}),
    )
    .await;
    dispatch_ok(
        &stores,
        &tx,
        &wm,
        &ic,
        "bundle.transition",
        json!({"id": bundle3_id, "target-status": "Rejected", "role": "reviewer"}),
    )
    .await;
    {
        let bundles = stores.bundles.read().unwrap();
        assert_eq!(
            bundles[&bundle3_id].status(),
            crate::domain::bundle::BundleStatus::Rejected
        );
    }

    // --- 9. Tick publish with validation in tmpdir git repo ---
    let tick = dispatch_ok(&stores, &tx, &wm, &ic, "tick.create", json!({"number": 1})).await;
    let tick_id = tick["id"].as_str().unwrap().to_string();
    assert_eq!(tick["status"], "Open");

    let published = dispatch_ok(
        &stores,
        &tx,
        &wm,
        &ic,
        "integrator.publish",
        json!({"tick-id": tick_id}),
    )
    .await;
    assert_eq!(published["status"], "Published");
    assert!(published["integration_sha"].as_str().is_some_and(|s| !s.is_empty()));
    assert!(
        published["validation_log"]
            .as_str()
            .is_some_and(|s| s.contains("PASSED"))
    );

    // --- 10. Verify counts ---
    assert_eq!(stores.plans.read().unwrap().len(), 1);
    assert_eq!(stores.specs.read().unwrap().len(), 1);
    assert_eq!(stores.phases.read().unwrap().len(), 1);
    assert_eq!(stores.works.read().unwrap().len(), 2);
    assert_eq!(stores.bundles.read().unwrap().len(), 4);
    assert_eq!(stores.ticks.read().unwrap().len(), 1);

    // Cleanup
    let _ = std::fs::remove_dir_all(&dir);
}
