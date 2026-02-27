//! End-to-end integration tests for MVP4 multi-component flows.
//!
//! These tests exercise multi-step IPC flows through `dispatch()`, verifying
//! that components work correctly together across module boundaries.

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::sync::Arc;

    use serde_json::json;
    use tokio::sync::broadcast;

    use crate::agents::{AgentSession, AgentStatus, AgentType};
    use crate::config::{Config, IntegratorConfig};
    use crate::daemon::context::Stores;
    use crate::daemon::handlers::dispatch;
    use crate::domain::learning::{Learning, LearningScope};
    use crate::domain::tick::TickStatus;
    use crate::ipc::protocol::{DaemonEvent, DaemonRequest};
    use crate::worktree::manager::WorktreeManager;

    use std::path::PathBuf;

    fn test_stores() -> Arc<Stores> {
        Arc::new(Stores::new())
    }

    fn test_event_tx() -> broadcast::Sender<DaemonEvent> {
        let (tx, _) = broadcast::channel(64);
        tx
    }

    fn test_worktree_mgr() -> WorktreeManager {
        WorktreeManager::new(
            PathBuf::from("/nonexistent/repo"),
            PathBuf::from("/nonexistent/worktrees"),
        )
    }

    fn test_integrator_config() -> IntegratorConfig {
        IntegratorConfig {
            validation_commands: vec!["echo ok".to_string()],
            ..Default::default()
        }
    }

    /// Helper: dispatch a request and assert success, returning the result JSON.
    fn dispatch_ok(
        stores: &Arc<Stores>,
        tx: &broadcast::Sender<DaemonEvent>,
        wm: &WorktreeManager,
        ic: &IntegratorConfig,
        method: &str,
        params: serde_json::Value,
    ) -> serde_json::Value {
        let req = DaemonRequest::new(1, method, params);
        let resp = dispatch(stores, tx, wm, ic, req);
        assert!(!resp.is_error(), "{method} failed: {:?}", resp.error);
        resp.result.unwrap()
    }

    /// Helper: dispatch a request and assert error, returning the error code.
    fn dispatch_err(
        stores: &Arc<Stores>,
        tx: &broadcast::Sender<DaemonEvent>,
        wm: &WorktreeManager,
        ic: &IntegratorConfig,
        method: &str,
        params: serde_json::Value,
    ) -> i32 {
        let req = DaemonRequest::new(1, method, params);
        let resp = dispatch(stores, tx, wm, ic, req);
        assert!(resp.is_error(), "{method} expected error but got success");
        resp.error.unwrap().code
    }

    // ========================================================================
    // Test 1: Full hierarchy creation via IPC dispatch
    //         Plan → Spec → Phase → WorkItem → Bundle
    // ========================================================================

    #[test]
    fn test_full_hierarchy_creation_via_dispatch() {
        let stores = test_stores();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();
        let ic = test_integrator_config();

        // Create Plan
        let plan = dispatch_ok(
            &stores,
            &tx,
            &wm,
            &ic,
            "plan.create",
            json!({"title": "Auth System", "description": "Add authentication", "acceptance_criteria": "All tests pass"}),
        );
        let plan_id = plan["id"].as_str().unwrap().to_string();
        assert_eq!(plan["status"], "draft");

        // Transition Plan: Draft → Active (no validator, so no gate)
        let plan_active = dispatch_ok(
            &stores,
            &tx,
            &wm,
            &ic,
            "plan.transition",
            json!({"id": plan_id, "target": "active"}),
        );
        assert_eq!(plan_active["status"], "active");

        // Create Spec under Plan
        let spec = dispatch_ok(
            &stores,
            &tx,
            &wm,
            &ic,
            "spec.create",
            json!({"plan_id": plan_id, "title": "JWT Auth", "description": "Implement JWT-based auth", "acceptance_criteria": "JWT tokens work"}),
        );
        let spec_id = spec["id"].as_str().unwrap().to_string();
        assert_eq!(spec["status"], "draft");

        // Transition Spec: Draft → Active
        dispatch_ok(
            &stores,
            &tx,
            &wm,
            &ic,
            "spec.transition",
            json!({"id": spec_id, "target": "active"}),
        );

        // Create Phase under Spec
        let phase = dispatch_ok(
            &stores,
            &tx,
            &wm,
            &ic,
            "phase.create",
            json!({"spec_id": spec_id, "title": "Token Generation", "description": "Create token gen module", "acceptance_criteria": "Tokens are signed"}),
        );
        let phase_id = phase["id"].as_str().unwrap().to_string();

        // Transition Phase: Draft → Active
        dispatch_ok(
            &stores,
            &tx,
            &wm,
            &ic,
            "phase.transition",
            json!({"id": phase_id, "target": "active"}),
        );

        // Create WorkItem under Phase
        let wi = dispatch_ok(
            &stores,
            &tx,
            &wm,
            &ic,
            "work_item.create",
            json!({"phase_id": phase_id, "title": "Implement sign()", "description": "JWT signing function"}),
        );
        let wi_id = wi["id"].as_str().unwrap().to_string();
        assert_eq!(wi["status"], "open");

        // Transition WorkItem: Open → InProgress
        dispatch_ok(
            &stores,
            &tx,
            &wm,
            &ic,
            "work_item.transition",
            json!({"id": wi_id, "target": "in_progress", "role": "implementer"}),
        );

        // Create Bundle for WorkItem
        let bundle = dispatch_ok(
            &stores,
            &tx,
            &wm,
            &ic,
            "bundle.create",
            json!({"work_item_id": wi_id, "branch_name": "feat/jwt-sign", "claims": "Added sign() function"}),
        );
        let bundle_id = bundle["id"].as_str().unwrap().to_string();
        assert_eq!(bundle["status"], "proposed");

        // Verify full hierarchy in stores
        assert_eq!(stores.plans.read().unwrap().len(), 1);
        assert_eq!(stores.specs.read().unwrap().len(), 1);
        assert_eq!(stores.phases.read().unwrap().len(), 1);
        assert_eq!(stores.work_items.read().unwrap().len(), 1);
        assert_eq!(stores.bundles.read().unwrap().len(), 1);

        // Verify correct parent-child relationships
        let specs = stores.specs.read().unwrap();
        assert_eq!(specs[&spec_id].plan_id, plan_id);
        let phases = stores.phases.read().unwrap();
        assert_eq!(phases[&phase_id].spec_id, spec_id);
        let work_items = stores.work_items.read().unwrap();
        assert_eq!(work_items[&wi_id].phase_id, phase_id);
        let bundles = stores.bundles.read().unwrap();
        assert_eq!(bundles[&bundle_id].work_item_id, wi_id);
    }

    // ========================================================================
    // Test 2: Bundle lifecycle through full FSM
    //         Proposed → InReview → Approved → Accepted
    // ========================================================================

    #[test]
    fn test_bundle_lifecycle_via_dispatch() {
        let stores = test_stores();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();
        let ic = test_integrator_config();

        // Create WorkItem
        let wi = dispatch_ok(
            &stores,
            &tx,
            &wm,
            &ic,
            "work_item.create",
            json!({"phase_id": "ph-1", "title": "Task", "description": "desc"}),
        );
        let wi_id = wi["id"].as_str().unwrap().to_string();

        // Create Bundle
        let bundle = dispatch_ok(
            &stores,
            &tx,
            &wm,
            &ic,
            "bundle.create",
            json!({"work_item_id": wi_id, "branch_name": "feat/task", "claims": "Did it"}),
        );
        let bundle_id = bundle["id"].as_str().unwrap().to_string();

        // Proposed → InReview (Reviewer picks it up)
        dispatch_ok(
            &stores,
            &tx,
            &wm,
            &ic,
            "bundle.transition",
            json!({"id": bundle_id, "target": "in_review", "role": "reviewer"}),
        );

        // InReview → Approved (Reviewer approves)
        dispatch_ok(
            &stores,
            &tx,
            &wm,
            &ic,
            "bundle.transition",
            json!({"id": bundle_id, "target": "approved", "role": "reviewer"}),
        );

        // Approved → Accepted (Coordinator accepts)
        dispatch_ok(
            &stores,
            &tx,
            &wm,
            &ic,
            "bundle.transition",
            json!({"id": bundle_id, "target": "accepted", "role": "coordinator"}),
        );

        // Verify final state
        let bundles = stores.bundles.read().unwrap();
        assert_eq!(
            bundles[&bundle_id].status,
            crate::domain::bundle::BundleStatus::Accepted
        );
    }

    // ========================================================================
    // Test 3: Learning propagation — create, reinforce to auto-promotion
    // ========================================================================

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
                "scope": "workitem",
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

    // ========================================================================
    // Test 4: Learning contradiction prevents auto-promotion
    // ========================================================================

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

    // ========================================================================
    // Test 5: Pool exhaustion — multi-type enforcement
    // ========================================================================

    #[test]
    fn test_pool_exhaustion_multi_type() {
        let stores = test_stores();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();
        let ic = test_integrator_config();

        // Fill Coordinator pool (pool_size = 1)
        let session = AgentSession::new(AgentType::Coordinator, "model".into());
        stores
            .agent_sessions
            .write()
            .unwrap()
            .insert(session.id.clone(), session);

        // Second Coordinator should be rejected
        let code = dispatch_err(
            &stores,
            &tx,
            &wm,
            &ic,
            "agent.start",
            json!({"agent_type": "coordinator"}),
        );
        assert_eq!(code, -32004, "expected pool_exhausted error code");

        // But Implementer should still work (different pool, pool_size = 2)
        let resp = dispatch_ok(
            &stores,
            &tx,
            &wm,
            &ic,
            "agent.start",
            json!({"agent_type": "implementer"}),
        );
        assert!(resp["session_id"].as_str().is_some());

        // Fill Implementer pool (1 more to reach pool_size = 2)
        let resp2 = dispatch_ok(
            &stores,
            &tx,
            &wm,
            &ic,
            "agent.start",
            json!({"agent_type": "implementer"}),
        );
        assert!(resp2["session_id"].as_str().is_some());

        // Third Implementer should be rejected
        let code2 = dispatch_err(
            &stores,
            &tx,
            &wm,
            &ic,
            "agent.start",
            json!({"agent_type": "implementer"}),
        );
        assert_eq!(code2, -32004);
    }

    // ========================================================================
    // Test 6: Pool allows new session after terminal
    // ========================================================================

    #[test]
    fn test_pool_allows_after_terminal_session() {
        let stores = test_stores();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();
        let ic = test_integrator_config();

        // Add a Completed Coordinator session
        let mut session = AgentSession::new(AgentType::Coordinator, "model".into());
        let _ = session.transition_to(AgentStatus::Running);
        let _ = session.transition_to(AgentStatus::Completed);
        stores
            .agent_sessions
            .write()
            .unwrap()
            .insert(session.id.clone(), session);

        // Should be allowed — terminal sessions don't count
        let resp = dispatch_ok(
            &stores,
            &tx,
            &wm,
            &ic,
            "agent.start",
            json!({"agent_type": "coordinator"}),
        );
        assert!(resp["session_id"].as_str().is_some());
    }

    // ========================================================================
    // Test 7: Goal management — set, replace, clear
    // ========================================================================

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

        // Set second goal — first should be deactivated
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

    // ========================================================================
    // Test 8: Tick crash recovery — stuck Sealing/Validating → Failed
    //         Tests via direct store manipulation + Tick FSM verification
    // ========================================================================

    #[test]
    fn test_tick_crash_recovery_state() {
        let stores = test_stores();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();
        let ic = test_integrator_config();

        // Create a Tick via IPC
        let tick = dispatch_ok(&stores, &tx, &wm, &ic, "tick.create", json!({"number": 1}));
        let tick_id = tick["id"].as_str().unwrap().to_string();

        // Transition to Sealing (simulating normal path before crash)
        dispatch_ok(
            &stores,
            &tx,
            &wm,
            &ic,
            "tick.transition",
            json!({"id": tick_id, "target": "sealing", "role": "integrator"}),
        );

        // Verify stuck in Sealing
        let ticks = stores.ticks.read().unwrap();
        assert_eq!(ticks[&tick_id].status, TickStatus::Sealing);
        drop(ticks);

        // Transition to Failed (as crash recovery would do)
        dispatch_ok(
            &stores,
            &tx,
            &wm,
            &ic,
            "tick.transition",
            json!({"id": tick_id, "target": "failed", "role": "integrator"}),
        );

        // Tick should be Failed
        let ticks = stores.ticks.read().unwrap();
        assert_eq!(ticks[&tick_id].status, TickStatus::Failed);
    }

    // ========================================================================
    // Test 9: Lock management — create, conflict detection, release
    // ========================================================================

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
                "owner": "coordinator"
            }),
        );
        let lock_id = lock["id"].as_str().unwrap().to_string();
        assert_eq!(lock["resource"], "src/main.rs");
        assert_eq!(lock["status"], "active");

        // List locks — should have one active
        let locks = dispatch_ok(&stores, &tx, &wm, &ic, "lock.list", json!({}));
        assert_eq!(locks.as_array().unwrap().len(), 1);

        // Release the lock
        dispatch_ok(&stores, &tx, &wm, &ic, "lock.release", json!({"id": lock_id}));

        // Lock should be released
        let lock_state = stores.locks.read().unwrap();
        assert_eq!(lock_state[&lock_id].status, crate::domain::lock::LockStatus::Released);
    }

    // ========================================================================
    // Test 10: Coordinator state summary includes all record types
    // ========================================================================

    #[test]
    fn test_coordinator_state_summary_multi_record() {
        let stores = test_stores();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();
        let ic = test_integrator_config();

        // Populate stores via IPC
        dispatch_ok(
            &stores,
            &tx,
            &wm,
            &ic,
            "plan.create",
            json!({"title": "Plan A", "description": "desc", "acceptance_criteria": "pass"}),
        );
        dispatch_ok(
            &stores,
            &tx,
            &wm,
            &ic,
            "work_item.create",
            json!({"phase_id": "ph-1", "title": "WI-1", "description": "desc"}),
        );
        dispatch_ok(
            &stores,
            &tx,
            &wm,
            &ic,
            "learning.create",
            json!({"source_id": "wi-1", "scope": "global", "content": "Test insight"}),
        );

        // Add agent session
        let session = AgentSession::new(AgentType::Implementer, "model".into());
        stores
            .agent_sessions
            .write()
            .unwrap()
            .insert(session.id.clone(), session);

        // Build state summary (used by Coordinator to understand current state)
        let summary = crate::agents::coordinator::build_state_summary(&stores);
        assert!(summary.contains("Plan A"), "summary should include plan");
        assert!(summary.contains("WI-1"), "summary should include work item");
        assert!(summary.contains("Test insight"), "summary should include learning");
        assert!(summary.contains("implementer"), "summary should include agent session");
    }

    // ========================================================================
    // Test 11: Context builder per-role produces different learnings
    // ========================================================================

    #[test]
    fn test_context_builder_role_filtering() {
        use crate::domain::role::Role;

        let mut learnings = HashMap::new();

        // Create learnings for specific roles
        let mut l1 = Learning::new("wi-1".into(), LearningScope::WorkItem, "Impl insight".into());
        l1.applicable_roles = Some(vec![Role::Implementer]);
        l1.confidence = 0.8;
        learnings.insert(l1.id.clone(), l1);

        let mut l2 = Learning::new("wi-1".into(), LearningScope::WorkItem, "Review insight".into());
        l2.applicable_roles = Some(vec![Role::Reviewer]);
        l2.confidence = 0.8;
        learnings.insert(l2.id.clone(), l2);

        let mut l3 = Learning::new("wi-1".into(), LearningScope::Global, "Global insight".into());
        l3.applicable_roles = None; // All roles
        l3.confidence = 0.8;
        learnings.insert(l3.id.clone(), l3);

        // Select learnings for Implementer
        let scope_ids = [("wi-1", LearningScope::WorkItem)];
        let impl_learnings =
            crate::agents::context::select_learnings(&learnings, &scope_ids, Role::Implementer, 0.3, 100);

        // Should include implementer-specific and global, but NOT reviewer-specific
        assert!(
            impl_learnings.iter().any(|l| l.content == "Impl insight"),
            "should include implementer learning"
        );
        assert!(
            impl_learnings.iter().any(|l| l.content == "Global insight"),
            "should include global learning"
        );
        assert!(
            !impl_learnings.iter().any(|l| l.content == "Review insight"),
            "should NOT include reviewer learning"
        );

        // Select learnings for Reviewer
        let rev_learnings = crate::agents::context::select_learnings(&learnings, &scope_ids, Role::Reviewer, 0.3, 100);

        assert!(
            rev_learnings.iter().any(|l| l.content == "Review insight"),
            "should include reviewer learning"
        );
        assert!(
            rev_learnings.iter().any(|l| l.content == "Global insight"),
            "should include global learning"
        );
        assert!(
            !rev_learnings.iter().any(|l| l.content == "Impl insight"),
            "should NOT include implementer learning"
        );
    }

    // ========================================================================
    // Test 12: Researcher path sandboxing rejects dangerous paths
    // ========================================================================

    #[test]
    fn test_researcher_path_sandboxing() {
        use std::path::Path;

        let repo_root = Path::new("/tmp/test-repo");

        // Valid relative path
        assert!(
            crate::agents::researcher::validate_path(repo_root, "src/main.rs").is_ok(),
            "relative path should be valid"
        );

        // Absolute path rejected
        assert!(
            crate::agents::researcher::validate_path(repo_root, "/etc/passwd").is_err(),
            "absolute path should be rejected"
        );

        // Path traversal rejected
        assert!(
            crate::agents::researcher::validate_path(repo_root, "../../../etc/passwd").is_err(),
            "traversal path should be rejected"
        );

        // Denied file patterns
        assert!(
            crate::agents::researcher::validate_path(repo_root, ".env").is_err(),
            ".env should be denied"
        );
        assert!(
            crate::agents::researcher::validate_path(repo_root, "keys/server.key").is_err(),
            "*.key should be denied"
        );
        assert!(
            crate::agents::researcher::validate_path(repo_root, "certs/server.pem").is_err(),
            "*.pem should be denied"
        );
        assert!(
            crate::agents::researcher::validate_path(repo_root, "credentials.json").is_err(),
            "credentials.* should be denied"
        );
    }

    // ========================================================================
    // Test 13: AgentAction::Transition with role inference
    // ========================================================================

    #[test]
    fn test_role_inference_from_agent_type() {
        use crate::domain::role::Role;

        assert_eq!(AgentType::Implementer.default_role(), Role::Implementer);
        assert_eq!(AgentType::Reviewer.default_role(), Role::Reviewer);
        assert_eq!(AgentType::Coordinator.default_role(), Role::Coordinator);
        assert_eq!(AgentType::Researcher.default_role(), Role::Researcher);
        assert_eq!(AgentType::Integrator.default_role(), Role::Integrator);
    }

    // ========================================================================
    // Test 14: WorkItem FSM rejects invalid transitions
    // ========================================================================

    #[test]
    fn test_work_item_fsm_enforcement_via_dispatch() {
        let stores = test_stores();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();
        let ic = test_integrator_config();

        // Create WorkItem (starts as Draft)
        let wi = dispatch_ok(
            &stores,
            &tx,
            &wm,
            &ic,
            "work_item.create",
            json!({"phase_id": "ph-1", "title": "Task", "description": "desc"}),
        );
        let wi_id = wi["id"].as_str().unwrap().to_string();

        // Invalid: Draft → Done (must go through InProgress first)
        let code = dispatch_err(
            &stores,
            &tx,
            &wm,
            &ic,
            "work_item.transition",
            json!({"id": wi_id, "target": "done", "role": "coordinator"}),
        );
        assert_ne!(code, 0, "should reject invalid transition");

        // Verify state unchanged
        let wis = stores.work_items.read().unwrap();
        assert_eq!(wis[&wi_id].status, crate::domain::work_item::WorkItemStatus::Draft);
    }

    // ========================================================================
    // Test 15: Multi-agent session management
    // ========================================================================

    #[test]
    fn test_multi_agent_session_coexistence() {
        let stores = test_stores();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();
        let ic = test_integrator_config();

        // Start sessions of different types
        let coord = dispatch_ok(
            &stores,
            &tx,
            &wm,
            &ic,
            "agent.start",
            json!({"agent_type": "coordinator"}),
        );
        let impl1 = dispatch_ok(
            &stores,
            &tx,
            &wm,
            &ic,
            "agent.start",
            json!({"agent_type": "implementer"}),
        );
        let impl2 = dispatch_ok(
            &stores,
            &tx,
            &wm,
            &ic,
            "agent.start",
            json!({"agent_type": "implementer"}),
        );
        let researcher = dispatch_ok(
            &stores,
            &tx,
            &wm,
            &ic,
            "agent.start",
            json!({"agent_type": "researcher"}),
        );

        // All should have unique session IDs
        let ids: Vec<&str> = [&coord, &impl1, &impl2, &researcher]
            .iter()
            .map(|r| r["session_id"].as_str().unwrap())
            .collect();
        let unique: std::collections::HashSet<&&str> = ids.iter().collect();
        assert_eq!(unique.len(), 4, "all session IDs should be unique");

        // List all agents
        let list = dispatch_ok(&stores, &tx, &wm, &ic, "agent.list", json!({}));
        assert_eq!(list.as_array().unwrap().len(), 4);
    }

    // ========================================================================
    // Test 16: Tick lifecycle — create → transition through states
    // ========================================================================

    #[test]
    fn test_tick_lifecycle_via_dispatch() {
        let stores = test_stores();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();
        let ic = test_integrator_config();

        // Create Tick
        let tick = dispatch_ok(&stores, &tx, &wm, &ic, "tick.create", json!({"number": 1}));
        let tick_id = tick["id"].as_str().unwrap().to_string();
        assert_eq!(tick["status"], "open");

        // Open → Sealing
        dispatch_ok(
            &stores,
            &tx,
            &wm,
            &ic,
            "tick.transition",
            json!({"id": tick_id, "target": "sealing", "role": "integrator"}),
        );

        // Sealing → Validating
        dispatch_ok(
            &stores,
            &tx,
            &wm,
            &ic,
            "tick.transition",
            json!({"id": tick_id, "target": "validating", "role": "integrator"}),
        );

        // Validating → Published
        dispatch_ok(
            &stores,
            &tx,
            &wm,
            &ic,
            "tick.transition",
            json!({"id": tick_id, "target": "published", "role": "integrator", "sha": "abc123"}),
        );

        // Verify final state
        let ticks = stores.ticks.read().unwrap();
        assert_eq!(ticks[&tick_id].status, TickStatus::Published);
    }

    // ========================================================================
    // Test 17: Generation level determination state machine
    // ========================================================================

    #[test]
    fn test_generation_level_progression() {
        use crate::agents::generation::{GenerationLevel, determine_generation_level};
        use crate::domain::phase::Phase;
        use crate::domain::plan::{HierarchyStatus, Plan};
        use crate::domain::spec::Spec;

        let stores = test_stores();

        // Empty stores → needs Plan
        assert_eq!(determine_generation_level(&stores), Some(GenerationLevel::Plan));

        // Add active Plan → needs Spec
        let mut plan = Plan::new("P".into(), "d".into(), "c".into());
        plan.status = HierarchyStatus::Active;
        let plan_id = plan.id.clone();
        stores.plans.write().unwrap().insert(plan.id.clone(), plan);

        assert_eq!(determine_generation_level(&stores), Some(GenerationLevel::Spec));

        // Add active Spec → needs Phase
        let mut spec = Spec::new(plan_id.clone(), "S".into(), "d".into());
        spec.status = HierarchyStatus::Active;
        let spec_id = spec.id.clone();
        stores.specs.write().unwrap().insert(spec.id.clone(), spec);

        assert_eq!(determine_generation_level(&stores), Some(GenerationLevel::Phase));

        // Add active Phase → needs WorkItem
        let mut phase = Phase::new(spec_id.clone(), "Ph".into(), "d".into(), 1);
        phase.status = HierarchyStatus::Active;
        stores.phases.write().unwrap().insert(phase.id.clone(), phase);

        assert_eq!(determine_generation_level(&stores), Some(GenerationLevel::WorkItem));
    }

    // ========================================================================
    // Test 18: Agent pause/resume lifecycle
    // ========================================================================

    #[test]
    fn test_agent_pause_resume_lifecycle() {
        let stores = test_stores();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();
        let ic = test_integrator_config();

        // Start a coordinator
        let resp = dispatch_ok(
            &stores,
            &tx,
            &wm,
            &ic,
            "agent.start",
            json!({"agent_type": "coordinator"}),
        );
        let session_id = resp["session_id"].as_str().unwrap().to_string();

        // Transition session to Running first (agent.start creates in Starting state)
        {
            let mut sessions = stores.agent_sessions.write().unwrap();
            let session = sessions.get_mut(&session_id).unwrap();
            let _ = session.transition_to(AgentStatus::Running);
        }

        // Pause
        let paused = dispatch_ok(&stores, &tx, &wm, &ic, "agent.pause", json!({"session_id": session_id}));
        assert_eq!(paused["status"], "paused");

        // Resume
        let resumed = dispatch_ok(
            &stores,
            &tx,
            &wm,
            &ic,
            "agent.resume",
            json!({"session_id": session_id}),
        );
        assert_eq!(resumed["status"], "running");

        // Stop
        let stopped = dispatch_ok(&stores, &tx, &wm, &ic, "agent.stop", json!({"session_id": session_id}));
        assert_eq!(stopped["status"], "cancelled");
    }

    // ========================================================================
    // Test 19: Strategy knobs configuration
    // ========================================================================

    #[test]
    fn test_strategy_knobs_defaults() {
        use crate::config::{ConflictPolicy, StalePolicy, StrategyConfig, TickCadence, ValidatorStrictness};

        let config = StrategyConfig::default();

        assert!(matches!(config.stale_policy, StalePolicy::ReplanAtSafePoint));
        assert!(matches!(config.conflict_policy, ConflictPolicy::LockAdvisory));
        assert!(matches!(config.tick_cadence, TickCadence::Continuous));
        assert_eq!(config.bundle_size.max_files_touched, 8);
        assert_eq!(config.bundle_size.max_loc_changed, 300);
        assert!(matches!(
            config.validator_strictness,
            ValidatorStrictness::HardFailOnAnyAmbiguity
        ));
        assert!(config.promotion.auto_promote);
        assert_eq!(config.promotion.min_reinforcements, 3);
        assert_eq!(config.max_lock_ttl_minutes, 60);
    }

    // ========================================================================
    // Test 20: Agents disabled by default
    // ========================================================================

    #[test]
    fn test_agents_disabled_by_default() {
        let config = Config::default();

        assert!(
            !config.agents.auto_start_coordinator,
            "coordinator should not auto-start by default"
        );
        assert!(!config.integrator.enabled, "integrator should be disabled by default");
    }

    // ========================================================================
    // Test 21: Full dispatch routing covers all MVP4 methods
    // ========================================================================

    #[test]
    fn test_dispatch_routes_mvp4_methods() {
        let stores = test_stores();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();
        let ic = test_integrator_config();

        // All these methods should NOT return "method not found"
        let methods = vec![
            "coordinator.set_goal",
            "coordinator.clear_goal",
            "lock.create",
            "lock.list",
            "lock.release",
            "lock.expire",
            "agent.start",
            "agent.stop",
            "agent.pause",
            "agent.resume",
            "agent.status",
            "agent.list",
        ];

        for method in methods {
            let req = DaemonRequest::new(1, method, json!({}));
            let resp = dispatch(&stores, &tx, &wm, &ic, req);
            // May fail with invalid_params, but should NOT fail with method_not_found
            if let Some(err) = &resp.error {
                assert_ne!(err.code, -32601, "{method} returned method_not_found");
            }
        }
    }

    // ========================================================================
    // Test 22: Full pipeline via dispatch — plan through bundle acceptance
    //          with parallel hierarchy verification
    // ========================================================================

    #[test]
    fn test_full_pipeline_plan_to_bundle_acceptance() {
        let stores = test_stores();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();
        let ic = test_integrator_config();

        // 1. Set a goal
        dispatch_ok(
            &stores,
            &tx,
            &wm,
            &ic,
            "coordinator.set_goal",
            json!({"goal": "Implement user auth"}),
        );

        // 2. Create full hierarchy
        let plan = dispatch_ok(
            &stores,
            &tx,
            &wm,
            &ic,
            "plan.create",
            json!({"title": "User Auth", "description": "Auth system", "acceptance_criteria": "Tests pass"}),
        );
        let plan_id = plan["id"].as_str().unwrap().to_string();
        dispatch_ok(
            &stores,
            &tx,
            &wm,
            &ic,
            "plan.transition",
            json!({"id": plan_id, "target": "active"}),
        );

        let spec = dispatch_ok(
            &stores,
            &tx,
            &wm,
            &ic,
            "spec.create",
            json!({"plan_id": plan_id, "title": "JWT", "description": "JWT auth", "acceptance_criteria": "OK"}),
        );
        let spec_id = spec["id"].as_str().unwrap().to_string();
        dispatch_ok(
            &stores,
            &tx,
            &wm,
            &ic,
            "spec.transition",
            json!({"id": spec_id, "target": "active"}),
        );

        let phase = dispatch_ok(
            &stores,
            &tx,
            &wm,
            &ic,
            "phase.create",
            json!({"spec_id": spec_id, "title": "Token", "description": "Token gen", "acceptance_criteria": "OK"}),
        );
        let phase_id = phase["id"].as_str().unwrap().to_string();
        dispatch_ok(
            &stores,
            &tx,
            &wm,
            &ic,
            "phase.transition",
            json!({"id": phase_id, "target": "active"}),
        );

        let wi = dispatch_ok(
            &stores,
            &tx,
            &wm,
            &ic,
            "work_item.create",
            json!({"phase_id": phase_id, "title": "sign()", "description": "Sign JWT"}),
        );
        let wi_id = wi["id"].as_str().unwrap().to_string();

        // 3. Implementer works on it
        dispatch_ok(
            &stores,
            &tx,
            &wm,
            &ic,
            "work_item.transition",
            json!({"id": wi_id, "target": "in_progress", "role": "implementer"}),
        );

        let bundle = dispatch_ok(
            &stores,
            &tx,
            &wm,
            &ic,
            "bundle.create",
            json!({"work_item_id": wi_id, "branch_name": "feat/sign", "claims": "Added sign()"}),
        );
        let bundle_id = bundle["id"].as_str().unwrap().to_string();

        // 4. Reviewer reviews and approves
        dispatch_ok(
            &stores,
            &tx,
            &wm,
            &ic,
            "bundle.transition",
            json!({"id": bundle_id, "target": "in_review", "role": "reviewer"}),
        );
        dispatch_ok(
            &stores,
            &tx,
            &wm,
            &ic,
            "bundle.transition",
            json!({"id": bundle_id, "target": "approved", "role": "reviewer"}),
        );

        // 5. Coordinator accepts
        dispatch_ok(
            &stores,
            &tx,
            &wm,
            &ic,
            "bundle.transition",
            json!({"id": bundle_id, "target": "accepted", "role": "coordinator"}),
        );

        // 6. Integrator creates tick and publishes
        let tick = dispatch_ok(&stores, &tx, &wm, &ic, "tick.create", json!({"number": 1}));
        let tick_id = tick["id"].as_str().unwrap().to_string();
        dispatch_ok(
            &stores,
            &tx,
            &wm,
            &ic,
            "tick.transition",
            json!({"id": tick_id, "target": "sealing", "role": "integrator"}),
        );
        dispatch_ok(
            &stores,
            &tx,
            &wm,
            &ic,
            "tick.transition",
            json!({"id": tick_id, "target": "validating", "role": "integrator"}),
        );
        dispatch_ok(
            &stores,
            &tx,
            &wm,
            &ic,
            "tick.transition",
            json!({"id": tick_id, "target": "published", "role": "integrator", "sha": "abc123"}),
        );

        // 7. Complete the work item
        dispatch_ok(
            &stores,
            &tx,
            &wm,
            &ic,
            "work_item.transition",
            json!({"id": wi_id, "target": "done", "role": "coordinator"}),
        );

        // Verify final state across all stores
        let plans = stores.plans.read().unwrap();
        assert_eq!(plans[&plan_id].status, crate::domain::plan::HierarchyStatus::Active);

        let bundles = stores.bundles.read().unwrap();
        assert_eq!(
            bundles[&bundle_id].status,
            crate::domain::bundle::BundleStatus::Accepted
        );

        let ticks = stores.ticks.read().unwrap();
        assert_eq!(ticks[&tick_id].status, TickStatus::Published);

        let wis = stores.work_items.read().unwrap();
        assert_eq!(wis[&wi_id].status, crate::domain::work_item::WorkItemStatus::Done);

        // Goal is still active
        let goals = stores.coordinator_goals.read().unwrap();
        assert!(goals.values().any(|g| g.active && g.goal == "Implement user auth"));
    }

    // ========================================================================
    // Test 23: Learning confidence computation across operations
    // ========================================================================

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
}
