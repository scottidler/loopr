    use super::*;
    use crate::agents::bridge::AgentIpcBridge;
    use crate::agents::{Agent, AgentContext, AgentSession};
    use crate::config::{Config, InterviewMode, ProjectConfig};
    use crate::domain::bundle::Bundle;
    use crate::domain::lock::Lock;
    use crate::domain::phase::Phase;
    use crate::domain::plan::Plan;
    use crate::domain::spec::Spec;
    use crate::domain::tick::Tick;
    use crate::domain::work::{Work, WorkStatus};
    use crate::test_util::TestDir;
    use crate::worktree::manager::WorktreeManager;
    use async_trait::async_trait;
    use std::sync::Arc;
    use std::sync::Mutex as StdMutex;
    use taskstore::Store;
    use tokio::sync::broadcast;

    /// Mock LLM client for testing.
    struct MockLlm {
        responses: StdMutex<Vec<String>>,
    }

    impl MockLlm {
        fn new(responses: Vec<String>) -> Self {
            Self {
                responses: StdMutex::new(responses),
            }
        }
    }

    #[async_trait]
    impl LlmClient for MockLlm {
        async fn call(&self, _system_prompt: &str, _user_message: &str) -> Result<String> {
            let mut responses = self.responses.lock().unwrap();
            if responses.is_empty() {
                Ok(r#"[{"action": "done", "summary": "No more responses"}]"#.to_string())
            } else {
                Ok(responses.remove(0))
            }
        }
    }

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

    fn test_stores_with_validator(dir: &std::path::Path) -> Arc<Stores> {
        use crate::config::ValidatorConfig;
        use crate::validator::DocValidator;

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
        stores.validator = Some(Arc::new(DocValidator::new(ValidatorConfig {
            enabled: true,
            ..ValidatorConfig::default()
        })));
        Arc::new(stores)
    }

    fn test_agent_logger(dir: &std::path::Path) -> AgentLogger {
        use crate::agents::AgentType;
        let file_path = dir.join("test-coordinator.log");
        let file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&file_path)
            .unwrap();
        AgentLogger::_new_for_test(AgentType::Coordinator, "test-session", file, file_path)
    }

    /// Build a CoordinatorAgent for testing with the given stores and LLM responses.
    fn test_coordinator(
        dir: &std::path::Path,
        stores: &Arc<Stores>,
        responses: Vec<String>,
        config: CoordinatorConfig,
    ) -> CoordinatorAgent {
        let (event_tx, _rx) = broadcast::channel(16);
        let session = AgentSession::new(AgentType::Coordinator, "test-model".into());
        let worktree_mgr = WorktreeManager::new(dir.to_path_buf(), dir.join(".wt"));
        let bridge = AgentIpcBridge::new(stores.clone(), event_tx.clone(), worktree_mgr, stores.config.clone());
        let agent_log = test_agent_logger(dir);
        let ctx = AgentContext {
            session,
            stores: stores.clone(),
            bridge,
            event_tx,
            tool_runner: stores.read_tool_runner().unwrap(),
            tool_executor: stores.read_tool_executor().unwrap(),
            log: agent_log,
            read_cache: std::sync::Mutex::new(crate::agents::cache::ReadCache::default()),
        };
        let llm = Box::new(MockLlm::new(responses));
        CoordinatorAgent::new(ctx, llm, config)
    }

    /// Insert an active goal so `load_or_create_coordinator_state` returns `Some`.
    fn insert_test_goal(stores: &Arc<Stores>) {
        use crate::domain::coordinator_goal::CoordinatorGoal;
        let goal = CoordinatorGoal::new("test goal".to_string());
        stores.coordinator_goals.write().unwrap().insert(goal.id.clone(), goal);
    }

    // --- build_state_summary tests ---

    #[test]
    fn test_build_state_summary_empty() {
        let dir = TestDir::new("loopr-coord-empty");
        let stores = test_stores(&dir);

        let agent_log = test_agent_logger(&dir);
        let summary = build_state_summary(&stores, &agent_log);
        assert!(summary.contains("No active records"));
    }

    #[test]
    fn test_build_state_summary_with_plan() {
        let dir = TestDir::new("loopr-coord-plan");
        let stores = test_stores(&dir);

        let plan = Plan::new("Test Plan".into(), "A test plan".into(), "Tests pass".into());
        let plan_id = plan.id.clone();
        stores.plans.write().unwrap().insert(plan_id.clone(), plan);

        let agent_log = test_agent_logger(&dir);
        let summary = build_state_summary(&stores, &agent_log);
        assert!(summary.contains("### Plans"));
        assert!(summary.contains("Test Plan"));
        assert!(summary.contains("draft"));
    }

    #[test]
    fn test_build_state_summary_excludes_completed() {
        let dir = TestDir::new("loopr-coord-excl");
        let stores = test_stores(&dir);

        let mut plan = Plan::new("Done Plan".into(), "desc".into(), "crit".into());
        plan.status = HierarchyStatus::Complete;
        stores.plans.write().unwrap().insert(plan.id.clone(), plan);

        let agent_log = test_agent_logger(&dir);
        let summary = build_state_summary(&stores, &agent_log);
        assert!(!summary.contains("Done Plan"));
        assert!(summary.contains("No active records"));
    }

    #[test]
    fn test_build_state_summary_with_works() {
        let dir = TestDir::new("loopr-coord-wi");
        let stores = test_stores(&dir);

        let wi = Work::new("ph-1".into(), "Add auth".into(), "desc".into());
        stores.works.write().unwrap().insert(wi.id.clone(), wi);

        let agent_log = test_agent_logger(&dir);
        let summary = build_state_summary(&stores, &agent_log);
        assert!(summary.contains("### Works"));
        assert!(summary.contains("Add auth"));
    }

    #[test]
    fn test_build_state_summary_with_bundles() {
        let dir = TestDir::new("loopr-coord-bun");
        let stores = test_stores(&dir);

        // Proposed bundle
        let bundle = Bundle::new("wi-1".into(), None, "branch-1".into(), vec!["claims".into()]);
        stores.bundles.write().unwrap().insert(bundle.id.clone(), bundle);

        let agent_log = test_agent_logger(&dir);
        let summary = build_state_summary(&stores, &agent_log);
        assert!(summary.contains("### Proposed Bundles (use triage_bundle)"));
        assert!(summary.contains("Proposed"));
        assert!(!summary.contains("### Reviewed Bundles"));
    }

    #[test]
    fn test_build_state_summary_with_reviewed_bundles() {
        let dir = TestDir::new("loopr-coord-bun-rev");
        let stores = test_stores(&dir);

        // Reviewed bundle
        let mut bundle = Bundle::new("wi-2".into(), None, "branch-2".into(), vec!["claims".into()]);
        bundle.status = BundleStatus::Reviewed;
        stores.bundles.write().unwrap().insert(bundle.id.clone(), bundle);

        let agent_log = test_agent_logger(&dir);
        let summary = build_state_summary(&stores, &agent_log);
        assert!(summary.contains("### Reviewed Bundles (use accept_bundle)"));
        assert!(!summary.contains("### Proposed Bundles"));
    }

    #[test]
    fn test_build_state_summary_with_active_sessions() {
        let dir = TestDir::new("loopr-coord-sess");
        let stores = test_stores(&dir);

        let session = AgentSession::new(AgentType::Implementer, "model".into());
        stores
            .agent_sessions
            .write()
            .unwrap()
            .insert(session.id.clone(), session);

        let agent_log = test_agent_logger(&dir);
        let summary = build_state_summary(&stores, &agent_log);
        assert!(summary.contains("### Active Agents"));
        assert!(summary.contains("implementer"));
    }

    #[test]
    fn test_build_state_summary_with_locks() {
        let dir = TestDir::new("loopr-coord-lock");
        let stores = test_stores(&dir);

        let lock = Lock::new("src/main.rs".into(), "wi-1".into(), "coordinator".into());
        stores.locks.write().unwrap().insert(lock.id.clone(), lock);

        let agent_log = test_agent_logger(&dir);
        let summary = build_state_summary(&stores, &agent_log);
        assert!(summary.contains("### Active Locks"));
        assert!(summary.contains("src/main.rs"));
    }

    #[test]
    fn test_build_state_summary_excludes_terminal_sessions() {
        let dir = TestDir::new("loopr-coord-termsess");
        let stores = test_stores(&dir);

        let mut session = AgentSession::new(AgentType::Implementer, "model".into());
        session.status = AgentStatus::Completed;
        stores
            .agent_sessions
            .write()
            .unwrap()
            .insert(session.id.clone(), session);

        let agent_log = test_agent_logger(&dir);
        let summary = build_state_summary(&stores, &agent_log);
        assert!(!summary.contains("### Active Agents"));
    }

    #[test]
    fn test_build_state_summary_works_before_plans() {
        let dir = TestDir::new("loopr-coord-order");
        let stores = test_stores(&dir);

        let plan = Plan::new("Test Plan".into(), "desc".into(), "crit".into());
        stores.plans.write().unwrap().insert(plan.id.clone(), plan);

        let wi = Work::new("ph-1".into(), "Add auth".into(), "desc".into());
        stores.works.write().unwrap().insert(wi.id.clone(), wi);

        let agent_log = test_agent_logger(&dir);
        let summary = build_state_summary(&stores, &agent_log);

        let works_pos = summary.find("### Works").expect("Works section missing");
        let plans_pos = summary.find("### Plans").expect("Plans section missing");
        assert!(
            works_pos < plans_pos,
            "Works ({}) should appear before Plans ({}) in summary",
            works_pos,
            plans_pos
        );
    }

    #[test]
    fn test_build_state_summary_excludes_done_works() {
        let dir = TestDir::new("loopr-coord-donewi");
        let stores = test_stores(&dir);

        let mut wi = Work::new("ph-1".into(), "Done Work".into(), "desc".into());
        wi.status = WorkStatus::Done;
        stores.works.write().unwrap().insert(wi.id.clone(), wi);

        let agent_log = test_agent_logger(&dir);
        let summary = build_state_summary(&stores, &agent_log);
        assert!(!summary.contains("Done Work"));
    }

    #[test]
    fn test_build_state_summary_excludes_merged_bundles_from_active() {
        let dir = TestDir::new("loopr-coord-mergedbun");
        let stores = test_stores(&dir);

        let mut bundle = Bundle::new("wi-1".into(), None, "branch-1".into(), vec!["claims".into()]);
        bundle.status = BundleStatus::Merged;
        stores.bundles.write().unwrap().insert(bundle.id.clone(), bundle);

        let agent_log = test_agent_logger(&dir);
        let summary = build_state_summary(&stores, &agent_log);
        // Merged bundles should NOT appear in the "### Bundles" section (non-terminal only)
        assert!(!summary.contains("### Bundles\n"));
    }

    // --- is_cancelled tests (via AgentContext) ---

    #[test]
    fn test_is_cancelled_false() {
        let dir = TestDir::new("loopr-coord-canc1");
        let stores = test_stores(&dir);
        let agent = test_coordinator(&dir, &stores, vec![], CoordinatorConfig::default());

        // Insert the agent's session as Running
        let mut session = agent.ctx.session.clone();
        let _ = session.transition_to(AgentStatus::Running);
        stores
            .agent_sessions
            .write()
            .unwrap()
            .insert(session.id.clone(), session);

        assert!(!agent.ctx.is_cancelled());
    }

    #[test]
    fn test_is_cancelled_true() {
        let dir = TestDir::new("loopr-coord-canc2");
        let stores = test_stores(&dir);
        let agent = test_coordinator(&dir, &stores, vec![], CoordinatorConfig::default());

        // Insert the agent's session as Cancelled
        let mut session = agent.ctx.session.clone();
        let _ = session.transition_to(AgentStatus::Running);
        let _ = session.transition_to(AgentStatus::Cancelled);
        stores
            .agent_sessions
            .write()
            .unwrap()
            .insert(session.id.clone(), session);

        assert!(agent.ctx.is_cancelled());
    }

    #[test]
    fn test_is_cancelled_missing() {
        let dir = TestDir::new("loopr-coord-canc3");
        let stores = test_stores(&dir);
        // Agent session not inserted into stores — should treat as cancelled
        let agent = test_coordinator(&dir, &stores, vec![], CoordinatorConfig::default());

        assert!(agent.ctx.is_cancelled());
    }

    // --- run_iteration tests ---

    #[tokio::test]
    async fn test_coordinator_iteration_done() {
        let dir = TestDir::new("loopr-coord-itdone");
        let stores = test_stores(&dir);

        let agent = test_coordinator(
            &dir,
            &stores,
            vec![r#"[{"action": "done", "summary": "Nothing to do"}]"#.to_string()],
            CoordinatorConfig::default(),
        );

        let outcome = agent
            .run_iteration(
                &mut CoordinatorState::new("test-goal".to_string(), InterviewMode::Interactive),
                &mut Lifeguard::new(),
            )
            .await
            .unwrap();

        assert!(matches!(outcome, IterationOutcome::Done(ref s) if s.contains("Nothing to do")));
    }

    #[tokio::test]
    async fn test_coordinator_iteration_need_help() {
        crate::prompts::init_defaults();
        let dir = TestDir::new("loopr-coord-ithelp");
        let stores = test_stores(&dir);

        let agent = test_coordinator(
            &dir,
            &stores,
            vec![r#"[{"action": "need_help", "reason": "Unclear requirements"}]"#.to_string()],
            CoordinatorConfig::default(),
        );

        let outcome = agent
            .run_iteration(
                &mut CoordinatorState::new("test-goal".to_string(), InterviewMode::Interactive),
                &mut Lifeguard::new(),
            )
            .await
            .unwrap();

        assert!(matches!(outcome, IterationOutcome::NeedHelp(ref s) if s.contains("Unclear")));
    }

    #[tokio::test]
    async fn test_coordinator_iteration_continue_with_stub_actions() {
        crate::prompts::init_defaults();
        let dir = TestDir::new("loopr-coord-itstub");
        let stores = test_stores(&dir);

        let agent = test_coordinator(
            &dir,
            &stores,
            vec![r#"[{"action": "create_plan", "title": "Auth", "description": "Add auth", "acceptance_criteria": "Tests pass"}]"#.to_string()],
            CoordinatorConfig::default(),
        );

        let outcome = agent
            .run_iteration(
                &mut CoordinatorState::new("test-goal".to_string(), InterviewMode::Interactive),
                &mut Lifeguard::new(),
            )
            .await
            .unwrap();

        // CreatePlan is now wired — creates a real plan via bridge, returns Continue
        assert!(matches!(outcome, IterationOutcome::Continue(_)));
    }

    #[tokio::test]
    async fn test_coordinator_iteration_empty_actions_is_done() {
        let dir = TestDir::new("loopr-coord-itempty");
        let stores = test_stores(&dir);

        let agent = test_coordinator(&dir, &stores, vec!["[]".to_string()], CoordinatorConfig::default());

        let outcome = agent
            .run_iteration(
                &mut CoordinatorState::new("test-goal".to_string(), InterviewMode::Interactive),
                &mut Lifeguard::new(),
            )
            .await
            .unwrap();

        assert!(matches!(outcome, IterationOutcome::Done(_)));
    }

    // --- Agent::run tests ---

    #[tokio::test]
    async fn test_coordinator_exits_on_need_help() {
        let dir = TestDir::new("loopr-coord-runhelp");
        let stores = test_stores(&dir);
        insert_test_goal(&stores);

        let mut agent = test_coordinator(
            &dir,
            &stores,
            vec![r#"[{"action": "need_help", "reason": "I'm stuck"}]"#.to_string()],
            CoordinatorConfig::default(),
        );

        // Insert the session as Running
        let mut session = agent.ctx.session.clone();
        let _ = session.transition_to(AgentStatus::Running);
        stores
            .agent_sessions
            .write()
            .unwrap()
            .insert(session.id.clone(), session);

        let result = agent.run().await;
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("needs help"));
    }

    #[tokio::test]
    async fn test_coordinator_exits_on_cancellation() {
        let dir = TestDir::new("loopr-coord-runcanc");
        let stores = test_stores(&dir);
        insert_test_goal(&stores);

        let mut agent = test_coordinator(&dir, &stores, vec![], CoordinatorConfig::default());

        // Insert the session as Cancelled
        let mut session = agent.ctx.session.clone();
        let _ = session.transition_to(AgentStatus::Running);
        let _ = session.transition_to(AgentStatus::Cancelled);
        stores
            .agent_sessions
            .write()
            .unwrap()
            .insert(session.id.clone(), session);

        let result = agent.run().await;
        assert!(result.is_ok()); // Cancelled = graceful exit
    }

    // --- format_action_summary tests ---

    #[test]
    fn test_format_action_summary_done() {
        let result = ActionResult::Done("Complete".into());
        assert_eq!(format_action_summary(&result), "done: Complete");
    }

    #[test]
    fn test_format_action_summary_record_created() {
        let result = ActionResult::RecordCreated {
            collection: "plans".into(),
            id: "plan-123".into(),
        };
        let summary = format_action_summary(&result);
        assert!(summary.contains("created plans"));
        assert!(summary.contains("plan-123"));
    }

    #[test]
    fn test_format_action_summary_agent_spawned() {
        let result = ActionResult::AgentSpawned {
            session_id: "sess-abc".into(),
            agent_type: "implementer".into(),
        };
        let summary = format_action_summary(&result);
        assert!(summary.contains("spawned implementer"));
        assert!(summary.contains("sess-abc"));
    }

    #[test]
    fn test_format_action_summary_error() {
        let result = ActionResult::ActionError("something broke".into());
        assert_eq!(format_action_summary(&result), "ERROR: something broke");
    }

    #[test]
    fn test_format_action_summary_document_validated_pass() {
        let result = ActionResult::DocumentValidated {
            verdict: "pass".into(),
            summary: "All criteria met".into(),
            issues: vec![],
        };
        let summary = format_action_summary(&result);
        assert!(summary.contains("validated: pass"));
        assert!(summary.contains("All criteria met"));
        assert!(!summary.contains("issues"));
    }

    #[test]
    fn test_format_action_summary_document_validated_fail_with_issues() {
        let result = ActionResult::DocumentValidated {
            verdict: "fail".into(),
            summary: "Incomplete document".into(),
            issues: vec!["Missing criteria".into(), "Too vague".into()],
        };
        let summary = format_action_summary(&result);
        assert!(summary.contains("validated: fail"));
        assert!(summary.contains("2 issues"));
    }

    // --- system prompt tests ---

    #[test]
    fn test_system_prompt_contains_key_sections() {
        crate::prompts::init_defaults();
        let prompt = &crate::prompts::store().coordinator;
        assert!(prompt.contains("Coordinator agent"));
        assert!(prompt.contains("create_plan"));
        assert!(prompt.contains("assign_agent"));
        assert!(prompt.contains("need_help"));
        assert!(prompt.contains("JSON array"));
        // Phase-gated FSM sections from MVP5
        assert!(prompt.contains("Phase-Gated Control Loop"));
        assert!(prompt.contains("Planning"));
        assert!(prompt.contains("ActivatePhase"));
        assert!(prompt.contains("Executing"));
        assert!(prompt.contains("PhaseGate"));
        assert!(prompt.contains("GoalComplete"));
        assert!(prompt.contains("dependencies"));
    }

    // --- comprehensive state summary ---

    #[test]
    fn test_build_state_summary_comprehensive() {
        let dir = TestDir::new("loopr-coord-comp");
        let stores = test_stores(&dir);

        // Add plan
        let plan = Plan::new("My Plan".into(), "desc".into(), "crit".into());
        let plan_id = plan.id.clone();
        stores.plans.write().unwrap().insert(plan_id.clone(), plan);

        // Add spec
        let spec = Spec::new(plan_id.clone(), "My Spec".into(), "spec desc".into());
        let spec_id = spec.id.clone();
        stores.specs.write().unwrap().insert(spec_id.clone(), spec);

        // Add phase
        let phase = Phase::new(spec_id.clone(), "Phase 1".into(), "phase desc".into(), 1);
        let phase_id = phase.id.clone();
        stores.phases.write().unwrap().insert(phase_id.clone(), phase);

        // Add work item
        let wi = Work::new(phase_id.clone(), "WI 1".into(), "wi desc".into());
        stores.works.write().unwrap().insert(wi.id.clone(), wi);

        // Add tick
        let tick = Tick::new(1);
        stores.ticks.write().unwrap().insert(tick.id.clone(), tick);

        let agent_log = test_agent_logger(&dir);
        let summary = build_state_summary(&stores, &agent_log);
        assert!(summary.contains("### Plans"));
        assert!(summary.contains("### Specs"));
        assert!(summary.contains("### Phases"));
        assert!(summary.contains("### Works"));
        assert!(summary.contains("### Ticks"));
    }

    #[tokio::test]
    async fn test_coordinator_iteration_persists() {
        let dir = TestDir::new("loopr-coord-itpersist");
        let stores = test_stores(&dir);
        insert_test_goal(&stores);

        let config = CoordinatorConfig {
            active_interval_secs: 0,
            idle_interval_secs: 0,
            ..CoordinatorConfig::default()
        };

        // MockLlm: iterations 1,2 return Continue, iteration 3 returns NeedHelp to exit loop
        let mut agent = test_coordinator(
            &dir,
            &stores,
            vec![
                r#"[{"action": "create_learning", "content": "iter 1", "scope": "global", "source_id": "test"}]"#
                    .to_string(),
                r#"[{"action": "create_learning", "content": "iter 2", "scope": "global", "source_id": "test"}]"#
                    .to_string(),
                r#"[{"action": "need_help", "reason": "done testing"}]"#.to_string(),
            ],
            config,
        );

        // Insert the session as Running
        let mut session = agent.ctx.session.clone();
        let _ = session.transition_to(AgentStatus::Running);
        stores
            .agent_sessions
            .write()
            .unwrap()
            .insert(session.id.clone(), session);

        let _ = agent.run().await;

        // Session iteration should be 3 (need_help on iteration 3)
        assert_eq!(agent.ctx.session.iteration, 3);

        // The iteration should also be persisted in stores
        let stored_iteration = stores
            .agent_sessions
            .read()
            .unwrap()
            .get(&agent.ctx.session.id)
            .map(|s| s.iteration)
            .unwrap_or(0);
        assert_eq!(stored_iteration, 3, "iteration should be persisted in stores");
    }

    // --- infer_action_level tests ---

    #[test]
    fn test_infer_action_level_returns_none_for_done() {
        let action = AgentAction::Done {
            summary: "all done".into(),
        };
        assert!(infer_action_level(&action).is_none());
    }

    #[test]
    fn test_infer_action_level_returns_none_for_create_learning() {
        let action = AgentAction::CreateLearning {
            content: "learned something".into(),
            scope: "global".into(),
            source_id: "test".into(),
            applicable_roles: None,
            resource_tags: None,
        };
        assert!(infer_action_level(&action).is_none());
    }

    // --- build_generation_footer tests ---

    #[test]
    fn test_build_generation_footer_generation_needed() {
        crate::prompts::init_defaults();
        // No plans exist → generation is needed at Plan level
        let dir = TestDir::new("loopr-coord-genftr");
        let stores = test_stores(&dir);

        let agent_log = test_agent_logger(&dir);
        let footer = build_generation_footer(&stores, "Build an auth system", 3, None, &agent_log, None, None, None);
        assert!(footer.is_some(), "should return generation footer when no plan exists");
        let text = footer.unwrap();
        // The Plan-level generation prompt should mention creating a plan
        assert!(
            text.contains("plan") || text.contains("Plan"),
            "footer should mention plan generation"
        );
    }

    #[test]
    fn test_build_generation_footer_validation_cap_reached() {
        use crate::domain::validation::{ValidationReport, ValidationVerdict};

        let dir = TestDir::new("loopr-coord-valcap");
        let stores = test_stores_with_validator(&dir);

        // Create a Draft plan so determine_generation_level returns None
        // (Draft exists, so no generation needed)
        let plan = Plan::new("Draft Plan".into(), "desc".into(), "crit".into());
        let plan_id = plan.id.clone();
        stores.plans.write().unwrap().insert(plan_id.clone(), plan);

        // Create 3 failed validation reports for this plan (cap = 3)
        let store_lock = stores.store.as_ref().unwrap();
        {
            let mut store = store_lock.lock().unwrap();
            for _ in 0..3 {
                let report = ValidationReport::new(
                    "plans".into(),
                    plan_id.clone(),
                    ValidationVerdict::Fail,
                    vec![],
                    "Failed criteria".into(),
                    "test-model".into(),
                );
                store.create(report).unwrap();
            }
        }

        let agent_log = test_agent_logger(&dir);
        let footer = build_generation_footer(&stores, "Build auth", 3, None, &agent_log, None, None, None);
        assert!(footer.is_some(), "should return footer when validation cap is reached");
        let text = footer.unwrap();
        assert!(text.contains("need_help"), "should signal need_help when cap reached");
    }

    #[test]
    fn test_build_generation_footer_draft_needs_regen() {
        use crate::domain::validation::{ValidationReport, ValidationVerdict};

        let dir = TestDir::new("loopr-coord-regen");
        let stores = test_stores_with_validator(&dir);

        // Create a Draft plan
        let plan = Plan::new("Draft Plan".into(), "desc".into(), "crit".into());
        let plan_id = plan.id.clone();
        stores.plans.write().unwrap().insert(plan_id.clone(), plan);

        // Create 1 failed validation report (below cap of 3)
        let store_lock = stores.store.as_ref().unwrap();
        {
            let mut store = store_lock.lock().unwrap();
            let report = ValidationReport::new(
                "plans".into(),
                plan_id.clone(),
                ValidationVerdict::Fail,
                vec![],
                "Missing acceptance criteria".into(),
                "test-model".into(),
            );
            store.create(report).unwrap();
        }

        let agent_log = test_agent_logger(&dir);
        let footer = build_generation_footer(&stores, "Build auth", 3, None, &agent_log, None, None, None);
        assert!(
            footer.is_some(),
            "should return regen footer when draft has failures below cap"
        );
        let text = footer.unwrap();
        assert!(
            text.contains("Re-generation Required"),
            "should contain re-generation header"
        );
    }

    // --- check_phase_completion tests ---

    #[test]
    fn test_check_phase_completion_no_active_plan() {
        let dir = TestDir::new("loopr-coord-noplan");
        let stores = test_stores(&dir);

        // No plans at all → should return empty
        let completed = check_phase_completion(&stores);
        assert!(completed.is_empty());
    }

    #[test]
    fn test_check_phase_completion_all_done() {
        let dir = TestDir::new("loopr-coord-phase-done");
        std::process::Command::new("git")
            .args(["init"])
            .current_dir(&*dir)
            .output()
            .unwrap();
        let stores = test_stores(&dir);

        // Create an Active plan
        let mut plan = Plan::new("Test Plan".into(), "desc".into(), "criteria".into());
        plan.status = HierarchyStatus::Active;
        let plan_id = plan.id.clone();
        stores.plans.write().unwrap().insert(plan_id.clone(), plan);

        // Create an Active spec
        let mut spec = Spec::new(plan_id.clone(), "Test Spec".into(), "desc".into());
        spec.status = HierarchyStatus::Active;
        let spec_id = spec.id.clone();
        stores.specs.write().unwrap().insert(spec_id.clone(), spec);

        // Create an Active phase
        let mut phase = Phase::new(spec_id.clone(), "Phase 1".into(), "desc".into(), 1);
        phase.status = HierarchyStatus::Active;
        let phase_id = phase.id.clone();
        stores.phases.write().unwrap().insert(phase_id.clone(), phase);

        // Create works that are all Done
        let mut w1 = Work::new(phase_id.clone(), "Work 1".into(), "desc".into());
        w1.status = WorkStatus::Done;
        stores.works.write().unwrap().insert(w1.id.clone(), w1);

        let mut w2 = Work::new(phase_id.clone(), "Work 2".into(), "desc".into());
        w2.status = WorkStatus::Done;
        stores.works.write().unwrap().insert(w2.id.clone(), w2);

        let completed = check_phase_completion(&stores);
        assert_eq!(completed.len(), 1);
        assert!(completed[0].contains("Phase 1"));
    }

    #[test]
    fn test_check_phase_completion_partial() {
        let dir = TestDir::new("loopr-coord-phase-partial");
        std::process::Command::new("git")
            .args(["init"])
            .current_dir(&*dir)
            .output()
            .unwrap();
        let stores = test_stores(&dir);

        // Active plan → Active spec → Active phase
        let mut plan = Plan::new("Test Plan".into(), "desc".into(), "criteria".into());
        plan.status = HierarchyStatus::Active;
        let plan_id = plan.id.clone();
        stores.plans.write().unwrap().insert(plan_id.clone(), plan);

        let mut spec = Spec::new(plan_id.clone(), "Test Spec".into(), "desc".into());
        spec.status = HierarchyStatus::Active;
        let spec_id = spec.id.clone();
        stores.specs.write().unwrap().insert(spec_id.clone(), spec);

        let mut phase = Phase::new(spec_id.clone(), "Phase 1".into(), "desc".into(), 1);
        phase.status = HierarchyStatus::Active;
        let phase_id = phase.id.clone();
        stores.phases.write().unwrap().insert(phase_id.clone(), phase);

        // One work Done, one still InProgress
        let mut w1 = Work::new(phase_id.clone(), "Work 1".into(), "desc".into());
        w1.status = WorkStatus::Done;
        stores.works.write().unwrap().insert(w1.id.clone(), w1);

        let mut w2 = Work::new(phase_id.clone(), "Work 2".into(), "desc".into());
        w2.status = WorkStatus::InProgress;
        stores.works.write().unwrap().insert(w2.id.clone(), w2);

        let completed = check_phase_completion(&stores);
        assert!(completed.is_empty());
    }

    #[test]
    fn test_check_phase_completion_no_works() {
        let dir = TestDir::new("loopr-coord-phase-noworks");
        std::process::Command::new("git")
            .args(["init"])
            .current_dir(&*dir)
            .output()
            .unwrap();
        let stores = test_stores(&dir);

        // Active plan → Active spec → Active phase, but NO works
        let mut plan = Plan::new("Test Plan".into(), "desc".into(), "criteria".into());
        plan.status = HierarchyStatus::Active;
        let plan_id = plan.id.clone();
        stores.plans.write().unwrap().insert(plan_id.clone(), plan);

        let mut spec = Spec::new(plan_id.clone(), "Test Spec".into(), "desc".into());
        spec.status = HierarchyStatus::Active;
        let spec_id = spec.id.clone();
        stores.specs.write().unwrap().insert(spec_id.clone(), spec);

        let mut phase = Phase::new(spec_id.clone(), "Phase 1".into(), "desc".into(), 1);
        phase.status = HierarchyStatus::Active;
        let phase_id = phase.id.clone();
        stores.phases.write().unwrap().insert(phase_id.clone(), phase);

        // is_phase_complete returns false when there are no works
        let completed = check_phase_completion(&stores);
        assert!(completed.is_empty());
    }

    #[test]
    fn test_check_phase_completion_multiple_phases() {
        let dir = TestDir::new("loopr-coord-phase-multi");
        std::process::Command::new("git")
            .args(["init"])
            .current_dir(&*dir)
            .output()
            .unwrap();
        let stores = test_stores(&dir);

        // Active plan → Active spec → 2 Active phases
        let mut plan = Plan::new("Test Plan".into(), "desc".into(), "criteria".into());
        plan.status = HierarchyStatus::Active;
        let plan_id = plan.id.clone();
        stores.plans.write().unwrap().insert(plan_id.clone(), plan);

        let mut spec = Spec::new(plan_id.clone(), "Test Spec".into(), "desc".into());
        spec.status = HierarchyStatus::Active;
        let spec_id = spec.id.clone();
        stores.specs.write().unwrap().insert(spec_id.clone(), spec);

        // Phase 1: all works Done
        let mut phase1 = Phase::new(spec_id.clone(), "Phase 1".into(), "desc".into(), 1);
        phase1.status = HierarchyStatus::Active;
        let phase1_id = phase1.id.clone();
        stores.phases.write().unwrap().insert(phase1_id.clone(), phase1);

        let mut w1 = Work::new(phase1_id.clone(), "Work 1".into(), "desc".into());
        w1.status = WorkStatus::Done;
        stores.works.write().unwrap().insert(w1.id.clone(), w1);

        // Phase 2: one work still InProgress
        let mut phase2 = Phase::new(spec_id.clone(), "Phase 2".into(), "desc".into(), 2);
        phase2.status = HierarchyStatus::Active;
        let phase2_id = phase2.id.clone();
        stores.phases.write().unwrap().insert(phase2_id.clone(), phase2);

        let mut w2 = Work::new(phase2_id.clone(), "Work 2".into(), "desc".into());
        w2.status = WorkStatus::InProgress;
        stores.works.write().unwrap().insert(w2.id.clone(), w2);

        let completed = check_phase_completion(&stores);
        assert_eq!(completed.len(), 1);
        assert!(completed[0].contains("Phase 1"));
        assert!(!completed.iter().any(|c| c.contains("Phase 2")));
    }

    // --- multi-level action filter tests ---

    #[tokio::test]
    async fn test_coordinator_iteration_filters_multi_level_actions() {
        crate::prompts::init_defaults();
        let dir = TestDir::new("loopr-coord-multilevel");
        let stores = test_stores(&dir);

        let agent = test_coordinator(
            &dir,
            &stores,
            vec![r#"[
                {"action": "create_plan", "title": "Auth", "description": "Add auth", "acceptance_criteria": "Tests pass"},
                {"action": "create_spec", "plan_id": "plan-1", "title": "Spec1", "description": "desc"}
            ]"#.to_string()],
            CoordinatorConfig::default(),
        );

        let outcome = agent
            .run_iteration(
                &mut CoordinatorState::new("test-goal".to_string(), InterviewMode::Interactive),
                &mut Lifeguard::new(),
            )
            .await
            .unwrap();

        // Should still produce a Continue (plan created), but spec should have been filtered
        assert!(matches!(outcome, IterationOutcome::Continue(_)));

        // Only one plan should exist (the spec with nonexistent plan_id would have been filtered)
        let plans = stores.plans.read().unwrap();
        assert_eq!(plans.len(), 1, "only the plan-level action should have executed");
    }

    #[tokio::test]
    async fn test_coordinator_iteration_empty_after_filter() {
        crate::prompts::init_defaults();
        let dir = TestDir::new("loopr-coord-emptyfilter");
        let stores = test_stores(&dir);

        let agent = test_coordinator(
            &dir,
            &stores,
            vec![r#"[{"action": "done", "summary": "Finished planning"}]"#.to_string()],
            CoordinatorConfig::default(),
        );

        let outcome = agent
            .run_iteration(
                &mut CoordinatorState::new("test-goal".to_string(), InterviewMode::Interactive),
                &mut Lifeguard::new(),
            )
            .await
            .unwrap();

        assert!(
            matches!(outcome, IterationOutcome::Done(ref s) if s.contains("Finished planning")),
            "done action should yield Done outcome"
        );
    }

    // --- format_action_summary additional coverage ---

    #[test]
    fn test_format_action_summary_document_validated_empty_issues() {
        // Exercises the empty-issues branch with a different verdict
        let result = ActionResult::DocumentValidated {
            verdict: "fail".into(),
            summary: "Criteria not met".into(),
            issues: vec![],
        };
        let summary = format_action_summary(&result);
        assert!(summary.contains("validated: fail"), "should contain the fail verdict");
        assert!(summary.contains("Criteria not met"), "should contain the summary");
        // Empty issues → no "(N issues)" suffix
        assert!(
            !summary.contains("issues"),
            "should not mention issues count when issues is empty"
        );
    }

    // --- FSM state management tests ---

    #[test]
    fn test_load_or_create_coordinator_state_no_goal() {
        let dir = TestDir::new("loopr-coord-fsm-nogoal");
        let stores = test_stores(&dir);

        let result = load_or_create_coordinator_state(&stores);
        assert!(result.is_none());
    }

    #[test]
    fn test_load_or_create_coordinator_state_with_goal() {
        let dir = TestDir::new("loopr-coord-fsm-goal");
        let stores = test_stores(&dir);

        let goal = crate::domain::coordinator_goal::CoordinatorGoal::new("Build app".to_string());
        let goal_id = goal.id.clone();
        stores.coordinator_goals.write().unwrap().insert(goal_id.clone(), goal);

        let state = load_or_create_coordinator_state(&stores).unwrap();
        assert_eq!(state.goal_id, goal_id);
        assert_eq!(state.fsm_state, CoordinatorFsmState::Interviewing);
    }

    #[test]
    fn test_load_or_create_coordinator_state_resumes_existing() {
        let dir = TestDir::new("loopr-coord-fsm-resume");
        let stores = test_stores(&dir);

        let goal = crate::domain::coordinator_goal::CoordinatorGoal::new("Build app".to_string());
        let goal_id = goal.id.clone();
        stores.coordinator_goals.write().unwrap().insert(goal_id.clone(), goal);

        // Create an existing state in Executing
        let mut existing = CoordinatorState::new(goal_id.clone(), InterviewMode::Interactive);
        existing.transition_to(CoordinatorFsmState::Executing);
        existing.current_phase_id = Some("phase-1".to_string());
        let existing_id = existing.id.clone();
        stores
            .coordinator_states
            .write()
            .unwrap()
            .insert(existing_id.clone(), existing);

        let state = load_or_create_coordinator_state(&stores).unwrap();
        assert_eq!(state.id, existing_id);
        assert_eq!(state.fsm_state, CoordinatorFsmState::Executing);
        assert_eq!(state.current_phase_id.as_deref(), Some("phase-1"));
    }

    #[test]
    fn test_check_fsm_transition_planning_to_activate() {
        let dir = TestDir::new("loopr-coord-fsm-plan2act");
        let stores = test_stores(&dir);

        // Create Plan → Spec → Phase hierarchy (all Active)
        let mut plan = Plan::new("Plan".into(), "desc".into(), "crit".into());
        plan.status = HierarchyStatus::Active;
        let plan_id = plan.id.clone();
        stores.plans.write().unwrap().insert(plan_id.clone(), plan);

        let mut spec = Spec::new(plan_id.clone(), "Spec".into(), "desc".into());
        spec.status = HierarchyStatus::Active;
        let spec_id = spec.id.clone();
        stores.specs.write().unwrap().insert(spec_id.clone(), spec);

        let mut phase = Phase::new(spec_id.clone(), "Phase 1".into(), "desc".into(), 1);
        phase.status = HierarchyStatus::Active;
        stores.phases.write().unwrap().insert(phase.id.clone(), phase);

        let mut coord_state = CoordinatorState::new("goal-1".to_string(), InterviewMode::Interactive);
        coord_state.fsm_state = CoordinatorFsmState::Planning;
        let config = CoordinatorConfig::default();

        let transition = check_fsm_transition(&stores, &coord_state, &config);
        assert_eq!(transition, Some(CoordinatorFsmState::ActivatePhase));
    }

    #[test]
    fn test_check_fsm_transition_executing_to_phase_gate() {
        let dir = TestDir::new("loopr-coord-fsm-exec2gate");
        let stores = test_stores(&dir);

        let phase = Phase::new("spec-1".into(), "Phase 1".into(), "desc".into(), 1);
        let phase_id = phase.id.clone();
        stores.phases.write().unwrap().insert(phase_id.clone(), phase);

        // Create a Done work item in the phase
        let mut wi = Work::new(phase_id.clone(), "WI 1".into(), "desc".into());
        wi.status = WorkStatus::Done;
        stores.works.write().unwrap().insert(wi.id.clone(), wi);

        let mut coord_state = CoordinatorState::new("goal-1".to_string(), InterviewMode::Interactive);
        coord_state.fsm_state = CoordinatorFsmState::Executing;
        coord_state.current_phase_id = Some(phase_id);
        let config = CoordinatorConfig::default();

        let transition = check_fsm_transition(&stores, &coord_state, &config);
        assert_eq!(transition, Some(CoordinatorFsmState::PhaseGate));
    }

    #[test]
    fn test_check_fsm_transition_phase_gate_to_goal_complete() {
        let dir = TestDir::new("loopr-coord-fsm-gate2done");
        let stores = test_stores(&dir);

        // No more phases to activate
        let mut coord_state = CoordinatorState::new("goal-1".to_string(), InterviewMode::Interactive);
        coord_state.fsm_state = CoordinatorFsmState::PhaseGate;
        let config = CoordinatorConfig::default();

        let transition = check_fsm_transition(&stores, &coord_state, &config);
        assert_eq!(transition, Some(CoordinatorFsmState::GoalComplete));
    }

    #[test]
    fn test_persist_coordinator_state() {
        let dir = TestDir::new("loopr-coord-fsm-persist");
        let stores = test_stores(&dir);

        let mut state = CoordinatorState::new("goal-1".to_string(), InterviewMode::Interactive);
        state.transition_to(CoordinatorFsmState::Executing);
        let state_id = state.id.clone();

        persist_coordinator_state(&stores, &state);

        let stored = stores.coordinator_states.read().unwrap();
        let retrieved = stored.get(&state_id).unwrap();
        assert_eq!(retrieved.fsm_state, CoordinatorFsmState::Executing);
    }

    #[test]
    fn test_build_phase_status_no_phase() {
        let dir = TestDir::new("loopr-coord-fsm-nophase");
        let stores = test_stores(&dir);

        let coord_state = CoordinatorState::new("goal-1".to_string(), InterviewMode::Interactive);
        let status = build_phase_status(&stores, &coord_state);
        assert!(status.contains("No active phase"));
    }

    #[test]
    fn test_build_phase_status_with_works() {
        let dir = TestDir::new("loopr-coord-fsm-phstatus");
        let stores = test_stores(&dir);

        let phase = Phase::new("spec-1".into(), "Build Phase".into(), "desc".into(), 1);
        let phase_id = phase.id.clone();
        stores.phases.write().unwrap().insert(phase_id.clone(), phase);

        let wi1 = Work::new(phase_id.clone(), "WI 1".into(), "desc".into());
        let mut wi2 = Work::new(phase_id.clone(), "WI 2".into(), "desc".into());
        wi2.status = WorkStatus::Done;
        stores.works.write().unwrap().insert(wi1.id.clone(), wi1);
        stores.works.write().unwrap().insert(wi2.id.clone(), wi2);

        let mut coord_state = CoordinatorState::new("goal-1".to_string(), InterviewMode::Interactive);
        coord_state.current_phase_id = Some(phase_id);

        let status = build_phase_status(&stores, &coord_state);
        assert!(status.contains("Build Phase"));
        assert!(status.contains("2 total"));
        // Verify grouping: actionable vs terminal sections
        assert!(status.contains("Actionable Works (eligible for assignment)"));
        assert!(status.contains("Terminal Works (COMPLETED - do NOT assign agents to these)"));
        assert!(status.contains("1 actionable, 1 terminal"));
    }

    #[test]
    fn test_build_phase_status_all_terminal() {
        let dir = TestDir::new("loopr-coord-fsm-allterm");
        let stores = test_stores(&dir);

        let phase = Phase::new("spec-1".into(), "Done Phase".into(), "desc".into(), 1);
        let phase_id = phase.id.clone();
        stores.phases.write().unwrap().insert(phase_id.clone(), phase);

        let mut wi1 = Work::new(phase_id.clone(), "WI 1".into(), "desc".into());
        wi1.status = WorkStatus::Done;
        let mut wi2 = Work::new(phase_id.clone(), "WI 2".into(), "desc".into());
        wi2.status = WorkStatus::Abandoned;
        stores.works.write().unwrap().insert(wi1.id.clone(), wi1);
        stores.works.write().unwrap().insert(wi2.id.clone(), wi2);

        let mut coord_state = CoordinatorState::new("goal-1".to_string(), InterviewMode::Interactive);
        coord_state.current_phase_id = Some(phase_id);

        let status = build_phase_status(&stores, &coord_state);
        assert!(status.contains("0 actionable, 2 terminal"));
        assert!(status.contains("None - all works are in a terminal state"));
    }

    // =========================================================================
    // Exhaustive FSM transition matrix tests
    // =========================================================================

    // --- Planning state: stays Planning ---

    #[test]
    fn test_fsm_planning_stays_when_no_plan() {
        let dir = TestDir::new("loopr-fsm-plannoplan");
        let stores = test_stores(&dir);

        let coord_state = CoordinatorState::new("goal-1".to_string(), InterviewMode::Interactive);
        let config = CoordinatorConfig::default();
        assert_eq!(check_fsm_transition(&stores, &coord_state, &config), None);
    }

    #[test]
    fn test_fsm_planning_stays_when_plan_but_no_spec() {
        let dir = TestDir::new("loopr-fsm-plannospec");
        let stores = test_stores(&dir);

        let mut plan = Plan::new("P".into(), "d".into(), "c".into());
        plan.status = HierarchyStatus::Active;
        stores.plans.write().unwrap().insert(plan.id.clone(), plan);

        let coord_state = CoordinatorState::new("goal-1".to_string(), InterviewMode::Interactive);
        let config = CoordinatorConfig::default();
        assert_eq!(check_fsm_transition(&stores, &coord_state, &config), None);
    }

    #[test]
    fn test_fsm_planning_stays_when_plan_spec_but_no_phases() {
        let dir = TestDir::new("loopr-fsm-plannophase");
        let stores = test_stores(&dir);

        let mut plan = Plan::new("P".into(), "d".into(), "c".into());
        plan.status = HierarchyStatus::Active;
        let plan_id = plan.id.clone();
        stores.plans.write().unwrap().insert(plan_id.clone(), plan);

        let mut spec = Spec::new(plan_id, "S".into(), "d".into());
        spec.status = HierarchyStatus::Active;
        stores.specs.write().unwrap().insert(spec.id.clone(), spec);

        let coord_state = CoordinatorState::new("goal-1".to_string(), InterviewMode::Interactive);
        let config = CoordinatorConfig::default();
        assert_eq!(check_fsm_transition(&stores, &coord_state, &config), None);
    }

    #[test]
    fn test_fsm_planning_stays_when_plan_is_draft() {
        let dir = TestDir::new("loopr-fsm-plandraft");
        let stores = test_stores(&dir);

        // Plan exists but is Draft, not Active
        let plan = Plan::new("P".into(), "d".into(), "c".into());
        stores.plans.write().unwrap().insert(plan.id.clone(), plan);

        let coord_state = CoordinatorState::new("goal-1".to_string(), InterviewMode::Interactive);
        let config = CoordinatorConfig::default();
        assert_eq!(check_fsm_transition(&stores, &coord_state, &config), None);
    }

    // --- ActivatePhase → Executing ---

    #[test]
    fn test_fsm_activate_phase_to_executing_when_wis_exist() {
        let dir = TestDir::new("loopr-fsm-act2exec");
        let stores = test_stores(&dir);

        let phase = Phase::new("spec-1".into(), "Phase 1".into(), "desc".into(), 1);
        let phase_id = phase.id.clone();
        stores.phases.write().unwrap().insert(phase_id.clone(), phase);

        let wi = Work::new(phase_id.clone(), "WI 1".into(), "desc".into());
        stores.works.write().unwrap().insert(wi.id.clone(), wi);

        let mut coord_state = CoordinatorState::new("goal-1".to_string(), InterviewMode::Interactive);
        coord_state.fsm_state = CoordinatorFsmState::ActivatePhase;
        coord_state.current_phase_id = Some(phase_id);
        let config = CoordinatorConfig::default();

        assert_eq!(
            check_fsm_transition(&stores, &coord_state, &config),
            Some(CoordinatorFsmState::Executing)
        );
    }

    #[test]
    fn test_fsm_activate_phase_stays_when_no_phase_id() {
        let dir = TestDir::new("loopr-fsm-actnopid");
        let stores = test_stores(&dir);

        let mut coord_state = CoordinatorState::new("goal-1".to_string(), InterviewMode::Interactive);
        coord_state.fsm_state = CoordinatorFsmState::ActivatePhase;
        // current_phase_id is None
        let config = CoordinatorConfig::default();

        assert_eq!(check_fsm_transition(&stores, &coord_state, &config), None);
    }

    #[test]
    fn test_fsm_activate_phase_stays_when_no_wis() {
        let dir = TestDir::new("loopr-fsm-actnowi");
        let stores = test_stores(&dir);

        let phase = Phase::new("spec-1".into(), "Phase 1".into(), "desc".into(), 1);
        let phase_id = phase.id.clone();
        stores.phases.write().unwrap().insert(phase_id.clone(), phase);

        let mut coord_state = CoordinatorState::new("goal-1".to_string(), InterviewMode::Interactive);
        coord_state.fsm_state = CoordinatorFsmState::ActivatePhase;
        coord_state.current_phase_id = Some(phase_id);
        let config = CoordinatorConfig::default();

        // Phase exists but no WIs yet
        assert_eq!(check_fsm_transition(&stores, &coord_state, &config), None);
    }

    // --- Executing state: all branches ---

    #[test]
    fn test_fsm_executing_stays_when_wis_in_progress() {
        let dir = TestDir::new("loopr-fsm-execwip");
        let stores = test_stores(&dir);

        let phase = Phase::new("spec-1".into(), "Phase 1".into(), "desc".into(), 1);
        let phase_id = phase.id.clone();
        stores.phases.write().unwrap().insert(phase_id.clone(), phase);

        let mut wi = Work::new(phase_id.clone(), "WI 1".into(), "desc".into());
        wi.status = WorkStatus::InProgress;
        stores.works.write().unwrap().insert(wi.id.clone(), wi);

        let mut coord_state = CoordinatorState::new("goal-1".to_string(), InterviewMode::Interactive);
        coord_state.fsm_state = CoordinatorFsmState::Executing;
        coord_state.current_phase_id = Some(phase_id);
        let config = CoordinatorConfig::default();

        assert_eq!(check_fsm_transition(&stores, &coord_state, &config), None);
    }

    #[test]
    fn test_fsm_executing_to_phase_gate_with_mixed_done_abandoned() {
        let dir = TestDir::new("loopr-fsm-execmix");
        let stores = test_stores(&dir);

        let phase = Phase::new("spec-1".into(), "Phase 1".into(), "desc".into(), 1);
        let phase_id = phase.id.clone();
        stores.phases.write().unwrap().insert(phase_id.clone(), phase);

        let mut wi1 = Work::new(phase_id.clone(), "WI Done".into(), "desc".into());
        wi1.status = WorkStatus::Done;
        let mut wi2 = Work::new(phase_id.clone(), "WI Abandoned".into(), "desc".into());
        wi2.status = WorkStatus::Abandoned;
        stores.works.write().unwrap().insert(wi1.id.clone(), wi1);
        stores.works.write().unwrap().insert(wi2.id.clone(), wi2);

        let mut coord_state = CoordinatorState::new("goal-1".to_string(), InterviewMode::Interactive);
        coord_state.fsm_state = CoordinatorFsmState::Executing;
        coord_state.current_phase_id = Some(phase_id);
        let config = CoordinatorConfig::default();

        assert_eq!(
            check_fsm_transition(&stores, &coord_state, &config),
            Some(CoordinatorFsmState::PhaseGate)
        );
    }

    #[test]
    fn test_fsm_executing_stays_when_partial_done() {
        let dir = TestDir::new("loopr-fsm-execpartial");
        let stores = test_stores(&dir);

        let phase = Phase::new("spec-1".into(), "Phase 1".into(), "desc".into(), 1);
        let phase_id = phase.id.clone();
        stores.phases.write().unwrap().insert(phase_id.clone(), phase);

        let mut wi1 = Work::new(phase_id.clone(), "WI Done".into(), "desc".into());
        wi1.status = WorkStatus::Done;
        let mut wi2 = Work::new(phase_id.clone(), "WI Ready".into(), "desc".into());
        wi2.status = WorkStatus::Ready;
        stores.works.write().unwrap().insert(wi1.id.clone(), wi1);
        stores.works.write().unwrap().insert(wi2.id.clone(), wi2);

        let mut coord_state = CoordinatorState::new("goal-1".to_string(), InterviewMode::Interactive);
        coord_state.fsm_state = CoordinatorFsmState::Executing;
        coord_state.current_phase_id = Some(phase_id);
        let config = CoordinatorConfig::default();

        assert_eq!(check_fsm_transition(&stores, &coord_state, &config), None);
    }

    #[test]
    fn test_fsm_executing_to_phase_gate_on_zero_wis() {
        let dir = TestDir::new("loopr-fsm-exec0wi");
        let stores = test_stores(&dir);

        let phase = Phase::new("spec-1".into(), "Phase 1".into(), "desc".into(), 1);
        let phase_id = phase.id.clone();
        stores.phases.write().unwrap().insert(phase_id.clone(), phase);
        // No work items!

        let mut coord_state = CoordinatorState::new("goal-1".to_string(), InterviewMode::Interactive);
        coord_state.fsm_state = CoordinatorFsmState::Executing;
        coord_state.current_phase_id = Some(phase_id);
        let config = CoordinatorConfig::default();

        // BUG FIX: 0 WIs should transition to PhaseGate, not stay stuck
        assert_eq!(
            check_fsm_transition(&stores, &coord_state, &config),
            Some(CoordinatorFsmState::PhaseGate)
        );
    }

    #[test]
    fn test_fsm_executing_to_phase_gate_on_phase_timeout() {
        let dir = TestDir::new("loopr-fsm-exectimeout");
        let stores = test_stores(&dir);

        let phase = Phase::new("spec-1".into(), "Phase 1".into(), "desc".into(), 1);
        let phase_id = phase.id.clone();
        stores.phases.write().unwrap().insert(phase_id.clone(), phase);

        // WI in progress — would normally stay Executing
        let mut wi = Work::new(phase_id.clone(), "WI".into(), "desc".into());
        wi.status = WorkStatus::InProgress;
        stores.works.write().unwrap().insert(wi.id.clone(), wi);

        let mut coord_state = CoordinatorState::new("goal-1".to_string(), InterviewMode::Interactive);
        coord_state.fsm_state = CoordinatorFsmState::Executing;
        coord_state.current_phase_id = Some(phase_id);
        // Set phase_activated_at far in the past
        coord_state.phase_activated_at = Some(crate::id::now_millis() - 7_200_000); // 2 hours ago

        let config = CoordinatorConfig {
            phase_timeout_secs: 3600, // 1 hour
            ..CoordinatorConfig::default()
        };

        assert_eq!(
            check_fsm_transition(&stores, &coord_state, &config),
            Some(CoordinatorFsmState::PhaseGate)
        );
    }

    #[test]
    fn test_fsm_executing_to_goal_complete_on_goal_timeout() {
        let dir = TestDir::new("loopr-fsm-execgoalto");
        let stores = test_stores(&dir);

        let phase = Phase::new("spec-1".into(), "Phase 1".into(), "desc".into(), 1);
        let phase_id = phase.id.clone();
        stores.phases.write().unwrap().insert(phase_id.clone(), phase);

        let mut wi = Work::new(phase_id.clone(), "WI".into(), "desc".into());
        wi.status = WorkStatus::InProgress;
        stores.works.write().unwrap().insert(wi.id.clone(), wi);

        let mut coord_state = CoordinatorState::new("goal-1".to_string(), InterviewMode::Interactive);
        coord_state.fsm_state = CoordinatorFsmState::Executing;
        coord_state.current_phase_id = Some(phase_id);
        // Goal started 5 hours ago, timeout is 4 hours
        coord_state.goal_started_at = crate::id::now_millis() - 18_000_000;

        let config = CoordinatorConfig {
            goal_timeout_secs: 14400, // 4 hours
            ..CoordinatorConfig::default()
        };

        assert_eq!(
            check_fsm_transition(&stores, &coord_state, &config),
            Some(CoordinatorFsmState::GoalComplete)
        );
    }

    #[test]
    fn test_fsm_executing_no_current_phase_stays() {
        let dir = TestDir::new("loopr-fsm-execnophase");
        let stores = test_stores(&dir);

        let mut coord_state = CoordinatorState::new("goal-1".to_string(), InterviewMode::Interactive);
        coord_state.fsm_state = CoordinatorFsmState::Executing;
        // current_phase_id is None — shouldn't happen normally, but shouldn't panic
        let config = CoordinatorConfig::default();

        assert_eq!(check_fsm_transition(&stores, &coord_state, &config), None);
    }

    // --- PhaseGate → ActivatePhase (more phases) ---

    #[test]
    fn test_fsm_phase_gate_to_activate_when_more_phases() {
        let dir = TestDir::new("loopr-fsm-gate2act");
        let stores = test_stores(&dir);

        let mut plan = Plan::new("P".into(), "d".into(), "c".into());
        plan.status = HierarchyStatus::Active;
        let plan_id = plan.id.clone();
        stores.plans.write().unwrap().insert(plan_id.clone(), plan);

        let mut spec = Spec::new(plan_id, "S".into(), "d".into());
        spec.status = HierarchyStatus::Active;
        let spec_id = spec.id.clone();
        stores.specs.write().unwrap().insert(spec_id.clone(), spec);

        let mut phase1 = Phase::new(spec_id.clone(), "Phase 1".into(), "desc".into(), 1);
        phase1.status = HierarchyStatus::Active;
        let phase1_id = phase1.id.clone();
        stores.phases.write().unwrap().insert(phase1_id.clone(), phase1);

        let mut phase2 = Phase::new(spec_id, "Phase 2".into(), "desc".into(), 2);
        phase2.status = HierarchyStatus::Active;
        stores.phases.write().unwrap().insert(phase2.id.clone(), phase2);

        let mut coord_state = CoordinatorState::new("goal-1".to_string(), InterviewMode::Interactive);
        coord_state.fsm_state = CoordinatorFsmState::PhaseGate;
        coord_state.current_phase_id = Some(phase1_id.clone());
        coord_state.phases_completed.push(phase1_id); // Phase 1 is done
        let config = CoordinatorConfig::default();

        assert_eq!(
            check_fsm_transition(&stores, &coord_state, &config),
            Some(CoordinatorFsmState::ActivatePhase)
        );
    }

    // --- GoalComplete is terminal ---

    #[test]
    fn test_fsm_goal_complete_returns_none() {
        let dir = TestDir::new("loopr-fsm-goalnone");
        let stores = test_stores(&dir);

        let mut coord_state = CoordinatorState::new("goal-1".to_string(), InterviewMode::Interactive);
        coord_state.fsm_state = CoordinatorFsmState::GoalComplete;
        let config = CoordinatorConfig::default();

        assert_eq!(check_fsm_transition(&stores, &coord_state, &config), None);
    }

    // --- Phase timeout vs goal timeout priority ---

    #[test]
    fn test_fsm_phase_timeout_takes_priority_over_wi_check() {
        let dir = TestDir::new("loopr-fsm-phtopri");
        let stores = test_stores(&dir);

        let phase = Phase::new("spec-1".into(), "Phase 1".into(), "desc".into(), 1);
        let phase_id = phase.id.clone();
        stores.phases.write().unwrap().insert(phase_id.clone(), phase);

        // All WIs are Done — would normally trigger PhaseGate via WI check
        let mut wi = Work::new(phase_id.clone(), "WI".into(), "desc".into());
        wi.status = WorkStatus::Done;
        stores.works.write().unwrap().insert(wi.id.clone(), wi);

        let mut coord_state = CoordinatorState::new("goal-1".to_string(), InterviewMode::Interactive);
        coord_state.fsm_state = CoordinatorFsmState::Executing;
        coord_state.current_phase_id = Some(phase_id);
        coord_state.phase_activated_at = Some(crate::id::now_millis() - 7_200_000);

        let config = CoordinatorConfig {
            phase_timeout_secs: 3600,
            ..CoordinatorConfig::default()
        };

        // Phase timeout checked BEFORE WI terminal check — both produce PhaseGate
        assert_eq!(
            check_fsm_transition(&stores, &coord_state, &config),
            Some(CoordinatorFsmState::PhaseGate)
        );
    }

    #[test]
    fn test_fsm_goal_timeout_takes_priority_over_phase_timeout() {
        let dir = TestDir::new("loopr-fsm-goaltopri");
        let stores = test_stores(&dir);

        let phase = Phase::new("spec-1".into(), "Phase 1".into(), "desc".into(), 1);
        let phase_id = phase.id.clone();
        stores.phases.write().unwrap().insert(phase_id.clone(), phase);

        let mut wi = Work::new(phase_id.clone(), "WI".into(), "desc".into());
        wi.status = WorkStatus::InProgress;
        stores.works.write().unwrap().insert(wi.id.clone(), wi);

        let mut coord_state = CoordinatorState::new("goal-1".to_string(), InterviewMode::Interactive);
        coord_state.fsm_state = CoordinatorFsmState::Executing;
        coord_state.current_phase_id = Some(phase_id);
        // Both phase and goal timed out
        coord_state.phase_activated_at = Some(crate::id::now_millis() - 18_000_000);
        coord_state.goal_started_at = crate::id::now_millis() - 18_000_000;

        let config = CoordinatorConfig {
            phase_timeout_secs: 3600,
            goal_timeout_secs: 14400,
            ..CoordinatorConfig::default()
        };

        // Phase timeout fires first (checked before goal timeout in code)
        // Actually — phase timeout returns PhaseGate, which is checked before goal timeout
        let result = check_fsm_transition(&stores, &coord_state, &config);
        assert_eq!(result, Some(CoordinatorFsmState::PhaseGate));
    }

    // --- find_next_phase_to_activate ---

    #[test]
    fn test_find_next_phase_skips_completed() {
        let dir = TestDir::new("loopr-fsm-nextphskip");
        let stores = test_stores(&dir);

        let mut plan = Plan::new("P".into(), "d".into(), "c".into());
        plan.status = HierarchyStatus::Active;
        let plan_id = plan.id.clone();
        stores.plans.write().unwrap().insert(plan_id.clone(), plan);

        let mut spec = Spec::new(plan_id, "S".into(), "d".into());
        spec.status = HierarchyStatus::Active;
        let spec_id = spec.id.clone();
        stores.specs.write().unwrap().insert(spec_id.clone(), spec);

        let mut p1 = Phase::new(spec_id.clone(), "Phase 1".into(), "d".into(), 1);
        p1.status = HierarchyStatus::Active;
        let p1_id = p1.id.clone();
        stores.phases.write().unwrap().insert(p1_id.clone(), p1);

        let mut p2 = Phase::new(spec_id, "Phase 2".into(), "d".into(), 2);
        p2.status = HierarchyStatus::Active;
        let p2_id = p2.id.clone();
        stores.phases.write().unwrap().insert(p2_id.clone(), p2);

        let mut coord_state = CoordinatorState::new("goal-1".to_string(), InterviewMode::Interactive);
        coord_state.phases_completed.push(p1_id.clone());

        let result = find_next_phase_to_activate(&stores, &coord_state);
        assert!(result.is_some());
        let (id, title) = result.unwrap();
        assert_eq!(id, p2_id);
        assert_eq!(title, "Phase 2");
    }

    #[test]
    fn test_find_next_phase_returns_none_all_completed() {
        let dir = TestDir::new("loopr-fsm-nextphnone");
        let stores = test_stores(&dir);

        // No phases at all
        let coord_state = CoordinatorState::new("goal-1".to_string(), InterviewMode::Interactive);
        let result = find_next_phase_to_activate(&stores, &coord_state);
        assert!(result.is_none());
    }

    // --- Transition handler integration ---

    #[test]
    fn test_fsm_transition_handler_complete_phase_on_activate() {
        // When transitioning PhaseGate → ActivatePhase, the previous phase
        // should be completed (added to phases_completed).
        let dir = TestDir::new("loopr-fsm-thcomplete");
        let stores = test_stores(&dir);

        let mut plan = Plan::new("P".into(), "d".into(), "c".into());
        plan.status = HierarchyStatus::Active;
        let plan_id = plan.id.clone();
        stores.plans.write().unwrap().insert(plan_id.clone(), plan);

        let mut spec = Spec::new(plan_id, "S".into(), "d".into());
        spec.status = HierarchyStatus::Active;
        let spec_id = spec.id.clone();
        stores.specs.write().unwrap().insert(spec_id.clone(), spec);

        let mut p1 = Phase::new(spec_id.clone(), "Phase 1".into(), "d".into(), 1);
        p1.status = HierarchyStatus::Active;
        let p1_id = p1.id.clone();
        stores.phases.write().unwrap().insert(p1_id.clone(), p1);

        let mut p2 = Phase::new(spec_id, "Phase 2".into(), "d".into(), 2);
        p2.status = HierarchyStatus::Active;
        stores.phases.write().unwrap().insert(p2.id.clone(), p2);

        // Coordinator is in PhaseGate with phase 1 as current
        let mut coord_state = CoordinatorState::new("goal-1".to_string(), InterviewMode::Interactive);
        coord_state.fsm_state = CoordinatorFsmState::PhaseGate;
        coord_state.current_phase_id = Some(p1_id.clone());

        let config = CoordinatorConfig::default();

        // check_fsm_transition should return ActivatePhase
        let transition = check_fsm_transition(&stores, &coord_state, &config);
        assert_eq!(transition, Some(CoordinatorFsmState::ActivatePhase));

        // Simulate the transition handler: complete previous phase
        if coord_state.current_phase_id.is_some() {
            coord_state.complete_phase();
        }

        assert!(coord_state.phases_completed.contains(&p1_id));
        assert!(coord_state.current_phase_id.is_none());
    }

    // --- Fix #8: find_pending_draft_for_validation tests ---

    #[test]
    fn test_find_pending_draft_plan() {
        let dir = TestDir::new("loopr-coord-draft-plan");
        let stores = test_stores(&dir);

        // Insert a Draft plan
        let plan = Plan::new("Draft Plan".into(), "desc".into(), "criteria".into());
        // Plan::new creates with Draft status by default
        stores.plans.write().unwrap().insert(plan.id.clone(), plan);

        let result = find_pending_draft_for_validation(&stores);
        assert!(result.is_some(), "should find Draft plan");
        let (level, _id, title) = result.unwrap();
        assert_eq!(level, "Plan");
        assert_eq!(title, "Draft Plan");
    }

    #[test]
    fn test_find_pending_draft_none_when_active() {
        let dir = TestDir::new("loopr-coord-draft-none");
        let stores = test_stores(&dir);

        let mut plan = Plan::new("Active Plan".into(), "desc".into(), "criteria".into());
        plan.status = HierarchyStatus::Active;
        stores.plans.write().unwrap().insert(plan.id.clone(), plan);

        assert!(find_pending_draft_for_validation(&stores).is_none());
    }

    #[test]
    fn test_build_generation_footer_draft_validator_disabled_activates_directly() {
        crate::prompts::init_defaults();
        let dir = TestDir::new("loopr-coord-draftfooter");
        let stores = test_stores(&dir); // validator is None (disabled)

        // Create an Active plan
        let mut plan = Plan::new("Active Plan".into(), "desc".into(), "criteria".into());
        plan.status = HierarchyStatus::Active;
        let plan_id = plan.id.clone();
        stores.plans.write().unwrap().insert(plan_id.clone(), plan);

        // Create a Draft spec under the active plan (awaiting first validation)
        let spec = Spec::new(plan_id, "Draft Spec".into(), "desc".into());
        stores.specs.write().unwrap().insert(spec.id.clone(), spec);

        let agent_log = test_agent_logger(&dir);
        let footer = build_generation_footer(&stores, "Build a todo app", 3, None, &agent_log, None, None, None);
        assert!(footer.is_some(), "should emit footer for Draft activation");
        let footer_text = footer.unwrap();
        assert!(
            footer_text.contains("Draft"),
            "footer should mention Draft: {}",
            footer_text
        );
        assert!(
            footer_text.contains("Use Transition to move it from Draft to Active"),
            "footer should instruct direct activation when validator disabled: {}",
            footer_text
        );
        assert!(
            footer_text.contains("Validation is disabled"),
            "footer should note validator is disabled: {}",
            footer_text
        );
    }

    #[test]
    fn test_build_generation_footer_draft_validator_enabled_validates() {
        crate::prompts::init_defaults();
        let dir = TestDir::new("loopr-coord-draftval");
        let stores = test_stores_with_validator(&dir);

        // Create an Active plan
        let mut plan = Plan::new("Active Plan".into(), "desc".into(), "criteria".into());
        plan.status = HierarchyStatus::Active;
        let plan_id = plan.id.clone();
        stores.plans.write().unwrap().insert(plan_id.clone(), plan);

        // Create a Draft spec under the active plan
        let spec = Spec::new(plan_id, "Draft Spec".into(), "desc".into());
        stores.specs.write().unwrap().insert(spec.id.clone(), spec);

        let agent_log = test_agent_logger(&dir);
        let footer = build_generation_footer(&stores, "Build a todo app", 3, None, &agent_log, None, None, None);
        assert!(footer.is_some(), "should emit footer for Draft validation");
        let footer_text = footer.unwrap();
        assert!(
            footer_text.contains("ValidateDocument"),
            "footer should instruct validation when validator enabled: {}",
            footer_text
        );
    }

    // --- Fix #12: mark_phase_record_complete tests ---

    #[test]
    fn test_mark_phase_record_complete() {
        let dir = TestDir::new("loopr-coord-phasecomplete");
        let stores = test_stores(&dir);

        let mut phase = Phase::new("spec-1".into(), "Test Phase".into(), "desc".into(), 1);
        phase.status = HierarchyStatus::Active;
        let phase_id = phase.id.clone();
        stores.phases.write().unwrap().insert(phase_id.clone(), phase);

        let mut coord_state = CoordinatorState::new("goal-1".to_string(), InterviewMode::Interactive);
        coord_state.current_phase_id = Some(phase_id.clone());

        let agent_log = test_agent_logger(&dir);
        mark_phase_record_complete(&stores, &coord_state, &agent_log);

        let phases = stores.phases.read().unwrap();
        let updated = phases.get(&phase_id).unwrap();
        assert_eq!(updated.status, HierarchyStatus::Complete);
    }

    // --- Fix #2: resolve_batch_dependencies tests ---

    #[test]
    fn test_resolve_batch_deps_resolves_batch_0() {
        let dir = TestDir::new("loopr-coord-batch0");
        let agent_log = test_agent_logger(&dir);
        let batch_ids = vec!["wi-aaa".to_string(), "wi-bbb".to_string()];
        let action = AgentAction::CreateWork {
            phase_id: "phase-1".into(),
            title: "WI".into(),
            description: "d".into(),
            resource_tags: vec![],
            acceptance_criteria: vec![],
            dependencies: vec!["batch:0".to_string()],
        };
        let resolved = resolve_batch_dependencies(&action, &batch_ids, &agent_log);
        assert!(resolved.is_some());
        if let Some(AgentAction::CreateWork { dependencies, .. }) = resolved {
            assert_eq!(dependencies, vec!["wi-aaa".to_string()]);
        }
    }

    #[test]
    fn test_resolve_batch_deps_out_of_range() {
        let dir = TestDir::new("loopr-coord-batchoor");
        let agent_log = test_agent_logger(&dir);
        let batch_ids = vec!["wi-aaa".to_string()];
        let action = AgentAction::CreateWork {
            phase_id: "phase-1".into(),
            title: "WI".into(),
            description: "d".into(),
            resource_tags: vec![],
            acceptance_criteria: vec![],
            dependencies: vec!["batch:5".to_string()],
        };
        let resolved = resolve_batch_dependencies(&action, &batch_ids, &agent_log);
        assert!(resolved.is_some());
        // Out of range falls through — keeps the original "batch:5" string
        if let Some(AgentAction::CreateWork { dependencies, .. }) = resolved {
            assert_eq!(dependencies, vec!["batch:5".to_string()]);
        }
    }

    #[test]
    fn test_resolve_batch_deps_no_batch_refs() {
        let dir = TestDir::new("loopr-coord-batchnone");
        let agent_log = test_agent_logger(&dir);
        let batch_ids = vec!["wi-aaa".to_string()];
        let action = AgentAction::CreateWork {
            phase_id: "phase-1".into(),
            title: "WI".into(),
            description: "d".into(),
            resource_tags: vec![],
            acceptance_criteria: vec![],
            dependencies: vec!["wi-existing".to_string()],
        };
        let resolved = resolve_batch_dependencies(&action, &batch_ids, &agent_log);
        assert!(resolved.is_none(), "no batch refs should return None");
    }

    #[test]
    fn test_resolve_batch_deps_non_create_action() {
        let dir = TestDir::new("loopr-coord-batchnoncreate");
        let agent_log = test_agent_logger(&dir);
        let batch_ids = vec!["wi-aaa".to_string()];
        let action = AgentAction::Done { summary: "done".into() };
        let resolved = resolve_batch_dependencies(&action, &batch_ids, &agent_log);
        assert!(resolved.is_none(), "non-CreateWork should return None");
    }

    // --- Fix #6: prune_independent_deps tests ---

    #[test]
    fn test_prune_independent_deps_removes_disjoint() {
        let dir = TestDir::new("loopr-coord-prune1");
        let stores = test_stores(&dir);
        let agent_log = test_agent_logger(&dir);

        // Create two works with non-overlapping resource_tags — dep should be pruned
        let mut wi_a = Work::new("phase-1".into(), "Work A".into(), "d".into());
        wi_a.resource_tags = vec!["src/a.rs".into()];
        let a_id = wi_a.id.clone();

        let mut wi_b = Work::new("phase-1".into(), "Work B".into(), "d".into());
        wi_b.resource_tags = vec!["src/b.rs".into()];
        wi_b.dependencies = vec![a_id.clone()];
        let b_id = wi_b.id.clone();

        stores.works.write().unwrap().insert(a_id.clone(), wi_a);
        stores.works.write().unwrap().insert(b_id.clone(), wi_b);

        prune_independent_deps(&stores, &[a_id.clone(), b_id.clone()], &agent_log);

        let works = stores.works.read().unwrap();
        assert!(works[&b_id].dependencies.is_empty(), "disjoint dep should be pruned");
    }

    #[test]
    fn test_prune_independent_deps_keeps_overlapping() {
        let dir = TestDir::new("loopr-coord-prune2");
        let stores = test_stores(&dir);
        let agent_log = test_agent_logger(&dir);

        // Both works touch src/main.rs — dep should be kept
        let mut wi_a = Work::new("phase-1".into(), "Work A".into(), "d".into());
        wi_a.resource_tags = vec!["src/main.rs".into(), "src/a.rs".into()];
        let a_id = wi_a.id.clone();

        let mut wi_b = Work::new("phase-1".into(), "Work B".into(), "d".into());
        wi_b.resource_tags = vec!["src/main.rs".into(), "src/b.rs".into()];
        wi_b.dependencies = vec![a_id.clone()];
        let b_id = wi_b.id.clone();

        stores.works.write().unwrap().insert(a_id.clone(), wi_a);
        stores.works.write().unwrap().insert(b_id.clone(), wi_b);

        prune_independent_deps(&stores, &[a_id.clone(), b_id.clone()], &agent_log);

        let works = stores.works.read().unwrap();
        assert_eq!(works[&b_id].dependencies, vec![a_id], "overlapping dep should be kept");
    }

    #[test]
    fn test_prune_independent_deps_keeps_external() {
        let dir = TestDir::new("loopr-coord-prune3");
        let stores = test_stores(&dir);
        let agent_log = test_agent_logger(&dir);

        // wi_b depends on an external work (not in batch) — should be kept regardless
        let mut wi_a = Work::new("phase-1".into(), "Work A".into(), "d".into());
        wi_a.resource_tags = vec!["src/a.rs".into()];
        let a_id = wi_a.id.clone();

        let mut wi_b = Work::new("phase-1".into(), "Work B".into(), "d".into());
        wi_b.resource_tags = vec!["src/b.rs".into()];
        wi_b.dependencies = vec!["wi-external".to_string()];
        let b_id = wi_b.id.clone();

        stores.works.write().unwrap().insert(a_id.clone(), wi_a);
        stores.works.write().unwrap().insert(b_id.clone(), wi_b);

        prune_independent_deps(&stores, &[a_id, b_id.clone()], &agent_log);

        let works = stores.works.read().unwrap();
        assert_eq!(
            works[&b_id].dependencies,
            vec!["wi-external".to_string()],
            "external dep should be kept"
        );
    }

    #[test]
    fn test_prune_independent_deps_empty_batch() {
        let dir = TestDir::new("loopr-coord-prune4");
        let stores = test_stores(&dir);
        let agent_log = test_agent_logger(&dir);

        // Empty batch — no-op
        prune_independent_deps(&stores, &[], &agent_log);
    }

    // --- Fix #5: build_phase_status dependency info tests ---

    #[test]
    fn test_build_phase_status_shows_dependencies() {
        let dir = TestDir::new("loopr-coord-depstatus");
        let stores = test_stores(&dir);

        let phase = Phase::new("spec-1".into(), "Build Phase".into(), "d".into(), 1);
        let phase_id = phase.id.clone();
        stores.phases.write().unwrap().insert(phase_id.clone(), phase);

        let mut wi1 = Work::new(phase_id.clone(), "Setup".into(), "d".into());
        wi1.status = WorkStatus::Done;
        let wi1_id = wi1.id.clone();
        stores.works.write().unwrap().insert(wi1_id.clone(), wi1);

        let mut wi2 = Work::new(phase_id.clone(), "Build".into(), "d".into());
        wi2.dependencies = vec![wi1_id.clone()];
        let wi2_id = wi2.id.clone();
        stores.works.write().unwrap().insert(wi2_id.clone(), wi2);

        let mut coord_state = CoordinatorState::new("goal-1".to_string(), InterviewMode::Interactive);
        coord_state.current_phase_id = Some(phase_id);

        let status = build_phase_status(&stores, &coord_state);
        assert!(status.contains("READY"), "dependency met should show READY: {}", status);
        assert!(status.contains("deps:"), "should show deps info: {}", status);
    }

    #[test]
    fn test_build_phase_status_shows_blocked_deps() {
        let dir = TestDir::new("loopr-coord-depblocked");
        let stores = test_stores(&dir);

        let phase = Phase::new("spec-1".into(), "Build Phase".into(), "d".into(), 1);
        let phase_id = phase.id.clone();
        stores.phases.write().unwrap().insert(phase_id.clone(), phase);

        let wi1 = Work::new(phase_id.clone(), "Setup".into(), "d".into());
        // Default status is Draft, not Done
        let wi1_id = wi1.id.clone();
        stores.works.write().unwrap().insert(wi1_id.clone(), wi1);

        let mut wi2 = Work::new(phase_id.clone(), "Build".into(), "d".into());
        wi2.dependencies = vec![wi1_id.clone()];
        stores.works.write().unwrap().insert(wi2.id.clone(), wi2);

        let mut coord_state = CoordinatorState::new("goal-1".to_string(), InterviewMode::Interactive);
        coord_state.current_phase_id = Some(phase_id);

        let status = build_phase_status(&stores, &coord_state);
        assert!(
            status.contains("BLOCKED"),
            "unmet dependency should show BLOCKED: {}",
            status
        );
    }

    // --- Fix #4: retry enforcement tests ---

    #[test]
    fn test_increment_attempts_tracks_retries() {
        let mut coord_state = CoordinatorState::new("goal-1".to_string(), InterviewMode::Interactive);
        assert_eq!(coord_state.attempts("wi-1"), 0);
        assert_eq!(coord_state.increment_attempts("wi-1"), 1);
        assert_eq!(coord_state.increment_attempts("wi-1"), 2);
        assert_eq!(coord_state.increment_attempts("wi-1"), 3);
        assert_eq!(coord_state.attempts("wi-1"), 3);
    }

    #[test]
    fn test_decrement_attempts_on_dependency_not_met() {
        let mut coord_state = CoordinatorState::new("goal-1".to_string(), InterviewMode::Interactive);
        // Simulate: increment, then undo (DependencyNotMet)
        coord_state.increment_attempts("wi-1");
        assert_eq!(coord_state.attempts("wi-1"), 1);

        // Undo — same as the production code does
        if let Some(count) = coord_state.work_attempts.get_mut("wi-1") {
            *count = count.saturating_sub(1);
        }
        assert_eq!(coord_state.attempts("wi-1"), 0);
    }

    // --- Fix #9: failure learnings in build_phase_status ---

    #[test]
    fn test_build_phase_status_includes_failure_learnings() {
        let dir = TestDir::new("loopr-coord-learnings");
        let stores = test_stores(&dir);

        let phase = Phase::new("spec-1".into(), "Build Phase".into(), "d".into(), 1);
        let phase_id = phase.id.clone();
        stores.phases.write().unwrap().insert(phase_id.clone(), phase);

        let mut wi1 = Work::new(phase_id.clone(), "Failing WI".into(), "d".into());
        wi1.status = WorkStatus::InProgress;
        let wi1_id = wi1.id.clone();
        stores.works.write().unwrap().insert(wi1_id.clone(), wi1);

        // Insert a failure Learning scoped to this WI's phase
        let learning = crate::domain::learning::Learning {
            id: crate::id::generate_id("ln"),
            source_id: wi1_id,
            scope: LearningScope::Phase,
            content: "Build failed due to missing dependency".to_string(),
            reinforcements: 0,
            contradictions: 0,
            promoted: false,
            created_at: crate::id::now_millis(),
            updated_at: crate::id::now_millis(),
            applicable_roles: None,
            resource_tags: vec![],
            confidence: 0.5,
        };
        stores.learnings.write().unwrap().insert(learning.id.clone(), learning);

        let mut coord_state = CoordinatorState::new("goal-1".to_string(), InterviewMode::Interactive);
        coord_state.current_phase_id = Some(phase_id);

        let status = build_phase_status(&stores, &coord_state);
        assert!(
            status.contains("failure learnings"),
            "should include failure learnings section: {}",
            status
        );
        assert!(
            status.contains("missing dependency"),
            "should include learning content: {}",
            status
        );
    }

    // --- C2: Recently Merged Bundles in state summary ---

    #[test]
    fn test_build_state_summary_includes_recently_merged_bundles() {
        let dir = TestDir::new("loopr-coord-c2-merged");
        let stores = test_stores(&dir);

        // Create a WI in Integrated status (not Done)
        let mut wi = Work::new("phase-1".into(), "Test WI".into(), "desc".into());
        wi.status = WorkStatus::Integrated;
        let wi_id = wi.id.clone();
        stores.works.write().unwrap().insert(wi_id.clone(), wi);

        // Create a merged bundle for that WI
        let mut bundle = Bundle::new(wi_id.clone(), None, "feature/test".into(), vec!["claim".into()]);
        bundle.status = BundleStatus::Merged;
        let bundle_id = bundle.id.clone();
        stores.bundles.write().unwrap().insert(bundle_id.clone(), bundle);

        let agent_log = test_agent_logger(&dir);
        let summary = build_state_summary(&stores, &agent_log);
        assert!(
            summary.contains("Recently Merged Bundles"),
            "should include recently merged bundles section: {}",
            summary
        );
        assert!(summary.contains(&wi_id), "should link to parent WI: {}", summary);
    }

    #[test]
    fn test_build_state_summary_excludes_merged_when_wi_done() {
        let dir = TestDir::new("loopr-coord-c2-done");
        let stores = test_stores(&dir);

        // Create a WI in Done status
        let mut wi = Work::new("phase-1".into(), "Done WI".into(), "desc".into());
        wi.status = WorkStatus::Done;
        let wi_id = wi.id.clone();
        stores.works.write().unwrap().insert(wi_id.clone(), wi);

        // Create a merged bundle for that WI
        let mut bundle = Bundle::new(wi_id.clone(), None, "feature/done".into(), vec!["claim".into()]);
        bundle.status = BundleStatus::Merged;
        stores.bundles.write().unwrap().insert(bundle.id.clone(), bundle);

        let agent_log = test_agent_logger(&dir);
        let summary = build_state_summary(&stores, &agent_log);
        assert!(
            !summary.contains("Recently Merged Bundles"),
            "should NOT include merged bundles when WI is Done: {}",
            summary
        );
    }

    // --- C3: Rejected Bundles state summary tests ---

    #[test]
    fn test_build_state_summary_includes_rejected_bundle_with_inreview_work() {
        let dir = TestDir::new("loopr-coord-c3-rej");
        let stores = test_stores(&dir);

        // Create a WI in InReview status
        let mut wi = Work::new("phase-1".into(), "Test WI".into(), "desc".into());
        wi.status = WorkStatus::InReview;
        let wi_id = wi.id.clone();
        stores.works.write().unwrap().insert(wi_id.clone(), wi);

        // Create a rejected bundle for that WI
        let mut bundle = Bundle::new(wi_id.clone(), None, "agent/test".into(), vec!["claim".into()]);
        bundle.status = BundleStatus::Rejected;
        let bundle_id = bundle.id.clone();
        stores.bundles.write().unwrap().insert(bundle_id.clone(), bundle);

        let agent_log = test_agent_logger(&dir);
        let summary = build_state_summary(&stores, &agent_log);
        assert!(
            summary.contains("Rejected Bundles"),
            "should include rejected bundles section: {}",
            summary
        );
        assert!(
            summary.contains(&bundle_id),
            "should include the bundle ID: {}",
            summary
        );
        assert!(summary.contains(&wi_id), "should include the work ID: {}", summary);
        assert!(
            summary.contains("override_work"),
            "should include override_work directive: {}",
            summary
        );
    }

    #[test]
    fn test_build_state_summary_rejected_bundle_includes_verification_reason() {
        let dir = TestDir::new("loopr-coord-c3-reason");
        let stores = test_stores(&dir);

        let mut wi = Work::new("phase-1".into(), "Test WI".into(), "desc".into());
        wi.status = WorkStatus::InReview;
        let wi_id = wi.id.clone();
        stores.works.write().unwrap().insert(wi_id.clone(), wi);

        let mut bundle = Bundle::new(wi_id.clone(), None, "agent/test".into(), vec!["claim".into()]);
        bundle.status = BundleStatus::Rejected;
        bundle.verification = "Rejected: missing error handling".to_string();
        stores.bundles.write().unwrap().insert(bundle.id.clone(), bundle);

        let agent_log = test_agent_logger(&dir);
        let summary = build_state_summary(&stores, &agent_log);
        assert!(
            summary.contains("missing error handling"),
            "should include rejection reason from verification: {}",
            summary
        );
    }

    #[test]
    fn test_build_state_summary_rejected_bundle_fallback_reason_when_empty() {
        let dir = TestDir::new("loopr-coord-c3-noverify");
        let stores = test_stores(&dir);

        let mut wi = Work::new("phase-1".into(), "Test WI".into(), "desc".into());
        wi.status = WorkStatus::InReview;
        let wi_id = wi.id.clone();
        stores.works.write().unwrap().insert(wi_id.clone(), wi);

        let mut bundle = Bundle::new(wi_id.clone(), None, "agent/test".into(), vec!["claim".into()]);
        bundle.status = BundleStatus::Rejected;
        // verification left empty
        stores.bundles.write().unwrap().insert(bundle.id.clone(), bundle);

        let agent_log = test_agent_logger(&dir);
        let summary = build_state_summary(&stores, &agent_log);
        assert!(
            summary.contains("bundle was rejected by reviewer"),
            "should use fallback reason when verification is empty: {}",
            summary
        );
    }

    #[test]
    fn test_build_state_summary_excludes_rejected_when_work_not_inreview() {
        let dir = TestDir::new("loopr-coord-c3-noshow");
        let stores = test_stores(&dir);

        // Work already transitioned back to InProgress
        let mut wi = Work::new("phase-1".into(), "Test WI".into(), "desc".into());
        wi.status = WorkStatus::InProgress;
        let wi_id = wi.id.clone();
        stores.works.write().unwrap().insert(wi_id.clone(), wi);

        let mut bundle = Bundle::new(wi_id.clone(), None, "agent/test".into(), vec!["claim".into()]);
        bundle.status = BundleStatus::Rejected;
        stores.bundles.write().unwrap().insert(bundle.id.clone(), bundle);

        let agent_log = test_agent_logger(&dir);
        let summary = build_state_summary(&stores, &agent_log);
        assert!(
            !summary.contains("Rejected Bundles"),
            "should NOT show rejected bundles when work is not InReview: {}",
            summary
        );
    }

    // --- L2: find_pending_draft_for_validation scoping tests ---

    #[test]
    fn test_find_draft_scoped_to_active_plan() {
        let dir = TestDir::new("loopr-coord-l2-scope");
        let stores = test_stores(&dir);

        // Create an active plan
        let mut plan = Plan::new("Active Plan".into(), "desc".into(), "crit".into());
        plan.status = HierarchyStatus::Active;
        let plan_id = plan.id.clone();
        stores.plans.write().unwrap().insert(plan_id.clone(), plan);

        // Create a Draft Spec for the active plan
        let mut spec = Spec::new(plan_id.clone(), "Draft Spec".into(), "desc".into());
        spec.status = HierarchyStatus::Draft;
        let spec_id = spec.id.clone();
        stores.specs.write().unwrap().insert(spec_id.clone(), spec);

        let result = find_pending_draft_for_validation(&stores);
        assert!(result.is_some(), "should find Draft Spec under active Plan");
        let (level, id, _) = result.unwrap();
        assert_eq!(level, "Spec");
        assert_eq!(id, spec_id);
    }

    #[test]
    fn test_find_draft_ignores_orphan_from_other_plan() {
        let dir = TestDir::new("loopr-coord-l2-orphan");
        let stores = test_stores(&dir);

        // Create an active plan
        let mut plan = Plan::new("Active Plan".into(), "desc".into(), "crit".into());
        plan.status = HierarchyStatus::Active;
        let plan_id = plan.id.clone();
        stores.plans.write().unwrap().insert(plan_id.clone(), plan);

        // Create an active spec for the active plan
        let mut spec = Spec::new(plan_id, "Active Spec".into(), "desc".into());
        spec.status = HierarchyStatus::Active;
        let spec_id = spec.id.clone();
        stores.specs.write().unwrap().insert(spec_id.clone(), spec);

        // Create a Draft Spec from a DIFFERENT plan (orphan)
        let mut orphan_spec = Spec::new("old-plan-id".into(), "Orphan Spec".into(), "desc".into());
        orphan_spec.status = HierarchyStatus::Draft;
        stores
            .specs
            .write()
            .unwrap()
            .insert(orphan_spec.id.clone(), orphan_spec);

        // Should NOT find the orphan spec, and no Draft Phase exists for active spec
        let result = find_pending_draft_for_validation(&stores);
        assert!(result.is_none(), "should NOT find orphan Draft from different Plan");
    }

    #[test]
    fn test_find_draft_returns_none_when_no_drafts() {
        let dir = TestDir::new("loopr-coord-l2-none");
        let stores = test_stores(&dir);

        let result = find_pending_draft_for_validation(&stores);
        assert!(result.is_none());
    }

    // --- Self-correction loop tests for Coordinator ---

    #[tokio::test]
    async fn test_coordinator_self_correction_parse_failure_then_success() {
        // First LLM response is malformed, second is valid JSON.
        // Self-correction loop should re-prompt and succeed within the same iteration.
        let dir = TestDir::new("loopr-coord-selfcorr1");
        let stores = test_stores(&dir);

        let agent = test_coordinator(
            &dir,
            &stores,
            vec![
                "Let me think about the plan first.".to_string(), // malformed
                r#"[{"action": "done", "summary": "Self-corrected coordinator"}]"#.to_string(), // valid
            ],
            CoordinatorConfig::default(),
        );

        let outcome = agent
            .run_iteration(
                &mut CoordinatorState::new("test-goal".to_string(), InterviewMode::Interactive),
                &mut Lifeguard::new(),
            )
            .await
            .unwrap();

        assert!(
            matches!(outcome, IterationOutcome::Done(ref s) if s.contains("Self-corrected")),
            "expected Done after self-correction, got: {:?}",
            outcome
        );
    }

    #[tokio::test]
    async fn test_coordinator_self_correction_max_requeries_exceeded() {
        // All responses are malformed. After max_requeries retries, should return Err.
        let dir = TestDir::new("loopr-coord-selfcorr2");
        let stores = test_stores(&dir);

        let agent = test_coordinator(
            &dir,
            &stores,
            vec![
                "bad 1".to_string(),
                "bad 2".to_string(),
                "bad 3".to_string(),
                "bad 4".to_string(), // max_requeries=3: initial + 3 retries
            ],
            CoordinatorConfig::default(),
        );

        let result = agent
            .run_iteration(
                &mut CoordinatorState::new("test-goal".to_string(), InterviewMode::Interactive),
                &mut Lifeguard::new(),
            )
            .await;

        assert!(result.is_err(), "expected error when max_requeries exceeded");
        assert!(
            result.unwrap_err().to_string().contains("failed to parse"),
            "error should be a parse error"
        );
    }

    // --- sweep_integrated_to_done tests ---

    #[test]
    fn test_sweep_integrated_to_done_transitions_works() {
        let dir = TestDir::new("loopr-coord-sweep-basic");
        let stores = test_stores(&dir);

        let phase = Phase::new("spec-1".into(), "Phase 1".into(), "desc".into(), 1);
        let phase_id = phase.id.clone();
        stores.phases.write().unwrap().insert(phase_id.clone(), phase);

        // Insert a Work directly at Integrated status
        let mut wi = Work::new(phase_id.clone(), "WI 1".into(), "desc".into());
        wi.status = WorkStatus::Integrated;
        let wi_id = wi.id.clone();
        stores.works.write().unwrap().insert(wi_id.clone(), wi);

        let mut coord_state = CoordinatorState::new("goal-1".to_string(), InterviewMode::Interactive);
        coord_state.fsm_state = CoordinatorFsmState::Executing;
        coord_state.current_phase_id = Some(phase_id);

        let (event_tx, _rx) = broadcast::channel(16);
        let worktree_mgr = WorktreeManager::new(dir.to_path_buf(), dir.join(".wt"));
        let bridge = AgentIpcBridge::new(stores.clone(), event_tx, worktree_mgr, stores.config.clone());
        let agent_log = test_agent_logger(&dir);

        sweep_integrated_to_done(&stores, &coord_state, &bridge, &agent_log);

        // Verify the Work is now Done
        let works = stores.works.read().unwrap();
        let updated = works.get(&wi_id).unwrap();
        assert_eq!(updated.status, WorkStatus::Done);
    }

    #[test]
    fn test_sweep_noop_when_no_integrated_works() {
        let dir = TestDir::new("loopr-coord-sweep-noop");
        let stores = test_stores(&dir);

        let phase = Phase::new("spec-1".into(), "Phase 1".into(), "desc".into(), 1);
        let phase_id = phase.id.clone();
        stores.phases.write().unwrap().insert(phase_id.clone(), phase);

        // Insert a Work at Ready (not Integrated)
        let wi = Work::new(phase_id.clone(), "WI 1".into(), "desc".into());
        let wi_id = wi.id.clone();
        stores.works.write().unwrap().insert(wi_id.clone(), wi);

        let mut coord_state = CoordinatorState::new("goal-1".to_string(), InterviewMode::Interactive);
        coord_state.fsm_state = CoordinatorFsmState::Executing;
        coord_state.current_phase_id = Some(phase_id);

        let (event_tx, _rx) = broadcast::channel(16);
        let worktree_mgr = WorktreeManager::new(dir.to_path_buf(), dir.join(".wt"));
        let bridge = AgentIpcBridge::new(stores.clone(), event_tx, worktree_mgr, stores.config.clone());
        let agent_log = test_agent_logger(&dir);

        sweep_integrated_to_done(&stores, &coord_state, &bridge, &agent_log);

        // Work should still be Draft (unchanged by sweep)
        let works = stores.works.read().unwrap();
        let unchanged = works.get(&wi_id).unwrap();
        assert_eq!(unchanged.status, WorkStatus::Draft);
    }

    #[test]
    fn test_sweep_then_fsm_advances_to_phase_gate() {
        let dir = TestDir::new("loopr-coord-sweep-fsmadv");
        let stores = test_stores(&dir);

        let phase = Phase::new("spec-1".into(), "Phase 1".into(), "desc".into(), 1);
        let phase_id = phase.id.clone();
        stores.phases.write().unwrap().insert(phase_id.clone(), phase);

        // All Works in phase are Integrated — sweep should transition them to Done
        let mut wi1 = Work::new(phase_id.clone(), "WI 1".into(), "desc".into());
        wi1.status = WorkStatus::Integrated;
        stores.works.write().unwrap().insert(wi1.id.clone(), wi1);

        let mut wi2 = Work::new(phase_id.clone(), "WI 2".into(), "desc".into());
        wi2.status = WorkStatus::Integrated;
        stores.works.write().unwrap().insert(wi2.id.clone(), wi2);

        let mut coord_state = CoordinatorState::new("goal-1".to_string(), InterviewMode::Interactive);
        coord_state.fsm_state = CoordinatorFsmState::Executing;
        coord_state.current_phase_id = Some(phase_id);

        let (event_tx, _rx) = broadcast::channel(16);
        let worktree_mgr = WorktreeManager::new(dir.to_path_buf(), dir.join(".wt"));
        let bridge = AgentIpcBridge::new(stores.clone(), event_tx, worktree_mgr, stores.config.clone());
        let agent_log = test_agent_logger(&dir);

        // Before sweep: FSM should NOT advance (Works are Integrated, not terminal)
        let config = CoordinatorConfig::default();
        assert_eq!(
            check_fsm_transition(&stores, &coord_state, &config),
            None,
            "FSM should not advance while Works are Integrated"
        );

        // Run sweep
        sweep_integrated_to_done(&stores, &coord_state, &bridge, &agent_log);

        // After sweep: FSM should advance to PhaseGate (all Works now Done)
        assert_eq!(
            check_fsm_transition(&stores, &coord_state, &config),
            Some(CoordinatorFsmState::PhaseGate),
            "FSM should advance to PhaseGate after sweep transitions all Works to Done"
        );
    }

    // --- query_learnings_for_level tests ---

    #[test]
    fn test_query_learnings_for_level_scope_filtering() {
        use crate::domain::learning::Learning;

        let dir = TestDir::new("loopr-coord-learnscope");
        let stores = test_stores(&dir);

        // Populate learnings at every scope
        let scoped = [
            ("work-src", LearningScope::Work, "work insight"),
            ("phase-src", LearningScope::Phase, "phase insight"),
            ("spec-src", LearningScope::Spec, "spec insight"),
            ("plan-src", LearningScope::Plan, "plan insight"),
            ("global-src", LearningScope::Global, "global insight"),
        ];
        for (src, scope, content) in &scoped {
            let l = Learning::new(src.to_string(), *scope, content.to_string());
            stores.learnings.write().unwrap().insert(l.id.clone(), l);
        }

        // Plan level: Plan + Global
        let plan_learnings = query_learnings_for_level(&stores, GenerationLevel::Plan);
        assert_eq!(plan_learnings.len(), 2);
        assert!(plan_learnings.contains(&"plan insight".to_string()));
        assert!(plan_learnings.contains(&"global insight".to_string()));

        // Spec level: Spec + Plan + Global
        let spec_learnings = query_learnings_for_level(&stores, GenerationLevel::Spec);
        assert_eq!(spec_learnings.len(), 3);
        assert!(spec_learnings.contains(&"spec insight".to_string()));
        assert!(spec_learnings.contains(&"plan insight".to_string()));
        assert!(spec_learnings.contains(&"global insight".to_string()));

        // Phase level: Phase + Spec + Plan + Global
        let phase_learnings = query_learnings_for_level(&stores, GenerationLevel::Phase);
        assert_eq!(phase_learnings.len(), 4);
        assert!(phase_learnings.contains(&"phase insight".to_string()));
        assert!(!phase_learnings.contains(&"work insight".to_string()));

        // Work level: all 5 scopes
        let work_learnings = query_learnings_for_level(&stores, GenerationLevel::Work);
        assert_eq!(work_learnings.len(), 5);
        assert!(work_learnings.contains(&"work insight".to_string()));
    }

    // --- Case 0 bubble-up decision tree tests ---

    #[test]
    fn test_build_generation_footer_case0_revise_parent() {
        use crate::domain::coverage::{CoverageReport, CoverageReportParams, CoverageVerdict};

        crate::prompts::init_defaults();
        let dir = TestDir::new("loopr-coord-case0-revise");
        let stores = test_stores(&dir);

        // Active Plan
        let mut plan = Plan::new("Active Plan".into(), "desc".into(), "criteria".into());
        plan.status = HierarchyStatus::Active;
        let plan_id = plan.id.clone();
        stores.plans.write().unwrap().insert(plan_id.clone(), plan);

        // Active Spec under plan (so decomposition target is spec, not plan)
        let mut spec = Spec::new(plan_id, "Spec 1".into(), "desc".into());
        spec.status = HierarchyStatus::Active;
        let spec_id = spec.id.clone();
        stores.specs.write().unwrap().insert(spec_id.clone(), spec);

        // Incomplete coverage report for spec
        let cr = CoverageReport::new(CoverageReportParams {
            parent_collection: "spec".into(),
            parent_id: spec_id.clone(),
            children_collection: "phases".into(),
            children_ids: vec![],
            verdict: CoverageVerdict::Incomplete,
            gaps: vec![],
            out_of_scope: vec![],
            summary: "Gaps found".into(),
            model_used: "test".into(),
        });
        stores.coverage_reports.write().unwrap().insert(cr.id.clone(), cr);

        // CoordinatorState with decomposition attempts >= max (3 >= 3)
        let mut coord_state = CoordinatorState::new("goal-1".to_string(), InterviewMode::Interactive);
        for _ in 0..3 {
            coord_state.increment_decomposition_attempts(&spec_id);
        }
        // bubble_up_count = 0 (under max_bubble_up_depth = 2)

        let agent_log = test_agent_logger(&dir);
        let footer = build_generation_footer(
            &stores,
            "Build auth",
            3,
            None,
            &agent_log,
            Some(&coord_state),
            Some(3),
            Some(2),
        );

        assert!(footer.is_some(), "should emit revise_parent prompt");
        let text = footer.unwrap();
        assert!(
            text.contains("revise_parent"),
            "should contain revise_parent action: {}",
            text
        );
        assert!(
            text.contains("Bubble-Up Required"),
            "should contain bubble-up header: {}",
            text
        );
    }

    #[test]
    fn test_build_generation_footer_case0_need_help_depth_exhausted() {
        use crate::domain::coverage::{CoverageReport, CoverageReportParams, CoverageVerdict};

        crate::prompts::init_defaults();
        let dir = TestDir::new("loopr-coord-case0-depth");
        let stores = test_stores(&dir);

        // Active Plan
        let mut plan = Plan::new("Active Plan".into(), "desc".into(), "criteria".into());
        plan.status = HierarchyStatus::Active;
        let plan_id = plan.id.clone();
        stores.plans.write().unwrap().insert(plan_id.clone(), plan);

        // Active Spec
        let mut spec = Spec::new(plan_id, "Spec 1".into(), "desc".into());
        spec.status = HierarchyStatus::Active;
        let spec_id = spec.id.clone();
        stores.specs.write().unwrap().insert(spec_id.clone(), spec);

        // Incomplete coverage report
        let cr = CoverageReport::new(CoverageReportParams {
            parent_collection: "spec".into(),
            parent_id: spec_id.clone(),
            children_collection: "phases".into(),
            children_ids: vec![],
            verdict: CoverageVerdict::Incomplete,
            gaps: vec![],
            out_of_scope: vec![],
            summary: "Gaps found".into(),
            model_used: "test".into(),
        });
        stores.coverage_reports.write().unwrap().insert(cr.id.clone(), cr);

        // decomposition_attempts >= max
        let mut coord_state = CoordinatorState::new("goal-1".to_string(), InterviewMode::Interactive);
        for _ in 0..3 {
            coord_state.increment_decomposition_attempts(&spec_id);
        }
        // bubble_up_count >= max_bubble_up_depth (2 >= 2)
        coord_state.increment_bubble_up();
        coord_state.increment_bubble_up();

        let agent_log = test_agent_logger(&dir);
        let footer = build_generation_footer(
            &stores,
            "Build auth",
            3,
            None,
            &agent_log,
            Some(&coord_state),
            Some(3),
            Some(2),
        );

        assert!(footer.is_some(), "should emit need_help prompt");
        let text = footer.unwrap();
        assert!(
            text.contains("need_help"),
            "should contain need_help when depth exhausted: {}",
            text
        );
    }

    #[test]
    fn test_build_generation_footer_case0_need_help_plan_level() {
        use crate::domain::coverage::{CoverageReport, CoverageReportParams, CoverageVerdict};

        crate::prompts::init_defaults();
        let dir = TestDir::new("loopr-coord-case0-plan");
        let stores = test_stores(&dir);

        // Active Plan (the parent that has incomplete coverage)
        let mut plan = Plan::new("Active Plan".into(), "desc".into(), "criteria".into());
        plan.status = HierarchyStatus::Active;
        let plan_id = plan.id.clone();
        stores.plans.write().unwrap().insert(plan_id.clone(), plan);

        // Incomplete coverage report for the plan itself
        let cr = CoverageReport::new(CoverageReportParams {
            parent_collection: "plan".into(),
            parent_id: plan_id.clone(),
            children_collection: "specs".into(),
            children_ids: vec![],
            verdict: CoverageVerdict::Incomplete,
            gaps: vec![],
            out_of_scope: vec![],
            summary: "Gaps found".into(),
            model_used: "test".into(),
        });
        stores.coverage_reports.write().unwrap().insert(cr.id.clone(), cr);

        // decomposition_attempts >= max
        let mut coord_state = CoordinatorState::new("goal-1".to_string(), InterviewMode::Interactive);
        for _ in 0..3 {
            coord_state.increment_decomposition_attempts(&plan_id);
        }
        // bubble_up_count = 0 (under limit, but collection is "plan" so can't bubble up)

        let agent_log = test_agent_logger(&dir);
        let footer = build_generation_footer(
            &stores,
            "Build auth",
            3,
            None,
            &agent_log,
            Some(&coord_state),
            Some(3),
            Some(2),
        );

        assert!(footer.is_some(), "should emit need_help for plan-level");
        let text = footer.unwrap();
        assert!(
            text.contains("need_help"),
            "should contain need_help when collection is plan (can't revise above plan): {}",
            text
        );
    }

    // --- Case 1 learning injection test ---

    #[test]
    fn test_build_generation_footer_case1_includes_learnings() {
        use crate::domain::learning::Learning;

        crate::prompts::init_defaults();
        let dir = TestDir::new("loopr-coord-case1-learn");
        let stores = test_stores(&dir);

        // No plans exist -> generation needed at Plan level (Case 1)
        // Add a Plan-scoped learning and a Global-scoped learning
        let l1 = Learning::new(
            "src-1".into(),
            LearningScope::Plan,
            "Plan-level diagnostic context".into(),
        );
        let l2 = Learning::new("src-2".into(), LearningScope::Global, "Global best practice".into());
        stores.learnings.write().unwrap().insert(l1.id.clone(), l1);
        stores.learnings.write().unwrap().insert(l2.id.clone(), l2);

        let agent_log = test_agent_logger(&dir);
        let footer = build_generation_footer(&stores, "Build auth system", 3, None, &agent_log, None, None, None);

        assert!(footer.is_some(), "should generate Plan-level prompt");
        let text = footer.unwrap();
        assert!(
            text.contains("Plan-level diagnostic context"),
            "prompt should include Plan-scoped learning: {}",
            text
        );
        assert!(
            text.contains("Global best practice"),
            "prompt should include Global-scoped learning: {}",
            text
        );
    }

    #[test]
    fn test_last_error_kind_for_work_returns_none_when_no_sessions() {
        let dir = TestDir::new("loopr-coord-errk-none");
        let stores = test_stores(&dir);
        assert!(last_error_kind_for_work(&stores, "wi-1").is_none());
    }

    #[test]
    fn test_last_error_kind_for_work_returns_structural_error() {
        let dir = TestDir::new("loopr-coord-errk-struct");
        let stores = test_stores(&dir);

        let mut session = AgentSession::new(AgentType::Implementer, "model".into());
        session.work_id = Some("wi-1".to_string());
        session.status = AgentStatus::Failed;
        session.error_kind = Some(AgentErrorKind::ContextOverflow);
        stores
            .agent_sessions
            .write()
            .unwrap()
            .insert(session.id.clone(), session);

        let kind = last_error_kind_for_work(&stores, "wi-1");
        assert_eq!(kind, Some(AgentErrorKind::ContextOverflow));
    }

    #[test]
    fn test_last_error_kind_for_work_ignores_non_failed_sessions() {
        let dir = TestDir::new("loopr-coord-errk-nonfail");
        let stores = test_stores(&dir);

        let mut session = AgentSession::new(AgentType::Implementer, "model".into());
        session.work_id = Some("wi-1".to_string());
        session.status = AgentStatus::Completed;
        session.error_kind = Some(AgentErrorKind::ContextOverflow);
        stores
            .agent_sessions
            .write()
            .unwrap()
            .insert(session.id.clone(), session);

        assert!(last_error_kind_for_work(&stores, "wi-1").is_none());
    }

    #[test]
    fn test_last_error_kind_for_work_ignores_other_works() {
        let dir = TestDir::new("loopr-coord-errk-other");
        let stores = test_stores(&dir);

        let mut session = AgentSession::new(AgentType::Implementer, "model".into());
        session.work_id = Some("wi-2".to_string());
        session.status = AgentStatus::Failed;
        session.error_kind = Some(AgentErrorKind::ContextOverflow);
        stores
            .agent_sessions
            .write()
            .unwrap()
            .insert(session.id.clone(), session);

        assert!(last_error_kind_for_work(&stores, "wi-1").is_none());
    }

    // --- phase_missing_test_tool tests ---

    #[test]
    fn test_phase_missing_test_tool_no_validation_commands() {
        let dir = TestDir::new("loopr-coord-toolguard-novc");
        let stores = test_stores(&dir);

        let phase = Phase::new("spec-1".into(), "Phase 1".into(), "desc".into(), 1);
        let phase_id = phase.id.clone();
        stores.phases.write().unwrap().insert(phase_id.clone(), phase);

        let mut coord_state = CoordinatorState::new("goal-1".to_string(), InterviewMode::Interactive);
        coord_state.current_phase_id = Some(phase_id);

        let warning = phase_missing_test_tool(&stores, &coord_state);
        assert!(warning.is_empty(), "no warning when phase has no validation_commands");
    }

    #[test]
    fn test_phase_missing_test_tool_warns_when_no_tool() {
        let dir = TestDir::new("loopr-coord-toolguard-warn");
        let stores = test_stores(&dir);

        let mut phase = Phase::new("spec-1".into(), "Phase 1".into(), "desc".into(), 1);
        phase.validation_commands = vec!["cargo test".to_string()];
        let phase_id = phase.id.clone();
        stores.phases.write().unwrap().insert(phase_id.clone(), phase);

        let mut coord_state = CoordinatorState::new("goal-1".to_string(), InterviewMode::Interactive);
        coord_state.current_phase_id = Some(phase_id);

        let warning = phase_missing_test_tool(&stores, &coord_state);
        assert!(
            !warning.is_empty(),
            "should warn when validation_commands exist but no test tool"
        );
        assert!(warning.contains("WARNING"));
        assert!(warning.contains("register_tool"));
        assert!(warning.contains("cargo test"));
    }

    #[test]
    fn test_phase_missing_test_tool_no_warning_when_tool_registered() {
        let dir = TestDir::new("loopr-coord-toolguard-ok");
        let stores = test_stores(&dir);

        let mut phase = Phase::new("spec-1".into(), "Phase 1".into(), "desc".into(), 1);
        phase.validation_commands = vec!["cargo test".to_string()];
        let phase_id = phase.id.clone();
        stores.phases.write().unwrap().insert(phase_id.clone(), phase);

        // Register a test tool in the runner
        let tool = crate::config::ToolEntry {
            name: "test".into(),
            command: "echo ok".into(),
            timeout_secs: 300,
            worktree: true,
        };
        *stores.tool_runner.write().unwrap() = Arc::new(crate::tools::ToolRunner::new(&[tool]));

        let mut coord_state = CoordinatorState::new("goal-1".to_string(), InterviewMode::Interactive);
        coord_state.current_phase_id = Some(phase_id);

        let warning = phase_missing_test_tool(&stores, &coord_state);
        assert!(warning.is_empty(), "no warning when test tool is registered");
    }
