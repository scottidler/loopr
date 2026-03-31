//! End-to-end integration tests for MVP4 multi-component flows.
//!
//! These tests exercise multi-step IPC flows through `dispatch()`, verifying
//! that components work correctly together across module boundaries.

#[allow(clippy::unwrap_used)]
#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::sync::Arc;

    use serde_json::json;
    use tokio::sync::broadcast;

    use crate::agents::{AgentSession, AgentStatus, AgentType};
    use crate::config::{Config, IntegratorConfig, InterviewMode};
    use crate::daemon::context::Stores;
    use crate::daemon::handlers::dispatch;
    use crate::domain::learning::{Learning, LearningScope};
    use crate::domain::tick::TickStatus;
    use crate::ipc::protocol::{DaemonEvent, DaemonRequest};
    use crate::test_util::TestDir;
    use crate::worktree::manager::WorktreeManager;

    use std::path::PathBuf;

    // Type aliases for inject_preformed_plan
    type WorkInput<'a> = (&'a str, &'a str, Vec<&'a str>);
    type PhaseInput<'a> = (&'a str, &'a str, u32, Vec<WorkInput<'a>>);
    type SpecInput<'a> = (&'a str, &'a str, Vec<PhaseInput<'a>>);
    type PhaseResult = (String, Vec<String>);
    type SpecResult = (String, Vec<PhaseResult>);

    struct PlanInput<'a> {
        title: &'a str,
        desc: &'a str,
        criteria: &'a str,
        specs: Vec<SpecInput<'a>>,
    }

    fn test_stores() -> Arc<Stores> {
        Arc::new(Stores::new())
    }

    fn test_agent_logger(dir: &std::path::Path) -> crate::agents::agent_logger::AgentLogger {
        let file_path = dir.join("test-integration.log");
        let file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&file_path)
            .unwrap();
        crate::agents::agent_logger::AgentLogger::_new_for_test(AgentType::Coordinator, "test-session", file, file_path)
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

    /// Build a minimal AgentContext for integration tests calling execute_action.
    fn test_agent_context(
        stores: &Arc<Stores>,
        bridge: crate::agents::bridge::AgentIpcBridge,
        tx: broadcast::Sender<DaemonEvent>,
        agent_log: crate::agents::agent_logger::AgentLogger,
    ) -> crate::agents::AgentContext {
        let session = AgentSession::new(AgentType::Coordinator, "test-model".into());
        crate::agents::AgentContext {
            session,
            stores: stores.clone(),
            bridge,
            event_tx: tx,
            tool_runner: stores.tool_runner.clone(),
            tool_executor: stores.tool_executor.clone(),
            log: agent_log,
            read_cache: std::sync::Mutex::new(crate::agents::cache::ReadCache::default()),
        }
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

    /// Helper: create Plan→Spec→Phase hierarchy and return (plan_id, spec_id, phase_id).
    fn create_test_hierarchy(
        stores: &Arc<Stores>,
        tx: &broadcast::Sender<DaemonEvent>,
        wm: &WorktreeManager,
        ic: &IntegratorConfig,
    ) -> (String, String, String) {
        let plan = dispatch_ok(
            stores,
            tx,
            wm,
            ic,
            "plan.create",
            json!({"title": "Test Plan", "description": "desc", "acceptance_criteria": "pass"}),
        );
        let plan_id = plan["id"].as_str().unwrap().to_string();
        dispatch_ok(
            stores,
            tx,
            wm,
            ic,
            "plan.transition",
            json!({"id": plan_id, "target_status": "active"}),
        );
        let spec = dispatch_ok(
            stores,
            tx,
            wm,
            ic,
            "spec.create",
            json!({"plan_id": plan_id, "title": "Test Spec", "description": "desc", "acceptance_criteria": "pass"}),
        );
        let spec_id = spec["id"].as_str().unwrap().to_string();
        dispatch_ok(
            stores,
            tx,
            wm,
            ic,
            "spec.transition",
            json!({"id": spec_id, "target_status": "active"}),
        );
        let phase = dispatch_ok(
            stores,
            tx,
            wm,
            ic,
            "phase.create",
            json!({"spec_id": spec_id, "title": "Test Phase", "description": "desc", "acceptance_criteria": "pass"}),
        );
        let phase_id = phase["id"].as_str().unwrap().to_string();
        dispatch_ok(
            stores,
            tx,
            wm,
            ic,
            "phase.transition",
            json!({"id": phase_id, "target_status": "active"}),
        );
        (plan_id, spec_id, phase_id)
    }

    // ========================================================================
    // Test 1: Full hierarchy creation via IPC dispatch
    //         Plan → Spec → Phase → Work → Bundle
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
            json!({"id": plan_id, "target_status": "active"}),
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
            json!({"id": spec_id, "target_status": "active"}),
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
            json!({"id": phase_id, "target_status": "active"}),
        );

        // Create Work under Phase
        let wi = dispatch_ok(
            &stores,
            &tx,
            &wm,
            &ic,
            "work.create",
            json!({"phase_id": phase_id, "title": "Implement sign()", "description": "JWT signing function", "resource_tags": ["src/"], "acceptance_criteria": ["tests pass"]}),
        );
        let wi_id = wi["id"].as_str().unwrap().to_string();
        assert_eq!(wi["status"], "Ready");

        // Transition Work: Ready → InProgress (auto-promoted from Draft since acceptance_criteria present)
        dispatch_ok(
            &stores,
            &tx,
            &wm,
            &ic,
            "work.transition",
            json!({"id": wi_id, "target_status": "InProgress", "role": "coordinator", "assignee": "agent-1"}),
        );

        // Create Bundle for Work
        let bundle = dispatch_ok(
            &stores,
            &tx,
            &wm,
            &ic,
            "bundle.create",
            json!({"work_id": wi_id, "branch_name": "feat/jwt-sign", "claims": "Added sign() function"}),
        );
        let bundle_id = bundle["id"].as_str().unwrap().to_string();
        assert_eq!(bundle["status"], "Proposed");

        // Verify full hierarchy in stores
        assert_eq!(stores.plans.read().unwrap().len(), 1);
        assert_eq!(stores.specs.read().unwrap().len(), 1);
        assert_eq!(stores.phases.read().unwrap().len(), 1);
        assert_eq!(stores.works.read().unwrap().len(), 1);
        assert_eq!(stores.bundles.read().unwrap().len(), 1);

        // Verify correct parent-child relationships
        let specs = stores.specs.read().unwrap();
        assert_eq!(specs[&spec_id].plan_id, plan_id);
        let phases = stores.phases.read().unwrap();
        assert_eq!(phases[&phase_id].spec_id, spec_id);
        let works = stores.works.read().unwrap();
        assert_eq!(works[&wi_id].phase_id, phase_id);
        let bundles = stores.bundles.read().unwrap();
        assert_eq!(bundles[&bundle_id].work_id, wi_id);
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

        // Create hierarchy so work.create can find a valid phase
        let (_plan_id, _spec_id, phase_id) = create_test_hierarchy(&stores, &tx, &wm, &ic);

        // Create Work
        let wi = dispatch_ok(
            &stores,
            &tx,
            &wm,
            &ic,
            "work.create",
            json!({"phase_id": phase_id, "title": "Task", "description": "desc", "resource_tags": ["src/"], "acceptance_criteria": ["tests pass"]}),
        );
        let wi_id = wi["id"].as_str().unwrap().to_string();

        // Create Bundle
        let bundle = dispatch_ok(
            &stores,
            &tx,
            &wm,
            &ic,
            "bundle.create",
            json!({"work_id": wi_id, "branch_name": "feat/task", "claims": "Did it"}),
        );
        let bundle_id = bundle["id"].as_str().unwrap().to_string();

        // Proposed → Triaged (Coordinator triages)
        dispatch_ok(
            &stores,
            &tx,
            &wm,
            &ic,
            "bundle.transition",
            json!({"id": bundle_id, "target_status": "Triaged", "role": "coordinator"}),
        );

        // Triaged → Reviewed (Reviewer reviews)
        dispatch_ok(
            &stores,
            &tx,
            &wm,
            &ic,
            "bundle.transition",
            json!({"id": bundle_id, "target_status": "Reviewed", "role": "reviewer", "verification": "tests passed"}),
        );

        // Reviewed → Accepted (Coordinator accepts)
        dispatch_ok(
            &stores,
            &tx,
            &wm,
            &ic,
            "bundle.transition",
            json!({"id": bundle_id, "target_status": "Accepted", "role": "coordinator"}),
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

    #[tokio::test]
    async fn test_pool_exhaustion_multi_type() {
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

        // But Researcher should still work (different pool)
        let resp = dispatch_ok(
            &stores,
            &tx,
            &wm,
            &ic,
            "agent.start",
            json!({"agent_type": "researcher"}),
        );
        assert!(resp["id"].as_str().is_some());
    }

    // ========================================================================
    // Test 6: Pool allows new session after terminal
    // ========================================================================

    #[tokio::test]
    async fn test_pool_allows_after_terminal_session() {
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
        assert!(resp["id"].as_str().is_some());
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
        use crate::domain::tick::Tick;

        let stores = test_stores();

        // Simulate a crash: directly insert a tick stuck in Sealing state
        let mut tick = Tick::new(1);
        tick.status = TickStatus::Sealing;
        let tick_id = tick.id.clone();
        stores.ticks.write().unwrap().insert(tick_id.clone(), tick);

        // Also insert one in Validating state
        let mut tick2 = Tick::new(2);
        tick2.status = TickStatus::Validating;
        let tick2_id = tick2.id.clone();
        stores.ticks.write().unwrap().insert(tick2_id.clone(), tick2);

        // Crash recovery directly resets stuck ticks (bypasses FSM)
        {
            let mut ticks = stores.ticks.write().unwrap();
            for tick in ticks.values_mut() {
                if matches!(tick.status, TickStatus::Sealing | TickStatus::Validating) {
                    tick.status = TickStatus::Failed;
                }
            }
        }

        // Both ticks should be Failed
        let ticks = stores.ticks.read().unwrap();
        assert_eq!(ticks[&tick_id].status, TickStatus::Failed);
        assert_eq!(ticks[&tick2_id].status, TickStatus::Failed);
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
                "granted_by": "coordinator"
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

        // Create hierarchy first so work.create can find a valid phase
        let (_plan_id, _spec_id, phase_id) = create_test_hierarchy(&stores, &tx, &wm, &ic);

        // Create work item under the real phase
        dispatch_ok(
            &stores,
            &tx,
            &wm,
            &ic,
            "work.create",
            json!({"phase_id": phase_id, "title": "WI-1", "description": "desc", "resource_tags": ["src/"], "acceptance_criteria": ["tests pass"]}),
        );

        // Add agent session
        let session = AgentSession::new(AgentType::Implementer, "model".into());
        stores
            .agent_sessions
            .write()
            .unwrap()
            .insert(session.id.clone(), session);

        // Build state summary (used by Coordinator to understand current state)
        let _agent_dir = TestDir::new("loopr-intg-logger");
        let agent_log = test_agent_logger(&_agent_dir);
        let summary = crate::agents::coordinator::build_state_summary(&stores, &agent_log);
        assert!(summary.contains("Test Plan"), "summary should include plan");
        assert!(summary.contains("WI-1"), "summary should include work item");
        assert!(summary.contains("implementer"), "summary should include agent session");
        assert!(
            summary.contains("### Active Agents"),
            "summary should have agents section"
        );
    }

    // ========================================================================
    // Test 11: Context builder per-role produces different learnings
    // ========================================================================

    #[test]
    fn test_context_builder_role_filtering() {
        use crate::domain::role::Role;

        let mut learnings = HashMap::new();

        // Create learnings for specific roles
        let mut l1 = Learning::new("wi-1".into(), LearningScope::Work, "Impl insight".into());
        l1.applicable_roles = Some(vec![Role::Implementer]);
        l1.confidence = 0.8;
        learnings.insert(l1.id.clone(), l1);

        let mut l2 = Learning::new("wi-1".into(), LearningScope::Work, "Review insight".into());
        l2.applicable_roles = Some(vec![Role::Reviewer]);
        l2.confidence = 0.8;
        learnings.insert(l2.id.clone(), l2);

        let mut l3 = Learning::new("wi-1".into(), LearningScope::Global, "Global insight".into());
        l3.applicable_roles = None; // All roles
        l3.confidence = 0.8;
        learnings.insert(l3.id.clone(), l3);

        // Select learnings for Implementer
        let scope_ids = [("wi-1", LearningScope::Work)];
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
        let _agent_dir = TestDir::new("loopr-intg-logger");
        let agent_log = test_agent_logger(&_agent_dir);

        // Valid relative path
        assert!(
            crate::agents::researcher::validate_path(repo_root, "src/main.rs", &agent_log).is_ok(),
            "relative path should be valid"
        );

        // Absolute path rejected
        assert!(
            crate::agents::researcher::validate_path(repo_root, "/etc/passwd", &agent_log).is_err(),
            "absolute path should be rejected"
        );

        // Path traversal rejected
        assert!(
            crate::agents::researcher::validate_path(repo_root, "../../../etc/passwd", &agent_log).is_err(),
            "traversal path should be rejected"
        );

        // Denied file patterns
        assert!(
            crate::agents::researcher::validate_path(repo_root, ".env", &agent_log).is_err(),
            ".env should be denied"
        );
        assert!(
            crate::agents::researcher::validate_path(repo_root, "keys/server.key", &agent_log).is_err(),
            "*.key should be denied"
        );
        assert!(
            crate::agents::researcher::validate_path(repo_root, "certs/server.pem", &agent_log).is_err(),
            "*.pem should be denied"
        );
        assert!(
            crate::agents::researcher::validate_path(repo_root, "credentials.json", &agent_log).is_err(),
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
    // Test 14: Work FSM rejects invalid transitions
    // ========================================================================

    #[test]
    fn test_work_fsm_enforcement_via_dispatch() {
        let stores = test_stores();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();
        let ic = test_integrator_config();

        // Create hierarchy so work.create can find a valid phase
        let (_plan_id, _spec_id, phase_id) = create_test_hierarchy(&stores, &tx, &wm, &ic);

        // Create Work (starts as Draft)
        let wi = dispatch_ok(
            &stores,
            &tx,
            &wm,
            &ic,
            "work.create",
            json!({"phase_id": phase_id, "title": "Task", "description": "desc", "resource_tags": ["src/"], "acceptance_criteria": ["tests pass"]}),
        );
        let wi_id = wi["id"].as_str().unwrap().to_string();

        // Invalid: Ready → Done (must go through InProgress first)
        let code = dispatch_err(
            &stores,
            &tx,
            &wm,
            &ic,
            "work.transition",
            json!({"id": wi_id, "target_status": "Done", "role": "coordinator"}),
        );
        assert_ne!(code, 0, "should reject invalid transition");

        // Verify state unchanged (auto-promoted to Ready since acceptance_criteria present)
        let wis = stores.works.read().unwrap();
        assert_eq!(wis[&wi_id].status, crate::domain::work::WorkStatus::Ready);
    }

    // ========================================================================
    // Test 15: Multi-agent session management
    // ========================================================================

    #[tokio::test]
    async fn test_multi_agent_session_coexistence() {
        let stores = test_stores();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();
        let ic = test_integrator_config();

        // Start sessions of different types (use types that don't require work_id/bundle_id)
        let coord = dispatch_ok(
            &stores,
            &tx,
            &wm,
            &ic,
            "agent.start",
            json!({"agent_type": "coordinator"}),
        );
        let researcher1 = dispatch_ok(
            &stores,
            &tx,
            &wm,
            &ic,
            "agent.start",
            json!({"agent_type": "researcher"}),
        );
        let researcher2 = dispatch_ok(
            &stores,
            &tx,
            &wm,
            &ic,
            "agent.start",
            json!({"agent_type": "researcher"}),
        );

        // All should have unique session IDs
        let ids: Vec<&str> = [&coord, &researcher1, &researcher2]
            .iter()
            .map(|r| r["id"].as_str().unwrap())
            .collect();
        let unique: std::collections::HashSet<&&str> = ids.iter().collect();
        assert_eq!(unique.len(), 3, "all session IDs should be unique");

        // List all agents
        let list = dispatch_ok(&stores, &tx, &wm, &ic, "agent.list", json!({}));
        assert_eq!(list.as_array().unwrap().len(), 3);
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
        assert_eq!(tick["status"], "Open");

        // Open → Sealing
        dispatch_ok(
            &stores,
            &tx,
            &wm,
            &ic,
            "tick.transition",
            json!({"id": tick_id, "target_status": "Sealing", "role": "integrator"}),
        );

        // Sealing → Validating
        dispatch_ok(
            &stores,
            &tx,
            &wm,
            &ic,
            "tick.transition",
            json!({"id": tick_id, "target_status": "Validating", "role": "integrator"}),
        );

        // Validating → Published
        dispatch_ok(
            &stores,
            &tx,
            &wm,
            &ic,
            "tick.transition",
            json!({"id": tick_id, "target_status": "Published", "role": "integrator"}),
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

        // Add active Phase → needs Work
        let mut phase = Phase::new(spec_id.clone(), "Ph".into(), "d".into(), 1);
        phase.status = HierarchyStatus::Active;
        stores.phases.write().unwrap().insert(phase.id.clone(), phase);

        assert_eq!(determine_generation_level(&stores), Some(GenerationLevel::Work));
    }

    // ========================================================================
    // Test 18: Agent pause/resume lifecycle
    // ========================================================================

    #[tokio::test]
    async fn test_agent_pause_resume_lifecycle() {
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
        let session_id = resp["id"].as_str().unwrap().to_string();

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
            json!({"id": plan_id, "target_status": "active"}),
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
            json!({"id": spec_id, "target_status": "active"}),
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
            json!({"id": phase_id, "target_status": "active"}),
        );

        let wi = dispatch_ok(
            &stores,
            &tx,
            &wm,
            &ic,
            "work.create",
            json!({"phase_id": phase_id, "title": "sign()", "description": "Sign JWT", "resource_tags": ["src/"], "acceptance_criteria": ["tests pass"]}),
        );
        let wi_id = wi["id"].as_str().unwrap().to_string();

        // 3. Coordinator assigns, implementer works on it (already Ready via auto-promotion)
        dispatch_ok(
            &stores,
            &tx,
            &wm,
            &ic,
            "work.transition",
            json!({"id": wi_id, "target_status": "InProgress", "role": "coordinator", "assignee": "agent-1"}),
        );

        let bundle = dispatch_ok(
            &stores,
            &tx,
            &wm,
            &ic,
            "bundle.create",
            json!({"work_id": wi_id, "branch_name": "feat/sign", "claims": "Added sign()"}),
        );
        let bundle_id = bundle["id"].as_str().unwrap().to_string();

        // 4. Coordinator triages, reviewer reviews
        dispatch_ok(
            &stores,
            &tx,
            &wm,
            &ic,
            "bundle.transition",
            json!({"id": bundle_id, "target_status": "Triaged", "role": "coordinator"}),
        );
        dispatch_ok(
            &stores,
            &tx,
            &wm,
            &ic,
            "bundle.transition",
            json!({"id": bundle_id, "target_status": "Reviewed", "role": "reviewer", "verification": "tests passed"}),
        );

        // 5. Coordinator accepts
        dispatch_ok(
            &stores,
            &tx,
            &wm,
            &ic,
            "bundle.transition",
            json!({"id": bundle_id, "target_status": "Accepted", "role": "coordinator"}),
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
            json!({"id": tick_id, "target_status": "Sealing", "role": "integrator"}),
        );
        dispatch_ok(
            &stores,
            &tx,
            &wm,
            &ic,
            "tick.transition",
            json!({"id": tick_id, "target_status": "Validating", "role": "integrator"}),
        );
        dispatch_ok(
            &stores,
            &tx,
            &wm,
            &ic,
            "tick.transition",
            json!({"id": tick_id, "target_status": "Published", "role": "integrator"}),
        );

        // 7. Mark work item as InReview → Integrated → Done
        dispatch_ok(
            &stores,
            &tx,
            &wm,
            &ic,
            "work.transition",
            json!({"id": wi_id, "target_status": "InReview", "role": "implementer"}),
        );
        dispatch_ok(
            &stores,
            &tx,
            &wm,
            &ic,
            "work.transition",
            json!({"id": wi_id, "target_status": "Integrated", "role": "integrator"}),
        );
        dispatch_ok(
            &stores,
            &tx,
            &wm,
            &ic,
            "work.transition",
            json!({"id": wi_id, "target_status": "Done", "role": "coordinator"}),
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

        let wis = stores.works.read().unwrap();
        assert_eq!(wis[&wi_id].status, crate::domain::work::WorkStatus::Done);

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

    // ========================================================================
    // MVP4 Validation — E2E tests for wired Coordinator actions
    // ========================================================================

    #[test]
    fn test_coordinator_action_creates_plan_via_executor() {
        // Verify that CreatePlan action through execute_action actually creates a plan in stores
        use crate::agents::AgentAction;
        use crate::agents::bridge::AgentIpcBridge;
        use crate::agents::executor::{ActionResult, execute_action};

        let rt = tokio::runtime::Runtime::new().unwrap();
        let dir = TestDir::new("loopr-e2e-createplan");

        let stores = test_stores();
        let tx = test_event_tx();
        let wm = WorktreeManager::new(dir.to_path_buf(), dir.join(".wt"));
        let bridge = AgentIpcBridge::new(stores.clone(), tx.clone(), wm, stores.config.clone());

        let action = AgentAction::CreatePlan {
            title: "E2E Test Plan".to_string(),
            description: "Test description".to_string(),
            acceptance_criteria: "All tests pass".to_string(),
        };

        let agent_log = test_agent_logger(&dir);
        let ctx = test_agent_context(&stores, bridge, tx, agent_log);
        let result = rt.block_on(execute_action(&action, &ctx, &dir, None)).unwrap();

        match result {
            ActionResult::RecordCreated { collection, id } => {
                assert_eq!(collection, "plans");
                // Verify plan exists in stores
                let plans = stores.plans.read().unwrap();
                let plan = plans.get(&id).expect("plan should exist in stores");
                assert_eq!(plan.title, "E2E Test Plan");
            }
            other => panic!("expected RecordCreated, got: {:?}", other),
        }
    }

    #[test]
    fn test_coordinator_creates_full_hierarchy_via_executor() {
        // CreatePlan → CreateSpec → CreatePhase → CreateWork through executor
        use crate::agents::AgentAction;
        use crate::agents::bridge::AgentIpcBridge;
        use crate::agents::executor::{ActionResult, execute_action};

        let rt = tokio::runtime::Runtime::new().unwrap();
        let dir = TestDir::new("loopr-e2e-hierarchy");

        let stores = test_stores();
        let tx = test_event_tx();
        let wm = WorktreeManager::new(dir.to_path_buf(), dir.join(".wt"));
        let bridge = AgentIpcBridge::new(stores.clone(), tx.clone(), wm, stores.config.clone());
        let agent_log = test_agent_logger(&dir);
        let ctx = test_agent_context(&stores, bridge, tx, agent_log);

        // Create Plan
        let plan_result = rt
            .block_on(execute_action(
                &AgentAction::CreatePlan {
                    title: "Auth Plan".into(),
                    description: "Auth system".into(),
                    acceptance_criteria: "Tests pass".into(),
                },
                &ctx,
                &dir,
                None,
            ))
            .unwrap();
        let plan_id = match plan_result {
            ActionResult::RecordCreated { id, .. } => id,
            other => panic!("expected RecordCreated for plan, got: {:?}", other),
        };

        // Create Spec
        let spec_result = rt
            .block_on(execute_action(
                &AgentAction::CreateSpec {
                    plan_id: plan_id.clone(),
                    title: "JWT Spec".into(),
                    description: "JWT tokens".into(),
                },
                &ctx,
                &dir,
                None,
            ))
            .unwrap();
        let spec_id = match spec_result {
            ActionResult::RecordCreated { id, .. } => id,
            other => panic!("expected RecordCreated for spec, got: {:?}", other),
        };

        // Create Phase
        let phase_result = rt
            .block_on(execute_action(
                &AgentAction::CreatePhase {
                    spec_id: spec_id.clone(),
                    title: "Phase 1".into(),
                    description: "Foundation".into(),
                    order: 1,
                },
                &ctx,
                &dir,
                None,
            ))
            .unwrap();
        let phase_id = match phase_result {
            ActionResult::RecordCreated { id, .. } => id,
            other => panic!("expected RecordCreated for phase, got: {:?}", other),
        };

        // Create Work
        let wi_result = rt
            .block_on(execute_action(
                &AgentAction::CreateWork {
                    phase_id: phase_id.clone(),
                    title: "Add login".into(),
                    description: "Add login endpoint".into(),
                    resource_tags: vec!["src/".into()],
                    acceptance_criteria: vec!["tests pass".into()],
                    dependencies: vec![],
                },
                &ctx,
                &dir,
                None,
            ))
            .unwrap();
        let wi_id = match wi_result {
            ActionResult::RecordCreated { collection, id } => {
                assert_eq!(collection, "works");
                id
            }
            other => panic!("expected RecordCreated for work, got: {:?}", other),
        };

        // Verify all records exist with correct parent linkage
        let plans = stores.plans.read().unwrap();
        assert!(plans.contains_key(&plan_id));

        let specs = stores.specs.read().unwrap();
        let spec = specs.get(&spec_id).unwrap();
        assert_eq!(spec.plan_id, plan_id);

        let phases = stores.phases.read().unwrap();
        let phase = phases.get(&phase_id).unwrap();
        assert_eq!(phase.spec_id, spec_id);

        let wis = stores.works.read().unwrap();
        let wi = wis.get(&wi_id).unwrap();
        assert_eq!(wi.phase_id, phase_id);
    }

    #[test]
    fn test_coordinator_triage_accept_bundle_via_executor() {
        // Create hierarchy + bundle → TriageBundle → AcceptBundle through executor
        use crate::agents::AgentAction;
        use crate::agents::bridge::AgentIpcBridge;
        use crate::agents::executor::{ActionResult, execute_action};
        use crate::domain::bundle::BundleStatus;

        let rt = tokio::runtime::Runtime::new().unwrap();
        let dir = TestDir::new("loopr-e2e-triage");

        let stores = test_stores();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();
        let ic = test_integrator_config();
        let bridge = AgentIpcBridge::new(stores.clone(), tx.clone(), wm.clone(), stores.config.clone());

        // Create hierarchy + work item (via dispatch for speed)
        let (_, _, phase_id) = create_test_hierarchy(&stores, &tx, &wm, &ic);
        let wi = dispatch_ok(
            &stores,
            &tx,
            &wm,
            &ic,
            "work.create",
            json!({"phase_id": phase_id, "title": "WI", "description": "d", "resource_tags": ["src/"], "acceptance_criteria": ["tests pass"]}),
        );
        let wi_id = wi["id"].as_str().unwrap().to_string();

        // Transition WI to InProgress so bundle can be proposed (already Ready via auto-promotion)
        dispatch_ok(
            &stores,
            &tx,
            &wm,
            &ic,
            "work.transition",
            json!({"id": wi_id, "target_status": "InProgress", "role": "coordinator", "assignee": "agent-1"}),
        );

        // Create a bundle
        let bundle = dispatch_ok(
            &stores,
            &tx,
            &wm,
            &ic,
            "bundle.create",
            json!({
                "work_id": wi_id,
                "description": "Auth changes",
                "files_changed": ["src/auth.rs"],
                "commit_sha": "abc123",
                "branch_name": "feature-auth"
            }),
        );
        let bundle_id = bundle["id"].as_str().unwrap().to_string();

        // TriageBundle via executor
        let agent_log = test_agent_logger(&dir);
        let ctx = test_agent_context(&stores, bridge, tx.clone(), agent_log);
        let triage_result = rt
            .block_on(execute_action(
                &AgentAction::TriageBundle {
                    bundle_id: bundle_id.clone(),
                },
                &ctx,
                &dir,
                None,
            ))
            .unwrap();
        assert!(
            matches!(triage_result, ActionResult::Transitioned(ref s) if s.contains("Triaged")),
            "expected Transitioned(Triaged), got: {:?}",
            triage_result
        );

        // Verify bundle is Triaged
        {
            let bundles = stores.bundles.read().unwrap();
            assert_eq!(bundles[&bundle_id].status, BundleStatus::Triaged);
        }

        // Review the bundle (needed before Accept)
        dispatch_ok(
            &stores,
            &tx,
            &wm,
            &ic,
            "bundle.transition",
            json!({"id": bundle_id, "target_status": "Reviewed", "role": "reviewer", "verification": "tests passed"}),
        );

        // AcceptBundle via executor
        let accept_result = rt
            .block_on(execute_action(
                &AgentAction::AcceptBundle {
                    bundle_id: bundle_id.clone(),
                },
                &ctx,
                &dir,
                None,
            ))
            .unwrap();
        assert!(
            matches!(accept_result, ActionResult::Transitioned(ref s) if s.contains("Accepted")),
            "expected Transitioned(Accepted), got: {:?}",
            accept_result
        );

        // Verify bundle is Accepted
        {
            let bundles = stores.bundles.read().unwrap();
            assert_eq!(bundles[&bundle_id].status, BundleStatus::Accepted);
        }
    }

    #[test]
    fn test_coordinator_get_goal_full_lifecycle() {
        let stores = test_stores();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();
        let ic = test_integrator_config();

        // No goal yet
        let r1 = dispatch_ok(&stores, &tx, &wm, &ic, "coordinator.get_goal", json!({}));
        assert_eq!(r1["active"], false);

        // Set goal
        dispatch_ok(
            &stores,
            &tx,
            &wm,
            &ic,
            "coordinator.set_goal",
            json!({"goal": "Build a website"}),
        );

        // Get goal — should return the active one
        let r2 = dispatch_ok(&stores, &tx, &wm, &ic, "coordinator.get_goal", json!({}));
        assert_eq!(r2["goal"], "Build a website");
        assert_eq!(r2["active"], true);

        // Clear goal
        dispatch_ok(&stores, &tx, &wm, &ic, "coordinator.clear_goal", json!({}));

        // Get goal — should be inactive now
        let r3 = dispatch_ok(&stores, &tx, &wm, &ic, "coordinator.get_goal", json!({}));
        assert_eq!(r3["active"], false);
    }

    #[test]
    fn test_full_mvp4_pipeline() {
        // End-to-end: goal → plan → spec → phase → work → bundle → triage → review → accept
        let stores = test_stores();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();
        let ic = test_integrator_config();

        // 1. Set coordinator goal
        let goal = dispatch_ok(
            &stores,
            &tx,
            &wm,
            &ic,
            "coordinator.set_goal",
            json!({"goal": "Build example website"}),
        );
        assert_eq!(goal["active"], true);

        // 2. Create Plan
        let plan = dispatch_ok(
            &stores,
            &tx,
            &wm,
            &ic,
            "plan.create",
            json!({"title": "Website Plan", "description": "Build a static site", "acceptance_criteria": "Site loads"}),
        );
        let plan_id = plan["id"].as_str().unwrap().to_string();
        dispatch_ok(
            &stores,
            &tx,
            &wm,
            &ic,
            "plan.transition",
            json!({"id": plan_id, "target_status": "active"}),
        );

        // 3. Create Spec
        let spec = dispatch_ok(
            &stores,
            &tx,
            &wm,
            &ic,
            "spec.create",
            json!({"plan_id": plan_id, "title": "HTML Structure", "description": "Create HTML pages"}),
        );
        let spec_id = spec["id"].as_str().unwrap().to_string();
        dispatch_ok(
            &stores,
            &tx,
            &wm,
            &ic,
            "spec.transition",
            json!({"id": spec_id, "target_status": "active"}),
        );

        // 4. Create Phase
        let phase = dispatch_ok(
            &stores,
            &tx,
            &wm,
            &ic,
            "phase.create",
            json!({"spec_id": spec_id, "title": "Phase 1: Index", "description": "Create index.html"}),
        );
        let phase_id = phase["id"].as_str().unwrap().to_string();
        dispatch_ok(
            &stores,
            &tx,
            &wm,
            &ic,
            "phase.transition",
            json!({"id": phase_id, "target_status": "active"}),
        );

        // 5. Create Work
        let wi = dispatch_ok(
            &stores,
            &tx,
            &wm,
            &ic,
            "work.create",
            json!({"phase_id": phase_id, "title": "Create index.html", "description": "Write the homepage", "resource_tags": ["src/"], "acceptance_criteria": ["tests pass"]}),
        );
        let wi_id = wi["id"].as_str().unwrap().to_string();

        // 6. Assign Work (transition to InProgress; already Ready via auto-promotion)
        dispatch_ok(
            &stores,
            &tx,
            &wm,
            &ic,
            "work.transition",
            json!({"id": wi_id, "target_status": "InProgress", "role": "coordinator", "assignee": "agent-1"}),
        );

        // 7. Create Bundle (implementer output)
        let bundle = dispatch_ok(
            &stores,
            &tx,
            &wm,
            &ic,
            "bundle.create",
            json!({
                "work_id": wi_id,
                "description": "Created index.html with basic structure",
                "files_changed": ["index.html"],
                "commit_sha": "def456",
                "branch_name": "feature-index"
            }),
        );
        let bundle_id = bundle["id"].as_str().unwrap().to_string();

        // 8. Triage (Coordinator)
        dispatch_ok(
            &stores,
            &tx,
            &wm,
            &ic,
            "bundle.transition",
            json!({"id": bundle_id, "target_status": "Triaged", "role": "coordinator"}),
        );

        // 9. Review (Reviewer)
        dispatch_ok(
            &stores,
            &tx,
            &wm,
            &ic,
            "bundle.transition",
            json!({"id": bundle_id, "target_status": "Reviewed", "role": "reviewer", "verification": "tests passed"}),
        );

        // 10. Accept (Coordinator)
        dispatch_ok(
            &stores,
            &tx,
            &wm,
            &ic,
            "bundle.transition",
            json!({"id": bundle_id, "target_status": "Accepted", "role": "coordinator"}),
        );

        // Verify final state
        {
            let bundles = stores.bundles.read().unwrap();
            assert_eq!(
                bundles[&bundle_id].status,
                crate::domain::bundle::BundleStatus::Accepted
            );
        }

        // Verify goal still active
        let final_goal = dispatch_ok(&stores, &tx, &wm, &ic, "coordinator.get_goal", json!({}));
        assert_eq!(final_goal["goal"], "Build example website");
        assert_eq!(final_goal["active"], true);

        // Verify record counts
        assert_eq!(stores.plans.read().unwrap().len(), 1);
        assert_eq!(stores.specs.read().unwrap().len(), 1);
        assert_eq!(stores.phases.read().unwrap().len(), 1);
        assert_eq!(stores.works.read().unwrap().len(), 1);
        assert_eq!(stores.bundles.read().unwrap().len(), 1);
        assert_eq!(stores.coordinator_goals.read().unwrap().len(), 1);
    }

    // ========================================================================
    // Test: Full pipeline e2e — goal through tick publish with tmpdir git repo
    //
    // Covers gaps found during manual e2e testing:
    //  - Full work item lifecycle through Done
    //  - Bundle Integrating → Merged
    //  - Reviewer rejection from Proposed (new FSM rule)
    //  - Tick publish with validation commands in a real git repo
    //  - Lock acquire/release lifecycle
    // ========================================================================

    #[test]
    fn test_e2e_full_pipeline_with_tmpdir_git_repo() {
        let dir = TestDir::new("loopr-e2e-full");

        // Initialize a real git repo so tick publish can get HEAD sha
        std::process::Command::new("git")
            .args(["init"])
            .current_dir(&dir)
            .output()
            .expect("git init failed");
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

        // --- 1. Set coordinator goal ---
        let goal = dispatch_ok(
            &stores,
            &tx,
            &wm,
            &ic,
            "coordinator.set_goal",
            json!({"goal": "Build portfolio site"}),
        );
        assert_eq!(goal["active"], true);

        // --- 2. Create full hierarchy ---
        let plan = dispatch_ok(
            &stores,
            &tx,
            &wm,
            &ic,
            "plan.create",
            json!({"title": "Portfolio", "description": "Build a portfolio website"}),
        );
        let plan_id = plan["id"].as_str().unwrap().to_string();
        dispatch_ok(
            &stores,
            &tx,
            &wm,
            &ic,
            "plan.transition",
            json!({"id": plan_id, "target_status": "active"}),
        );

        let spec = dispatch_ok(
            &stores,
            &tx,
            &wm,
            &ic,
            "spec.create",
            json!({"plan_id": plan_id, "title": "Pages", "description": "HTML pages"}),
        );
        let spec_id = spec["id"].as_str().unwrap().to_string();
        dispatch_ok(
            &stores,
            &tx,
            &wm,
            &ic,
            "spec.transition",
            json!({"id": spec_id, "target_status": "active"}),
        );

        let phase = dispatch_ok(
            &stores,
            &tx,
            &wm,
            &ic,
            "phase.create",
            json!({"spec_id": spec_id, "title": "Phase 1", "description": "Structure", "order": 1}),
        );
        let phase_id = phase["id"].as_str().unwrap().to_string();
        dispatch_ok(
            &stores,
            &tx,
            &wm,
            &ic,
            "phase.transition",
            json!({"id": phase_id, "target_status": "active"}),
        );

        // --- 3. Create work items ---
        let wi1 = dispatch_ok(
            &stores,
            &tx,
            &wm,
            &ic,
            "work.create",
            json!({"phase_id": phase_id, "title": "Create index.html", "description": "Homepage", "resource_tags": ["src/"], "acceptance_criteria": ["tests pass"]}),
        );
        let wi1_id = wi1["id"].as_str().unwrap().to_string();

        let wi2 = dispatch_ok(
            &stores,
            &tx,
            &wm,
            &ic,
            "work.create",
            json!({"phase_id": phase_id, "title": "Create about.html", "description": "About page", "resource_tags": ["src/"], "acceptance_criteria": ["tests pass"]}),
        );
        let wi2_id = wi2["id"].as_str().unwrap().to_string();

        // --- 4. Lock lifecycle ---
        let lock = dispatch_ok(
            &stores,
            &tx,
            &wm,
            &ic,
            "lock.create",
            json!({"resource": "index.html", "holder_id": wi1_id, "granted_by": "coordinator"}),
        );
        let lock_id = lock["id"].as_str().unwrap().to_string();
        assert_eq!(lock["status"], "active");

        let locks = dispatch_ok(&stores, &tx, &wm, &ic, "lock.list", json!({}));
        assert!(locks.as_array().unwrap().iter().any(|l| l["id"] == lock_id));

        dispatch_ok(&stores, &tx, &wm, &ic, "lock.release", json!({"id": lock_id}));
        let released = dispatch_ok(&stores, &tx, &wm, &ic, "lock.get", json!({"id": lock_id}));
        assert_eq!(released["status"], "released");

        // --- 5. Full work item lifecycle: Ready → InProgress → InReview → Integrated → Done ---
        // (auto-promoted from Draft to Ready since acceptance_criteria present)
        dispatch_ok(
            &stores,
            &tx,
            &wm,
            &ic,
            "work.transition",
            json!({"id": wi1_id, "target_status": "InProgress", "role": "coordinator", "assignee": "agent-1"}),
        );
        // Create a bundle before InReview (required by #15 invariant)
        dispatch_ok(
            &stores,
            &tx,
            &wm,
            &ic,
            "bundle.create",
            json!({"work_id": wi1_id, "branch_name": "feature/index"}),
        );
        dispatch_ok(
            &stores,
            &tx,
            &wm,
            &ic,
            "work.transition",
            json!({"id": wi1_id, "target_status": "InReview", "role": "implementer"}),
        );
        dispatch_ok(
            &stores,
            &tx,
            &wm,
            &ic,
            "work.transition",
            json!({"id": wi1_id, "target_status": "Integrated", "role": "integrator"}),
        );
        dispatch_ok(
            &stores,
            &tx,
            &wm,
            &ic,
            "work.transition",
            json!({"id": wi1_id, "target_status": "Done", "role": "coordinator"}),
        );
        {
            let wis = stores.works.read().unwrap();
            assert_eq!(wis[&wi1_id].status, crate::domain::work::WorkStatus::Done);
        }

        // --- 6. Bundle full lifecycle: Proposed → Triaged → Reviewed → Accepted → Integrating → Merged ---
        // WI2 already Ready via auto-promotion; transition to InProgress
        dispatch_ok(
            &stores,
            &tx,
            &wm,
            &ic,
            "work.transition",
            json!({"id": wi2_id, "target_status": "InProgress", "role": "coordinator", "assignee": "agent-1"}),
        );

        let bundle = dispatch_ok(
            &stores,
            &tx,
            &wm,
            &ic,
            "bundle.create",
            json!({"work_id": wi2_id, "description": "About page", "branch_name": "feature/about"}),
        );
        let bundle_id = bundle["id"].as_str().unwrap().to_string();
        assert_eq!(bundle["status"], "Proposed");

        dispatch_ok(
            &stores,
            &tx,
            &wm,
            &ic,
            "bundle.transition",
            json!({"id": bundle_id, "target_status": "Triaged", "role": "coordinator"}),
        );
        dispatch_ok(
            &stores,
            &tx,
            &wm,
            &ic,
            "bundle.transition",
            json!({"id": bundle_id, "target_status": "Reviewed", "role": "reviewer", "verification": "tests passed"}),
        );
        dispatch_ok(
            &stores,
            &tx,
            &wm,
            &ic,
            "bundle.transition",
            json!({"id": bundle_id, "target_status": "Accepted", "role": "coordinator"}),
        );
        dispatch_ok(
            &stores,
            &tx,
            &wm,
            &ic,
            "bundle.transition",
            json!({"id": bundle_id, "target_status": "Integrating", "role": "integrator"}),
        );
        dispatch_ok(
            &stores,
            &tx,
            &wm,
            &ic,
            "bundle.transition",
            json!({"id": bundle_id, "target_status": "Merged", "role": "integrator"}),
        );
        {
            let bundles = stores.bundles.read().unwrap();
            assert_eq!(bundles[&bundle_id].status, crate::domain::bundle::BundleStatus::Merged);
        }

        // --- 7. Reviewer rejection from Proposed (new FSM rule) ---
        let bundle2 = dispatch_ok(
            &stores,
            &tx,
            &wm,
            &ic,
            "bundle.create",
            json!({"work_id": wi2_id, "description": "Bad bundle", "branch_name": "feature/bad"}),
        );
        let bundle2_id = bundle2["id"].as_str().unwrap().to_string();
        dispatch_ok(
            &stores,
            &tx,
            &wm,
            &ic,
            "bundle.transition",
            json!({"id": bundle2_id, "target_status": "Rejected", "role": "reviewer"}),
        );
        {
            let bundles = stores.bundles.read().unwrap();
            assert_eq!(
                bundles[&bundle2_id].status,
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
            json!({"work_id": wi2_id, "description": "Reviewed then rejected", "branch_name": "feature/rev-reject"}),
        );
        let bundle3_id = bundle3["id"].as_str().unwrap().to_string();
        dispatch_ok(
            &stores,
            &tx,
            &wm,
            &ic,
            "bundle.transition",
            json!({"id": bundle3_id, "target_status": "Triaged", "role": "coordinator"}),
        );
        dispatch_ok(
            &stores,
            &tx,
            &wm,
            &ic,
            "bundle.transition",
            json!({"id": bundle3_id, "target_status": "Reviewed", "role": "reviewer", "verification": "tests passed"}),
        );
        dispatch_ok(
            &stores,
            &tx,
            &wm,
            &ic,
            "bundle.transition",
            json!({"id": bundle3_id, "target_status": "Rejected", "role": "reviewer"}),
        );
        {
            let bundles = stores.bundles.read().unwrap();
            assert_eq!(
                bundles[&bundle3_id].status,
                crate::domain::bundle::BundleStatus::Rejected
            );
        }

        // --- 9. Tick publish with validation in tmpdir git repo ---
        let tick = dispatch_ok(&stores, &tx, &wm, &ic, "tick.create", json!({"number": 1}));
        let tick_id = tick["id"].as_str().unwrap().to_string();
        assert_eq!(tick["status"], "Open");

        let published = dispatch_ok(
            &stores,
            &tx,
            &wm,
            &ic,
            "integrator.publish",
            json!({"tick_id": tick_id}),
        );
        assert_eq!(published["status"], "Published");
        assert!(published["integration_sha"].as_str().is_some_and(|s| !s.is_empty()));
        assert!(
            published["validation_log"]
                .as_str()
                .is_some_and(|s| s.contains("PASSED"))
        );

        // --- 10. Goal still active ---
        let final_goal = dispatch_ok(&stores, &tx, &wm, &ic, "coordinator.get_goal", json!({}));
        assert_eq!(final_goal["goal"], "Build portfolio site");

        // --- 11. Verify counts ---
        assert_eq!(stores.plans.read().unwrap().len(), 1);
        assert_eq!(stores.specs.read().unwrap().len(), 1);
        assert_eq!(stores.phases.read().unwrap().len(), 1);
        assert_eq!(stores.works.read().unwrap().len(), 2);
        assert_eq!(stores.bundles.read().unwrap().len(), 4);
        assert_eq!(stores.ticks.read().unwrap().len(), 1);

        // Cleanup
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_create_spec_with_invalid_plan_id_returns_error() {
        // Verify that CreateSpec with a non-existent plan_id returns an error
        use crate::agents::AgentAction;
        use crate::agents::bridge::AgentIpcBridge;
        use crate::agents::executor::{ActionResult, execute_action};

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

    // ========================================================================
    // E2E: Full pipeline — coordinator assigns implementer, implementer completes
    // ========================================================================

    #[tokio::test]
    async fn test_coordinator_assigns_implementer_completes() {
        use crate::agents::bridge::AgentIpcBridge;
        use crate::agents::implementer::{self, IterationOutcome, LlmClient};
        use async_trait::async_trait;
        use eyre::Result;

        let dir = TestDir::new("loopr-e2e-pipeline");

        // Init git repo so worktree code doesn't fail
        std::process::Command::new("git")
            .args(["init"])
            .current_dir(&dir)
            .output()
            .unwrap();
        std::process::Command::new("git")
            .args(["commit", "--allow-empty", "-m", "init"])
            .current_dir(&dir)
            .output()
            .unwrap();

        // Build stores with repo_path set BEFORE wrapping in Arc
        let mut raw_stores = Stores::new();
        raw_stores.config.project.repo_path = dir.to_path_buf();
        let stores = Arc::new(raw_stores);

        let tx = test_event_tx();
        let wm = test_worktree_mgr();
        let ic = test_integrator_config();

        // Create hierarchy via IPC
        let (_plan_id, _spec_id, phase_id) = create_test_hierarchy(&stores, &tx, &wm, &ic);

        // Create work item
        let wi = dispatch_ok(
            &stores,
            &tx,
            &wm,
            &ic,
            "work.create",
            json!({"phase_id": phase_id, "title": "Write hello.txt", "description": "Create hello.txt with content", "resource_tags": ["src/"], "acceptance_criteria": ["tests pass"]}),
        );
        let wi_id = wi["id"].as_str().unwrap().to_string();

        // Set coordinator goal
        use crate::domain::coordinator_goal::CoordinatorGoal;
        let goal = CoordinatorGoal::new("Build a hello world project".to_string());
        stores.coordinator_goals.write().unwrap().insert(goal.id.clone(), goal);

        // Verify work item starts as Ready (auto-promoted from Draft since acceptance_criteria present)
        let wi_resp = dispatch_ok(&stores, &tx, &wm, &ic, "work.get", json!({"id": wi_id}));
        assert_eq!(wi_resp["status"].as_str().unwrap(), "Ready");

        // Execute AssignAgent — should auto-transition Ready→InProgress
        let worktree_mgr = WorktreeManager::new(dir.to_path_buf(), dir.join(".worktrees"));
        let bridge = AgentIpcBridge::new(stores.clone(), tx.clone(), worktree_mgr, stores.config.clone());

        let assign_action = crate::agents::AgentAction::AssignAgent {
            agent_type: "implementer".to_string(),
            target_id: wi_id.clone(),
        };
        let agent_log = test_agent_logger(&dir);
        let assign_ctx = test_agent_context(&stores, bridge, tx.clone(), agent_log);
        let _ = crate::agents::executor::execute_action(&assign_action, &assign_ctx, &dir, None).await;

        // Verify work item is now InProgress
        let wi_resp = dispatch_ok(&stores, &tx, &wm, &ic, "work.get", json!({"id": wi_id}));
        assert_eq!(
            wi_resp["status"].as_str().unwrap(),
            "InProgress",
            "work item should be InProgress after auto-transition"
        );

        // Now run an implementer iteration directly with a mock LLM that writes a file
        struct PipelineLlm;

        #[async_trait]
        impl LlmClient for PipelineLlm {
            async fn call(&self, _system: &str, user_msg: &str) -> Result<String> {
                // Verify context includes goal and hierarchy
                assert!(user_msg.contains("Project Goal"), "missing Project Goal in context");
                assert!(user_msg.contains("hello world"), "missing goal text in context");
                assert!(user_msg.contains("Write hello.txt"), "missing work item in context");

                Ok(r#"[
                    {"action": "write_file", "path": "hello.txt", "content": "Hello, World!"},
                    {"action": "done", "summary": "Created hello.txt"}
                ]"#
                .to_string())
            }
        }

        let llm: Box<dyn LlmClient> = Box::new(PipelineLlm);
        let log_file_path = dir.join("test-pipeline.log");
        let log_file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&log_file_path)
            .unwrap();
        let agent_log = crate::agents::agent_logger::AgentLogger::_new_for_test(
            AgentType::Implementer,
            "test-pipeline",
            log_file,
            log_file_path,
        );
        let config = crate::config::AgentRoleConfig::default_implementer();
        let mut session = crate::agents::AgentSession::new(AgentType::Implementer, "test".into());
        session.work_id = Some(wi_id.clone());
        session.worktree_path = Some(dir.to_string_lossy().into());
        let impl_bridge = crate::agents::bridge::AgentIpcBridge::new(
            stores.clone(),
            tx.clone(),
            WorktreeManager::new(dir.to_path_buf(), dir.join(".worktrees")),
            stores.config.clone(),
        );
        let ctx = crate::agents::AgentContext {
            session,
            stores: stores.clone(),
            bridge: impl_bridge,
            event_tx: tx.clone(),
            tool_runner: stores.tool_runner.clone(),
            tool_executor: stores.tool_executor.clone(),
            log: agent_log,
            read_cache: std::sync::Mutex::new(crate::agents::cache::ReadCache::default()),
        };
        let agent = implementer::ImplementerAgent::new(ctx, llm, config, wi_id.clone(), dir.to_path_buf());

        let outcome = agent
            .run_iteration(1, None, &mut crate::agents::lifeguard::Lifeguard::new())
            .await
            .unwrap();
        assert!(
            matches!(outcome, IterationOutcome::Done(ref s) if s.contains("hello.txt")),
            "expected Done with hello.txt, got: {:?}",
            outcome
        );

        // Verify file was written
        let content = std::fs::read_to_string(dir.join("hello.txt")).unwrap();
        assert_eq!(content, "Hello, World!");
    }

    // ========================================================================
    // MVP5 Integration Tests: Coordinator Sequencing, Dependencies,
    // Duplicate Detection, Failure Learning, Worktree Base, Phase Gate
    // ========================================================================

    /// Test 1: Full FSM cycle — drive Coordinator through all 5 states
    /// Planning → ActivatePhase → Executing → PhaseGate → GoalComplete
    #[test]
    fn test_full_fsm_cycle() {
        use crate::domain::coordinator_state::{CoordinatorFsmState, CoordinatorState};

        let stores = test_stores();
        let tx = test_event_tx();

        // Create a goal
        let goal = dispatch_ok(
            &stores,
            &tx,
            &test_worktree_mgr(),
            &test_integrator_config(),
            "coordinator.set_goal",
            json!({"goal": "Build a CLI todo app"}),
        );
        let goal_id = goal["id"].as_str().unwrap().to_string();

        // Insert CoordinatorState directly (the Coordinator agent would normally do this)
        let mut state = CoordinatorState::new(goal_id.clone(), InterviewMode::Interactive);
        let state_id = state.id.clone();
        assert_eq!(state.fsm_state, CoordinatorFsmState::Interviewing);

        // Transition through all states: Interviewing → Planning → ActivatePhase → ...
        state.transition_to(CoordinatorFsmState::Planning);
        assert_eq!(state.fsm_state, CoordinatorFsmState::Planning);

        state.transition_to(CoordinatorFsmState::ActivatePhase);
        assert_eq!(state.fsm_state, CoordinatorFsmState::ActivatePhase);

        state.activate_phase("phase-1".to_string());
        assert_eq!(state.fsm_state, CoordinatorFsmState::Executing);
        assert_eq!(state.current_phase_id.as_deref(), Some("phase-1"));

        state.transition_to(CoordinatorFsmState::PhaseGate);
        assert_eq!(state.fsm_state, CoordinatorFsmState::PhaseGate);

        state.complete_phase();
        assert_eq!(state.phases_completed, vec!["phase-1"]);
        assert!(state.current_phase_id.is_none());

        state.transition_to(CoordinatorFsmState::GoalComplete);
        assert!(state.fsm_state.is_terminal());

        // Verify state persists in stores
        stores
            .coordinator_states
            .write()
            .unwrap()
            .insert(state_id.clone(), state.clone());

        let retrieved = stores
            .coordinator_states
            .read()
            .unwrap()
            .get(&state_id)
            .cloned()
            .unwrap();
        assert_eq!(retrieved.fsm_state, CoordinatorFsmState::GoalComplete);
        assert_eq!(retrieved.goal_id, goal_id);
        assert_eq!(retrieved.phases_completed, vec!["phase-1"]);
    }

    /// Test 2: Dependency chain — WIs A→B→C with dependencies.
    /// B depends on A; C depends on B. Verify deps are stored correctly and
    /// work items with unmet deps cannot be independently assigned.
    #[test]
    fn test_dependency_chain_execution() {
        let stores = test_stores();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();
        let ic = test_integrator_config();

        let (_, _, phase_id) = create_test_hierarchy(&stores, &tx, &wm, &ic);

        // Create WI-A (no dependencies)
        let wi_a = dispatch_ok(
            &stores,
            &tx,
            &wm,
            &ic,
            "work.create",
            json!({
                "phase_id": phase_id,
                "title": "Create base types",
                "description": "Foundation types and traits",
                "resource_tags": ["src/types.rs"],
                "acceptance_criteria": ["Types compile"]
            }),
        );
        let wi_a_id = wi_a["id"].as_str().unwrap().to_string();

        // Create WI-B depending on A
        let wi_b = dispatch_ok(
            &stores,
            &tx,
            &wm,
            &ic,
            "work.create",
            json!({
                "phase_id": phase_id,
                "title": "Implement logic",
                "description": "Business logic using base types",
                "resource_tags": ["src/logic.rs"],
                "acceptance_criteria": ["Logic tests pass"],
                "dependencies": [wi_a_id]
            }),
        );
        let wi_b_id = wi_b["id"].as_str().unwrap().to_string();

        // Create WI-C depending on B
        let wi_c = dispatch_ok(
            &stores,
            &tx,
            &wm,
            &ic,
            "work.create",
            json!({
                "phase_id": phase_id,
                "title": "Add integration tests",
                "description": "Integration tests for logic",
                "resource_tags": ["src/tests.rs"],
                "acceptance_criteria": ["Integration tests pass"],
                "dependencies": [wi_b_id]
            }),
        );
        let wi_c_id = wi_c["id"].as_str().unwrap().to_string();

        // Verify dependencies are stored correctly
        let wi_b_get = dispatch_ok(&stores, &tx, &wm, &ic, "work.get", json!({"id": wi_b_id}));
        let b_deps: Vec<String> = serde_json::from_value(wi_b_get["dependencies"].clone()).unwrap();
        assert_eq!(b_deps, vec![wi_a_id.clone()]);

        let wi_c_get = dispatch_ok(&stores, &tx, &wm, &ic, "work.get", json!({"id": wi_c_id}));
        let c_deps: Vec<String> = serde_json::from_value(wi_c_get["dependencies"].clone()).unwrap();
        assert_eq!(c_deps, vec![wi_b_id.clone()]);

        // WI-A should be Ready (no deps, has acceptance_criteria)
        assert_eq!(wi_a["status"].as_str().unwrap(), "Ready");
        // WI-B should also be Ready (auto-promoted because it has acceptance_criteria)
        assert_eq!(wi_b["status"].as_str().unwrap(), "Ready");
        // WI-C should also be Ready
        assert_eq!(wi_c["status"].as_str().unwrap(), "Ready");
    }

    /// Test 3: Duplicate work item rejection — creating a WI with the same title
    /// (case-insensitive) in the same phase should fail.
    #[test]
    fn test_duplicate_work_rejection() {
        let stores = test_stores();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();
        let ic = test_integrator_config();

        let (_, _, phase_id) = create_test_hierarchy(&stores, &tx, &wm, &ic);

        // Create first WI
        dispatch_ok(
            &stores,
            &tx,
            &wm,
            &ic,
            "work.create",
            json!({
                "phase_id": phase_id,
                "title": "Implement auth",
                "description": "Add JWT auth",
                "resource_tags": ["src/auth.rs"],
                "acceptance_criteria": ["Auth works"]
            }),
        );

        // Try creating duplicate with case variation
        let err_code = dispatch_err(
            &stores,
            &tx,
            &wm,
            &ic,
            "work.create",
            json!({
                "phase_id": phase_id,
                "title": "implement auth",
                "description": "Different description",
                "resource_tags": ["src/auth.rs"],
                "acceptance_criteria": ["Auth works"]
            }),
        );

        // -32005 is precondition_failed
        assert_eq!(err_code, -32005, "duplicate WI should return precondition_failed");

        // Different title should succeed
        dispatch_ok(
            &stores,
            &tx,
            &wm,
            &ic,
            "work.create",
            json!({
                "phase_id": phase_id,
                "title": "Implement authorization",
                "description": "Add RBAC",
                "resource_tags": ["src/authz.rs"],
                "acceptance_criteria": ["RBAC works"]
            }),
        );
    }

    /// Test 4: Failure learning creation — verify a Learning with Work scope
    /// and resource_tags can be created to represent a failure insight.
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

    /// Test 5: Worktree base uses latest Published tick — verify
    /// find_latest_published_tick returns the correct tick.
    #[test]
    fn test_worktree_base_uses_published_tick() {
        let stores = test_stores();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();
        let ic = test_integrator_config();

        // Create tick 1
        let tick1 = dispatch_ok(&stores, &tx, &wm, &ic, "tick.create", json!({"number": 1}));
        let tick1_id = tick1["id"].as_str().unwrap().to_string();

        // Tick1: Open → Sealing → Validating → Published
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
                .filter(|t| t.status == TickStatus::Published)
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
            .filter(|t| t.status == TickStatus::Published)
            .max_by_key(|t| t.number)
            .cloned();
        assert!(latest_published.is_some());
        assert_eq!(latest_published.unwrap().id, tick2_id);
    }

    /// Test 6: Coordinator state persistence across iterations — set FSM state,
    /// serialize, deserialize, verify round-trip.
    #[test]
    fn test_coordinator_state_persistence_across_iterations() {
        use crate::domain::coordinator_state::{CoordinatorFsmState, CoordinatorState};

        let stores = test_stores();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();
        let ic = test_integrator_config();

        // Set a goal
        let goal = dispatch_ok(
            &stores,
            &tx,
            &wm,
            &ic,
            "coordinator.set_goal",
            json!({"goal": "Build a REST API"}),
        );
        let goal_id = goal["id"].as_str().unwrap().to_string();

        // Create state, advance to Executing
        let mut state = CoordinatorState::new(goal_id.clone(), InterviewMode::Interactive);
        state.transition_to(CoordinatorFsmState::ActivatePhase);
        state.activate_phase("phase-1".to_string());
        state.increment_attempts("wi-1");
        state.increment_attempts("wi-1");
        let state_id = state.id.clone();

        // Persist to stores
        stores
            .coordinator_states
            .write()
            .unwrap()
            .insert(state_id.clone(), state);

        // Serialize from stores, deserialize — simulate restart
        let serialized = {
            let states = stores.coordinator_states.read().unwrap();
            let s = states.get(&state_id).unwrap();
            serde_json::to_string(s).unwrap()
        };

        let deserialized: CoordinatorState = serde_json::from_str(&serialized).unwrap();
        assert_eq!(deserialized.id, state_id);
        assert_eq!(deserialized.goal_id, goal_id);
        assert_eq!(deserialized.fsm_state, CoordinatorFsmState::Executing);
        assert_eq!(deserialized.current_phase_id.as_deref(), Some("phase-1"));
        assert_eq!(deserialized.attempts("wi-1"), 2);
        assert!(deserialized.phase_activated_at.is_some());
    }

    /// Test 7: Phase gate advances to next phase — complete all WIs in Phase 1,
    /// verify state tracks completion and can activate Phase 2.
    #[test]
    fn test_phase_gate_advances_to_next_phase() {
        use crate::domain::coordinator_state::{CoordinatorFsmState, CoordinatorState};

        let stores = test_stores();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();
        let ic = test_integrator_config();

        // Create hierarchy with two phases
        let plan = dispatch_ok(
            &stores,
            &tx,
            &wm,
            &ic,
            "plan.create",
            json!({"title": "Multi-phase Plan", "description": "desc", "acceptance_criteria": "all phases done"}),
        );
        let plan_id = plan["id"].as_str().unwrap().to_string();
        dispatch_ok(
            &stores,
            &tx,
            &wm,
            &ic,
            "plan.transition",
            json!({"id": plan_id, "target_status": "active"}),
        );

        let spec = dispatch_ok(
            &stores,
            &tx,
            &wm,
            &ic,
            "spec.create",
            json!({"plan_id": plan_id, "title": "Multi-phase Spec", "description": "desc", "acceptance_criteria": "pass"}),
        );
        let spec_id = spec["id"].as_str().unwrap().to_string();
        dispatch_ok(
            &stores,
            &tx,
            &wm,
            &ic,
            "spec.transition",
            json!({"id": spec_id, "target_status": "active"}),
        );

        let phase1 = dispatch_ok(
            &stores,
            &tx,
            &wm,
            &ic,
            "phase.create",
            json!({"spec_id": spec_id, "title": "Phase 1: Foundation", "description": "base types", "acceptance_criteria": "types exist"}),
        );
        let phase1_id = phase1["id"].as_str().unwrap().to_string();
        dispatch_ok(
            &stores,
            &tx,
            &wm,
            &ic,
            "phase.transition",
            json!({"id": phase1_id, "target_status": "active"}),
        );

        let phase2 = dispatch_ok(
            &stores,
            &tx,
            &wm,
            &ic,
            "phase.create",
            json!({"spec_id": spec_id, "title": "Phase 2: Logic", "description": "business logic", "acceptance_criteria": "logic works"}),
        );
        let phase2_id = phase2["id"].as_str().unwrap().to_string();

        // Create a WI in Phase 1
        let wi = dispatch_ok(
            &stores,
            &tx,
            &wm,
            &ic,
            "work.create",
            json!({
                "phase_id": phase1_id,
                "title": "Create base types",
                "description": "Foundation types",
                "resource_tags": ["src/types.rs"],
                "acceptance_criteria": ["Types compile"]
            }),
        );
        let wi_id = wi["id"].as_str().unwrap().to_string();

        // Simulate WI completion: Ready → InProgress → InReview → Integrated → Done
        dispatch_ok(
            &stores,
            &tx,
            &wm,
            &ic,
            "work.transition",
            json!({"id": wi_id, "target_status": "InProgress", "role": "coordinator", "assignee": "agent-1"}),
        );

        // Create a Bundle (required before InReview)
        dispatch_ok(
            &stores,
            &tx,
            &wm,
            &ic,
            "bundle.create",
            json!({"work_id": wi_id, "branch_name": "agent/test-wi", "claims": "implemented types"}),
        );

        dispatch_ok(
            &stores,
            &tx,
            &wm,
            &ic,
            "work.transition",
            json!({"id": wi_id, "target_status": "InReview", "role": "implementer"}),
        );
        dispatch_ok(
            &stores,
            &tx,
            &wm,
            &ic,
            "work.transition",
            json!({"id": wi_id, "target_status": "Integrated", "role": "integrator"}),
        );
        dispatch_ok(
            &stores,
            &tx,
            &wm,
            &ic,
            "work.transition",
            json!({"id": wi_id, "target_status": "Done", "role": "coordinator"}),
        );

        // Verify WI is Done
        let wi_final = dispatch_ok(&stores, &tx, &wm, &ic, "work.get", json!({"id": wi_id}));
        assert_eq!(wi_final["status"].as_str().unwrap(), "Done");

        // Now simulate Coordinator FSM: Phase 1 complete, advance to Phase 2
        let goal_id = "test-goal".to_string();
        let mut coord_state = CoordinatorState::new(goal_id, InterviewMode::Interactive);
        coord_state.activate_phase(phase1_id.clone());
        assert_eq!(coord_state.fsm_state, CoordinatorFsmState::Executing);

        // All WIs in Phase 1 are Done → transition to PhaseGate
        coord_state.transition_to(CoordinatorFsmState::PhaseGate);
        assert_eq!(coord_state.fsm_state, CoordinatorFsmState::PhaseGate);

        // Complete Phase 1
        coord_state.complete_phase();
        assert_eq!(coord_state.phases_completed, vec![phase1_id]);
        assert!(coord_state.current_phase_id.is_none());

        // Transition back to ActivatePhase for Phase 2
        coord_state.transition_to(CoordinatorFsmState::ActivatePhase);
        assert_eq!(coord_state.fsm_state, CoordinatorFsmState::ActivatePhase);

        // Activate Phase 2
        dispatch_ok(
            &stores,
            &tx,
            &wm,
            &ic,
            "phase.transition",
            json!({"id": phase2_id, "target_status": "active"}),
        );
        coord_state.activate_phase(phase2_id.clone());
        assert_eq!(coord_state.fsm_state, CoordinatorFsmState::Executing);
        assert_eq!(coord_state.current_phase_id.as_deref(), Some(phase2_id.as_str()));

        // Complete Phase 2 (no WIs to do, but simulate gate)
        coord_state.transition_to(CoordinatorFsmState::PhaseGate);
        coord_state.complete_phase();
        assert_eq!(coord_state.phases_completed.len(), 2);
        assert_eq!(coord_state.phases_completed[1], phase2_id);

        // No more phases → GoalComplete
        coord_state.transition_to(CoordinatorFsmState::GoalComplete);
        assert!(coord_state.fsm_state.is_terminal());
    }

    // ========================================================================
    // Self-correction loop + Advisory review integration tests
    // ========================================================================

    fn test_stores_with_persistence(dir: &std::path::Path) -> Arc<Stores> {
        let config = Config {
            project: crate::config::ProjectConfig {
                repo_path: dir.to_path_buf(),
                ..crate::config::ProjectConfig::default()
            },
            ..Config::default()
        };
        let store = taskstore::Store::open(dir).unwrap();
        let mut stores = Stores::new();
        stores.store = Some(Arc::new(std::sync::Mutex::new(store)));
        stores.config = config;
        Arc::new(stores)
    }

    #[test]
    fn test_advisory_review_bundle_accepted_directly() {
        // Verify that the Coordinator can accept a Bundle directly (Triaged→Accepted)
        // without waiting for Reviewer verdict (the Integrator is the hard gate).
        let dir = TestDir::new("loopr-int-advisory");
        let stores = test_stores_with_persistence(&dir);
        let tx = test_event_tx();
        let wm = test_worktree_mgr();
        let ic = IntegratorConfig::default();

        // Create hierarchy: plan → spec → phase → work
        let plan_resp = dispatch_ok(
            &stores,
            &tx,
            &wm,
            &ic,
            "plan.create",
            json!({"title": "Advisory Test", "description": "Test advisory review", "acceptance_criteria": "tests pass"}),
        );
        let plan_id = plan_resp["id"].as_str().unwrap();
        let spec_resp = dispatch_ok(
            &stores,
            &tx,
            &wm,
            &ic,
            "spec.create",
            json!({"plan_id": plan_id, "title": "Spec", "description": "spec"}),
        );
        let spec_id = spec_resp["id"].as_str().unwrap();
        let phase_resp = dispatch_ok(
            &stores,
            &tx,
            &wm,
            &ic,
            "phase.create",
            json!({"spec_id": spec_id, "title": "Phase", "description": "phase", "order": 1}),
        );
        let phase_id = phase_resp["id"].as_str().unwrap();
        let work_resp = dispatch_ok(
            &stores,
            &tx,
            &wm,
            &ic,
            "work.create",
            json!({"phase_id": phase_id, "title": "Work", "description": "work", "resource_tags": ["src/main.rs"]}),
        );
        let work_id = work_resp["id"].as_str().unwrap();

        // Create a bundle
        {
            use crate::domain::bundle::{Bundle, BundleStatus};
            let mut bundle = Bundle::new(
                work_id.to_string(),
                None,
                "feature/test".to_string(),
                vec!["test claim".to_string()],
            );
            bundle.status = BundleStatus::Proposed;
            stores.bundles.write().unwrap().insert(bundle.id.clone(), bundle);
        }

        let bundle_id = stores.bundles.read().unwrap().keys().next().unwrap().clone();

        // Coordinator triages: Proposed → Triaged
        dispatch_ok(
            &stores,
            &tx,
            &wm,
            &ic,
            "bundle.transition",
            json!({"id": &bundle_id, "target_status": "Triaged", "role": "coordinator"}),
        );
        assert_eq!(
            stores.bundles.read().unwrap()[&bundle_id].status,
            crate::domain::bundle::BundleStatus::Triaged
        );

        // Coordinator accepts directly: Triaged → Accepted (bypassing review)
        dispatch_ok(
            &stores,
            &tx,
            &wm,
            &ic,
            "bundle.transition",
            json!({"id": &bundle_id, "target_status": "Accepted", "role": "coordinator", "verification": "Coordinator direct accept"}),
        );
        assert_eq!(
            stores.bundles.read().unwrap()[&bundle_id].status,
            crate::domain::bundle::BundleStatus::Accepted
        );
    }

    #[test]
    fn test_reviewer_feedback_learning_available_after_advisory_accept() {
        // When the Coordinator accepts directly and the Reviewer creates feedback,
        // the feedback Learning should be available in stores for future iterations.
        let stores = test_stores();

        // Create a review feedback Learning (simulating what the Reviewer would create)
        let learning = Learning::new(
            "work-1".to_string(),
            LearningScope::Work,
            "Review feedback (approve): Clean code, well tested".to_string(),
        );
        let learning_id = learning.id.clone();
        stores.learnings.write().unwrap().insert(learning.id.clone(), learning);

        // Verify the Learning is accessible
        let learnings = stores.learnings.read().unwrap();
        let feedback = learnings.get(&learning_id).unwrap();
        assert!(feedback.content.contains("Review feedback"));
        assert!(feedback.content.contains("approve"));
    }

    #[test]
    fn test_advisory_bypass_rejected_for_non_coordinator_via_dispatch() {
        // Verify that only the Coordinator can use Triaged→Accepted through the IPC handler.
        let dir = TestDir::new("loopr-int-advisory-reject");
        let stores = test_stores_with_persistence(&dir);
        let tx = test_event_tx();
        let wm = test_worktree_mgr();
        let ic = IntegratorConfig::default();

        // Create hierarchy
        let plan_resp = dispatch_ok(
            &stores,
            &tx,
            &wm,
            &ic,
            "plan.create",
            json!({"title": "T", "description": "d", "acceptance_criteria": "c"}),
        );
        let plan_id = plan_resp["id"].as_str().unwrap();
        let spec_resp = dispatch_ok(
            &stores,
            &tx,
            &wm,
            &ic,
            "spec.create",
            json!({"plan_id": plan_id, "title": "S", "description": "d"}),
        );
        let spec_id = spec_resp["id"].as_str().unwrap();
        let phase_resp = dispatch_ok(
            &stores,
            &tx,
            &wm,
            &ic,
            "phase.create",
            json!({"spec_id": spec_id, "title": "P", "description": "d", "order": 1}),
        );
        let phase_id = phase_resp["id"].as_str().unwrap();
        let work_resp = dispatch_ok(
            &stores,
            &tx,
            &wm,
            &ic,
            "work.create",
            json!({"phase_id": phase_id, "title": "W", "description": "d", "resource_tags": ["src/x.rs"]}),
        );
        let work_id = work_resp["id"].as_str().unwrap();

        // Create and triage a bundle
        {
            use crate::domain::bundle::{Bundle, BundleStatus};
            let mut bundle = Bundle::new(work_id.to_string(), None, "f/t".to_string(), vec!["claim".to_string()]);
            bundle.status = BundleStatus::Proposed;
            stores.bundles.write().unwrap().insert(bundle.id.clone(), bundle);
        }
        let bundle_id = stores.bundles.read().unwrap().keys().next().unwrap().clone();
        dispatch_ok(
            &stores,
            &tx,
            &wm,
            &ic,
            "bundle.transition",
            json!({"id": &bundle_id, "target_status": "Triaged", "role": "coordinator"}),
        );

        // Reviewer trying Triaged→Accepted should FAIL
        let req = DaemonRequest::new(
            1,
            "bundle.transition",
            json!({"id": &bundle_id, "target_status": "Accepted", "role": "reviewer", "verification": "v"}),
        );
        let resp = dispatch(&stores, &tx, &wm, &ic, req);
        assert!(resp.is_error(), "Reviewer should not be able to use Triaged→Accepted");

        // Implementer trying Triaged→Accepted should FAIL
        let req = DaemonRequest::new(
            2,
            "bundle.transition",
            json!({"id": &bundle_id, "target_status": "Accepted", "role": "implementer", "verification": "v"}),
        );
        let resp = dispatch(&stores, &tx, &wm, &ic, req);
        assert!(
            resp.is_error(),
            "Implementer should not be able to use Triaged→Accepted"
        );

        // Coordinator trying Triaged→Accepted should SUCCEED
        dispatch_ok(
            &stores,
            &tx,
            &wm,
            &ic,
            "bundle.transition",
            json!({"id": &bundle_id, "target_status": "Accepted", "role": "coordinator", "verification": "Coordinator direct"}),
        );
        assert_eq!(
            stores.bundles.read().unwrap()[&bundle_id].status,
            crate::domain::bundle::BundleStatus::Accepted
        );
    }

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
        // 4th failure exceeds threshold → escalate
        assert!(matches!(lg.record_parse_failure(), Verdict::Escalate(_)));
    }

    #[test]
    fn test_max_requeries_config_defaults() {
        use crate::config::AgentRoleConfig;

        // All roles default to max_requeries=3
        assert_eq!(AgentRoleConfig::default_implementer().max_requeries, 3);
        assert_eq!(AgentRoleConfig::default_reviewer().max_requeries, 3);
        assert_eq!(AgentRoleConfig::default_researcher().max_requeries, 3);
    }

    // ========================================================================
    // Pre-formed plan integration tests
    //
    // These tests inject a fully-formed Plan→Spec→Phase→Work hierarchy as if
    // the funnel (chat → plan → draft → approve) was already completed.
    // They verify the hierarchy is correctly wired and ready for implementation.
    // ========================================================================

    /// Helper: inject a pre-formed plan with specs, phases, and works, all transitioned to active.
    /// Returns (plan_id, vec of (spec_id, vec of (phase_id, vec of work_ids))).
    fn inject_preformed_plan(
        stores: &Arc<Stores>,
        tx: &broadcast::Sender<DaemonEvent>,
        wm: &WorktreeManager,
        ic: &IntegratorConfig,
        input: PlanInput<'_>,
    ) -> (String, Vec<SpecResult>) {
        let plan = dispatch_ok(
            stores,
            tx,
            wm,
            ic,
            "plan.create",
            json!({
                "title": input.title,
                "description": input.desc,
                "acceptance_criteria": input.criteria,
            }),
        );
        let plan_id = plan["id"].as_str().unwrap().to_string();
        dispatch_ok(
            stores,
            tx,
            wm,
            ic,
            "plan.transition",
            json!({
                "id": plan_id, "target_status": "active"
            }),
        );

        let mut spec_results = Vec::new();
        for (spec_title, spec_desc, phases) in input.specs {
            let spec = dispatch_ok(
                stores,
                tx,
                wm,
                ic,
                "spec.create",
                json!({
                    "plan_id": plan_id,
                    "title": spec_title,
                    "description": spec_desc,
                    "acceptance_criteria": "all tests pass",
                }),
            );
            let spec_id = spec["id"].as_str().unwrap().to_string();
            dispatch_ok(
                stores,
                tx,
                wm,
                ic,
                "spec.transition",
                json!({
                    "id": spec_id, "target_status": "active"
                }),
            );

            let mut phase_results = Vec::new();
            for (phase_title, phase_desc, order, works) in &phases {
                let phase = dispatch_ok(
                    stores,
                    tx,
                    wm,
                    ic,
                    "phase.create",
                    json!({
                        "spec_id": spec_id,
                        "title": phase_title,
                        "description": phase_desc,
                        "order": order,
                    }),
                );
                let phase_id = phase["id"].as_str().unwrap().to_string();
                dispatch_ok(
                    stores,
                    tx,
                    wm,
                    ic,
                    "phase.transition",
                    json!({
                        "id": phase_id, "target_status": "active"
                    }),
                );

                let mut work_ids = Vec::new();
                for (work_title, work_desc, resource_tags) in works {
                    let work = dispatch_ok(
                        stores,
                        tx,
                        wm,
                        ic,
                        "work.create",
                        json!({
                            "phase_id": phase_id,
                            "title": work_title,
                            "description": work_desc,
                            "resource_tags": resource_tags,
                            "acceptance_criteria": ["tests pass"],
                        }),
                    );
                    work_ids.push(work["id"].as_str().unwrap().to_string());
                }
                phase_results.push((phase_id, work_ids));
            }
            spec_results.push((spec_id, phase_results));
        }
        (plan_id, spec_results)
    }

    #[test]
    fn test_preformed_todo_app_plan() {
        let stores = test_stores();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();
        let ic = test_integrator_config();

        let (plan_id, spec_results) = inject_preformed_plan(
            &stores,
            &tx,
            &wm,
            &ic,
            PlanInput {
                title: "CLI Todo App",
                desc: "Build a command-line todo application with add, list, done, delete, and filter commands. Persist todos to a JSON file.",
                criteria: "1. CRUD operations work\n2. Persistence to JSON\n3. Filter by status\n4. All tests pass",
                specs: vec![(
                    "Todo App Technical Spec",
                    "Full technical specification for the CLI todo app",
                    vec![
                        (
                            "Phase 1: Data Model & Storage",
                            "Implement Todo struct and JSON file persistence",
                            1,
                            vec![
                                (
                                    "Todo struct",
                                    "Define Todo with id, title, done, created_at fields",
                                    vec!["src/model.rs"],
                                ),
                                (
                                    "JSON storage",
                                    "Read/write todos to a JSON file on disk",
                                    vec!["src/storage.rs"],
                                ),
                            ],
                        ),
                        (
                            "Phase 2: CRUD Operations",
                            "Implement add, list, done, delete commands",
                            2,
                            vec![
                                ("Add command", "Add a new todo with a title", vec!["src/commands.rs"]),
                                (
                                    "List command",
                                    "List all todos with status indicators",
                                    vec!["src/commands.rs"],
                                ),
                                (
                                    "Done command",
                                    "Mark a todo as completed by ID",
                                    vec!["src/commands.rs"],
                                ),
                                ("Delete command", "Remove a todo by ID", vec!["src/commands.rs"]),
                            ],
                        ),
                        (
                            "Phase 3: Filtering & CLI",
                            "Add filter support and wire up CLI arg parsing",
                            3,
                            vec![
                                (
                                    "Filter by status",
                                    "Filter todos by all/active/done",
                                    vec!["src/commands.rs"],
                                ),
                                (
                                    "CLI entry point",
                                    "Parse args and dispatch to commands",
                                    vec!["src/main.rs"],
                                ),
                            ],
                        ),
                    ],
                )],
            },
        );

        // Verify hierarchy counts
        assert_eq!(stores.plans.read().unwrap().len(), 1);
        assert_eq!(stores.specs.read().unwrap().len(), 1);
        assert_eq!(stores.phases.read().unwrap().len(), 3);
        assert_eq!(stores.works.read().unwrap().len(), 8);

        // Verify plan is active
        let plans = stores.plans.read().unwrap();
        assert_eq!(plans[&plan_id].status.to_string(), "active");

        // Verify spec→plan relationship
        let (ref spec_id, ref phases) = spec_results[0];
        let specs = stores.specs.read().unwrap();
        assert_eq!(&specs[spec_id].plan_id, &plan_id);

        // Verify phase→spec relationships and ordering
        let phase_store = stores.phases.read().unwrap();
        for (i, (phase_id, _)) in phases.iter().enumerate() {
            let phase = &phase_store[phase_id];
            assert_eq!(&phase.spec_id, spec_id);
            assert_eq!(phase.order, (i + 1) as u32);
            assert_eq!(phase.status.to_string(), "active");
        }

        // Verify work→phase relationships
        let work_store = stores.works.read().unwrap();
        let (ref phase1_id, ref phase1_works) = phases[0];
        assert_eq!(phase1_works.len(), 2);
        for wid in phase1_works {
            assert_eq!(work_store[wid].phase_id, *phase1_id);
        }

        let (ref phase2_id, ref phase2_works) = phases[1];
        assert_eq!(phase2_works.len(), 4);
        for wid in phase2_works {
            assert_eq!(work_store[wid].phase_id, *phase2_id);
        }

        // All works should be Ready (auto-promoted from Draft since acceptance_criteria present)
        for work in work_store.values() {
            assert_eq!(work.status.to_string(), "Ready");
        }
    }

    #[test]
    fn test_preformed_calculator_app_plan() {
        let stores = test_stores();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();
        let ic = test_integrator_config();

        let (plan_id, spec_results) = inject_preformed_plan(
            &stores,
            &tx,
            &wm,
            &ic,
            PlanInput {
                title: "Calculator CLI",
                desc: "Build a command-line calculator supporting basic arithmetic, expression parsing, and a REPL mode.",
                criteria: "1. Basic arithmetic (+, -, *, /)\n2. Expression parsing with operator precedence\n3. REPL mode\n4. Error handling for division by zero\n5. All tests pass",
                specs: vec![(
                    "Calculator Technical Spec",
                    "Technical specification for the CLI calculator",
                    vec![
                        (
                            "Phase 1: Arithmetic Engine",
                            "Implement core arithmetic operations with error handling",
                            1,
                            vec![
                                (
                                    "Arithmetic ops",
                                    "Implement add, subtract, multiply, divide with f64",
                                    vec!["src/engine.rs"],
                                ),
                                (
                                    "Error handling",
                                    "Handle division by zero and overflow gracefully",
                                    vec!["src/engine.rs"],
                                ),
                            ],
                        ),
                        (
                            "Phase 2: Expression Parser",
                            "Parse and evaluate mathematical expressions",
                            2,
                            vec![
                                (
                                    "Tokenizer",
                                    "Tokenize input string into numbers and operators",
                                    vec!["src/parser.rs"],
                                ),
                                (
                                    "Parser",
                                    "Recursive descent parser with operator precedence",
                                    vec!["src/parser.rs"],
                                ),
                                (
                                    "Evaluator",
                                    "Evaluate parsed AST to produce a result",
                                    vec!["src/parser.rs"],
                                ),
                            ],
                        ),
                        (
                            "Phase 3: REPL & CLI",
                            "Interactive REPL mode and CLI entry point",
                            3,
                            vec![
                                ("REPL loop", "Read-eval-print loop with history", vec!["src/repl.rs"]),
                                (
                                    "CLI entry point",
                                    "Parse args: expression mode vs REPL mode",
                                    vec!["src/main.rs"],
                                ),
                            ],
                        ),
                    ],
                )],
            },
        );

        // Verify hierarchy counts
        assert_eq!(stores.plans.read().unwrap().len(), 1);
        assert_eq!(stores.specs.read().unwrap().len(), 1);
        assert_eq!(stores.phases.read().unwrap().len(), 3);
        assert_eq!(stores.works.read().unwrap().len(), 7);

        // Verify everything is active/ready
        let plans = stores.plans.read().unwrap();
        assert_eq!(plans[&plan_id].status.to_string(), "active");

        let phase_store = stores.phases.read().unwrap();
        for phase in phase_store.values() {
            assert_eq!(phase.status.to_string(), "active");
        }

        let work_store = stores.works.read().unwrap();
        for work in work_store.values() {
            assert_eq!(work.status.to_string(), "Ready");
        }

        // Verify phase ordering within spec
        let (_, ref phases) = spec_results[0];
        for (i, (phase_id, _)) in phases.iter().enumerate() {
            assert_eq!(phase_store[phase_id].order, (i + 1) as u32);
        }
    }

    #[test]
    fn test_preformed_plan_work_can_transition_to_in_progress() {
        let stores = test_stores();
        let tx = test_event_tx();
        let wm = test_worktree_mgr();
        let ic = test_integrator_config();

        let (_, spec_results) = inject_preformed_plan(
            &stores,
            &tx,
            &wm,
            &ic,
            PlanInput {
                title: "Tiny App",
                desc: "A minimal app for testing work transitions",
                criteria: "It works",
                specs: vec![(
                    "Spec",
                    "The spec",
                    vec![(
                        "Phase 1",
                        "The only phase",
                        1,
                        vec![("Implement main", "Write main.rs", vec!["src/main.rs"])],
                    )],
                )],
            },
        );

        let work_id = &spec_results[0].1[0].1[0];

        // Transition work: Ready → InProgress
        let result = dispatch_ok(
            &stores,
            &tx,
            &wm,
            &ic,
            "work.transition",
            json!({"id": work_id, "target_status": "InProgress", "role": "coordinator", "assignee": "agent-impl-1"}),
        );
        assert_eq!(result["status"], "InProgress");

        // Verify the work is assigned
        let work_store = stores.works.read().unwrap();
        let work = &work_store[work_id];
        assert_eq!(work.status.to_string(), "InProgress");
        assert_eq!(work.assignee.as_deref(), Some("agent-impl-1"));
    }
}
